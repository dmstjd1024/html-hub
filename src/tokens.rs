use crate::{
    AppState,
    auth::{current_user, escape, page},
    docs::{check_file, read_form},
};
use anyhow::Result;
use axum::{
    Json,
    extract::{Multipart, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use axum_extra::extract::cookie::PrivateCookieJar;
use openidconnect::CsrfToken;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// 직원 한 명당 활성 토큰 하나. 서버에는 해시만 저장하므로 DB 가 새어도 토큰은 알 수 없다.
#[derive(Clone)]
pub struct Tokens(Arc<Mutex<Connection>>);

pub struct TokenInfo {
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

fn hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

impl Tokens {
    pub fn open(db_path: &str) -> Result<Self> {
        let conn = Connection::open(db_path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS api_tokens (
                hash TEXT PRIMARY KEY,
                email TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                last_used_at INTEGER,
                revoked_at INTEGER
            )",
        )?;
        Ok(Self(Arc::new(Mutex::new(conn))))
    }

    /// 새 토큰을 만들어 한 번만 돌려준다. 그 사람의 이전 토큰은 폐기된다.
    pub fn issue(&self, email: &str) -> Result<String> {
        // 128비트 난수 둘 = 256비트. 이 정도면 빠른 해시(SHA-256)로 충분하다.
        let token = format!(
            "hh_{}{}",
            CsrfToken::new_random().secret(),
            CsrfToken::new_random().secret()
        );
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE api_tokens SET revoked_at = ?2 WHERE email = ?1 AND revoked_at IS NULL",
            params![email, now_unix()],
        )?;
        tx.execute(
            "INSERT INTO api_tokens (hash, email, created_at) VALUES (?1, ?2, ?3)",
            params![hash(&token), email, now_unix()],
        )?;
        tx.commit()?;
        Ok(token)
    }

    pub fn revoke(&self, email: &str) -> Result<()> {
        self.0.lock().unwrap().execute(
            "UPDATE api_tokens SET revoked_at = ?2 WHERE email = ?1 AND revoked_at IS NULL",
            params![email, now_unix()],
        )?;
        Ok(())
    }

    /// 폐기되지 않은 토큰의 주인 이메일. 쓴 시각을 남긴다.
    pub fn owner(&self, token: &str) -> Result<Option<String>> {
        let conn = self.0.lock().unwrap();
        let h = hash(token);
        let email: Option<String> = conn
            .query_row(
                "SELECT email FROM api_tokens WHERE hash = ?1 AND revoked_at IS NULL",
                [&h],
                |r| r.get(0),
            )
            .optional()?;
        if email.is_some() {
            conn.execute("UPDATE api_tokens SET last_used_at = ?2 WHERE hash = ?1", params![h, now_unix()])?;
        }
        Ok(email)
    }

    pub fn info(&self, email: &str) -> Result<Option<TokenInfo>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .query_row(
                "SELECT created_at, last_used_at FROM api_tokens WHERE email = ?1 AND revoked_at IS NULL",
                [email],
                |r| Ok(TokenInfo { created_at: r.get(0)?, last_used_at: r.get(1)? }),
            )
            .optional()?)
    }
}

fn show_time(unix: i64) -> String {
    crate::docs::show_time(unix)
}

pub async fn tokens_page(State(app): State<AppState>, jar: PrivateCookieJar) -> Response {
    let user = match current_user(&app, &jar) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let info = match app.tokens.info(&user.email) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("토큰 조회 실패: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let state = match info {
        Some(i) => format!(
            "<p>발급된 토큰이 있습니다 (발급 {}, 마지막 사용 {}).</p>\
             <form method=\"post\" action=\"/tokens/revoke\"><button>폐기</button></form>",
            show_time(i.created_at),
            i.last_used_at.map_or("없음".into(), show_time)
        ),
        None => "<p>발급된 토큰이 없습니다.</p>".into(),
    };
    page(&format!(
        "<h1>개인 토큰</h1>\
         <p>Claude 같은 도구가 터미널에서 문서를 올릴 때 쓰는 열쇠입니다. 이 토큰으로 올린 문서는 {} 님이 올린 것으로 기록됩니다. 남에게 알려 주지 마세요.</p>\
         {state}\
         <form method=\"post\" action=\"/tokens\"><button>새 토큰 발급 (이전 토큰은 폐기됩니다)</button></form>\
         <p><a href=\"/\">목록으로</a></p>",
        escape(&user.name)
    ))
    .into_response()
}

