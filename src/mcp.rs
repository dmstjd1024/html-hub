//! 원격 MCP 서버와 그 로그인(OAuth 2.1). claude.ai·Claude 데스크톱·모바일의 커넥터와 Claude Code·Codex 가 붙는다.
//! - MCP: `POST /mcp` (Streamable HTTP, 응답은 JSON 만). 도구는 upload_html 하나.
//! - OAuth: 메타데이터, 동적 등록(DCR), 승인 화면, 토큰 교환. PKCE(S256) 필수.
//!   돌아갈 주소는 claude.ai·claude.com 콜백과 이 컴퓨터(loopback)만 받으므로, 등록한 클라이언트를 따로 저장하지 않는다.
//! 직원 토큰(hh_)도 `Authorization: Bearer` 로 그대로 쓸 수 있다.

use crate::{
    AppState,
    auth::{current_user, current_user_next, escape, page},
    docs::check_file,
    users::User,
};
use axum::{
    Form, Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, Uri, header},
    response::{IntoResponse, Redirect, Response},
};
use axum_extra::extract::cookie::PrivateCookieJar;
use openidconnect::{CsrfToken, PkceCodeChallenge, PkceCodeVerifier, url::Url};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

/// 승인했지만 아직 토큰으로 바꾸지 않은 코드. 메모리에만 두고 10분 뒤 버린다 (서버를 다시 켜면 다시 승인하면 된다).
pub struct Grant {
    email: String,
    client_id: String,
    redirect_uri: String,
    challenge: String,
    client: &'static str,
    expires: u64,
}

pub type Grants = Arc<Mutex<HashMap<String, Grant>>>;

const TOKEN_TTL: u64 = 90 * 24 * 3600;

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn base(app: &AppState) -> &str {
    app.base_url.trim_end_matches('/')
}

/// 돌아갈 주소가 허용 목록에 있으면 승인 화면에 보일 이름을 준다.
fn client_of(redirect_uri: &str) -> Option<&'static str> {
    // claude.com 쪽은 앞으로 바뀔 수 있다고 안내된 주소다.
    if ["https://claude.ai/api/mcp/auth_callback", "https://claude.com/api/mcp/auth_callback"].contains(&redirect_uri) {
        return Some("Claude (claude.ai)");
    }
    let u = Url::parse(redirect_uri).ok()?;
    let loopback = u.scheme() == "http"
        && matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
        && u.username().is_empty()
        && u.password().is_none()
        && u.fragment().is_none();
    loopback.then_some("이 컴퓨터의 프로그램 (Claude Code·Codex 등)")
}

// ---------- OAuth ----------

pub async fn protected_resource(State(app): State<AppState>) -> Json<Value> {
    let b = base(&app);
    Json(json!({
        "resource": format!("{b}/mcp"),
        "authorization_servers": [b],
        "bearer_methods_supported": ["header"],
        "resource_name": "html-hub",
    }))
}

