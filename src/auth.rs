use crate::{AppState, users::User};
use anyhow::Result;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use axum_extra::extract::cookie::{Cookie, PrivateCookieJar, SameSite};
use openidconnect::core::{CoreClient, CoreProviderMetadata, CoreResponseType};
use openidconnect::{
    AuthenticationFlow, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet,
    EndpointNotSet, EndpointSet, IssuerUrl, Nonce, PkceCodeChallenge, PkceCodeVerifier,
    RedirectUrl, Scope, reqwest,
};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use time::Duration;

const OAUTH_COOKIE: &str = "oauth";
const SESSION_COOKIE: &str = "session";
const SESSION_HOURS: i64 = 8;

type Client = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

pub struct Oidc {
    client: Client,
    http: reqwest::Client,
}

impl Oidc {
    pub async fn discover(id: String, secret: String, redirect: String) -> Result<Self> {
        // 리다이렉트를 따라가면 SSRF 위험이 생기므로 막는다.
        let http = reqwest::ClientBuilder::new()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let meta = CoreProviderMetadata::discover_async(
            IssuerUrl::new("https://accounts.google.com".into())?,
            &http,
        )
        .await?;
        let client = CoreClient::from_provider_metadata(
            meta,
            ClientId::new(id),
            Some(ClientSecret::new(secret)),
        )
        .set_redirect_uri(RedirectUrl::new(redirect)?);
        Ok(Self { client, http })
    }
}

// 로그인을 시작하고 돌아올 때까지 서버가 기억해야 하는 값. 암호화된 쿠키에 담는다.
#[derive(Serialize, Deserialize)]
struct Pending {
    state: String,
    nonce: String,
    verifier: String,
}

fn cookie(secure: bool, name: &'static str, value: String, max_age: Duration) -> Cookie<'static> {
    Cookie::build((name, value))
        .path("/")
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Lax)
        .max_age(max_age)
        .build()
}