pub async fn tokens_issue(State(app): State<AppState>, jar: PrivateCookieJar) -> Response {
    let user = match current_user(&app, &jar) {
        Ok(u) => u,
        Err(r) => return r,
    };
    match app.tokens.issue(&user.email) {
        Ok(token) => page(&format!(
            "<h1>새 토큰</h1><p>지금 한 번만 보입니다. 복사해 두세요.</p>\
             <pre style=\"user-select:all\">{token}</pre>\
             <p>사용 예:</p>\
             <pre>curl -H \"Authorization: Bearer {token}\" \\\n  -F title=\"제목\" -F project=\"프로젝트\" -F file=@문서.html \\\n  {}/api/upload</pre>\
             <p><a href=\"/tokens\">돌아가기</a></p>",
            escape(app.base_url.trim_end_matches('/'))
        ))
        .into_response(),
        Err(e) => {
            eprintln!("토큰 발급 실패: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn tokens_revoke(State(app): State<AppState>, jar: PrivateCookieJar) -> Response {
    let user = match current_user(&app, &jar) {
        Ok(u) => u,
        Err(r) => return r,
    };
    match app.tokens.revoke(&user.email) {
        Ok(()) => Redirect::to("/tokens").into_response(),
        Err(e) => {
            eprintln!("토큰 폐기 실패: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn api_error(status: StatusCode, msg: &str) -> Response {
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

/// 토큰으로 올린다. 쿠키는 보지 않는다. 명단은 매번 다시 확인한다.
pub async fn api_upload(State(app): State<AppState>, headers: HeaderMap, form: Multipart) -> Response {
    let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return api_error(StatusCode::UNAUTHORIZED, "Authorization: Bearer 토큰이 필요합니다.");
    };
    let user = match app.tokens.owner(token).map(|e| e.map(|e| app.users.find_active(&e))) {
        Ok(Some(Ok(Some(u)))) => u,
        Ok(Some(Err(_))) | Err(_) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, "서버 오류"),
        _ => return api_error(StatusCode::UNAUTHORIZED, "토큰이 올바르지 않거나 폐기되었거나 등록되지 않은 계정입니다."),
    };
    let form = match read_form(form).await {
        Ok(f) => f,
        Err(msg) => return api_error(StatusCode::BAD_REQUEST, msg),
    };
    if form.title.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "title 이 필요합니다.");
    }
    if form.title.chars().count() > 200 || form.project.chars().count() > 100 {
        return api_error(StatusCode::BAD_REQUEST, "title 은 200자, project 는 100자 이하여야 합니다.");
    }
    let bytes = match check_file(form.file) {
        Ok(b) => b,
        Err(msg) => return api_error(StatusCode::BAD_REQUEST, msg),
    };
    match app.docs.insert(&form.title, &form.project, &user, &bytes) {
        Ok(id) => (
            StatusCode::CREATED,
            Json(serde_json::json!({ "id": id, "url": format!("{}/d/{id}", app.base_url.trim_end_matches('/')) })),
        )
            .into_response(),
        Err(e) => {
            eprintln!("문서 저장 실패: {e}");
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "서버 오류")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 토큰은_주인을_알려주고_재발급하면_이전_것이_폐기된다() {
        let t = Tokens::open(":memory:").unwrap();
        let first = t.issue("a@x.im").unwrap();
        assert!(first.starts_with("hh_") && first.len() == 3 + 22 + 22);
        assert_eq!(t.owner(&first).unwrap().as_deref(), Some("a@x.im"));
        assert!(t.info("a@x.im").unwrap().unwrap().last_used_at.is_some());
        assert_eq!(t.owner("hh_틀린값").unwrap(), None);
        let second = t.issue("a@x.im").unwrap();
        assert_eq!(t.owner(&first).unwrap(), None, "이전 토큰은 폐기");
        assert_eq!(t.owner(&second).unwrap().as_deref(), Some("a@x.im"));
        t.revoke("a@x.im").unwrap();
        assert_eq!(t.owner(&second).unwrap(), None);
        assert!(t.info("a@x.im").unwrap().is_none());
    }

    #[test]
    fn 다른_사람의_토큰은_섞이지_않는다() {
        let t = Tokens::open(":memory:").unwrap();
        let (a, b) = (t.issue("a@x.im").unwrap(), t.issue("b@x.im").unwrap());
        assert_eq!(t.owner(&a).unwrap().as_deref(), Some("a@x.im"));
        assert_eq!(t.owner(&b).unwrap().as_deref(), Some("b@x.im"));
    }

    #[test]
    fn 저장소에는_토큰_원문이_없다() {
        let t = Tokens::open(":memory:").unwrap();
        let token = t.issue("a@x.im").unwrap();
        let stored: String = t.0.lock().unwrap().query_row("SELECT hash FROM api_tokens", [], |r| r.get(0)).unwrap();
        assert!(!stored.contains(&token) && stored.len() == 64);
    }
}
