//! 설문 응답 모으기. 올린 문서의 스크립트가 `window.htmlhub.submit({질문: 답})` 을 부르면 보고 있는 직원 이름으로 저장한다.
//! 문서는 격리돼(출처 없음) 로그인 쿠키를 못 쓰므로, 문서를 내보낼 때 그 문서·그 직원에게만 통하는 제출 열쇠를 함께 넣는다.
//! 문서 스크립트에는 누가 보고 있는지 알려 주지 않는다. 응답은 올린 사람과 관리자만 보고, 한 사람이 다시 내면 덮어쓴다.

use crate::{
    AppState,
    auth::{escape, page, shell},
    docs::{SurveyResponse, can_modify, doc_header, server_error, user_and_doc},
};
use axum::{
    Json,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use axum_extra::extract::cookie::PrivateCookieJar;
use openidconnect::CsrfToken;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

/// 제출 열쇠 → (문서, 직원 이메일, 만료). 메모리에만 둔다. 서버를 다시 켜면 페이지를 새로고침해 새 열쇠를 받는다.
pub type Keys = Arc<Mutex<HashMap<String, (String, String, u64)>>>;

/// 로그인 세션과 같은 8시간.
const KEY_TTL: u64 = 8 * 3600;
pub const MAX_ANSWERS: usize = 64 * 1024;

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

/// 문서에 넣을 `window.htmlhub.submit`. 문서 번호와 열쇠는 영문·숫자·`-_` 뿐이라 그대로 넣어도 된다.
pub fn script(app: &AppState, doc_id: &str, email: &str) -> String {
    let key = format!("{}{}", CsrfToken::new_random().secret(), CsrfToken::new_random().secret());
    let mut keys = app.survey_keys.lock().unwrap();
    let t = now();
    keys.retain(|_, k| k.2 > t);
    keys.insert(key.clone(), (doc_id.to_owned(), email.to_owned(), t + KEY_TTL));
    // text/plain 이면 브라우저가 사전 확인(preflight) 없이 보낸다. 응답은 서버가 CORS 로 읽게 해 준다.
    format!(
        "window.htmlhub={{submit:function(answers){{return fetch('/d/{doc_id}/responses',{{method:'POST',\
         headers:{{'Content-Type':'text/plain'}},body:JSON.stringify({{key:'{key}',answers:answers}})}})\
         .then(function(r){{return r.json().then(function(j){{if(!r.ok)throw new Error(j.error||('HTTP '+r.status));return j}})}})}}}};"
    )
}

#[derive(Deserialize)]
struct Submission {
    key: String,
    answers: Value,
}

/// 문서에서 보낸 응답을 받는다. 쿠키 대신 제출 열쇠로 누구인지 안다 (그래서 main 의 출처 검사에서 뺀다).
pub async fn submit(State(app): State<AppState>, Path(id): Path<String>, body: String) -> Response {
    // 격리된 문서(출처 null)가 결과를 읽을 수 있게 한다. 쿠키를 쓰지 않으므로 * 로 충분하다.
    let reply = |status: StatusCode, v: Value| (status, [(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")], Json(v)).into_response();
    let fail = |status: StatusCode, msg: &str| reply(status, json!({ "error": msg }));
    let Ok(s) = serde_json::from_str::<Submission>(&body) else {
        return fail(StatusCode::BAD_REQUEST, "형식이 잘못됐습니다.");
    };
    let email = {
        let mut keys = app.survey_keys.lock().unwrap();
        let t = now();
        keys.retain(|_, k| k.2 > t);
        keys.get(&s.key).filter(|k| k.0 == id).map(|k| k.1.clone())
    };
    let Some(email) = email else {
        return fail(StatusCode::UNAUTHORIZED, "제출 기한이 지났습니다. 페이지를 새로고침한 뒤 다시 제출해 주세요.");
    };
    let user = match app.users.find_active(&email) {
        Ok(Some(u)) => u,
        Ok(None) => return fail(StatusCode::FORBIDDEN, "등록되지 않았거나 비활성화된 계정입니다."),
        Err(e) => return server_error("명단 조회", e),
    };
    if !s.answers.is_object() {
        return fail(StatusCode::BAD_REQUEST, "응답은 {질문: 답} 꼴의 객체여야 합니다.");
    }
    let answers = s.answers.to_string();
    if answers.len() > MAX_ANSWERS {
        return fail(StatusCode::PAYLOAD_TOO_LARGE, "응답이 64KB 를 넘습니다.");
    }
    match app.docs.get(&id) {
        Ok(Some(_)) => {}
        Ok(None) => return fail(StatusCode::NOT_FOUND, "없거나 삭제된 문서입니다."),
        Err(e) => return server_error("문서 조회", e),
    }
    match app.docs.save_response(&id, &user, &answers) {
        Ok(()) => reply(StatusCode::OK, json!({ "ok": true })),
        Err(e) => server_error("응답 저장", e),
    }
}

/// 질문 열은 처음 나온 순서대로 모은다. 답이 객체가 아니면 건너뛴다 (저장할 때 막지만 혹시 몰라).
fn table(list: &[SurveyResponse]) -> (Vec<String>, Vec<serde_json::Map<String, Value>>) {
    let rows: Vec<_> = list
        .iter()
        .map(|r| serde_json::from_str::<Value>(&r.answers).ok().and_then(|v| v.as_object().cloned()).unwrap_or_default())
        .collect();
    let mut cols: Vec<String> = Vec::new();
    for row in &rows {
        for k in row.keys() {
            if !cols.contains(k) {
                cols.push(k.clone());
            }
        }
    }
    (cols, rows)
}

fn cell(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a.iter().map(|x| cell(Some(x))).collect::<Vec<_>>().join(", "),
        Some(other) => other.to_string(),
    }
}

/// 엑셀에서 = + - @ 로 시작하는 칸은 수식으로 실행되므로 앞에 ' 를 붙인다.
fn csv_field(s: &str) -> String {
    let s = if s.starts_with(['=', '+', '-', '@', '\t', '\r']) { format!("'{s}") } else { s.to_owned() };
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn kst(unix: i64) -> String {
    let t = time::OffsetDateTime::from_unix_timestamp(unix).unwrap_or(time::OffsetDateTime::UNIX_EPOCH).to_offset(time::UtcOffset::from_hms(9, 0, 0).unwrap());
    format!("{}-{:02}-{:02} {:02}:{:02}", t.year(), t.month() as u8, t.day(), t.hour(), t.minute())
}

fn owner_only(app: &AppState, jar: &PrivateCookieJar, id: &str) -> Result<(crate::users::User, crate::docs::Doc, Vec<SurveyResponse>), Response> {
    let (user, doc) = user_and_doc(app, jar, id)?;
    if !can_modify(&user, &doc) {
        return Err((StatusCode::FORBIDDEN, page("올린 사람과 관리자만 응답을 볼 수 있습니다.")).into_response());
    }
    let list = app.docs.responses(id).map_err(|e| server_error("응답 조회", e))?;
    Ok((user, doc, list))
}

pub async fn responses_page(State(app): State<AppState>, jar: PrivateCookieJar, Path(id): Path<String>) -> Response {
    let (user, doc, list) = match owner_only(&app, &jar, &id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let body = if list.is_empty() {
        "<p class=\"empty\">아직 응답이 없습니다. 문서의 스크립트에서 <code>window.htmlhub.submit({질문: 답})</code> 을 부르면 \
         보고 있는 직원 이름으로 여기에 모입니다.</p>"
            .to_owned()
    } else {
        let (cols, rows) = table(&list);
        let head: String = cols.iter().map(|c| format!("<th scope=\"col\">{}</th>", escape(c))).collect();
        let body: String = list
            .iter()
            .zip(&rows)
            .map(|(r, row)| {
                let answers: String = cols.iter().map(|c| format!("<td>{}</td>", escape(&cell(row.get(c))))).collect();
                format!(
                    "<tr><td class=\"by\">{}<time>{}</time></td>{answers}</tr>",
                    escape(&r.name),
                    escape(&kst(r.updated_at))
                )
            })
            .collect();
        format!(
            "<p class=\"result\">응답 {}명<span class=\"sep\" aria-hidden=\"true\">·</span><a href=\"/d/{}/responses.csv\">CSV 내려받기</a></p>\
             <div class=\"table-wrap\"><table><thead><tr><th scope=\"col\">응답한 사람</th>{head}</tr></thead><tbody>{body}</tbody></table></div>",
            list.len(),
            escape(&doc.id)
        )
    };
    shell(&app, &user, "/", &format!("{}{body}", doc_header(&app, &doc, "응답"))).into_response()
}

pub async fn responses_csv(State(app): State<AppState>, jar: PrivateCookieJar, Path(id): Path<String>) -> Response {
    let (_, doc, list) = match owner_only(&app, &jar, &id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let (cols, rows) = table(&list);
    // 엑셀이 한글을 UTF-8 로 읽게 BOM 을 붙인다.
    let mut out = String::from("\u{feff}");
    let header: Vec<String> = ["이름", "이메일", "제출 시각"].iter().map(|s| s.to_string()).chain(cols.iter().cloned()).collect();
    out += &header.iter().map(|h| csv_field(h)).collect::<Vec<_>>().join(",");
    out += "\r\n";
    for (r, row) in list.iter().zip(&rows) {
        let fields = [r.name.clone(), r.email.clone(), kst(r.updated_at)].into_iter().chain(cols.iter().map(|c| cell(row.get(c))));
        out += &fields.map(|f| csv_field(&f)).collect::<Vec<_>>().join(",");
        out += "\r\n";
    }
    (
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_owned()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"responses-{}.csv\"", doc.id)),
        ],
        out,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::{cell, csv_field, kst};
    use serde_json::json;

    #[test]
    fn csv_칸은_따옴표로_감싸고_수식은_무력화한다() {
        assert_eq!(csv_field("보통"), "\"보통\"");
        assert_eq!(csv_field("그는 \"좋다\""), "\"그는 \"\"좋다\"\"\"");
        assert_eq!(csv_field("=HYPERLINK(\"x\")"), "\"'=HYPERLINK(\"\"x\"\")\"");
        assert_eq!(csv_field("-1"), "\"'-1\"");
    }

    #[test]
    fn 답은_글자로_펼친다() {
        assert_eq!(cell(Some(&json!("예"))), "예");
        assert_eq!(cell(Some(&json!(3))), "3");
        assert_eq!(cell(Some(&json!(["A", "B"]))), "A, B");
        assert_eq!(cell(Some(&json!(null))), "");
        assert_eq!(cell(None), "");
        assert_eq!(kst(0), "1970-01-01 09:00");
    }
}