fn expired(name: &'static str) -> Cookie<'static> {
    Cookie::build((name, "")).path("/").build()
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

pub(crate) fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub(crate) fn page(body: &str) -> Html<String> {
    Html(format!(
        "<!doctype html><html lang=\"ko\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>html-hub</title></head><body>{body}</body></html>"
    ))
}

// 세션 값은 "이메일|만료시각" 이고 쿠키 전체가 암호화·서명되므로 클라이언트가 고칠 수 없다.
fn session_email(jar: &PrivateCookieJar) -> Option<String> {
    let c = jar.get(SESSION_COOKIE)?;
    let (email, exp) = c.value().rsplit_once('|')?;
    (exp.parse::<u64>().ok()? > now_unix()).then(|| email.to_owned())
}

pub async fn login(State(app): State<AppState>, jar: PrivateCookieJar) -> impl IntoResponse {
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, state, nonce) = app
        .oidc
        .client
        .authorize_url(
            AuthenticationFlow::<CoreResponseType>::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("email".into()))
        .set_pkce_challenge(challenge)
        .url();
    let pending = Pending {
        state: state.secret().clone(),
        nonce: nonce.secret().clone(),
        verifier: verifier.secret().clone(),
    };
    let c = cookie(
        app.secure_cookies,
        OAUTH_COOKIE,
        serde_json::to_string(&pending).unwrap(),
        Duration::minutes(10),
    );
    (jar.add(c), Redirect::to(url.as_str()))
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

type Failure = (StatusCode, &'static str);

// 구글이 확인해 준 이메일만 돌려준다. 명단 확인은 호출한 쪽이 한다.
async fn verified_email(
    app: &AppState,
    jar: &PrivateCookieJar,
    q: CallbackQuery,
) -> Result<String, Failure> {
    const RETRY: &str = "로그인 세션이 만료되었습니다. 처음부터 다시 시도하세요.";
    if q.error.is_some() {
        return Err((StatusCode::UNAUTHORIZED, "로그인이 취소되었습니다."));
    }
    let pending: Pending = jar
        .get(OAUTH_COOKIE)
        .and_then(|c| serde_json::from_str(c.value()).ok())
        .ok_or((StatusCode::BAD_REQUEST, RETRY))?;
    let (code, state) = q
        .code
        .zip(q.state)
        .ok_or((StatusCode::BAD_REQUEST, "잘못된 요청입니다."))?;
    if state != pending.state {
        return Err((StatusCode::BAD_REQUEST, "잘못된 요청입니다."));
    }

    let fail = |e: &dyn std::fmt::Display| {
        eprintln!("로그인 실패: {e}");
        (StatusCode::BAD_GATEWAY, "구글 로그인에 실패했습니다.")
    };
    let token = app
        .oidc
        .client
        .exchange_code(AuthorizationCode::new(code))
        .map_err(|e| fail(&e))?
        .set_pkce_verifier(PkceCodeVerifier::new(pending.verifier))
        .request_async(&app.oidc.http)
        .await
        .map_err(|e| fail(&e))?;

    let id_token = token
        .extra_fields()
        .id_token()
        .ok_or_else(|| fail(&"ID 토큰이 없다"))?;
    let claims = id_token
        .claims(&app.oidc.client.id_token_verifier(), &Nonce::new(pending.nonce))
        .map_err(|e| fail(&e))?;
    if claims.email_verified() != Some(true) {
        return Err((StatusCode::FORBIDDEN, "이메일이 확인되지 않은 계정입니다."));
    }
    claims
        .email()
        .map(|e| e.as_str().to_owned())
        .ok_or((StatusCode::FORBIDDEN, "이메일 정보를 받지 못했습니다."))
}

pub async fn callback(
    State(app): State<AppState>,
    jar: PrivateCookieJar,
    Query(q): Query<CallbackQuery>,
) -> Response {
    let email = match verified_email(&app, &jar, q).await {
        Ok(email) => email,
        Err((status, msg)) => return (status, page(&escape(msg))).into_response(),
    };
    match app.users.find_active(&email) {
        Ok(Some(user)) => {
            let exp = now_unix() + SESSION_HOURS as u64 * 3600;
            let session = cookie(
                app.secure_cookies,
                SESSION_COOKIE,
                format!("{}|{exp}", user.email),
                Duration::hours(SESSION_HOURS),
            );
            (jar.remove(expired(OAUTH_COOKIE)).add(session), Redirect::to("/")).into_response()
        }
        Ok(None) => {
            eprintln!("명단에 없는 로그인 시도: {email}");
            (
            StatusCode::FORBIDDEN,
            page("등록되지 않았거나 비활성화된 계정입니다. IT 팀에 문의하세요."),
            )
                .into_response()
        }
        Err(e) => {
            eprintln!("명단 조회 실패: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// 로그인한 활성 직원을 돌려준다. 아니면 그대로 돌려줄 응답(로그인 이동 등)을 에러로 준다.
pub fn current_user(app: &AppState, jar: &PrivateCookieJar) -> Result<User, Response> {
    let Some(email) = session_email(jar) else {
        return Err(Redirect::to("/login").into_response());
    };
    // 로그인 뒤에 계정이 비활성화됐을 수 있으므로 매번 명단을 다시 본다.
    match app.users.find_active(&email) {
        Ok(Some(user)) => Ok(user),
        Ok(None) => Err((
            jar.clone().remove(expired(SESSION_COOKIE)),
            Redirect::to("/login"),
        )
            .into_response()),
        Err(e) => {
            eprintln!("명단 조회 실패: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

// POST 로만 받는다. 다른 사이트에서 로그아웃을 걸 수 없다.
pub async fn logout(jar: PrivateCookieJar) -> impl IntoResponse {
    (
        jar.remove(expired(SESSION_COOKIE)),
        page("로그아웃되었습니다. <a href=\"/login\">다시 로그인</a>"),
    )
}

// 구글 동의 화면에 연결하는 공개 안내다. 로그인 없이 열려야 하므로 명단 확인을 거치지 않는다.
pub async fn privacy() -> Html<String> {
    page(
        "<h1>개인정보 처리방침</h1>\
         <p>html-hub 는 운영자가 미리 등록한 사용자만 쓰는 내부 서비스다.</p>\
         <p>구글 로그인으로 받는 정보는 이메일 주소와 이름뿐이며, 등록된 사용자인지 확인하고 \
         화면에 이름을 보여 주는 데에만 쓴다.</p>\
         <p>로그인 상태는 암호화된 쿠키에 담아 8시간 동안 유지한다. 받은 정보를 제3자에게 제공하지 않는다.</p>\
         <p>업로드한 문서는 서비스 안에서 등록된 사용자에게만 보인다. 삭제나 문의는 운영자에게 한다.</p>",
    )
}

pub async fn terms() -> Html<String> {
    page(
        "<h1>서비스 약관</h1>\
         <p>html-hub 는 운영자가 등록한 사용자만 쓸 수 있다. 등록되지 않은 계정은 접근할 수 없다.</p>\
         <p>업로드한 문서의 내용에 대한 책임은 올린 사람에게 있다. 고객 정보나 비밀값을 올리지 않는다.</p>\
         <p>운영자는 서비스 운영에 필요하면 문서를 숨기거나 계정을 비활성화할 수 있다.</p>",
    )
}
