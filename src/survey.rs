//! 설문 응답 모으기. 올린 문서의 스크립트가 `window.htmlhub.submit({질문: 답})` 을 부르면, 앱 틀이 보는 사람에게
//! "○○ 이름으로 제출할까요?" 를 묻고 허락하면 로그인한 이름으로 저장한다.
//! 문서는 격리된 iframe 이라 쿠키도 이름도 모른다. 틀에 메시지(postMessage)만 보내고, 실제 제출은 틀(이 사이트)이 한다.
//! 그래서 문서가 보는 사람 몰래 이름을 모을 수 없다. 응답은 올린 사람과 관리자만 보고, 한 사람이 다시 내면 덮어쓴다.

use crate::{
    AppState,
    auth::{current_user, escape, page, shell},
    docs::{SurveyResponse, can_modify, doc_header, server_error, user_and_doc},
};
use axum::{
    Json,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use axum_extra::extract::cookie::PrivateCookieJar;
use serde::Deserialize;
use serde_json::{Value, json};

pub const MAX_ANSWERS: usize = 64 * 1024;

/// 문서(`/raw`)에 넣는 `window.htmlhub.submit`. 틀에 묻고 답을 기다린다. 틀 없이 열면 제출할 수 없다고 알린다.
pub const BRIDGE: &str = "window.htmlhub={submit:function(answers){return new Promise(function(ok,no){\
if(window.parent===window){no(new Error('html-hub 문서 화면(/d/번호)에서 열어야 제출할 수 있습니다.'));return}\
var id=Math.random().toString(36).slice(2);\
function on(e){if(e.source!==window.parent||!e.data||e.data.htmlhub!==id)return;removeEventListener('message',on);\
e.data.ok?ok(e.data.result):no(new Error(e.data.error))}\
addEventListener('message',on);window.parent.postMessage({htmlhub:'submit',id:id,answers:answers},'*')})}};";

/// 앱 틀에 붙는 확인 창과, 문서의 제출 요청을 받아 확인 뒤 내는 스크립트.
/// 보낼 답은 전부 보여 준다. 창이 뜬 뒤 0.8초는 '제출' 을 막아, 문서가 유도한 두 번째 클릭이 바로 제출되지 않게 한다.
pub fn confirm_dialog(name: &str) -> String {
    format!(
        "<dialog class=\"ask\" id=\"ask\" aria-labelledby=\"ask-h\"><h2 id=\"ask-h\">설문 응답을 제출할까요?</h2>\
         <p>이 문서가 아래 응답을 <b>{}</b> 이름으로 제출하려고 합니다. 문서를 올린 사람과 관리자가 봅니다.</p><ul></ul>\
         <form method=\"dialog\"><button class=\"btn line\" value=\"cancel\">취소</button><button class=\"btn\" value=\"send\">제출</button></form></dialog>\
         <script>(function(){{var f=document.getElementById('doc'),d=document.getElementById('ask'),ul=d.querySelector('ul');\
         function text(v){{return Array.isArray(v)?v.map(text).join(', '):(v!==null&&typeof v==='object')?JSON.stringify(v):String(v)}}\
         addEventListener('message',async function(e){{\
         if(e.source!==f.contentWindow||!e.data||e.data.htmlhub!=='submit')return;\
         var reply=function(m){{m.htmlhub=e.data.id;f.contentWindow.postMessage(m,'*')}},a=e.data.answers;\
         if(!a||typeof a!=='object'||Array.isArray(a))return reply({{ok:false,error:'응답은 {{질문: 답}} 꼴의 객체여야 합니다.'}});\
         if(d.open)return reply({{ok:false,error:'다른 제출을 확인하는 중입니다.'}});\
         ul.replaceChildren.apply(ul,Object.keys(a).map(function(k){{var li=document.createElement('li');li.textContent=k+': '+text(a[k]);return li}}));\
         if(document.fullscreenElement)document.exitFullscreen();\
         var b=d.querySelector('button[value=send]');b.disabled=true;d.returnValue='';d.showModal();\
         setTimeout(function(){{b.disabled=false}},800);\
         var send=await new Promise(function(r){{d.addEventListener('close',function(){{r(d.returnValue==='send')}},{{once:true}})}});\
         if(!send)return reply({{ok:false,error:'제출을 취소했습니다.'}});\
         try{{var r=await fetch(location.pathname+'/responses',{{method:'POST',headers:{{'Content-Type':'application/json'}},body:JSON.stringify({{answers:a}})}});\
         var j=await r.json().catch(function(){{return {{}}}});reply(r.ok?{{ok:true,result:j}}:{{ok:false,error:j.error||('HTTP '+r.status)}})}}\
         catch(err){{reply({{ok:false,error:String(err)}})}}}})}})()</script>",
        escape(name)
    )
}

#[derive(Deserialize)]
pub struct Submission {
    answers: Value,
}

/// 앱 틀이 확인을 받은 뒤 보내는 응답. 로그인 쿠키로 누구인지 알고, 이 사이트 화면에서 온 요청만 받는다 (main 의 출처 검사).
pub async fn submit(State(app): State<AppState>, jar: PrivateCookieJar, Path(id): Path<String>, Json(s): Json<Submission>) -> Response {
    let fail = |status: StatusCode, msg: &str| (status, Json(json!({ "error": msg }))).into_response();
    let Ok(user) = current_user(&app, &jar) else {
        return fail(StatusCode::UNAUTHORIZED, "로그인이 풀렸습니다. 새로고침한 뒤 다시 제출해 주세요.");
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
        Ok(()) => Json(json!({ "ok": true })).into_response(),
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
         보는 사람이 확인한 뒤 그 사람 이름으로 여기에 모입니다.</p>"
            .to_owned()
    } else {
        let (cols, rows) = table(&list);
        let head: String = cols.iter().map(|c| format!("<th scope=\"col\">{}</th>", escape(c))).collect();
        let body: String = list
            .iter()
            .zip(&rows)
            .map(|(r, row)| {
                let answers: String = cols
                    .iter()
                    .map(|c| match cell(row.get(c)) {
                        v if v.is_empty() => "<td class=\"zero\">—</td>".to_owned(),
                        v => format!("<td>{}</td>", escape(&v)),
                    })
                    .collect();
                format!(
                    "<tr><td class=\"by\">{}<time>{}</time></td>{answers}</tr>",
                    escape(&r.name),
                    escape(&kst(r.updated_at))
                )
            })
            .collect();
        format!(
            "<p class=\"result\">응답 {}명<span class=\"sep\" aria-hidden=\"true\">·</span><a href=\"/d/{}/responses/csv\">CSV 내려받기</a></p>\
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
