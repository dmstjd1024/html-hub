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
    // 로그인 뒤에 돌아갈 문서 주소. 안전한 값만 담는다 (safe_next).
    #[serde(default)]
    next: String,
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

// 숲색 메뉴 띠 하나만 눈에 띄게 두고 나머지는 조용하게. 다크 모드는 시스템 설정을 따른다.
const CSS: &str = r#"
:root{--paper:#F6F7F4;--surface:#FFFFFF;--ink:#1D2622;--ink-2:#55605A;--forest:#1F3B2D;--leaf:#2F6B4F;--on-leaf:#FFFFFF;--moss:#E3ECE5;--line:#D5DBD6;--alert:#B3261E;--alert-bg:#FBEAE8;color-scheme:light}
@media (prefers-color-scheme:dark){:root{--paper:#121A16;--surface:#18231D;--ink:#E4EAE6;--ink-2:#9AA6A0;--forest:#0C1A12;--leaf:#86C9A1;--on-leaf:#0C1A12;--moss:#1D2C24;--line:#2B3A31;--alert:#F2B8B5;--alert-bg:#3A1F1D;color-scheme:dark}}
*{box-sizing:border-box}
body{margin:0;background:var(--paper);color:var(--ink);font:15px/1.6 "IBM Plex Sans KR",system-ui,-apple-system,"Apple SD Gothic Neo",sans-serif;font-variant-numeric:tabular-nums}
a{color:var(--leaf)}
:focus-visible{outline:2px solid var(--leaf);outline-offset:2px;border-radius:4px}
h1{font-size:24px;font-weight:600;line-height:1.3;margin:0 0 4px}
h2{font-size:15px;font-weight:600;margin:0 0 10px}
.lede{color:var(--ink-2);margin:0}
.app{display:grid;grid-template-columns:216px minmax(0,1fr);min-height:100vh}
.rail{background:var(--forest);color:#C9D8CE;padding:24px 14px;display:flex;flex-direction:column;position:sticky;top:0;height:100vh}
.rail .brand{color:#fff;font-weight:600;font-size:18px;text-decoration:none;padding:0 10px;margin-bottom:28px}
.rail nav{display:flex;flex-direction:column;gap:2px}
.rail nav a{color:#C9D8CE;text-decoration:none;padding:8px 10px;border-radius:6px}
.rail nav a:hover{color:#fff;background:rgba(255,255,255,.06)}
.rail nav a[aria-current=page]{color:#fff;background:rgba(255,255,255,.13);font-weight:500}
.rail .group{font-size:12px;color:#8FA698;padding:16px 10px 4px}
.rail .who{margin-top:auto;padding:12px 10px 0;border-top:1px solid rgba(255,255,255,.12);font-size:13px}
.rail .who b{display:block;color:#fff;font-weight:500}
.rail .who button{background:none;border:0;padding:0;margin-top:6px;color:#C9D8CE;font:inherit;text-decoration:underline;cursor:pointer}
.main{padding:32px 40px 48px;max-width:1240px;width:100%}
.top{display:flex;justify-content:space-between;align-items:flex-end;gap:16px;flex-wrap:wrap;margin-bottom:24px}
.btn{display:inline-block;background:var(--leaf);color:var(--on-leaf);border:0;border-radius:6px;padding:9px 16px;font:inherit;font-weight:500;text-decoration:none;cursor:pointer;white-space:nowrap}
.btn:hover{filter:brightness(1.08)}
.link{background:none;border:0;padding:0;font:inherit;color:var(--leaf);cursor:pointer;text-decoration:none}
.link:hover{text-decoration:underline}
.link.danger{color:var(--alert)}
input[type=text],input[type=search],input:not([type]),input[type=file],textarea,select{font:inherit;color:var(--ink);background:var(--surface);border:1px solid var(--line);border-radius:6px;padding:9px 12px;width:100%}
textarea{resize:vertical}
.field{margin:0 0 18px}
.field label{display:block;font-weight:500;margin-bottom:6px}
.field .hint{display:block;color:var(--ink-2);font-size:13px;margin-top:4px}
.error{color:var(--alert);background:var(--alert-bg);border-radius:6px;padding:10px 14px;margin:0 0 18px}
.figures{display:flex;flex-wrap:wrap;gap:12px 48px;margin:0 0 28px}
.figures div{margin:0}
.figures dt{font-size:13px;color:var(--ink-2)}
.figures dd{margin:0;font-size:28px;font-weight:600;line-height:1.2}
.search{display:flex;gap:8px;margin-bottom:12px}
.chips{display:flex;flex-wrap:wrap;gap:6px;margin-bottom:24px}
.chips a{font-size:13px;padding:4px 12px;border-radius:999px;background:var(--moss);color:var(--ink);text-decoration:none}
.chips a[aria-current=true]{background:var(--leaf);color:var(--on-leaf)}
.chips span{font-size:13px;padding:4px 12px;border-radius:999px;background:var(--leaf);color:var(--on-leaf)}
.result{color:var(--ink-2);font-size:14px;margin:-8px 0 8px}
.narrow{max-width:760px}
.comments{max-width:760px}
.grid{display:grid;grid-template-columns:minmax(0,1fr) 272px;gap:32px;align-items:start}
.doc-table td:first-child{min-width:200px}
.table-wrap{overflow-x:auto}
table{width:100%;border-collapse:collapse}
th{text-align:left;font-size:13px;font-weight:500;color:var(--ink-2);padding:8px 12px;border-bottom:1px solid var(--line);white-space:nowrap}
td{padding:12px;border-bottom:1px solid var(--line);vertical-align:top}
td.num,th.num{text-align:right}
td.num{color:var(--ink-2)}
td.when{white-space:nowrap;color:var(--ink-2);font-size:14px}
.doc-title{color:var(--ink);font-weight:500;text-decoration:none}
.doc-title:hover{color:var(--leaf);text-decoration:underline}
.excerpt{display:block;font-size:13px;color:var(--ink-2);margin-top:4px}
.tag{display:inline-block;font-size:13px;background:var(--moss);border-radius:4px;padding:1px 8px;white-space:nowrap}
.tag.none{background:none;color:var(--ink-2);padding:0}
.actions{white-space:nowrap;font-size:14px}
.actions a,.actions .link{margin-right:12px;color:var(--ink-2);text-decoration:none}
.actions a:hover,.actions .link:hover{color:var(--leaf);text-decoration:underline}
.actions .link.danger{color:var(--alert)}
.panel a{color:var(--ink);font-weight:500;text-decoration:none}
.panel a:hover{color:var(--leaf);text-decoration:underline}
time{white-space:nowrap}
.actions form{display:inline}
.pager{display:flex;gap:12px;align-items:center;margin-top:16px;color:var(--ink-2)}
.pager form{display:inline}
.pager button{font:inherit;background:var(--surface);color:var(--ink);border:1px solid var(--line);border-radius:6px;padding:6px 14px;cursor:pointer}
.panel{background:var(--moss);border-radius:8px;padding:16px 18px;margin-bottom:16px}
.panel ol,.panel ul{list-style:none;margin:0;padding:0}
.panel li{padding:7px 0;border-top:1px solid var(--line);font-size:14px}
.panel li:first-child{border-top:0}
.panel .meta{display:block;color:var(--ink-2);font-size:13px}
.panel .quiet{color:var(--ink-2);font-size:14px;margin:0}
.empty{padding:28px 0;color:var(--ink-2)}
.sheet{max-width:640px;margin:12vh auto;padding:28px 32px 32px;background:var(--surface);border-radius:10px;font-size:16px}
.sheet .brand{display:inline-block;color:var(--leaf);font-weight:600;text-decoration:none;margin-bottom:16px}
.form{max-width:560px}
.copy{display:flex;gap:8px;margin:16px 0;max-width:760px}
.comments{list-style:none;padding:0;margin:0 0 24px}
.comments li{padding:14px 0;border-bottom:1px solid var(--line)}
.comments .body{white-space:pre-wrap;margin-top:4px}
.comments .meta{color:var(--ink-2);font-size:13px}
.visually-hidden{position:absolute;width:1px;height:1px;overflow:hidden;clip:rect(0 0 0 0);white-space:nowrap}
pre{background:var(--moss);border-radius:6px;padding:12px 14px;overflow-x:auto;font-size:13px}
@media (max-width:1360px){.grid{grid-template-columns:minmax(0,1fr)}.grid aside{display:grid;grid-template-columns:1fr 1fr;gap:16px}.grid aside .panel{margin:0}}
@media (max-width:1000px){
.doc-table thead{position:absolute;width:1px;height:1px;overflow:hidden;clip:rect(0 0 0 0)}
.doc-table,.doc-table tbody,.doc-table tr{display:block}
.doc-table tr{padding:14px 0;border-bottom:1px solid var(--line)}
.doc-table td{display:inline-block;border:0;padding:2px 14px 2px 0;font-size:14px}
.doc-table td:first-child{min-width:0;display:block;font-size:15px;padding-bottom:6px}
.doc-table td.num{text-align:left}
.doc-table td.num::before{content:attr(data-label) " ";color:var(--ink-2)}
.doc-table td.actions{display:block;padding-top:6px}
}
@media (max-width:860px){
.grid aside{grid-template-columns:1fr}
.app{grid-template-columns:1fr}
.rail{position:static;height:auto;flex-direction:row;flex-wrap:wrap;align-items:center;gap:4px 8px;padding:12px 16px}
.rail .brand{margin:0 8px 0 0;padding:0}
.rail nav{flex-direction:row;flex-wrap:wrap}
.rail .group{display:none}
.rail .who{margin:0 0 0 auto;padding:0;border:0}
.rail .who b{display:inline;margin-right:8px}
.rail .who .email{display:none}
.main{padding:20px 16px 40px}
.grid{grid-template-columns:1fr}
.figures dd{font-size:22px}
}
"#;

fn document(body: &str) -> Html<String> {
    Html(format!(
        "<!doctype html><html lang=\"ko\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>html-hub</title>\
         <link rel=\"preconnect\" href=\"https://fonts.googleapis.com\">\
         <link rel=\"preconnect\" href=\"https://fonts.gstatic.com\" crossorigin>\
         <link rel=\"stylesheet\" href=\"https://fonts.googleapis.com/css2?family=IBM+Plex+Sans+KR:wght@400;500;600&display=swap\">\
         <style>{CSS}</style></head><body>{body}</body></html>"
    ))
}

/// 로그인 없이 보이거나 혼자 떠 있는 안내 화면 (오류, 로그아웃, 약관).
pub(crate) fn page(body: &str) -> Html<String> {
    document(&format!("<main class=\"sheet\"><a class=\"brand\" href=\"/\">html-hub</a><div>{body}</div></main>"))
}

/// 로그인한 직원이 보는 화면. 왼쪽 메뉴 띠에 현재 위치를 표시한다.
pub(crate) fn shell(user: &User, current: &str, body: &str) -> Html<String> {
    let link = |href: &str, label: &str| {
        let cur = if href == current { " aria-current=\"page\"" } else { "" };
        format!("<a href=\"{href}\"{cur}>{label}</a>")
    };
    let admin = if user.is_admin() {
        format!("<span class=\"group\">관리</span>{}{}", link("/admin/hidden", "숨긴 문서"), link("/admin/tokens", "토큰 관리"))
    } else {
        String::new()
    };
    document(&format!(
        "<div class=\"app\"><aside class=\"rail\"><a class=\"brand\" href=\"/\">html-hub</a>\
         <nav aria-label=\"메뉴\">{}{}{}{admin}</nav>\
         <div class=\"who\"><b>{}</b><span class=\"email\">{}</span>\
         <form method=\"post\" action=\"/logout\"><button>로그아웃</button></form></div></aside>\
         <main class=\"main\">{body}</main></div>",
        link("/", "문서"),
        link("/upload", "올리기"),
        link("/tokens", "내 토큰"),
        escape(&user.name),
        escape(&user.email),
    ))
}

// 세션 값은 "이메일|만료시각" 이고 쿠키 전체가 암호화·서명되므로 클라이언트가 고칠 수 없다.
fn session_email(jar: &PrivateCookieJar) -> Option<String> {
    let c = jar.get(SESSION_COOKIE)?;
    let (email, exp) = c.value().rsplit_once('|')?;
    (exp.parse::<u64>().ok()? > now_unix()).then(|| email.to_owned())
}

#[derive(Deserialize)]
pub struct LoginQuery {
    next: Option<String>,
}

/// 로그인 뒤 돌아갈 곳은 "/d/<문서번호>" 꼴만 받는다. 아무 주소나 받으면 다른 사이트로 보내는 통로가 된다.
fn safe_next(next: &str) -> String {
    match next.strip_prefix("/d/") {
        Some(id) if !id.is_empty() && id.len() <= 64
            && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') => next.to_owned(),
        _ => "/".to_owned(),
    }
}

pub async fn login(
    State(app): State<AppState>,
    jar: PrivateCookieJar,
    Query(q): Query<LoginQuery>,
) -> impl IntoResponse {
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
        next: safe_next(&q.next.unwrap_or_default()),
    };
    let c = cookie(
        app.secure_cookies,
        OAUTH_COOKIE,
        serde_json::to_string(&pending).unwrap(),
        // 구글 비밀번호·2단계 인증이 길어져도 끊기지 않게 30분 (10분일 때 만료 안내가 떴다).
        Duration::minutes(30),
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
) -> Result<(String, String), Failure> {
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
        .map(|e| (e.as_str().to_owned(), pending.next))
        .ok_or((StatusCode::FORBIDDEN, "이메일 정보를 받지 못했습니다."))
}

pub async fn callback(
    State(app): State<AppState>,
    jar: PrivateCookieJar,
    Query(q): Query<CallbackQuery>,
) -> Response {
    let (email, next) = match verified_email(&app, &jar, q).await {
        Ok(v) => v,
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
            (jar.remove(expired(OAUTH_COOKIE)).add(session), Redirect::to(&next)).into_response()
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
    current_user_next(app, jar, "/")
}

/// `current_user` 와 같고, 로그인이 필요하면 로그인 뒤 `next` 로 돌아오게 보낸다.
pub fn current_user_next(app: &AppState, jar: &PrivateCookieJar, next: &str) -> Result<User, Response> {
    let Some(email) = session_email(jar) else {
        return Err(Redirect::to(&format!("/login?next={}", safe_next(next))).into_response());
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

#[cfg(test)]
mod tests {
    use super::safe_next;

    #[test]
    fn 돌아갈_곳은_문서_주소만_받는다() {
        assert_eq!(safe_next("/d/abc-DEF_123"), "/d/abc-DEF_123");
        for bad in ["", "/", "//evil.com", "https://evil.com", "/d/", "/d/a/b", "/d/../x", "/d/a?x=1", "/login", "/d//evil.com"] {
            assert_eq!(safe_next(bad), "/", "{bad}");
        }
    }
}