pub async fn authorization_server(State(app): State<AppState>) -> Json<Value> {
    let b = base(&app);
    Json(json!({
        "issuer": b,
        "authorization_endpoint": format!("{b}/oauth/authorize"),
        "token_endpoint": format!("{b}/oauth/token"),
        "registration_endpoint": format!("{b}/oauth/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
    }))
}

#[derive(Deserialize)]
pub struct Registration {
    #[serde(default)]
    redirect_uris: Vec<String>,
    client_name: Option<String>,
}

pub async fn register(Json(r): Json<Registration>) -> Response {
    if r.redirect_uris.is_empty() || !r.redirect_uris.iter().all(|u| client_of(u).is_some()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "invalid_redirect_uri",
                "error_description": "claude.ai·claude.com 콜백과 이 컴퓨터(localhost) 주소만 받습니다.",
            })),
        )
            .into_response();
    }
    (
        StatusCode::CREATED,
        Json(json!({
            "client_id": format!("hhc_{}", CsrfToken::new_random().secret()),
            "client_id_issued_at": now(),
            "client_name": r.client_name,
            "redirect_uris": r.redirect_uris,
            "grant_types": ["authorization_code"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        })),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct AuthorizeQuery {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    state: Option<String>,
}

fn bad_request(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, page(&escape(msg))).into_response()
}

/// 연결 요청을 확인한다. 돌아갈 주소가 이상하면 그리로 보내지 않고 이 화면에서 멈춘다.
fn check(q: &AuthorizeQuery) -> Result<&'static str, &'static str> {
    let client = q.redirect_uri.as_deref().and_then(client_of).ok_or("허용되지 않은 돌아갈 주소입니다.")?;
    if q.response_type.as_deref() != Some("code") || q.client_id.as_deref().is_none_or(str::is_empty) {
        return Err("잘못된 연결 요청입니다.");
    }
    let challenge_ok = q.code_challenge.as_deref().is_some_and(|c| (43..=128).contains(&c.len()));
    if q.code_challenge_method.as_deref() != Some("S256") || !challenge_ok {
        return Err("PKCE(S256) 가 필요합니다.");
    }
    Ok(client)
}

/// 승인 화면. 로그인 전이면 구글 로그인 뒤 이 화면으로 돌아온다.
pub async fn authorize(State(app): State<AppState>, jar: PrivateCookieJar, uri: Uri, Query(q): Query<AuthorizeQuery>) -> Response {
    let client = match check(&q) {
        Ok(c) => c,
        Err(msg) => return bad_request(msg),
    };
    let here = uri.path_and_query().map_or("/oauth/authorize", |p| p.as_str());
    let user = match current_user_next(&app, &jar, here) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let hidden: String = [("client_id", &q.client_id), ("redirect_uri", &q.redirect_uri), ("code_challenge", &q.code_challenge), ("state", &q.state)]
        .iter()
        .map(|(k, v)| format!("<input type=\"hidden\" name=\"{k}\" value=\"{}\">", escape(v.as_deref().unwrap_or(""))))
        .collect();
    let body = format!(
        "<h1>Claude 연결</h1>\
         <p><b>{client}</b> 이(가) <b>{}</b> ({}) 이름으로 html-hub 에 문서를 올리려고 합니다.</p>\
         <p class=\"quiet\">허용하면 Claude 에게 \"survey 에 올려줘\" 라고 했을 때 이 계정으로 올라갑니다. \
         연결은 <a href=\"/tokens\">내 토큰</a> 화면에서 언제든 끊을 수 있습니다.</p>\
         <form method=\"post\" action=\"/oauth/authorize\">{hidden}\
         <p><label><input type=\"checkbox\" name=\"confirm\" value=\"yes\" required> \
         방금 내가 Claude 에서 직접 '연결'을 눌러 이 화면이 열렸습니다. 남이 보낸 링크로 열었다면 거절하세요.</label></p>\
         <p><button class=\"btn\" name=\"decision\" value=\"allow\">허용</button> \
         <button class=\"btn line\" name=\"decision\" value=\"deny\">거절</button></p></form>",
        escape(&user.name),
        escape(&user.email),
    );
    // 다른 사이트가 이 화면을 몰래 겹쳐 띄우고 허용을 누르게 하지 못하게 한다.
    ([(header::X_FRAME_OPTIONS, "DENY"), (header::CONTENT_SECURITY_POLICY, "frame-ancestors 'none'")], page(&body)).into_response()
}

#[derive(Deserialize)]
pub struct Decision {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    #[serde(default)]
    state: String,
    decision: String,
    #[serde(default)]
    confirm: String,
}

/// 허용·거절을 받아 돌아갈 주소로 보낸다. 이 사이트 화면에서 보낸 요청만 온다 (main 의 출처 검사).
pub async fn approve(State(app): State<AppState>, jar: PrivateCookieJar, Form(d): Form<Decision>) -> Response {
    let Some(client) = client_of(&d.redirect_uri) else {
        return bad_request("허용되지 않은 돌아갈 주소입니다.");
    };
    let user = match current_user(&app, &jar) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let Ok(mut to) = Url::parse(&d.redirect_uri) else {
        return bad_request("허용되지 않은 돌아갈 주소입니다.");
    };
    // 확인란은 허용 버튼과 떨어져 있어, 남이 띄운 창에서 엉겁결에 누른 클릭(더블클릭 가로채기)으로는 허용되지 않는다.
    if d.decision == "allow" && d.confirm != "yes" {
        return bad_request("확인란을 체크한 뒤 허용해 주세요.");
    }
    if d.decision == "allow" {
        let code = format!("{}{}", CsrfToken::new_random().secret(), CsrfToken::new_random().secret());
        let mut grants = app.grants.lock().unwrap();
        let t = now();
        grants.retain(|_, g| g.expires > t);
        grants.insert(
            code.clone(),
            Grant { email: user.email.clone(), client_id: d.client_id, redirect_uri: d.redirect_uri, challenge: d.code_challenge, client, expires: t + 600 },
        );
        eprintln!("Claude 연결 승인: {} ({client})", user.email);
        to.query_pairs_mut().append_pair("code", &code);
    } else {
        to.query_pairs_mut().append_pair("error", "access_denied");
    }
    if !d.state.is_empty() {
        to.query_pairs_mut().append_pair("state", &d.state);
    }
    Redirect::to(to.as_str()).into_response()
}

#[derive(Deserialize)]
pub struct TokenForm {
    grant_type: String,
    code: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    code_verifier: Option<String>,
}

fn oauth_error(code: &str) -> Response {
    (StatusCode::BAD_REQUEST, [(header::CACHE_CONTROL, "no-store")], Json(json!({ "error": code }))).into_response()
}

/// 승인 코드를 토큰으로 바꾼다. 코드는 한 번만 쓸 수 있고, PKCE 로 승인을 시작한 쪽만 바꿀 수 있다.
pub async fn token(State(app): State<AppState>, Form(f): Form<TokenForm>) -> Response {
    if f.grant_type != "authorization_code" {
        return oauth_error("unsupported_grant_type");
    }
    let grant = f.code.as_deref().and_then(|c| app.grants.lock().unwrap().remove(c));
    let Some(g) = grant.filter(|g| g.expires > now()) else {
        return oauth_error("invalid_grant");
    };
    let verifier = f.code_verifier.unwrap_or_default();
    let ok = f.client_id.as_deref().is_none_or(|c| c == g.client_id)
        && f.redirect_uri.as_deref() == Some(g.redirect_uri.as_str())
        && (43..=128).contains(&verifier.len())
        && PkceCodeChallenge::from_code_verifier_sha256(&PkceCodeVerifier::new(verifier)).as_str() == g.challenge;
    if !ok || !matches!(app.users.find_active(&g.email), Ok(Some(_))) {
        return oauth_error("invalid_grant");
    }
    match app.tokens.issue_oauth(&g.email, g.client) {
        Ok(t) => (
            [(header::CACHE_CONTROL, "no-store")],
            Json(json!({ "access_token": t, "token_type": "Bearer", "expires_in": TOKEN_TTL })),
        )
            .into_response(),
        Err(e) => {
            eprintln!("OAuth 토큰 발급 실패: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ---------- MCP ----------

fn unauthorized(app: &AppState) -> Response {
    let challenge = format!("Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource\"", base(app));
    (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, challenge)], Json(json!({ "error": "invalid_token" }))).into_response()
}

fn tool() -> Value {
    json!({
        "name": "upload_html",
        "description": "survey(html-hub, 사내 HTML 문서 공유 사이트 survey.eunseong.dev)에 문서를 올리고 공유 링크를 돌려준다. \
                        Claude 가 만든 아티팩트의 내용 전체를 content 에 그대로 넣는다. 올린 문서는 전 직원이 볼 수 있다. \
                        설문을 만들 때는 문서 스크립트에서 window.htmlhub.submit({질문: 답}) 을 부르면 된다 (Promise, 실패하면 Error). 응답은 보고 있는 직원 이름으로 저장되고 한 사람이 다시 내면 덮어쓰며, 올린 사람과 관리자만 html-hub 의 '응답' 탭과 CSV 로 본다. 제출 성공·실패를 화면에 보여 줄 것.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "문서 제목 (200자 이하)" },
                "content": { "type": "string", "description": "문서 내용 전체 (10MB 이하)" },
                "format": {
                    "type": "string",
                    "enum": ["html", "md", "svg", "mermaid", "jsx"],
                    "description": "content 의 종류. 기본 html. jsx 는 export default 하는 React 컴포넌트"
                },
                "project": { "type": "string", "description": "프로젝트 이름 (선택, 100자 이하)" }
            },
            "required": ["title", "content"]
        }
    })
}

fn call(app: &AppState, user: &User, params: &Value) -> Value {
    let reply = |text: String, is_error: bool| json!({ "content": [{ "type": "text", "text": text }], "isError": is_error });
    if params["name"] != "upload_html" {
        return reply("알 수 없는 도구입니다.".into(), true);
    }
    let a = &params["arguments"];
    let (title, project) = (a["title"].as_str().unwrap_or("").trim(), a["project"].as_str().unwrap_or("").trim());
    let content = a["content"].as_str().unwrap_or("");
    if title.is_empty() || content.is_empty() {
        return reply("title 과 content 가 필요합니다.".into(), true);
    }
    if title.chars().count() > 200 || project.chars().count() > 100 {
        return reply("title 은 200자, project 는 100자 이하여야 합니다.".into(), true);
    }
    let ext = match a["format"].as_str().unwrap_or("html") {
        "html" => "html",
        "md" | "markdown" => "md",
        "svg" => "svg",
        "mermaid" | "mmd" => "mmd",
        "jsx" | "react" => "jsx",
        _ => return reply("format 은 html, md, svg, mermaid, jsx 중 하나입니다.".into(), true),
    };
    // 파일 이름은 감싼 문서의 <title> 에 쓰인다.
    let name = format!("{}.{ext}", title.replace(['/', '\\'], " "));
    let bytes = match check_file(Some((name, axum::body::Bytes::copy_from_slice(content.as_bytes())))) {
        Ok(b) => b,
        Err(msg) => return reply(msg.into(), true),
    };
    match app.docs.insert(title, project, user, &bytes) {
        Ok(id) => reply(format!("{}/d/{id}", base(app)), false),
        Err(e) => {
            eprintln!("문서 저장 실패: {e}");
            reply("서버 오류로 올리지 못했습니다.".into(), true)
        }
    }
}

/// 이 서버가 아는 MCP 규격 판. 클라이언트가 아는 판이면 그대로, 아니면 가장 최근 판으로 답한다.
fn protocol_version(asked: Option<&str>) -> &str {
    const KNOWN: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
    asked.filter(|v| KNOWN.contains(v)).unwrap_or(KNOWN[0])
}

pub async fn mcp(State(app): State<AppState>, headers: HeaderMap, Json(req): Json<Value>) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    // 토큰 주인이 명단에서 빠졌으면 토큰이 살아 있어도 막는다.
    let user = match token.map(|t| app.tokens.owner(t).map(|e| e.map(|e| app.users.find_active(&e)))) {
        Some(Ok(Some(Ok(Some(u))))) => u,
        Some(Err(_)) | Some(Ok(Some(Err(_)))) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        _ => return unauthorized(&app),
    };
    if req.is_array() {
        let e = json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32600, "message": "여러 요청을 한 번에 보내는 배치는 지원하지 않습니다." } });
        return (StatusCode::BAD_REQUEST, Json(e)).into_response();
    }
    // id 가 없으면 알림(notifications/initialized 등)이라 답하지 않는다.
    let Some(id) = req.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let result = match req["method"].as_str().unwrap_or("") {
        "initialize" => Ok(json!({
            "protocolVersion": protocol_version(req["params"]["protocolVersion"].as_str()),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "html-hub", "version": env!("CARGO_PKG_VERSION") },
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": [tool()] })),
        "tools/call" => Ok(call(&app, &user, &req["params"])),
        _ => Err(json!({ "code": -32601, "message": "지원하지 않는 메서드입니다." })),
    };
    Json(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::{client_of, protocol_version};

    #[test]
    fn 돌아갈_주소는_claude_콜백과_이_컴퓨터만_받는다() {
        assert!(client_of("https://claude.ai/api/mcp/auth_callback").is_some());
        assert!(client_of("https://claude.com/api/mcp/auth_callback").is_some());
        assert!(client_of("http://localhost:33418/callback").is_some());
        assert!(client_of("http://127.0.0.1:5000/cb").is_some());
        for bad in [
            "https://evil.com/api/mcp/auth_callback",
            "https://claude.ai/other",
            "https://claude.ai.evil.com/api/mcp/auth_callback",
            "https://localhost/callback",
            "http://localhost.evil.com/cb",
            "http://user@localhost/cb",
            "http://localhost/cb#x",
            "javascript:alert(1)",
            "",
        ] {
            assert!(client_of(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn 아는_규격_판은_그대로_모르면_최근_판() {
        assert_eq!(protocol_version(Some("2025-03-26")), "2025-03-26");
        assert_eq!(protocol_version(Some("1999-01-01")), "2025-11-25");
        assert_eq!(protocol_version(None), "2025-11-25");
    }
}
