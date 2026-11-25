mod auth;
mod users;

use anyhow::{Context, Result, bail};
use axum::{
    Router,
    extract::FromRef,
    routing::{get, post},
};
use axum_extra::extract::cookie::Key;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub users: users::Users,
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
    let oidc = auth::Oidc::discover(
        env("GOOGLE_CLIENT_ID")?,
        env("GOOGLE_CLIENT_SECRET")?,
        format!("{}/auth/callback", base_url.trim_end_matches('/')),
    )
    .await?;

    let app = Router::new()
        .route("/", get(auth::home))
        .route("/login", get(auth::login))
        .route("/auth/callback", get(auth::callback))
        .route("/logout", post(auth::logout))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(AppState {
            users,
            oidc: Arc::new(oidc),
            key: Key::from(session_key.as_bytes()),
            secure_cookies: base_url.starts_with("https://"),
        });

    let addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".into());
    println!("{addr} 에서 대기 중 ({base_url})");
    axum::serve(tokio::net::TcpListener::bind(&addr).await?, app).await?;
    Ok(())
}
