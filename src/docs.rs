use crate::{
    AppState,
    auth::{current_user, current_user_next, escape, page},
};
use anyhow::Result;
use axum::{
    extract::{Multipart, Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use axum_extra::extract::cookie::PrivateCookieJar;
use openidconnect::CsrfToken;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use time::{OffsetDateTime, UtcOffset};

pub const MAX_BYTES: usize = 10 * 1024 * 1024;
const LIST_LIMIT: i64 = 200;

pub struct Doc {
    pub id: String,
    pub title: String,
    pub project: String,
    pub uploader_email: String,
    pub uploader_name: String,
    pub created_at: i64,
}

/// 문서 메타데이터(SQLite)와 파일(디스크)을 함께 다룬다.
/// 파일 저장은 put_file/read_file 두 곳뿐이라 R2 로 옮길 때 여기만 바꾼다.
#[derive(Clone)]
pub struct Docs {
    db: Arc<Mutex<Connection>>,
    dir: PathBuf,
}

impl Docs {
    pub fn open(db_path: &str, dir: &str) -> Result<Self> {
        let conn = Connection::open(db_path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS documents (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                project TEXT NOT NULL,
                uploader_email TEXT NOT NULL,
                uploader_name TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                deleted_at INTEGER
            )",
        )?;
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            db: Arc::new(Mutex::new(conn)),
            dir: dir.into(),
        })
    }

    fn put_file(&self, id: &str, bytes: &[u8]) -> Result<()> {
        Ok(std::fs::write(self.dir.join(format!("{id}.html")), bytes)?)
    }

    fn read_file(&self, id: &str) -> Result<Vec<u8>> {
        Ok(std::fs::read(self.dir.join(format!("{id}.html")))?)
    }

    /// 새 문서를 저장하고 추측할 수 없는 번호를 돌려준다. 파일을 먼저 쓰고 기록을 남긴다.
    pub fn insert(&self, title: &str, project: &str, by: &crate::users::User, bytes: &[u8]) -> Result<String> {
        let id = CsrfToken::new_random().secret().clone();
        self.put_file(&id, bytes)?;
        self.db.lock().unwrap().execute(
            "INSERT INTO documents (id, title, project, uploader_email, uploader_name, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, title, project, by.email, by.name, now_unix()],
        )?;
        Ok(id)
    }

    /// 삭제(숨김)되지 않은 문서만 돌려준다.
    pub fn get(&self, id: &str) -> Result<Option<Doc>> {
        let conn = self.db.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT id, title, project, uploader_email, uploader_name, created_at
                 FROM documents WHERE id = ?1 AND deleted_at IS NULL",
                [id],
                row_to_doc,
            )
            .optional()?)
    }

    pub fn read(&self, id: &str) -> Result<Vec<u8>> {
        self.read_file(id)
    }

    /// 제목 또는 올린 사람에 검색어가 든 문서를 최신순으로 돌려준다. `project` 가 비어 있지 않으면 그 프로젝트만.
    /// LIKE 대신 instr 를 써서 검색어의 % 나 _ 가 특수문자로 해석되지 않게 한다.
    pub fn list(&self, q: &str, project: &str) -> Result<Vec<Doc>> {
        let conn = self.db.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, title, project, uploader_email, uploader_name, created_at
             FROM documents
             WHERE deleted_at IS NULL
               AND (?1 = '' OR instr(lower(title), lower(?1)) > 0 OR instr(lower(uploader_name), lower(?1)) > 0)
               AND (?2 = '' OR project = ?2)
             ORDER BY created_at DESC, rowid DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![q, project, LIST_LIMIT], row_to_doc)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 삭제되지 않은 문서에 쓰인 프로젝트 이름들.
    pub fn projects(&self) -> Result<Vec<String>> {
        let conn = self.db.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT project FROM documents
             WHERE deleted_at IS NULL AND project <> '' ORDER BY project",
        )?;
        Ok(stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    /// 화면에서만 숨긴다. 파일과 기록은 남긴다 (정책서 D4).
    pub fn hide(&self, id: &str) -> Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE documents SET deleted_at = ?2 WHERE id = ?1 AND deleted_at IS NULL",
            params![id, now_unix()],
        )?;
        Ok(())
    }
}

