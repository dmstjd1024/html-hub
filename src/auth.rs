use crate::{AppState, users::User};
use anyhow::Result;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use axum_extra::extract::cookie::{Cookie, PrivateCookieJar, SameSite};
use openidconnect::core::{CoreAuthPrompt, CoreClient, CoreProviderMetadata, CoreResponseType};
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
:root{--bg:#F5F5F1;--surface:#FFFFFF;--ink:#1D2622;--ink-2:#5B655F;--ink-3:#646D67;--line:#E3E5DF;--line-strong:#C9CEC6;--line-2:#EEF0EB;--forest:#1F3B2D;--leaf:#2F6B4F;--leaf-ink:#2F6B4F;--on-leaf:#FFFFFF;--track:#E6EBE6;--tag:#EEF2EE;--alert:#B3261E;--alert-bg:#FBEAE8;color-scheme:light}
@media (prefers-color-scheme:dark){.rail{border-right:1px solid #252D28}:root{--bg:#121815;--surface:#18201C;--ink:#E4EAE6;--ink-2:#A3AEA8;--ink-3:#909C95;--line:#2A3430;--line-strong:#3A463F;--line-2:#222B27;--forest:#16241C;--leaf:#5FA67F;--leaf-ink:#8FCBA8;--on-leaf:#0C1A12;--track:#26302B;--tag:#22302A;--alert:#F2B8B5;--alert-bg:#3A1F1D;color-scheme:dark}}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--ink);font:15px/1.6 "IBM Plex Sans KR",system-ui,-apple-system,"Apple SD Gothic Neo",sans-serif;font-variant-numeric:tabular-nums;word-break:keep-all;overflow-wrap:anywhere}
a{color:var(--leaf-ink)}
:focus-visible{outline:2px solid var(--leaf-ink);outline-offset:2px;border-radius:4px}
h1{font-size:24px;font-weight:600;line-height:1.3;margin:0 0 4px}
h2{font-size:15px;font-weight:600;margin:0 0 10px}
.lede{color:var(--ink-2);margin:0}
.visually-hidden{position:absolute;width:1px;height:1px;overflow:hidden;clip:rect(0 0 0 0);white-space:nowrap}
time{white-space:nowrap}
.app{display:grid;grid-template-columns:224px minmax(0,1fr);min-height:100vh}
.rail{background:var(--forest);color:#C9D8CE;padding:24px 12px 16px;display:flex;flex-direction:column;position:sticky;top:0;height:100vh}
.brand{color:#fff;font-weight:600;font-size:17px;text-decoration:none}
.brand small{font-weight:400;font-size:12px;color:#8FA698;margin-left:6px}
.rail .brand{padding:0 12px;margin-bottom:28px}
.rail nav{display:flex;flex-direction:column;gap:2px}
.rail nav a{display:flex;align-items:center;color:#C9D8CE;text-decoration:none;padding:8px 12px;border-radius:6px}
.rail nav a:hover{color:#fff;background:rgba(255,255,255,.06)}
.rail nav a[aria-current=page]{color:#fff;background:rgba(255,255,255,.12);font-weight:500}
.rail .count{margin-left:auto;font-size:12px;color:#8FA698}
.rail .group{font-size:12px;color:#8FA698;padding:20px 12px 6px}
.who{margin-top:auto;display:flex;align-items:center;gap:10px;padding:14px 12px 0;border-top:1px solid rgba(255,255,255,.1);font-size:13px;min-width:0}
.who div{min-width:0;flex:1}
.who b{display:block;color:#fff;font-weight:500}
.who small{display:block;color:#8FA698;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.who button{background:none;border:0;padding:4px 0;color:#C9D8CE;font:inherit;font-size:12px;cursor:pointer;white-space:nowrap}
.who button:hover{color:#fff;text-decoration:underline}
.avatar{flex:none;width:32px;height:32px;border-radius:50%;background:#2F5A44;color:#fff;display:inline-grid;place-items:center;font-size:13px;font-weight:600}
.avatar.sm{width:28px;height:28px;font-size:12px;background:var(--tag);color:var(--leaf-ink)}
.mbar,.mtabs,.mhead{display:none}
.crumb{font-size:13px;color:var(--ink-3);margin-bottom:6px}
.crumb a{color:var(--ink-2);text-decoration:none}
.crumb a:hover{text-decoration:underline}
.main{padding:32px 40px 48px;max-width:1320px;width:100%}
.head{display:flex;justify-content:space-between;align-items:flex-end;gap:16px;flex-wrap:wrap;margin-bottom:20px}
.head h1 small{font-size:15px;font-weight:400;color:var(--ink-3);margin-left:8px}
.btn{display:inline-block;background:var(--leaf);color:var(--on-leaf);border:0;border-radius:6px;padding:9px 16px;font:inherit;font-weight:500;text-decoration:none;cursor:pointer;white-space:nowrap;min-height:40px}
.btn:hover{filter:brightness(1.07)}
.btn.quiet{background:var(--tag);color:var(--ink);font-weight:500}
.btn.line{background:transparent;border:1px solid var(--line-strong);color:var(--ink)}
.btn.sm{padding:6px 12px;min-height:36px;font-size:14px}
.link{background:none;border:0;padding:0;font:inherit;color:var(--leaf-ink);cursor:pointer;text-decoration:none}
.link:hover{text-decoration:underline}
.link.danger{color:var(--alert)}
input[type=text],input[type=search],input:not([type]),input[type=file],textarea,select{font:inherit;color:var(--ink);background:var(--surface);border:1px solid var(--line);border-radius:6px;padding:9px 12px;width:100%}
textarea{resize:vertical}
.field{margin:0 0 20px}
.head+.form{margin-top:8px}
input[type=file]{height:44px;padding:0 6px;display:flex;align-items:center}
input[type=file]::file-selector-button{height:32px;padding:0 12px;margin:0 12px 0 0;border:1px solid var(--line);border-radius:5px;background:var(--bg);color:var(--ink);font:inherit;font-size:13.5px;font-weight:500;cursor:pointer}
.field label{display:block;font-weight:500;margin-bottom:6px}
.field .hint{display:block;color:var(--ink-2);font-size:13px;margin-top:4px}
.error{color:var(--alert);background:var(--alert-bg);border-radius:6px;padding:10px 14px;margin:0 0 18px}
.search{display:flex;align-items:center;gap:10px;height:48px;background:var(--surface);border:1px solid var(--line);border-radius:8px;padding:0 6px 0 14px;margin-bottom:16px}
.search .btn{height:36px;min-height:0;padding:0 14px;line-height:36px}
.doc-head{max-width:720px}
.mag{position:relative;flex:none;width:14px;height:14px;border:2px solid var(--ink-3);border-radius:50%}
.mag::after{content:"";position:absolute;width:6px;height:2px;background:var(--ink-3);right:-5px;bottom:-3px;transform:rotate(45deg);border-radius:1px}
.search:focus-within{border-color:var(--leaf-ink);outline:2px solid var(--leaf-ink);outline-offset:2px}
.search input{border:0;padding:0;height:36px;background:none;outline:none}
.search kbd{font:inherit;font-size:12px;color:var(--ink-3);border:1px solid var(--line);border-radius:4px;padding:0 6px}
.grid{display:grid;grid-template-columns:minmax(0,1fr) 280px;gap:28px;align-items:start}
.stats{display:grid;grid-template-columns:1fr 1.1fr 1fr;background:var(--surface);border:1px solid var(--line);border-radius:10px;margin:0 0 16px}
.stats>div{padding:14px 18px;border-left:1px solid var(--line-2);min-width:0}
.stats>div:first-child{border-left:0}
.stats dt{font-size:13px;color:var(--ink-2)}
.stats dd{margin:2px 0 0;font-size:28px;font-weight:600;line-height:1.2}
.stats dd small{font-size:15px;font-weight:400;color:var(--ink-3)}
.stats .sub{display:block;font-size:12px;font-weight:400;line-height:1.5;color:var(--ink-2);margin-top:4px}
.stats .sub a{text-decoration:none;font-weight:500}
.stats .with-spark{position:relative}
.spark-box{position:absolute;right:18px;bottom:14px}
.spark{display:grid;grid-template-columns:repeat(7,7px);gap:4px;align-items:end;height:30px}
.spark span{display:block;background:var(--track);border-radius:2px}
.spark span:not(.on){height:2px!important}
.spark span.on{background:var(--leaf);opacity:.55}
.spark span.today{opacity:1}
.spark-axis{display:flex;justify-content:space-between;font-size:11px;font-weight:400;line-height:1.4;color:var(--ink-3);margin-top:2px}
.toolbar{display:flex;justify-content:space-between;align-items:center;gap:12px;flex-wrap:wrap;margin-bottom:12px}
.chips{display:flex;flex-wrap:wrap;gap:6px;margin-bottom:20px}
.toolbar .chips{margin-bottom:0}
.chips a,.chips>span{font-size:13px;padding:4px 12px;border-radius:999px;background:var(--surface);border:1px solid var(--line);color:var(--ink);text-decoration:none;white-space:nowrap}
.chips a .n{color:var(--ink-3);margin-left:4px}
.chips a[aria-current=true],.chips>span[aria-current]{background:var(--leaf);border-color:var(--leaf);color:var(--on-leaf)}
.chips a[aria-current=true] .n{color:inherit;opacity:.75}
.sort{display:flex;align-items:center;gap:8px;font-size:13px;color:var(--ink-2)}
.sort select{width:auto;padding:5px 10px;font-size:13px}
.result{color:var(--ink-2);font-size:14px;margin:0 0 8px}
.board{background:var(--surface);border:1px solid var(--line);border-radius:10px;overflow:hidden}
.board table{width:100%;border-collapse:collapse}
th{text-align:left;font-size:12px;font-weight:500;color:var(--ink-2);padding:10px 12px;border-bottom:1px solid var(--line);white-space:nowrap}
td{padding:7px 12px;height:52px;line-height:1.35;border-bottom:1px solid var(--line-2);vertical-align:middle}
.zero{color:var(--ink-3)}
.sep{color:var(--ink-3);margin:0 6px}
tbody tr:last-child td{border-bottom:0}
th.num,td.num{text-align:right}
.sort-link{color:inherit;text-decoration:none}
.sort-link:hover{color:var(--ink);text-decoration:underline}
td.num{white-space:nowrap}
td.t{width:40%;max-width:0}
.doc-title{display:block;color:var(--ink);font-weight:500;text-decoration:none;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.doc-title:hover{color:var(--leaf-ink);text-decoration:underline}
.excerpt{display:block;font-size:13px;color:var(--ink-2);margin-top:2px;white-space:normal}
.by{font-size:14px;white-space:nowrap}
.by time{display:block;font-size:12.5px;color:var(--ink-3)}
.when{white-space:nowrap;color:var(--ink-2);font-size:14px}
.meter{display:inline-block;width:44px;height:4px;background:var(--track);border-radius:2px;vertical-align:middle;margin-right:8px;overflow:hidden}
.meter i{display:block;height:100%;background:var(--leaf);border-radius:2px}
.tag{display:inline-block;font-size:12px;background:var(--tag);border-radius:4px;padding:1px 8px;white-space:nowrap}
.tag.none{background:none;border:1px dashed var(--line);color:var(--ink-2)}
.acts{display:flex;justify-content:flex-end;gap:12px;font-size:13px;white-space:nowrap}
.acts a,.acts .link{color:var(--ink-2);text-decoration:none}
.acts a:hover,.acts .link:hover{color:var(--leaf-ink);text-decoration:underline}
.acts .link.danger{color:var(--alert)}
.acts form,.acts .slot{display:inline-block;width:2.5em;text-align:right}
.rowmenu form{display:inline}
.rowmenu{display:none}
.board-foot{display:flex;justify-content:space-between;align-items:center;gap:12px;padding:10px 14px;border-top:1px solid var(--line-2);font-size:13px;color:var(--ink-2)}
.pager{display:flex;gap:8px;align-items:center}
.pager form{display:inline}
.pager button{font:inherit;font-size:13px;background:var(--surface);color:var(--ink);border:1px solid var(--line);border-radius:6px;padding:4px 12px;cursor:pointer}
.side section{margin-bottom:28px}
.side h2{display:flex;justify-content:space-between;align-items:baseline;font-size:14px;line-height:1.2;padding:0 0 10px;border-bottom:1px solid var(--line);margin:0}
.side h2 small{font-size:12px;font-weight:400;color:var(--ink-3)}
.side ol,.side ul{list-style:none;margin:0;padding:0}
.rank li{display:grid;grid-template-columns:18px minmax(0,1fr) auto;gap:4px 8px;align-items:center;padding:10px 0;border-bottom:1px solid var(--line-2);font-size:14px}
.rank .no{color:var(--ink-3);font-size:12px}
.rank a{color:var(--ink);font-weight:500;text-decoration:none;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.rank a:hover{color:var(--leaf-ink);text-decoration:underline}
.rank .n{font-size:12px;color:var(--ink-2)}
.rank .meter{grid-column:2/4;width:100%;margin:0;height:3px}
.cmts li{display:grid;grid-template-columns:28px minmax(0,1fr);gap:10px;padding:12px 0;border-bottom:1px solid var(--line-2);font-size:14px}
.cmts b{font-weight:500}
.cmts time{font-size:12px;color:var(--ink-3);margin-left:4px}
.cmts p{margin:2px 0;color:var(--ink);display:-webkit-box;-webkit-line-clamp:2;-webkit-box-orient:vertical;overflow:hidden}
.cmts a{display:block;font-size:12px;text-decoration:none;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.cmts a:hover{text-decoration:underline}
.quiet{color:var(--ink-2);font-size:14px;margin:10px 0 0}
.empty{padding:28px 18px;color:var(--ink-2);margin:0}
.panel{background:var(--surface);border:1px solid var(--line);border-radius:10px;padding:16px 18px;margin-bottom:16px}
.sheet{max-width:640px;margin:12vh auto;padding:28px 32px 32px;background:var(--surface);border:1px solid var(--line);border-radius:10px;font-size:16px}
.sheet .brand{display:inline-block;color:var(--leaf-ink);margin-bottom:16px}
.landing h1{font-size:26px;margin-bottom:10px}
.landing .btn{margin-top:12px}
.landing .foot{font-size:13px;color:var(--ink-3);margin:28px 0 0;padding-top:16px;border-top:1px solid var(--line-2)}
.landing .foot a{color:var(--ink-2)}
.form{max-width:560px}
.copy{display:flex;gap:8px;margin:16px 0;max-width:760px}
.narrow{max-width:760px}
.table-wrap{overflow-x:auto}
.table-wrap.narrow table,.table-wrap table{width:100%;border-collapse:collapse}
.tabs{display:flex;gap:20px;border-bottom:1px solid var(--line);margin:4px 0 8px;max-width:720px}
.tabs a,.tabs>span{display:inline-flex;align-items:center;gap:4px;height:40px;font-size:14.5px;color:var(--ink-2);text-decoration:none;border-bottom:2px solid transparent;margin-bottom:-1px}
.tabs a:hover{color:var(--ink)}
.tabs [aria-current]{color:var(--ink);font-weight:600;border-color:var(--leaf-ink)}
.tabs .n{color:var(--ink-3);font-weight:400}
.comments{list-style:none;padding:0;margin:0 0 8px;max-width:720px}
.comments li{display:grid;grid-template-columns:28px minmax(0,1fr);gap:10px;padding:14px 0;border-bottom:1px solid var(--line-2)}
.who-line{display:flex;align-items:center;gap:8px;min-height:28px}
.who-line b{font-size:13.5px;font-weight:600}
.who-line time{font-size:13px;color:var(--ink-2)}
.who-line form{margin-left:auto}
.comments .body{font-size:15px;line-height:1.6;white-space:pre-wrap}
.ghost{background:none;border:0;font:inherit;font-size:13px;padding:5px 7px;border-radius:5px;cursor:pointer;color:var(--ink-2)}
.ghost.danger{color:var(--alert)}
.ghost.danger:hover{background:var(--alert-bg)}
.reply{max-width:720px}
.reply label{display:block;font-weight:500;margin:20px 0 6px}
.reply-foot{display:flex;justify-content:space-between;align-items:center;gap:12px;margin-top:8px}
.reply-foot .hint{color:var(--ink-2);font-size:13px}
.reply .btn{min-height:40px}
.actions{white-space:nowrap;font-size:14px}
.actions a,.actions .link{margin-right:12px;color:var(--ink-2);text-decoration:none}
.actions a:hover,.actions .link:hover{color:var(--leaf-ink);text-decoration:underline}
.actions .link.danger{color:var(--alert)}
.actions form{display:inline}
pre{background:var(--tag);border-radius:6px;padding:12px 14px;overflow-x:auto;font-size:13px}
.viewer{display:flex;flex-direction:column;height:100vh;height:100dvh}
.vbar{display:flex;align-items:center;gap:12px;height:44px;padding:0 8px 0 16px;background:var(--forest);flex:none;min-width:0}
.vbar .brand{font-size:15px;flex:none}
.vtitle{flex:1;min-width:0;margin:0;font-size:14.5px;font-weight:500;color:#fff;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.vbar nav{display:flex;flex:none}
.vbar nav a{display:flex;align-items:center;min-height:44px;padding:0 8px;color:#C9D8CE;font-size:13.5px;text-decoration:none;white-space:nowrap}
.vbar nav a:hover{color:#fff}
.vbar .n{color:#8FA698;margin-left:3px}
.viewer iframe{flex:1;width:100%;border:0;background:#fff}
dialog.ask{border:1px solid var(--line);border-radius:12px;padding:20px 22px;width:min(440px,92vw);background:var(--surface);color:var(--ink)}
dialog.ask::backdrop{background:rgba(0,0,0,.4)}
dialog.ask h2{font-size:17px}
dialog.ask ul{margin:8px 0 16px;padding-left:18px;max-height:40vh;overflow:auto;font-size:14px;color:var(--ink-2)}
dialog.ask form{display:flex;gap:8px;justify-content:flex-end}
@media (max-width:560px){.vbar .brand,.vbar nav a.opt{display:none}}
@media (max-width:1200px){.grid{grid-template-columns:minmax(0,1fr)}.side{display:grid;grid-template-columns:1fr 1fr;gap:24px}}
@media (max-width:860px){
.app{display:block}
.rail{display:none}
.mbar{display:flex;align-items:center;gap:10px;background:var(--forest);padding:8px 12px 8px 16px}
.mbar .brand{margin-right:auto}
.mtabs{display:flex;gap:4px;height:44px;padding:0 6px;background:var(--forest);overflow-x:auto;scrollbar-width:none;position:sticky;top:0;z-index:5}
.mtabs::-webkit-scrollbar{display:none}
.mtabs a{display:flex;align-items:center;padding:0 10px;font-size:14.5px;color:#C9D8CE;text-decoration:none;white-space:nowrap;border-bottom:2px solid transparent}
.mtabs a[aria-current=page]{color:#fff;font-weight:600;border-color:#8FCBA8}
.mtabs .count{font-size:12px;color:#8FA698;margin-left:4px}
.mtabs .group,.mtabs a[href="/upload"]{display:none}
.menu{position:relative}
.menu summary{list-style:none;cursor:pointer;display:grid;place-items:center;width:44px;height:44px}
.menu summary::-webkit-details-marker{display:none}
.menu>div{position:absolute;right:0;top:48px;z-index:6;width:240px;background:var(--surface);border:1px solid var(--line);border-radius:10px;padding:8px}
.menu .me{padding:8px 12px 0;font-size:13px;color:var(--ink-2)}
.menu .me button{display:block;width:100%;text-align:left;background:none;border:0;border-top:1px solid var(--line-2);margin-top:8px;font:inherit;color:var(--leaf-ink);min-height:44px;cursor:pointer}
.main{padding:16px 16px 40px}
.head{margin-bottom:12px}
.head .btn,.head .lede{display:none}
.stats{grid-template-columns:repeat(3,1fr)}
.stats>div{padding:10px 12px;display:flex;flex-direction:column;justify-content:flex-start}
.stats dt{font-size:12px;line-height:1.4}
.stats dt .long{display:none}
.stats dd{font-size:22px}
.stats .with-spark .sub,.stats .spark-axis{display:none}
.spark-box{position:static;display:block}
.spark{grid-template-columns:repeat(7,5px);gap:2px;height:17px;margin-top:4px}
.toolbar .sort,.search kbd,.board-foot.plain>span,.unfiled-note{display:none}
.board-foot.plain:not(:has(.pager)){display:none}
.chips{flex-wrap:nowrap;overflow-x:auto;scrollbar-width:none;padding-bottom:4px}
.chips::-webkit-scrollbar{display:none}
.mhead{display:grid;grid-template-columns:minmax(0,1fr) 32px 32px 44px;align-items:center;gap:0 8px;padding:6px 4px 6px 14px;border-bottom:1px solid var(--line);font-size:13px;color:var(--ink-2)}
.mhead span{text-align:center}
.mhead select{width:auto;border:0;padding:6px 0;font-size:13px;color:var(--ink-2);background:none}
.board thead{display:none}
.board table,.board tbody{display:block}
.board tr{display:grid;grid-template-columns:auto minmax(0,1fr) 32px 32px 44px;grid-template-areas:"t t v c m" "g b v c m";gap:2px 8px;align-items:center;padding:10px 4px 10px 14px;border-bottom:1px solid var(--line-2)}
.board td{display:block;border:0;padding:0;height:auto;font-size:13px}
.board td.t{grid-area:t;width:auto;max-width:none;font-size:15px}
.board td.t .doc-title{white-space:normal;display:-webkit-box;-webkit-line-clamp:2;-webkit-box-orient:vertical}
.board td.g{grid-area:g}
.board td.by{grid-area:b;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:var(--ink-2)}
.board .by time{display:inline;margin-left:6px}
.board td.v{grid-area:v}
.board td.c{grid-area:c}
.board td.num{text-align:center;font-size:15px;color:var(--ink)}
.board td.num.zero{color:var(--ink-3)}
.board td.m{grid-area:m}
.board .meter{display:none}
.acts{display:none}
.rowmenu{display:block;position:relative}
.rowmenu summary{list-style:none;cursor:pointer;width:44px;height:44px;display:grid;place-items:center;color:var(--ink-2);font-size:18px}
.rowmenu summary::-webkit-details-marker{display:none}
.rowmenu div{position:absolute;right:0;top:44px;z-index:3;min-width:150px;background:var(--surface);border:1px solid var(--line);border-radius:8px;padding:6px;display:flex;flex-direction:column}
.rowmenu a,.rowmenu button{display:flex;align-items:center;min-height:44px;padding:0 12px;color:var(--ink);text-decoration:none;font:inherit;font-size:14px;background:none;border:0;cursor:pointer;text-align:left;width:100%}
.rowmenu .danger{color:var(--alert)}
.side{grid-template-columns:1fr}
.sheet{margin:0;min-height:100vh;border:0;border-radius:0;padding:32px 20px}
.landing .btn{display:block;text-align:center}
}
"#;

fn document(body: &str) -> Html<String> {
    document_titled("html-hub", body)
}

pub(crate) fn document_titled(title: &str, body: &str) -> Html<String> {
    let title = escape(title);
    Html(format!(
        "<!doctype html><html lang=\"ko\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{title}</title>\
         <link rel=\"preconnect\" href=\"https://fonts.googleapis.com\">\
         <link rel=\"preconnect\" href=\"https://fonts.gstatic.com\" crossorigin>\
         <link rel=\"stylesheet\" href=\"https://fonts.googleapis.com/css2?family=IBM+Plex+Sans+KR:wght@400;500;600&display=swap\">\
         <style>{CSS}</style></head><body>{body}</body></html>"
    ))
}

/// 로그인 없이 보이거나 혼자 떠 있는 안내 화면 (오류, 로그아웃, 약관).
pub(crate) fn page(body: &str) -> Html<String> {
    document(&format!("<main class=\"sheet\"><h1 class=\"visually-hidden\">html-hub 안내</h1><a class=\"brand\" href=\"/\">html-hub</a><div>{body}</div></main>"))
}

/// 로그인 전 첫 화면. 구글로 바로 넘기지 않고 무슨 서비스인지와 로그인 버튼을 먼저 보여 준다.
/// 세션이 있으면 None 이라 호출한 쪽이 평소 화면을 그린다.
pub(crate) fn landing(jar: &PrivateCookieJar) -> Option<Response> {
    if session_email(jar).is_some() {
        return None;
    }
    Some(
        document(
            "<main class=\"sheet landing\"><span class=\"brand\">html-hub</span>\
             <h1>사내 HTML 문서 공유</h1>\
             <p class=\"lede\">보고서·대시보드처럼 HTML 로 만든 문서를 올리고 링크 하나로 동료와 나눕니다. \
             댓글과 고친 기록도 함께 남습니다.</p>\
             <p><a class=\"btn\" href=\"/login\">Google 계정으로 로그인</a></p>\
             <p class=\"quiet\">회사 Google 계정으로 로그인하세요. 등록된 직원만 들어올 수 있습니다.</p>\
             <p class=\"foot\"><a href=\"/privacy\">개인정보 처리방침</a><span class=\"sep\">·</span><a href=\"/terms\">서비스 약관</a></p></main>",
        )
        .into_response(),
    )
}

/// 로그인한 직원이 보는 화면. 왼쪽 메뉴 띠(휴대폰에서는 위쪽 줄)에 현재 위치와 문서 수를 표시한다.
pub(crate) fn shell(app: &AppState, user: &User, current: &str, body: &str) -> Html<String> {
    let sum = app.docs.summary(&user.email).unwrap_or_default();
    let link = |href: &str, label: &str, count: Option<i64>| {
        let cur = if href == current { " aria-current=\"page\"" } else { "" };
        let count = count.map(|n| format!("<span class=\"count\">{n}</span>")).unwrap_or_default();
        format!("<a href=\"{href}\"{cur}>{label}{count}</a>")
    };
    let mut nav = format!("{}{}{}", link("/", "문서", Some(sum.total)), link("/upload", "올리기", None), link("/tokens", "내 토큰", None));
    if user.is_admin() {
        nav += &format!(
            "<span class=\"group\">관리자</span>{}{}",
            link("/admin/hidden", "숨긴 문서", Some(sum.hidden)),
            link("/admin/tokens", "토큰 관리", None)
        );
    }
    let initial: String = user.name.chars().next().map(|c| c.to_string()).unwrap_or_default();
    let (name, email, initial) = (escape(&user.name), escape(&user.email), escape(&initial));
    let logout = "<form method=\"post\" action=\"/logout\"><button>로그아웃</button></form>";
    document(&format!(
        "<header class=\"mbar\"><a class=\"brand\" href=\"/\">html-hub</a><a class=\"btn sm\" href=\"/upload\">올리기</a>\
         <details class=\"menu\"><summary aria-label=\"내 계정\"><span class=\"avatar\">{initial}</span></summary>\
         <div><div class=\"me\">{name}<br>{email}{logout}</div></div></details></header>\
         <nav class=\"mtabs\" aria-label=\"메뉴\">{nav}</nav>\
         <div class=\"app\"><aside class=\"rail\" aria-label=\"사이드 메뉴\"><a class=\"brand\" href=\"/\">html-hub<small>Greenery</small></a>\
         <nav aria-label=\"메뉴\">{nav}</nav>\
         <div class=\"who\"><span class=\"avatar\">{initial}</span><div><b>{name}</b><small>{email}</small></div>{logout}</div></aside>\
         <main class=\"main\">{body}</main></div>"
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

/// 로그인 뒤 돌아갈 곳은 "/d/<문서번호>" 와 Claude 연결 승인 화면("/oauth/authorize?...")만 받는다.
/// 아무 주소나 받으면 다른 사이트로 보내는 통로가 된다.
fn safe_next(next: &str) -> String {
    if let Some(q) = next.strip_prefix("/oauth/authorize?") {
        // 로그인 중 쿠키(4KB 한도)에 암호화해 담으므로 길이를 묶는다.
        if q.len() <= 2048 && q.chars().all(|c| c.is_ascii_graphic() && c != '\\') {
            return next.to_owned();
        }
    }
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
        // 구글은 마지막에 쓴 계정(개인 Gmail 등)으로 조용히 넘길 수 있다. 늘 계정을 고르게 한다.
        .add_prompt(CoreAuthPrompt::SelectAccount)
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
                jar.remove(expired(OAUTH_COOKIE)),
                page(&format!(
                    "<p><b>{}</b> 계정은 등록되지 않았거나 비활성화되어 들어올 수 없습니다.</p>\
                     <p>회사 계정으로 다시 로그인하세요. 회사 계정인데도 안 되면 IT 팀에 문의하세요.</p>\
                     <p><a class=\"btn\" href=\"/login\">다른 계정으로 로그인</a></p>",
                    escape(&email)
                )),
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
        // 승인 화면 주소에는 & 가 들어 있어 next 값 안에 넣을 때 바꿔 쓴다.
        let next = safe_next(next).replace('%', "%25").replace('&', "%26").replace('+', "%2B").replace('#', "%23");
        return Err(Redirect::to(&format!("/login?next={next}")).into_response());
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
        let oauth = "/oauth/authorize?client_id=x&redirect_uri=https%3A%2F%2Fclaude.ai%2Fapi%2Fmcp%2Fauth_callback";
        assert_eq!(safe_next(oauth), oauth);
        for bad in ["", "/", "//evil.com", "https://evil.com", "/d/", "/d/a/b", "/d/../x", "/d/a?x=1", "/login", "/d//evil.com",
                    "/oauth/authorize", "/oauth/authorize?a b", "/oauth/authorize?a\\b", "/oauth/token?x=1"] {
            assert_eq!(safe_next(bad), "/", "{bad}");
        }
    }
}
