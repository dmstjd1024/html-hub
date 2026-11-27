mod auth;
mod convert;
mod docs;
mod mcp;
mod tokens;
mod users;

use anyhow::{Context, Result, bail};
use axum::{
    Router,
    extract::{DefaultBodyLimit, FromRef, Request, State},
    http::{Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use axum_extra::extract::cookie::Key;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub users: users::Users,
    pub docs: docs::Docs,
    pub tokens: tokens::Tokens,
    pub base_url: String,
    pub oidc: Arc<auth::Oidc>,
    pub key: Key,
    pub secure_cookies: bool,
    pub grants: mcp::Grants,
}

impl FromRef<AppState> for Key {
    fn from_ref(app: &AppState) -> Key {
        app.key.clone()
    }
}

/// 상태를 바꾸는 요청(POST 등)은 이 사이트의 화면에서 보낸 것만 받는다 (위조 요청 방지).
/// 격리된 문서 속 폼은 Origin: null 을 보내고, 다른 사이트는 자기 출처를 보낸다. 쿠키가 실려 와도 거절한다.
/// 토큰으로 받는 /api/upload·/mcp 와 OAuth 토큰 교환·등록은 쿠키를 쓰지 않으므로 검사하지 않는다.
async fn same_origin_only(State(origin): State<String>, req: Request, next: Next) -> Response {
    let reads = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    let from_here = req.headers().get(header::ORIGIN).is_some_and(|o| o.as_bytes() == origin.as_bytes());
    let tokens_only = matches!(req.uri().path(), "/api/upload" | "/mcp" | "/oauth/token" | "/oauth/register");
    if reads || from_here || tokens_only {
        return next.run(req).await;
    }
    eprintln!("다른 출처의 요청 거절: {} {} (Origin {:?})", req.method(), req.uri().path(), req.headers().get(header::ORIGIN));
    (StatusCode::FORBIDDEN, auth::page("다른 화면에서 보낸 요청이라 처리하지 않았습니다. <a href=\"/\">목록으로</a>")).into_response()
}

/// BASE_URL 에서 출처(scheme://host[:port])만 뗀다. Origin 헤더와 같은 꼴이다.
fn origin_of(base_url: &str) -> String {
    let rest = base_url.split_once("://").map_or(base_url, |(_, r)| r);
    let host = rest.split('/').next().unwrap_or(rest);
    let scheme = base_url.split_once("://").map_or("http", |(s, _)| s);
    format!("{scheme}://{host}")
}

fn env(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("환경변수 {name} 가 없다"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let db_path = std::env::var("DATABASE_PATH").unwrap_or_else(|_| "data/html-hub.db".into());
    if let Some(dir) = std::path::Path::new(&db_path).parent() {
        std::fs::create_dir_all(dir)?;
    }
    let users = users::Users::open(&db_path)?;

    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("import-users") {
        let path = args.next().context("사용법: html-hub import-users <내보내기.json>")?;
        let count = users.import(&std::fs::read_to_string(path)?)?;
        println!("{count}명 반영");
        return Ok(());
    }

    let base_url = std::env::var("BASE_URL").unwrap_or_else(|_| "http://localhost:3000".into());
    let session_key = env("SESSION_KEY")?;
    if session_key.len() < 64 {
        bail!("SESSION_KEY 는 64자 이상이어야 한다 (예: openssl rand -base64 64 | tr -d '\\n')");
    }
    let files_dir = std::env::var("FILES_DIR").unwrap_or_else(|_| "data/files".into());
    let docs = docs::Docs::open(&db_path, &files_dir)?;
    let tokens = tokens::Tokens::open(&db_path)?;
    let oidc = auth::Oidc::discover(
        env("GOOGLE_CLIENT_ID")?,
        env("GOOGLE_CLIENT_SECRET")?,
        format!("{}/auth/callback", base_url.trim_end_matches('/')),
    )
    .await?;

    let app = Router::new()
        .route("/", get(docs::index))
        .route("/login", get(auth::login))
        .route("/auth/callback", get(auth::callback))
        .route("/logout", post(auth::logout))
        .route("/healthz", get(|| async { "ok" }))
        .route("/privacy", get(auth::privacy))
        .route("/terms", get(auth::terms))
        .route("/tokens", get(tokens::tokens_page).post(tokens::tokens_issue))
        .route("/tokens/revoke", post(tokens::tokens_revoke))
        .route("/admin/tokens", get(tokens::admin_tokens))
        .route("/admin/tokens/revoke", post(tokens::admin_tokens_revoke))
        .route(
            "/api/upload",
            post(tokens::api_upload).layer(DefaultBodyLimit::max(docs::MAX_BYTES + 64 * 1024)),
        )
        .route("/tokens/connections/revoke", post(tokens::connections_revoke))
        // 원격 MCP 와 그 로그인(OAuth). 내용을 JSON 문자열로 받으므로 한도를 넉넉히 둔다.
        .route("/mcp", post(mcp::mcp).layer(DefaultBodyLimit::max(docs::MAX_BYTES * 2 + 64 * 1024)))
        .route("/.well-known/oauth-protected-resource", get(mcp::protected_resource))
        .route("/.well-known/oauth-protected-resource/mcp", get(mcp::protected_resource))
        .route("/.well-known/oauth-authorization-server", get(mcp::authorization_server))
        .route("/oauth/register", post(mcp::register))
        .route("/oauth/authorize", get(mcp::authorize).post(mcp::approve))
        .route("/oauth/token", post(mcp::token))
        .route("/upload", get(docs::upload_page))
        // 파일 한도(10MB)에 다른 입력칸 몫만 더한다. 넘으면 업로드 처리에서 안내한다.
        .route(
            "/upload",
            post(docs::upload).layer(DefaultBodyLimit::max(docs::MAX_BYTES + 64 * 1024)),
        )
        .route("/uploaded/{id}", get(docs::uploaded))
        .route("/d/{id}", get(docs::view))
        .route("/d/{id}/delete", post(docs::delete))
        .route("/d/{id}/unhide", post(docs::unhide))
        .route("/admin/hidden", get(docs::hidden_list))
        .route("/d/{id}/history", get(docs::history))
        .route("/d/{id}/comments", get(docs::comments_view).post(docs::comment_add))
        .route("/d/{id}/comments/{cid}/delete", post(docs::comment_delete))
        .route("/d/{id}/v/{n}", get(docs::view_version))
        .route("/d/{id}/restore/{n}", post(docs::restore))
        .route("/d/{id}/replace", get(docs::replace_page))
        .route(
            "/d/{id}/replace",
            post(docs::replace).layer(DefaultBodyLimit::max(docs::MAX_BYTES + 64 * 1024)),
        )
        .with_state(AppState {
            users,
            docs,
            tokens,
            base_url: base_url.clone(),
            oidc: Arc::new(oidc),
            key: Key::from(session_key.as_bytes()),
            secure_cookies: base_url.starts_with("https://"),
            grants: Default::default(),
        })
        .layer(middleware::from_fn_with_state(origin_of(&base_url), same_origin_only));

    let addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".into());
    println!("{addr} 에서 대기 중 ({base_url})");
    axum::serve(tokio::net::TcpListener::bind(&addr).await?, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::origin_of;

    #[test]
    fn base_url_에서_출처만_뗀다() {
        assert_eq!(origin_of("https://survey.eunseong.dev"), "https://survey.eunseong.dev");
        assert_eq!(origin_of("https://survey.eunseong.dev/"), "https://survey.eunseong.dev");
        assert_eq!(origin_of("http://127.0.0.1:3000/hub"), "http://127.0.0.1:3000");
    }
}