fn row_to_doc(row: &rusqlite::Row) -> rusqlite::Result<Doc> {
    Ok(Doc {
        id: row.get(0)?,
        title: row.get(1)?,
        project: row.get(2)?,
        uploader_email: row.get(3)?,
        uploader_name: row.get(4)?,
        created_at: row.get(5)?,
    })
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

/// 한국 시간으로 "2026-11-25 14:03" 꼴로 보인다.
fn show_time(unix: i64) -> String {
    let Ok(t) = OffsetDateTime::from_unix_timestamp(unix) else {
        return String::new();
    };
    let t = t.to_offset(UtcOffset::from_hms(9, 0, 0).unwrap());
    format!("{} {:02}:{:02}", t.date(), t.hour(), t.minute())
}

/// 올린 HTML 을 내보내는 응답. sandbox 로 앱과 분리해 실행해서 문서 안의 JS 가 로그인 세션을 쓰지 못하게 한다 (정책서 S1).
/// allow-same-origin 은 주지 않는다.
fn document_response(bytes: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CONTENT_SECURITY_POLICY, "sandbox allow-scripts"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CACHE_CONTROL, "private, no-store"),
        ],
        bytes,
    )
        .into_response()
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, page("없는 문서입니다. <a href=\"/\">목록으로</a>")).into_response()
}

