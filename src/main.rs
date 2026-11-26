mod auth;
mod docs;
mod tokens;
mod users;

use anyhow::{Context, Result, bail};
use axum::{
    Router,
    extract::{DefaultBodyLimit, FromRef},
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
}

impl FromRef<AppState> for Key {
    fn from_ref(app: &AppState) -> Key {
        app.key.clone()
    }
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
        .route(
            "/api/upload",
            post(tokens::api_upload).layer(DefaultBodyLimit::max(docs::MAX_BYTES + 64 * 1024)),
        )
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
        });

    let addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".into());
    println!("{addr} 에서 대기 중 ({base_url})");
    axum::serve(tokio::net::TcpListener::bind(&addr).await?, app).await?;
    Ok(())
}
