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
const PAGE_SIZE: i64 = 50;

pub struct Comment {
    pub id: i64,
    pub by_email: String,
    pub by_name: String,
    pub body: String,
    pub created_at: i64,
}

pub struct Version {
    pub n: i64,
    pub by_name: String,
    pub created_at: i64,
}

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
            );
            -- 문서마다 버전이 쌓인다. 현재 버전은 번호가 가장 큰 것이다. 파일은 한 번 쓰면 바꾸지 않는다.
            CREATE TABLE IF NOT EXISTS versions (
                doc_id TEXT NOT NULL,
                n INTEGER NOT NULL,
                file TEXT NOT NULL,
                by_email TEXT NOT NULL,
                by_name TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                PRIMARY KEY (doc_id, n)
            );
            -- 버전 기능이 생기기 전에 올린 문서는 1번 버전으로 채운다.
            INSERT INTO versions (doc_id, n, file, by_email, by_name, created_at)
                SELECT id, 1, id || '.html', uploader_email, uploader_name, created_at FROM documents
                WHERE id NOT IN (SELECT doc_id FROM versions);
            -- 본문 검색용으로 문서마다 현재 버전의 보이는 글자만 둔다 (정책서 F2, D4-3).
            CREATE TABLE IF NOT EXISTS bodies (doc_id TEXT PRIMARY KEY, text TEXT NOT NULL);
            -- 댓글은 지우면 화면에서만 숨기고 기록은 남긴다.
            CREATE TABLE IF NOT EXISTS comments (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                doc_id TEXT NOT NULL,
                by_email TEXT NOT NULL,
                by_name TEXT NOT NULL,
                body TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                deleted_at INTEGER
            );
            -- 북극성 지표(주간 열람된 문서 수)용. 누가 봤는지는 남기지 않는다.
            CREATE TABLE IF NOT EXISTS views (doc_id TEXT NOT NULL, at INTEGER NOT NULL);",
        )?;
        std::fs::create_dir_all(dir)?;
        let docs = Self {
            db: Arc::new(Mutex::new(conn)),
            dir: dir.into(),
        };
        docs.index_missing()?;
        Ok(docs)
    }

    /// 본문 색인이 없는 문서(색인 기능 이전에 올린 문서)를 현재 버전 파일로 채운다.
    fn index_missing(&self) -> Result<()> {
        let ids: Vec<String> = {
            let conn = self.db.lock().unwrap();
            let mut stmt = conn.prepare("SELECT id FROM documents WHERE id NOT IN (SELECT doc_id FROM bodies)")?;
            stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for id in ids {
            // 파일을 못 읽는 문서는 건너뛴다. 제목·올린 사람 검색에는 영향이 없다.
            if let Ok(bytes) = self.read(&id) {
                self.index_body(&id, &bytes)?;
            }
        }
        Ok(())
    }

    fn index_body(&self, id: &str, bytes: &[u8]) -> Result<()> {
        let text = visible_text(&String::from_utf8_lossy(bytes));
        self.db.lock().unwrap().execute(
            "INSERT INTO bodies (doc_id, text) VALUES (?1, ?2)
             ON CONFLICT(doc_id) DO UPDATE SET text = excluded.text",
            params![id, text],
        )?;
        Ok(())
    }

    fn put_file(&self, name: &str, bytes: &[u8]) -> Result<()> {
        Ok(std::fs::write(self.dir.join(name), bytes)?)
    }

    fn read_file(&self, name: &str) -> Result<Vec<u8>> {
        Ok(std::fs::read(self.dir.join(name))?)
    }

    /// 새 문서를 저장하고 추측할 수 없는 번호를 돌려준다. 파일을 먼저 쓰고 기록을 남긴다.
    pub fn insert(&self, title: &str, project: &str, by: &crate::users::User, bytes: &[u8]) -> Result<String> {
        let id = CsrfToken::new_random().secret().clone();
        let file = format!("{id}.html");
        self.put_file(&file, bytes)?;
        let now = now_unix();
        let mut conn = self.db.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO documents (id, title, project, uploader_email, uploader_name, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, title, project, by.email, by.name, now],
        )?;
        tx.execute(
            "INSERT INTO versions (doc_id, n, file, by_email, by_name, created_at) VALUES (?1, 1, ?2, ?3, ?4, ?5)",
            params![id, file, by.email, by.name, now],
        )?;
        tx.commit()?;
        drop(conn);
        self.index_body(&id, bytes)?;
        Ok(id)
    }

    fn add_version(&self, id: &str, by: &crate::users::User, file_for: impl FnOnce(i64) -> Result<String>) -> Result<i64> {
        let conn = self.db.lock().unwrap();
        let n: i64 = conn.query_row("SELECT COALESCE(MAX(n), 0) + 1 FROM versions WHERE doc_id = ?1", [id], |r| r.get(0))?;
        let file = file_for(n)?;
        conn.execute(
            "INSERT INTO versions (doc_id, n, file, by_email, by_name, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, n, file, by.email, by.name, now_unix()],
        )?;
        Ok(n)
    }

    /// 같은 문서에 새 버전을 올린다. 이전 버전은 이력으로 남는다 (정책서 V1·V2).
    pub fn replace(&self, id: &str, by: &crate::users::User, bytes: &[u8]) -> Result<i64> {
        let n = self.add_version(id, by, |n| {
            let file = format!("{id}-{n}.html");
            self.put_file(&file, bytes)?;
            Ok(file)
        })?;
        self.index_body(id, bytes)?;
        Ok(n)
    }

    /// 이전 버전의 내용을 새 버전으로 한 번 더 저장한다. 사이의 버전은 지우지 않는다 (V8). 없는 버전이면 None.
    pub fn restore(&self, id: &str, n: i64, by: &crate::users::User) -> Result<Option<i64>> {
        let Some(file) = self.version_file(id, n)? else { return Ok(None) };
        // 파일은 바뀌지 않으므로 같은 파일을 새 버전이 가리키게 한다.
        let n = self.add_version(id, by, |_| Ok(file.clone()))?;
        self.index_body(id, &self.read_file(&file)?)?;
        Ok(Some(n))
    }

    fn version_file(&self, id: &str, n: i64) -> Result<Option<String>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row("SELECT file FROM versions WHERE doc_id = ?1 AND n = ?2", params![id, n], |r| r.get(0))
            .optional()?)
    }

    /// 최신 버전이 먼저 온다.
    pub fn versions(&self, id: &str) -> Result<Vec<Version>> {
        let conn = self.db.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT n, by_name, created_at FROM versions WHERE doc_id = ?1 ORDER BY n DESC",
        )?;
        let rows = stmt.query_map([id], |r| Ok(Version { n: r.get(0)?, by_name: r.get(1)?, created_at: r.get(2)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 현재(가장 큰 번호) 버전의 내용.
    pub fn read(&self, id: &str) -> Result<Vec<u8>> {
        let file: String = self.db.lock().unwrap().query_row(
            "SELECT file FROM versions WHERE doc_id = ?1 ORDER BY n DESC LIMIT 1",
            [id],
            |r| r.get(0),
        )?;
        self.read_file(&file)
    }

    /// 특정 버전의 내용. 없는 버전이면 None.
    pub fn read_version(&self, id: &str, n: i64) -> Result<Option<Vec<u8>>> {
        match self.version_file(id, n)? {
            Some(file) => Ok(Some(self.read_file(&file)?)),
            None => Ok(None),
        }
    }

    pub fn add_comment(&self, id: &str, by: &crate::users::User, body: &str) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO comments (doc_id, by_email, by_name, body, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, by.email, by.name, body, now_unix()],
        )?;
        Ok(())
    }

    /// 지우지 않은 댓글을 오래된 것부터 돌려준다.
    pub fn comments(&self, id: &str) -> Result<Vec<Comment>> {
        let conn = self.db.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, by_email, by_name, body, created_at FROM comments
             WHERE doc_id = ?1 AND deleted_at IS NULL ORDER BY id",
        )?;
        let rows = stmt.query_map([id], |r| {
            Ok(Comment { id: r.get(0)?, by_email: r.get(1)?, by_name: r.get(2)?, body: r.get(3)?, created_at: r.get(4)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 그 문서의 지우지 않은 댓글 하나.
    pub fn comment(&self, id: &str, comment_id: i64) -> Result<Option<Comment>> {
        Ok(self.comments(id)?.into_iter().find(|c| c.id == comment_id))
    }

    pub fn hide_comment(&self, comment_id: i64) -> Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE comments SET deleted_at = ?2 WHERE id = ?1 AND deleted_at IS NULL",
            params![comment_id, now_unix()],
        )?;
        Ok(())
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

    /// 제목, 올린 사람, 본문(현재 버전의 보이는 글자)에 검색어가 든 문서를 최신순으로 돌려준다. `project` 가 비어 있지 않으면 그 프로젝트만.
    /// LIKE 대신 instr 를 써서 검색어의 % 나 _ 가 특수문자로 해석되지 않게 한다.
    /// shortcut: 본문을 전부 훑는다. 문서가 수천 건을 넘어 느려지면 FTS5(trigram)로 바꾼다.
    /// `offset` 부터 한 쪽(PAGE_SIZE)을 돌려주되, 다음 쪽이 있는지 알 수 있게 한 개 더 가져온다.
    pub fn list(&self, q: &str, project: &str, offset: i64) -> Result<Vec<Doc>> {
        let conn = self.db.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, title, project, uploader_email, uploader_name, created_at
             FROM documents
             WHERE deleted_at IS NULL
               AND (?1 = '' OR instr(lower(title), lower(?1)) > 0 OR instr(lower(uploader_name), lower(?1)) > 0
                    OR EXISTS (SELECT 1 FROM bodies b WHERE b.doc_id = documents.id AND instr(lower(b.text), lower(?1)) > 0))
               AND (?2 = '' OR project = ?2)
             ORDER BY created_at DESC, rowid DESC
             LIMIT ?3 OFFSET ?4",
        )?;
        let rows = stmt.query_map(params![q, project, PAGE_SIZE + 1, offset], row_to_doc)?;
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

    pub fn record_view(&self, id: &str) -> Result<()> {
        self.db
            .lock()
            .unwrap()
            .execute("INSERT INTO views (doc_id, at) VALUES (?1, ?2)", params![id, now_unix()])?;
        Ok(())
    }
}

/// HTML 에서 화면에 보이는 글자만 뽑는다 (정책서 F2). 태그, 주석, script·style 안의 내용은 뺀다.
fn visible_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase(); // 길이가 같아서 같은 위치로 자를 수 있다
    let (mut out, mut i) = (String::new(), 0);
    while i < html.len() {
        let rest = &lower[i..];
        let starts_tag = rest
            .strip_prefix('<')
            .and_then(|r| r.chars().next())
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '/' || c == '!');
        if rest.starts_with("<!--") {
            i += rest.find("-->").map_or(rest.len(), |e| e + 3);
        } else if rest.starts_with("<script") || rest.starts_with("<style") {
            let close = if rest.starts_with("<script") { "</script" } else { "</style" };
            i += rest
                .find(close)
                .and_then(|e| rest[e..].find('>').map(|g| e + g + 1))
                .unwrap_or(rest.len());
            out.push(' ');
        } else if starts_tag {
            i += rest.find('>').map_or(rest.len(), |e| e + 1);
            out.push(' ');
        } else {
            let ch = html[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    let out = out
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
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
pub fn show_time(unix: i64) -> String {
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
    page: Option<i64>,
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
    // 큰 수를 넣어도 곱셈이 넘치지 않게 상한을 둔다.
    let page_no = query.page.unwrap_or(1).clamp(1, 1_000_000);
    let (mut docs, projects) = match (app.docs.list(q.trim(), &project, (page_no - 1) * PAGE_SIZE), app.docs.projects()) {
        (Ok(d), Ok(p)) => (d, p),
        (Err(e), _) | (_, Err(e)) => return server_error("목록 조회", e),
    };
    let has_next = docs.len() as i64 > PAGE_SIZE;
    docs.truncate(PAGE_SIZE as usize);
    // 검색어·프로젝트를 숨은 칸으로 넘겨 브라우저가 주소를 인코딩하게 한다.
    let nav_button = |n: i64, label: &str| {
        format!(
            "<form method=\"get\" action=\"/\" style=\"display:inline\">\
             <input type=\"hidden\" name=\"q\" value=\"{}\"><input type=\"hidden\" name=\"project\" value=\"{}\">\
             <button name=\"page\" value=\"{n}\">{label}</button></form> ",
            escape(&q),
            escape(&project),
        )
    };
    let nav = format!(
        "<div id=\"pages\">{}{}쪽 {}</div>",
        if page_no > 1 { nav_button(page_no - 1, "이전") } else { String::new() },
        page_no,
        if has_next { nav_button(page_no + 1, "다음") } else { String::new() },
    );

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
            let delete = if can_modify(&user, d) {
                format!(
                    "<form method=\"post\" action=\"/d/{}/delete\" style=\"display:inline\">\
                     <button>삭제</button></form>",
                    escape(&d.id)
                )
            } else {
                String::new()
            };
            format!(
                "<tr><td><a href=\"/d/{id}\">{}</a></td><td>{}</td><td>{}</td><td>{}</td><td><a href=\"/d/{id}/history\">이력</a> <a href=\"/d/{id}/comments\">댓글</a> {delete}</td></tr>",
                escape(&d.title),
                if d.project.is_empty() { "미분류".into() } else { escape(&d.project) },
                escape(&d.uploader_name),
                show_time(d.created_at),
                id = escape(&d.id),
            )
        })
        .collect();
    let empty = if docs.is_empty() { "<p>문서가 없습니다.</p>" } else { "" };

    page(&format!(
        "<p>{}님, 안녕하세요. <a href=\"/upload\">올리기</a> <a href=\"/tokens\">토큰</a> \
         <form method=\"post\" action=\"/logout\" style=\"display:inline\"><button>로그아웃</button></form></p>\
         <form method=\"get\" action=\"/\">\
         <input name=\"q\" value=\"{}\" placeholder=\"제목, 올린 사람, 본문\"> \
         <select name=\"project\"><option value=\"\">모든 프로젝트</option>{options}</select> \
         <button>검색</button></form>\
         <table><tr><th>제목</th><th>프로젝트</th><th>올린 사람</th><th>올린 시각</th><th></th></tr>{rows}</table>{empty}{nav}",
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

const BAD_FILE: &str = "파일이 너무 크거나 올바르지 않습니다. 10MB 이하의 HTML 파일만 올릴 수 있습니다.";

pub struct Form {
    pub title: String,
    pub project: String,
    pub file: Option<(String, axum::body::Bytes)>,
}

pub async fn read_form(mut form: Multipart) -> Result<Form, &'static str> {
    let mut out = Form { title: String::new(), project: String::new(), file: None };
    while let Some(field) = form.next_field().await.map_err(|_| BAD_FILE)? {
        let name = field.name().unwrap_or_default().to_owned();
        let filename = field.file_name().unwrap_or_default().to_owned();
        let bytes = field.bytes().await.map_err(|_| BAD_FILE)?;
        match name.as_str() {
            "title" => out.title = String::from_utf8_lossy(&bytes).trim().to_owned(),
            "project" => out.project = String::from_utf8_lossy(&bytes).trim().to_owned(),
            "file" => out.file = Some((filename, bytes)),
            _ => {}
        }
    }
    Ok(out)
}

/// 올린 파일이 규칙(U3·U4)에 맞는지 본다. 맞으면 내용을 돌려준다.
pub fn check_file(file: Option<(String, axum::body::Bytes)>) -> Result<axum::body::Bytes, &'static str> {
    let Some((filename, bytes)) = file.filter(|(_, b)| !b.is_empty()) else {
        return Err("HTML 파일을 선택해 주세요.");
    };
    let lower = filename.to_lowercase();
    if !(lower.ends_with(".html") || lower.ends_with(".htm")) {
        return Err(".html 파일만 올릴 수 있습니다.");
    }
    if bytes.len() > MAX_BYTES {
        return Err("파일이 10MB 를 넘습니다.");
    }
    Ok(bytes)
}

pub async fn upload(State(app): State<AppState>, jar: PrivateCookieJar, form: Multipart) -> Response {
    let user = match current_user(&app, &jar) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let form = match read_form(form).await {
        Ok(f) => f,
        Err(msg) => return upload_form(&app, msg),
    };
    if form.title.is_empty() {
        return upload_form(&app, "제목을 입력해 주세요.");
    }
    if form.title.chars().count() > 200 || form.project.chars().count() > 100 {
        return upload_form(&app, "제목은 200자, 프로젝트는 100자 이하로 입력해 주세요.");
    }
    let bytes = match check_file(form.file) {
        Ok(b) => b,
        Err(msg) => return upload_form(&app, msg),
    };
    match app.docs.insert(&form.title, &form.project, &user, &bytes) {
        Ok(id) => Redirect::to(&format!("/uploaded/{id}")).into_response(),
        Err(e) => server_error("문서 저장", e),
    }
}

fn can_modify(user: &crate::users::User, doc: &Doc) -> bool {
    doc.uploader_email == user.email || user.is_admin()
}

/// 로그인한 직원과 삭제되지 않은 문서를 함께 돌려준다. 안 되면 그대로 돌려줄 응답을 에러로 준다.
fn user_and_doc(
    app: &AppState,
    jar: &PrivateCookieJar,
    id: &str,
) -> Result<(crate::users::User, Doc), Response> {
    let user = current_user_next(app, jar, &format!("/d/{id}"))?;
    match app.docs.get(id) {
        Ok(Some(doc)) => Ok((user, doc)),
        Ok(None) => Err(not_found()),
        Err(e) => Err(server_error("문서 조회", e)),
    }
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, page("올린 사람이나 관리자만 할 수 있습니다.")).into_response()
}

fn replace_form(doc: &Doc, error: &str) -> Response {
    let status = if error.is_empty() { StatusCode::OK } else { StatusCode::BAD_REQUEST };
    let error = if error.is_empty() { String::new() } else { format!("<p style=\"color:#c00\">{}</p>", escape(error)) };
    (
        status,
        page(&format!(
            "<h1>새 버전 올리기: {}</h1>{error}\
             <p>링크는 그대로이고 내용만 새 파일로 바뀝니다. 이전 버전은 이력에 남습니다.</p>\
             <form method=\"post\" action=\"/d/{id}/replace\" enctype=\"multipart/form-data\">\
             <p><label>HTML 파일 (10MB 이하) <input type=\"file\" name=\"file\" accept=\".html,.htm\" required></label></p>\
             <button>올리기</button></form><p><a href=\"/d/{id}/history\">이력으로</a></p>",
            escape(&doc.title),
            id = escape(&doc.id),
        )),
    )
        .into_response()
}

pub async fn replace_page(State(app): State<AppState>, jar: PrivateCookieJar, Path(id): Path<String>) -> Response {
    match user_and_doc(&app, &jar, &id) {
        Ok((user, doc)) if can_modify(&user, &doc) => replace_form(&doc, ""),
        Ok(_) => forbidden(),
        Err(r) => r,
    }
}

pub async fn replace(
    State(app): State<AppState>,
    jar: PrivateCookieJar,
    Path(id): Path<String>,
    form: Multipart,
) -> Response {
    let (user, doc) = match user_and_doc(&app, &jar, &id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if !can_modify(&user, &doc) {
        return forbidden();
    }
    let form = match read_form(form).await {
        Ok(f) => f,
        Err(msg) => return replace_form(&doc, msg),
    };
    let bytes = match check_file(form.file) {
        Ok(b) => b,
        Err(msg) => return replace_form(&doc, msg),
    };
    match app.docs.replace(&id, &user, &bytes) {
        Ok(_) => Redirect::to(&format!("/d/{id}/history")).into_response(),
        Err(e) => server_error("새 버전 저장", e),
    }
}

pub async fn history(State(app): State<AppState>, jar: PrivateCookieJar, Path(id): Path<String>) -> Response {
    let (user, doc) = match user_and_doc(&app, &jar, &id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let versions = match app.docs.versions(&id) {
        Ok(v) => v,
        Err(e) => return server_error("이력 조회", e),
    };
    let modify = can_modify(&user, &doc);
    let latest = versions.first().map(|v| v.n);
    let rows: String = versions
        .iter()
        .map(|v| {
            let current = if Some(v.n) == latest { " (현재)" } else { "" };
            let restore = if modify && Some(v.n) != latest {
                format!(
                    "<form method=\"post\" action=\"/d/{}/restore/{}\" style=\"display:inline\">\
                     <button>이 버전으로 되돌리기</button></form>",
                    escape(&doc.id),
                    v.n
                )
            } else {
                String::new()
            };
            format!(
                "<tr><td>{}{current}</td><td>{}</td><td>{}</td><td><a href=\"/d/{}/v/{}\">열기</a></td><td>{restore}</td></tr>",
                v.n,
                escape(&v.by_name),
                show_time(v.created_at),
                escape(&doc.id),
                v.n,
            )
        })
        .collect();
    let replace_link = if modify {
        format!(" <a href=\"/d/{}/replace\">새 버전 올리기</a>", escape(&doc.id))
    } else {
        String::new()
    };
    page(&format!(
        "<h1>{}</h1><p><a href=\"/d/{id}\">현재 버전 열기</a>{replace_link} <a href=\"/\">목록으로</a></p>\
         <table><tr><th>버전</th><th>올린 사람</th><th>올린 시각</th><th></th><th></th></tr>{rows}</table>",
        escape(&doc.title),
        id = escape(&doc.id),
    ))
    .into_response()
}

const MAX_COMMENT: usize = 2000;

fn comments_page(doc: &Doc, user: &crate::users::User, comments: &[Comment], error: &str) -> Response {
    let status = if error.is_empty() { StatusCode::OK } else { StatusCode::BAD_REQUEST };
    let error = if error.is_empty() { String::new() } else { format!("<p style=\"color:#c00\">{}</p>", escape(error)) };
    let id = escape(&doc.id);
    let items: String = comments
        .iter()
        .map(|c| {
            let delete = if c.by_email == user.email || user.is_admin() {
                format!(
                    "<form method=\"post\" action=\"/d/{id}/comments/{}/delete\" style=\"display:inline\"><button>삭제</button></form>",
                    c.id
                )
            } else {
                String::new()
            };
            format!(
                "<li><b>{}</b> {} {delete}<div style=\"white-space:pre-wrap\">{}</div></li>",
                escape(&c.by_name),
                show_time(c.created_at),
                escape(&c.body)
            )
        })
        .collect();
    let empty = if comments.is_empty() { "<p>댓글이 없습니다.</p>" } else { "" };
    (
        status,
        page(&format!(
            "<h1>{}</h1><p><a href=\"/d/{id}\">문서 열기</a> <a href=\"/d/{id}/history\">이력</a> <a href=\"/\">목록으로</a></p>\
             <h2>댓글</h2>{empty}<ul>{items}</ul>{error}\
             <form method=\"post\" action=\"/d/{id}/comments\"><p><textarea name=\"body\" rows=\"4\" cols=\"60\" maxlength=\"{MAX_COMMENT}\" required></textarea></p>\
             <button>댓글 남기기</button></form>",
            escape(&doc.title)
        )),
    )
        .into_response()
}

pub async fn comments_view(State(app): State<AppState>, jar: PrivateCookieJar, Path(id): Path<String>) -> Response {
    let (user, doc) = match user_and_doc(&app, &jar, &id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    match app.docs.comments(&id) {
        Ok(list) => comments_page(&doc, &user, &list, ""),
        Err(e) => server_error("댓글 조회", e),
    }
}

#[derive(Deserialize)]
pub struct CommentForm {
    body: String,
}

pub async fn comment_add(
    State(app): State<AppState>,
    jar: PrivateCookieJar,
    Path(id): Path<String>,
    axum::Form(form): axum::Form<CommentForm>,
) -> Response {
    let (user, doc) = match user_and_doc(&app, &jar, &id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let body = form.body.trim();
    let list = || app.docs.comments(&id).unwrap_or_default();
    if body.is_empty() {
        return comments_page(&doc, &user, &list(), "댓글 내용을 입력해 주세요.");
    }
    if body.chars().count() > MAX_COMMENT {
        return comments_page(&doc, &user, &list(), "댓글은 2000자 이하로 입력해 주세요.");
    }
    match app.docs.add_comment(&id, &user, body) {
        Ok(()) => Redirect::to(&format!("/d/{id}/comments")).into_response(),
        Err(e) => server_error("댓글 저장", e),
    }
}

pub async fn comment_delete(
    State(app): State<AppState>,
    jar: PrivateCookieJar,
    Path((id, cid)): Path<(String, i64)>,
) -> Response {
    let (user, _) = match user_and_doc(&app, &jar, &id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let c = match app.docs.comment(&id, cid) {
        Ok(Some(c)) => c,
        Ok(None) => return not_found(),
        Err(e) => return server_error("댓글 조회", e),
    };
    if c.by_email != user.email && !user.is_admin() {
        return (StatusCode::FORBIDDEN, page("쓴 사람이나 관리자만 지울 수 있습니다.")).into_response();
    }
    match app.docs.hide_comment(cid) {
        Ok(()) => Redirect::to(&format!("/d/{id}/comments")).into_response(),
        Err(e) => server_error("댓글 삭제", e),
    }
}

pub async fn view_version(
    State(app): State<AppState>,
    jar: PrivateCookieJar,
    Path((id, n)): Path<(String, i64)>,
) -> Response {
    if let Err(r) = user_and_doc(&app, &jar, &id) {
        return r;
    }
    match app.docs.read_version(&id, n) {
        Ok(Some(bytes)) => document_response(bytes),
        Ok(None) => not_found(),
        Err(e) => server_error("문서 파일 읽기", e),
    }
}

pub async fn restore(
    State(app): State<AppState>,
    jar: PrivateCookieJar,
    Path((id, n)): Path<(String, i64)>,
) -> Response {
    let (user, doc) = match user_and_doc(&app, &jar, &id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if !can_modify(&user, &doc) {
        return forbidden();
    }
    match app.docs.restore(&id, n, &user) {
        Ok(Some(_)) => Redirect::to(&format!("/d/{id}/history")).into_response(),
        Ok(None) => not_found(),
        Err(e) => server_error("되돌리기", e),
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
             <p><input id=\"link\" readonly size=\"60\" value=\"{base}/d/{id}\" onfocus=\"this.select()\"> \
             <button type=\"button\" onclick=\"navigator.clipboard.writeText(link.value);this.textContent='복사됨'\">링크 복사</button></p>\
             <p><a href=\"/\">목록으로</a></p>",
            escape(&d.title),
            id = escape(&d.id),
            base = escape(app.base_url.trim_end_matches('/')),
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
            Ok(bytes) => {
                // 기록이 실패해도 문서는 보여 준다.
                if let Err(e) = app.docs.record_view(&id) {
                    eprintln!("열람 기록 실패: {e}");
                }
                document_response(bytes)
            }
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
    if !can_modify(&user, &doc) {
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
        assert_eq!(d.list("", "", 0).unwrap().len(), 2);
        assert_eq!(d.list("보고", "", 0).unwrap()[0].title, "월간 보고");
        assert_eq!(d.list("다라", "", 0).unwrap()[0].title, "설계 메모");
        assert_eq!(d.list("", "A", 0).unwrap().len(), 1);
        assert_eq!(d.list("%", "", 0).unwrap().len(), 0, "% 는 특수문자가 아니라 글자로 찾는다");
        assert_eq!(d.projects().unwrap(), ["A", "B"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 최신_문서가_먼저_나온다() {
        let (d, dir) = docs();
        d.insert("첫째", "", &user("a@x.im", "가나"), b"1").unwrap();
        d.insert("둘째", "", &user("a@x.im", "가나"), b"2").unwrap();
        assert_eq!(d.list("", "", 0).unwrap()[0].title, "둘째");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 숨긴_문서는_조회와_목록에서_빠지지만_파일은_남는다() {
        let (d, dir) = docs();
        let id = d.insert("지울 것", "A", &user("a@x.im", "가나"), b"x").unwrap();
        d.hide(&id).unwrap();
        assert!(d.get(&id).unwrap().is_none());
        assert!(d.list("", "", 0).unwrap().is_empty());
        assert!(d.projects().unwrap().is_empty());
        assert_eq!(d.read(&id).unwrap(), b"x");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 새_버전을_올려도_이전_버전이_남고_현재_버전이_바뀐다() {
        let (d, dir) = docs();
        let (a, b) = (user("a@x.im", "가나"), user("b@x.im", "다라"));
        let id = d.insert("문서", "", &a, b"v1").unwrap();
        assert_eq!(d.replace(&id, &b, b"v2").unwrap(), 2);
        assert_eq!(d.read(&id).unwrap(), b"v2");
        assert_eq!(d.read_version(&id, 1).unwrap().unwrap(), b"v1");
        let v = d.versions(&id).unwrap();
        assert_eq!((v[0].n, v[0].by_name.as_str(), v[1].n), (2, "다라", 1));
        assert!(d.read_version(&id, 9).unwrap().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 되돌리면_옛_내용이_새_버전으로_저장되고_중간_버전은_남는다() {
        let (d, dir) = docs();
        let a = user("a@x.im", "가나");
        let id = d.insert("문서", "", &a, b"v1").unwrap();
        d.replace(&id, &a, b"v2").unwrap();
        assert_eq!(d.restore(&id, 1, &a).unwrap(), Some(3));
        assert_eq!(d.read(&id).unwrap(), b"v1");
        assert_eq!(d.read_version(&id, 2).unwrap().unwrap(), b"v2");
        assert_eq!(d.versions(&id).unwrap().len(), 3);
        assert_eq!(d.restore(&id, 9, &a).unwrap(), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 버전_기능_이전에_올린_문서는_1번_버전으로_채운다() {
        let dir = std::env::temp_dir().join(format!("html-hub-test-{}", CsrfToken::new_random().secret()));
        let db = dir.join("h.db");
        std::fs::create_dir_all(&dir).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE documents (id TEXT PRIMARY KEY, title TEXT NOT NULL, project TEXT NOT NULL,
               uploader_email TEXT NOT NULL, uploader_name TEXT NOT NULL, created_at INTEGER NOT NULL, deleted_at INTEGER);
             INSERT INTO documents VALUES ('old1', '옛 문서', '', 'a@x.im', '가나', 100, NULL);",
        )
        .unwrap();
        drop(conn);
        std::fs::write(dir.join("old1.html"), b"legacy").unwrap();
        // 옛 파일이 있는 폴더를 파일 폴더로 쓴다.
        let d = Docs::open(db.to_str().unwrap(), dir.to_str().unwrap()).unwrap();
        assert_eq!(d.read("old1").unwrap(), b"legacy");
        assert_eq!(d.versions("old1").unwrap().len(), 1);
        let a = user("a@x.im", "가나");
        assert_eq!(d.replace("old1", &a, b"new").unwrap(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 보이는_글자만_뽑는다() {
        let t = visible_text("<html><head><title>제목</title><style>.a{color:red}</style></head><body><!-- 숨김 -->\
            <h1 class=\"x\">안녕&nbsp;하세요</h1><script>var secret = 'zzz';</script><p>a &lt; b &amp; c</p>3 < 4</body></html>");
        assert_eq!(t, "제목 안녕 하세요 a < b & c 3 < 4");
        assert!(!t.contains("secret") && !t.contains("color") && !t.contains("숨김"));
        assert_eq!(visible_text("<SCRIPT>x</SCRIPT>끝"), "끝");
        assert_eq!(visible_text("열린 <b"), "열린");
    }

    #[test]
    fn 본문으로_찾고_교체_되돌리기_숨기기를_따른다() {
        let (d, dir) = docs();
        let a = user("a@x.im", "가나");
        let id = d.insert("보고", "", &a, "<p>분기 매출 요약</p><script>토큰</script>".as_bytes()).unwrap();
        assert_eq!(d.list("매출", "", 0).unwrap().len(), 1);
        assert_eq!(d.list("토큰", "", 0).unwrap().len(), 0, "script 안의 글자는 찾지 않는다");
        d.replace(&id, &a, "<p>새 내용</p>".as_bytes()).unwrap();
        assert_eq!(d.list("매출", "", 0).unwrap().len(), 0, "이전 버전 본문은 색인하지 않는다");
        assert_eq!(d.list("새 내용", "", 0).unwrap().len(), 1);
        d.restore(&id, 1, &a).unwrap();
        assert_eq!(d.list("매출", "", 0).unwrap().len(), 1);
        d.hide(&id).unwrap();
        assert_eq!(d.list("매출", "", 0).unwrap().len(), 0, "숨긴 문서는 본문으로도 안 나온다");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 댓글은_오래된_것부터_나오고_지우면_숨겨진다() {
        let (d, dir) = docs();
        let (a, b) = (user("a@x.im", "가나"), user("b@x.im", "다라"));
        let id = d.insert("문서", "", &a, b"x").unwrap();
        d.add_comment(&id, &a, "첫 댓글").unwrap();
        d.add_comment(&id, &b, "둘째").unwrap();
        let list = d.comments(&id).unwrap();
        assert_eq!((list[0].body.as_str(), list[1].by_name.as_str()), ("첫 댓글", "다라"));
        d.hide_comment(list[0].id).unwrap();
        assert_eq!(d.comments(&id).unwrap().len(), 1);
        assert!(d.comment(&id, list[0].id).unwrap().is_none());
        assert!(d.comment("다른문서", list[1].id).unwrap().is_none(), "다른 문서의 댓글 번호로는 못 찾는다");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 목록은_쪽마다_나뉘고_다음_쪽이_있는지_한_개_더_알려준다() {
        let (d, dir) = docs();
        let a = user("a@x.im", "가나");
        for i in 0..PAGE_SIZE + 3 {
            d.insert(&format!("문서{i}"), "", &a, b"x").unwrap();
        }
        let first = d.list("", "", 0).unwrap();
        assert_eq!(first.len() as i64, PAGE_SIZE + 1, "다음 쪽이 있으면 한 개 더 온다");
        assert_eq!(first[0].title, format!("문서{}", PAGE_SIZE + 2));
        let second = d.list("", "", PAGE_SIZE).unwrap();
        assert_eq!(second.len(), 3);
        assert_eq!(second[2].title, "문서0");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 열람을_문서번호와_시각만으로_기록한다() {
        let (d, dir) = docs();
        let id = d.insert("문서", "", &user("a@x.im", "가나"), b"x").unwrap();
        d.record_view(&id).unwrap();
        d.record_view(&id).unwrap();
        let conn = d.db.lock().unwrap();
        let (rows, docs): (i64, i64) = conn
            .query_row("SELECT COUNT(*), COUNT(DISTINCT doc_id) FROM views", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!((rows, docs), (2, 1));
        let cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('views')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(cols, ["doc_id", "at"], "누가 봤는지는 남기지 않는다");
        drop(conn);
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