fn server_error(what: &str, e: impl std::fmt::Display) -> Response {
    eprintln!("{what} 실패: {e}");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

#[derive(Deserialize, Default)]
pub struct ListQuery {
    q: Option<String>,
    project: Option<String>,
}

pub async fn index(
    State(app): State<AppState>,
    jar: PrivateCookieJar,
    Query(query): Query<ListQuery>,
) -> Response {
    let user = match current_user(&app, &jar) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let q = query.q.unwrap_or_default();
    let project = query.project.unwrap_or_default();
    let (docs, projects) = match (app.docs.list(q.trim(), &project), app.docs.projects()) {
        (Ok(d), Ok(p)) => (d, p),
        (Err(e), _) | (_, Err(e)) => return server_error("목록 조회", e),
    };

    let options: String = projects
        .iter()
        .map(|p| {
            let sel = if *p == project { " selected" } else { "" };
            format!("<option{sel}>{}</option>", escape(p))
        })
        .collect();
    let rows: String = docs
        .iter()
        .map(|d| {
            let can_delete = d.uploader_email == user.email || user.is_admin();
            let delete = if can_delete {
                format!(
                    "<form method=\"post\" action=\"/d/{}/delete\" style=\"display:inline\">\
                     <button>삭제</button></form>",
                    escape(&d.id)
                )
            } else {
                String::new()
            };
            format!(
                "<tr><td><a href=\"/d/{}\">{}</a></td><td>{}</td><td>{}</td><td>{}</td><td>{delete}</td></tr>",
                escape(&d.id),
                escape(&d.title),
                if d.project.is_empty() { "미분류".into() } else { escape(&d.project) },
                escape(&d.uploader_name),
                show_time(d.created_at),
            )
        })
        .collect();
    let empty = if docs.is_empty() { "<p>문서가 없습니다.</p>" } else { "" };

    page(&format!(
        "<p>{}님, 안녕하세요. <a href=\"/upload\">올리기</a> \
         <form method=\"post\" action=\"/logout\" style=\"display:inline\"><button>로그아웃</button></form></p>\
         <form method=\"get\" action=\"/\">\
         <input name=\"q\" value=\"{}\" placeholder=\"제목 또는 올린 사람\"> \
         <select name=\"project\"><option value=\"\">모든 프로젝트</option>{options}</select> \
         <button>검색</button></form>\
         <table><tr><th>제목</th><th>프로젝트</th><th>올린 사람</th><th>올린 시각</th><th></th></tr>{rows}</table>{empty}",
        escape(&user.name),
        escape(&q),
    ))
    .into_response()
}

fn upload_form(app: &AppState, error: &str) -> Response {
    let known: String = app
        .docs
        .projects()
        .unwrap_or_default()
        .iter()
        .map(|p| format!("<option value=\"{}\">", escape(p)))
        .collect();
    let error = if error.is_empty() {
        String::new()
    } else {
        format!("<p style=\"color:#c00\">{}</p>", escape(error))
    };
    let status = if error.is_empty() { StatusCode::OK } else { StatusCode::BAD_REQUEST };
    (
        status,
        page(&format!(
            "<h1>문서 올리기</h1>{error}\
             <p>올린 문서는 등록된 전 직원이 볼 수 있습니다.</p>\
             <form method=\"post\" action=\"/upload\" enctype=\"multipart/form-data\">\
             <p><label>제목 <input name=\"title\" required maxlength=\"200\"></label></p>\
             <p><label>프로젝트 <input name=\"project\" list=\"projects\" maxlength=\"100\"></label>\
             <datalist id=\"projects\">{known}</datalist></p>\
             <p><label>HTML 파일 (10MB 이하) <input type=\"file\" name=\"file\" accept=\".html,.htm\" required></label></p>\
             <button>올리기</button></form><p><a href=\"/\">목록으로</a></p>"
        )),
    )
        .into_response()
}

pub async fn upload_page(State(app): State<AppState>, jar: PrivateCookieJar) -> Response {
    match current_user(&app, &jar) {
        Ok(_) => upload_form(&app, ""),
        Err(r) => r,
    }
}

pub async fn upload(State(app): State<AppState>, jar: PrivateCookieJar, mut form: Multipart) -> Response {
    let user = match current_user(&app, &jar) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let (mut title, mut project, mut file) = (String::new(), String::new(), None);
    loop {
        let field = match form.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(_) => return upload_form(&app, "파일이 너무 크거나 올바르지 않습니다. 10MB 이하의 HTML 파일만 올릴 수 있습니다."),
        };
        let name = field.name().unwrap_or_default().to_owned();
        let filename = field.file_name().unwrap_or_default().to_owned();
        let Ok(bytes) = field.bytes().await else {
            return upload_form(&app, "파일이 너무 크거나 올바르지 않습니다. 10MB 이하의 HTML 파일만 올릴 수 있습니다.");
        };
        match name.as_str() {
            "title" => title = String::from_utf8_lossy(&bytes).trim().to_owned(),
            "project" => project = String::from_utf8_lossy(&bytes).trim().to_owned(),
            "file" => file = Some((filename, bytes)),
            _ => {}
        }
    }
    if title.is_empty() {
        return upload_form(&app, "제목을 입력해 주세요.");
    }
    if title.chars().count() > 200 || project.chars().count() > 100 {
        return upload_form(&app, "제목은 200자, 프로젝트는 100자 이하로 입력해 주세요.");
    }
    let Some((filename, bytes)) = file.filter(|(_, b)| !b.is_empty()) else {
        return upload_form(&app, "HTML 파일을 선택해 주세요.");
    };
    let lower = filename.to_lowercase();
    if !(lower.ends_with(".html") || lower.ends_with(".htm")) {
        return upload_form(&app, ".html 파일만 올릴 수 있습니다.");
    }
    if bytes.len() > MAX_BYTES {
        return upload_form(&app, "파일이 10MB 를 넘습니다.");
    }
    match app.docs.insert(&title, &project, &user, &bytes) {
        Ok(id) => Redirect::to(&format!("/uploaded/{id}")).into_response(),
        Err(e) => server_error("문서 저장", e),
    }
}

/// 올린 직후 링크를 보여 준다.
pub async fn uploaded(State(app): State<AppState>, jar: PrivateCookieJar, Path(id): Path<String>) -> Response {
    if let Err(r) = current_user(&app, &jar) {
        return r;
    }
    match app.docs.get(&id) {
        Ok(Some(d)) => page(&format!(
            "<p>올렸습니다: <a href=\"/d/{id}\">{}</a></p>\
             <p>링크: <code>/d/{id}</code> (이 주소 앞에 사이트 주소를 붙여 공유하세요)</p>\
             <p><a href=\"/\">목록으로</a></p>",
            escape(&d.title),
            id = escape(&d.id),
        ))
        .into_response(),
        Ok(None) => not_found(),
        Err(e) => server_error("문서 조회", e),
    }
}

pub async fn view(State(app): State<AppState>, jar: PrivateCookieJar, Path(id): Path<String>) -> Response {
    if let Err(r) = current_user_next(&app, &jar, &format!("/d/{id}")) {
        return r;
    }
    match app.docs.get(&id) {
        Ok(Some(_)) => match app.docs.read(&id) {
            Ok(bytes) => document_response(bytes),
            Err(e) => server_error("문서 파일 읽기", e),
        },
        Ok(None) => not_found(),
        Err(e) => server_error("문서 조회", e),
    }
}

pub async fn delete(State(app): State<AppState>, jar: PrivateCookieJar, Path(id): Path<String>) -> Response {
    let user = match current_user(&app, &jar) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let doc = match app.docs.get(&id) {
        Ok(Some(d)) => d,
        Ok(None) => return not_found(),
        Err(e) => return server_error("문서 조회", e),
    };
    if doc.uploader_email != user.email && !user.is_admin() {
        return (StatusCode::FORBIDDEN, page("올린 사람이나 관리자만 삭제할 수 있습니다.")).into_response();
    }
    match app.docs.hide(&id) {
        Ok(()) => Redirect::to("/").into_response(),
        Err(e) => server_error("문서 삭제", e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::users::User;
    use axum::http::HeaderValue;

    fn user(email: &str, name: &str) -> User {
        User { email: email.into(), name: name.into(), role: "member".into() }
    }

    fn docs() -> (Docs, PathBuf) {
        let dir = std::env::temp_dir().join(format!("html-hub-test-{}", CsrfToken::new_random().secret()));
        (Docs::open(":memory:", dir.to_str().unwrap()).unwrap(), dir)
    }

    #[test]
    fn 올린_문서를_번호로_읽는다() {
        let (d, dir) = docs();
        let id = d.insert("보고서", "A", &user("a@x.im", "가나"), b"<p>hi</p>").unwrap();
        assert_eq!(d.get(&id).unwrap().unwrap().title, "보고서");
        assert_eq!(d.read(&id).unwrap(), b"<p>hi</p>");
        assert!(id.len() >= 22, "번호가 너무 짧아 추측될 수 있다");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 제목과_올린_사람으로_찾고_프로젝트로_거른다() {
        let (d, dir) = docs();
        d.insert("월간 보고", "A", &user("a@x.im", "가나"), b"1").unwrap();
        d.insert("설계 메모", "B", &user("b@x.im", "다라"), b"2").unwrap();
        assert_eq!(d.list("", "").unwrap().len(), 2);
        assert_eq!(d.list("보고", "").unwrap()[0].title, "월간 보고");
        assert_eq!(d.list("다라", "").unwrap()[0].title, "설계 메모");
        assert_eq!(d.list("", "A").unwrap().len(), 1);
        assert_eq!(d.list("%", "").unwrap().len(), 0, "% 는 특수문자가 아니라 글자로 찾는다");
        assert_eq!(d.projects().unwrap(), ["A", "B"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 최신_문서가_먼저_나온다() {
        let (d, dir) = docs();
        d.insert("첫째", "", &user("a@x.im", "가나"), b"1").unwrap();
        d.insert("둘째", "", &user("a@x.im", "가나"), b"2").unwrap();
        assert_eq!(d.list("", "").unwrap()[0].title, "둘째");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 숨긴_문서는_조회와_목록에서_빠지지만_파일은_남는다() {
        let (d, dir) = docs();
        let id = d.insert("지울 것", "A", &user("a@x.im", "가나"), b"x").unwrap();
        d.hide(&id).unwrap();
        assert!(d.get(&id).unwrap().is_none());
        assert!(d.list("", "").unwrap().is_empty());
        assert!(d.projects().unwrap().is_empty());
        assert_eq!(d.read(&id).unwrap(), b"x");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 문서_응답은_격리_헤더를_붙인다() {
        let res = document_response(b"<script>1</script>".to_vec());
        let h = res.headers();
        assert_eq!(h[header::CONTENT_SECURITY_POLICY], HeaderValue::from_static("sandbox allow-scripts"));
        assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], HeaderValue::from_static("nosniff"));
        assert!(!h[header::CONTENT_SECURITY_POLICY].to_str().unwrap().contains("allow-same-origin"));
    }
}
