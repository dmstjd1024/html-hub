//! E2E: 실제 바이너리를 임시 DB·파일 폴더와 빈 포트로 띄우고 HTTP 로 PRD 인수 조건을 확인한다.
//! 구글 로그인은 자동화할 수 없어서, 서버와 같은 SESSION_KEY 로 세션 쿠키를 직접 암호화해 쓴다.
//! 서버가 켜질 때 구글 OIDC 설정을 받아오므로 네트워크가 필요하다.
//! 실행: cargo test --test e2e

use axum::response::IntoResponse;
use axum_extra::extract::cookie::{Cookie, Key, PrivateCookieJar};
use openidconnect::reqwest::{self, StatusCode, header};
use openidconnect::url::form_urlencoded;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SESSION_KEY: &str = "e2e-session-key-0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
const BIN: &str = env!("CARGO_BIN_EXE_html-hub");
const MAX_BYTES: usize = 10 * 1024 * 1024;

const OWNER: &str = "owner@e2e.test";
const OTHER: &str = "other@e2e.test";
const ADMIN: &str = "admin@e2e.test";
const GONE: &str = "gone@e2e.test";
const STRANGER: &str = "stranger@e2e.test";

struct Server {
    child: Child,
    base: String,
    dir: PathBuf,
    http: reqwest::Client,
}

struct Res {
    status: StatusCode,
    headers: header::HeaderMap,
    body: String,
}

impl Res {
    fn location(&self) -> &str {
        self.headers.get(header::LOCATION).map(|v| v.to_str().unwrap()).unwrap_or("")
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn enc(s: &str) -> String {
    form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

impl Server {
    async fn start() -> Self {
        // 시계만으로는 병렬 테스트끼리 이름이 겹친다 (macOS 는 마이크로초 단위). 순번을 붙인다.
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("html-hub-e2e-{}-{seq}-{}", std::process::id(), now_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("h.db");

        // 명단은 실제 CLI(import-users)로 넣는다. 퇴사자는 비활성, 관리자는 DB 에서 직접 지정한다 (정책서 D2-1).
        let export = serde_json::json!({ "users": [
            { "First Name [Required]": "올림", "Last Name [Required]": "김", "Email Address [Required]": OWNER, "Status [READ ONLY]": "Active" },
            { "First Name [Required]": "다른", "Last Name [Required]": "이", "Email Address [Required]": OTHER, "Status [READ ONLY]": "Active" },
            { "First Name [Required]": "관리", "Last Name [Required]": "박", "Email Address [Required]": ADMIN, "Status [READ ONLY]": "Active" },
            { "First Name [Required]": "퇴사", "Last Name [Required]": "최", "Email Address [Required]": GONE, "Status [READ ONLY]": "Suspended" },
        ]});
        let json = dir.join("users.json");
        std::fs::write(&json, export.to_string()).unwrap();
        let out = Command::new(BIN).arg("import-users").arg(&json).env("DATABASE_PATH", &db).output().unwrap();
        assert!(out.status.success(), "import-users 실패: {}", String::from_utf8_lossy(&out.stderr));
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute("UPDATE users SET role = 'admin' WHERE email = ?1", [ADMIN])
            .unwrap();

        let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        // 빈 포트를 고른 뒤 놓는 사이에 병렬 테스트가 같은 포트를 잡을 수 있다.
        // 서버마다 다른 client_id 를 주고 /login 리다이렉트로 내 서버인지 확인한다. 못 뜨면 다른 포트로 다시 띄운다.
        for _ in 0..5 {
            let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
            let base = format!("http://127.0.0.1:{port}");
            let client_id = format!("e2e-{port}-{}", now_nanos());
            let child = Command::new(BIN)
                .env("SESSION_KEY", SESSION_KEY)
                .env("GOOGLE_CLIENT_ID", &client_id)
                .env("GOOGLE_CLIENT_SECRET", "e2e-secret")
                .env("BASE_URL", &base)
                .env("BIND_ADDR", format!("127.0.0.1:{port}"))
                .env("DATABASE_PATH", &db)
                .env("FILES_DIR", dir.join("files"))
                .stdout(Stdio::null())
                // 서버 로그가 필요하면 E2E_LOG=1 로 돌린다.
                .stderr(if std::env::var_os("E2E_LOG").is_some() { Stdio::inherit() } else { Stdio::null() })
                .spawn()
                .unwrap();
            let mut s = Server { child, base, dir: dir.clone(), http: http.clone() };
            for _ in 0..150 {
                if s.child.try_wait().unwrap().is_some() {
                    break; // 포트를 뺏겨 서버가 끝났다
                }
                if let Ok(r) = s.http.get(format!("{}/login", s.base)).send().await {
                    let to = r.headers().get(header::LOCATION).map(|v| v.to_str().unwrap().to_owned()).unwrap_or_default();
                    if to.contains(&format!("client_id={client_id}")) {
                        return s;
                    }
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            // 다음 시도에서 같은 폴더를 다시 쓰므로 지우지 않고 프로세스만 끝낸다.
            let _ = s.child.kill();
            let _ = s.child.wait();
            std::mem::forget(s);
        }
        panic!("서버가 뜨지 않았다 (구글 OIDC 설정을 받으려면 네트워크가 필요하다)");
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.dir.join("h.db")).unwrap()
    }

    /// 서버와 같은 키로 암호화한 세션 쿠키 ("이메일|만료시각").
    fn login(&self, email: &str) -> String {
        let jar = PrivateCookieJar::new(Key::from(SESSION_KEY.as_bytes()))
            .add(Cookie::new("session", format!("{email}|{}", now() + 3600)));
        let res = jar.into_response();
        let set = res.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap();
        set.split(';').next().unwrap().to_owned()
    }

    async fn send(&self, req: reqwest::RequestBuilder, cookie: Option<&str>) -> Res {
        let req = match cookie {
            Some(c) => req.header(header::COOKIE, c),
            None => req,
        };
        let r = req.send().await.unwrap();
        let status = r.status();
        let headers = r.headers().clone();
        let body = r.text().await.unwrap();
        Res { status, headers, body }
    }

    async fn get(&self, path: &str, cookie: Option<&str>) -> Res {
        self.send(self.http.get(format!("{}{path}", self.base)), cookie).await
    }

    async fn post(&self, path: &str, cookie: Option<&str>, fields: &[(&str, &str)]) -> Res {
        let body: String = form_urlencoded::Serializer::new(String::new()).extend_pairs(fields).finish();
        let req = self
            .http
            .post(format!("{}{path}", self.base))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body);
        self.send(req, cookie).await
    }

    async fn multipart(&self, path: &str, cookie: Option<&str>, bearer: Option<&str>, fields: &[(&str, &str)], file: Option<(&str, &[u8])>) -> Res {
        let boundary = "e2eBoundary7MA4YWxkTrZu0gW";
        let mut body = Vec::new();
        for (k, v) in fields {
            body.extend(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes());
        }
        if let Some((name, bytes)) = file {
            body.extend(
                format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: text/html\r\n\r\n")
                    .as_bytes(),
            );
            body.extend(bytes);
            body.extend(b"\r\n");
        }
        body.extend(format!("--{boundary}--\r\n").as_bytes());
        let mut req = self
            .http
            .post(format!("{}{path}", self.base))
            .header(header::CONTENT_TYPE, format!("multipart/form-data; boundary={boundary}"))
            .body(body);
        if let Some(t) = bearer {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        self.send(req, cookie).await
    }

    /// 웹으로 올리고 문서 번호를 돌려준다.
    async fn upload(&self, cookie: &str, title: &str, project: &str, html: &str) -> String {
        let r = self
            .multipart("/upload", Some(cookie), None, &[("title", title), ("project", project)], Some(("doc.html", html.as_bytes())))
            .await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "올리기 실패: {}", r.body);
        r.location().strip_prefix("/uploaded/").expect("올린 뒤 /uploaded/<번호> 로 가야 한다").to_owned()
    }

    async fn doc_count(&self) -> i64 {
        self.db().query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0)).unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn now_nanos() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
}

/// 목록 표의 제목 칸들 (`<td><a href="/d/..">제목</a>`), 위에서부터.
fn listed_titles(body: &str) -> Vec<String> {
    body.split("<tr><td><a href=\"/d/")
        .skip(1)
        .map(|s| s.split_once("\">").unwrap().1.split_once("</a>").unwrap().0.to_owned())
        .collect()
}

// ---------- 공개 화면과 로그인 ----------

#[tokio::test]
async fn 공개_화면은_healthz_privacy_terms_login_뿐이다() {
    let s = Server::start().await;
    assert_eq!(s.get("/healthz", None).await.body, "ok");
    assert_eq!(s.get("/privacy", None).await.status, StatusCode::OK);
    assert_eq!(s.get("/terms", None).await.status, StatusCode::OK);
    for path in ["/", "/upload", "/tokens", "/admin/hidden", "/admin/tokens"] {
        let r = s.get(path, None).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{path} 는 로그인이 필요하다");
        assert!(r.location().starts_with("/login"), "{path}: {}", r.location());
    }

    // 로그인 시작은 구글로 보내고, 로그인 중 상태 쿠키는 30분 산다.
    let r = s.get("/login", None).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.location().starts_with("https://accounts.google.com/"), "{}", r.location());
    let set = r.headers.get(header::SET_COOKIE).unwrap().to_str().unwrap();
    assert!(set.starts_with("oauth=") && set.contains("Max-Age=1800") && set.contains("HttpOnly"), "{set}");
}

#[tokio::test]
async fn 명단에_없거나_비활성인_계정은_들어오지_못한다() {
    let s = Server::start().await;
    for email in [STRANGER, GONE] {
        let r = s.get("/", Some(&s.login(email))).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{email}");
        assert_eq!(r.location(), "/login");
    }
    // 위조한 쿠키(다른 키)는 로그인으로 취급하지 않는다.
    let r = s.get("/", Some("session=forged-value")).await;
    assert!(r.location().starts_with("/login"));
    assert_eq!(s.get("/", Some(&s.login(OWNER))).await.status, StatusCode::OK);
}

#[tokio::test]
async fn 로그아웃하면_세션_쿠키를_지운다() {
    let s = Server::start().await;
    let r = s.post("/logout", Some(&s.login(OWNER)), &[]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("로그아웃되었습니다"));
    let cleared = r
        .headers
        .get_all(header::SET_COOKIE)
        .iter()
        .any(|v| v.to_str().unwrap().starts_with("session=") && v.to_str().unwrap().contains("Max-Age=0"));
    assert!(cleared, "세션 쿠키를 만료시켜야 한다");
}

// ---------- R1-1 웹으로 올리기 ----------

#[tokio::test]
async fn r1_1_올리기_화면과_입력_검사() {
    let s = Server::start().await;
    let me = s.login(OWNER);

    let page = s.get("/upload", Some(&me)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("전 직원이 볼 수 있습니다"), "공개 범위 안내 문구");

    let html: &[u8] = b"<p>hi</p>";
    let long_title = "가".repeat(201);
    let cases: Vec<(&str, Option<(&str, Vec<u8>)>, &str)> = vec![
        ("", Some(("a.html", html.to_vec())), "제목을 입력해 주세요"),
        ("제목", Some(("a.txt", html.to_vec())), ".html 파일만"),
        ("제목", Some(("a.html", vec![])), "HTML 파일을 선택해 주세요"),
        ("제목", None, "HTML 파일을 선택해 주세요"),
        ("제목", Some(("big.html", vec![b'a'; MAX_BYTES + 1])), "10MB"),
        (&long_title, Some(("a.html", html.to_vec())), "200자"),
    ];
    for (title, file, msg) in &cases {
        let r = s
            .multipart("/upload", Some(&me), None, &[("title", title), ("project", "P")], file.as_ref().map(|(n, b)| (*n, b.as_slice())))
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{msg}");
        assert!(r.body.contains(msg), "안내 문구 '{msg}' 가 없다");
    }
    // 본문 한도를 크게 넘는 요청도 400 과 안내로 끝난다.
    let r = s.multipart("/upload", Some(&me), None, &[("title", "큼")], Some(("big.html", &vec![b'a'; 11 * 1024 * 1024]))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(s.doc_count().await, 0, "거부된 요청은 저장되지 않는다");

    // .htm 과 대문자 확장자도 받는다.
    let r = s.multipart("/upload", Some(&me), None, &[("title", "htm")], Some(("A.HTM", html))).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn r1_1_올리면_추측할_수_없는_링크가_나오고_같은_제목도_따로_저장된다() {
    let s = Server::start().await;
    let me = s.login(OWNER);
    let a = s.upload(&me, "같은 제목", "P", "<p>1</p>").await;
    let b = s.upload(&me, "같은 제목", "P", "<p>2</p>").await;
    assert_ne!(a, b);
    assert!(a.len() >= 22 && a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'), "{a}");
    assert_eq!(s.doc_count().await, 2);

    // 올린 직후 화면: 사이트 주소까지 붙인 전체 링크와 복사 버튼.
    let r = s.get(&format!("/uploaded/{a}"), Some(&me)).await;
    assert!(r.body.contains(&format!("value=\"{}/d/{a}\"", s.base)), "전체 링크");
    assert!(r.body.contains("링크 복사"));

    // 기록: 올린 사람·프로젝트가 DB 에, 파일은 파일 폴더에.
    let (who, project): (String, String) = s
        .db()
        .query_row("SELECT uploader_name, project FROM documents WHERE id = ?1", [&a], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!((who.as_str(), project.as_str()), ("김올림", "P"));
    assert!(std::fs::read_dir(s.dir.join("files")).unwrap().count() >= 2);
}

// ---------- R1-2 링크로 열기 ----------

#[tokio::test]
async fn r1_2_링크로_열기() {
    let s = Server::start().await;
    let me = s.login(OWNER);
    let html = "<!doctype html><p id=t>문서</p><script>document.cookie</script>";
    let id = s.upload(&me, "열기", "", html).await;

    // 로그인 전이면 로그인 뒤 같은 문서로 돌아오게 보낸다.
    let r = s.get(&format!("/d/{id}"), None).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(r.location(), format!("/login?next=/d/{id}"));

    // 명단에 없는 계정은 보지 못한다.
    assert_eq!(s.get(&format!("/d/{id}"), Some(&s.login(STRANGER))).await.location(), "/login");

    // 다른 직원도 볼 수 있다 (프로젝트는 열람 범위를 나누지 않는다).
    for who in [OWNER, OTHER] {
        let r = s.get(&format!("/d/{id}"), Some(&s.login(who))).await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(r.body, html);
        let csp = r.headers.get(header::CONTENT_SECURITY_POLICY).unwrap().to_str().unwrap();
        assert_eq!(csp, "sandbox allow-scripts", "문서 속 JS 가 앱 쿠키·세션을 쓰지 못하게 격리");
        assert_eq!(r.headers.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");
    }

    // 없는 번호는 안내만.
    let r = s.get("/d/없는번호", Some(&me)).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert!(r.body.contains("없는 문서"));

    // 열람은 문서 번호와 시각만 남는다 (북극성 지표).
    let views: i64 = s.db().query_row("SELECT COUNT(*) FROM views WHERE doc_id = ?1", [&id], |r| r.get(0)).unwrap();
    assert_eq!(views, 2);
}

// ---------- R1-3·4 목록, 검색, 프로젝트, 쪽 나누기 ----------

#[tokio::test]
async fn r1_3_목록은_최신순이고_제목_올린사람_본문으로_찾는다() {
    let s = Server::start().await;
    let (owner, other) = (s.login(OWNER), s.login(OTHER));
    s.upload(&owner, "월간 보고", "A", "<p>분기 매출 요약</p><script>숨은글자</script>").await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    s.upload(&other, "설계 메모", "B", "<p>아키텍처</p>").await;

    let r = s.get("/", Some(&owner)).await;
    assert_eq!(listed_titles(&r.body), ["설계 메모", "월간 보고"], "최신순");
    for col in ["A", "B", "김올림", "이다른"] {
        assert!(r.body.contains(&format!("<td>{col}</td>")), "목록에 {col}");
    }

    let find = |q: &str| format!("/?q={}", enc(q));
    assert_eq!(listed_titles(&s.get(&find("보고"), Some(&owner)).await.body), ["월간 보고"]);
    assert_eq!(listed_titles(&s.get(&find("이다른"), Some(&owner)).await.body), ["설계 메모"]);
    assert_eq!(listed_titles(&s.get(&find("%"), Some(&owner)).await.body), Vec::<String>::new(), "% 는 글자로 찾는다");

    // 본문 검색: 보이는 글자만, 걸리면 주변 글자를 발췌로 보여 준다.
    let r = s.get(&find("매출"), Some(&owner)).await;
    assert_eq!(listed_titles(&r.body), ["월간 보고"]);
    assert!(r.body.contains("…분기 매출 요약…"), "발췌");
    assert!(listed_titles(&s.get(&find("숨은글자"), Some(&owner)).await.body).is_empty(), "script 안은 찾지 않는다");
    assert!(!s.get(&find("보고"), Some(&owner)).await.body.contains("<small"), "제목으로만 걸리면 발췌 없음");

    let none = s.get(&find("없는검색어"), Some(&owner)).await;
    assert!(none.body.contains("문서가 없습니다"));
}

#[tokio::test]
async fn r1_4_프로젝트를_고르면_그_프로젝트만_보인다() {
    let s = Server::start().await;
    let me = s.login(OWNER);
    s.upload(&me, "가", "A", "<p>1</p>").await;
    s.upload(&me, "나", "B", "<p>2</p>").await;
    s.upload(&me, "다", "", "<p>3</p>").await;

    let r = s.get("/?project=A", Some(&me)).await;
    assert_eq!(listed_titles(&r.body), ["가"]);
    assert!(r.body.contains("<option selected>A</option>") && r.body.contains("<option>B</option>"));
    assert!(s.get("/", Some(&me)).await.body.contains("<td>미분류</td>"));

    // 올리기 화면은 기존 프로젝트를 제안한다.
    let up = s.get("/upload", Some(&me)).await.body;
    assert!(up.contains("<option value=\"A\">") && up.contains("<option value=\"B\">"));
}

#[tokio::test]
async fn 목록은_50개씩_쪽을_나누고_검색어를_유지한다() {
    let s = Server::start().await;
    let me = s.login(OWNER);
    for i in 0..51 {
        s.upload(&me, &format!("문서{i:02}"), "P", "<p>x</p>").await;
    }
    let first = s.get("/", Some(&me)).await;
    assert_eq!(listed_titles(&first.body).len(), 50);
    assert!(first.body.contains("<button name=\"page\" value=\"2\">다음</button>"));
    assert!(!first.body.contains(">이전<"));

    let second = s.get("/?q=%EB%AC%B8%EC%84%9C&project=P&page=2", Some(&me)).await;
    assert_eq!(listed_titles(&second.body).len(), 1);
    assert!(second.body.contains("<button name=\"page\" value=\"1\">이전</button>"));
    assert!(second.body.contains("name=\"q\" value=\"문서\"") && second.body.contains("name=\"project\" value=\"P\""), "쪽을 옮겨도 검색어 유지");

    assert_eq!(listed_titles(&s.get("/?page=-5", Some(&me)).await.body).len(), 50, "이상한 쪽 번호는 1쪽");
}

// ---------- R1-5 삭제(숨김), 관리자 살리기 ----------

#[tokio::test]
async fn r1_5_삭제는_올린_사람과_관리자만_하고_화면에서만_숨긴다() {
    let s = Server::start().await;
    let (owner, other, admin) = (s.login(OWNER), s.login(OTHER), s.login(ADMIN));
    let a = s.upload(&owner, "올린이 문서", "", "<p>a</p>").await;
    let b = s.upload(&other, "남의 문서", "", "<p>b</p>").await;

    // 삭제 단추는 올린 사람과 관리자에게만.
    let del = |id: &str| format!("action=\"/d/{id}/delete\"");
    assert!(s.get("/", Some(&owner)).await.body.contains(&del(&a)));
    assert!(!s.get("/", Some(&other)).await.body.contains(&del(&a)));
    assert!(s.get("/", Some(&admin)).await.body.contains(&del(&a)));

    // 다른 직원이 요청을 직접 보내도 거부.
    assert_eq!(s.post(&format!("/d/{a}/delete"), Some(&other), &[]).await.status, StatusCode::FORBIDDEN);
    assert_eq!(s.get(&format!("/d/{a}"), Some(&other)).await.status, StatusCode::OK);

    // 올린 사람 삭제, 관리자의 남의 문서 삭제.
    assert_eq!(s.post(&format!("/d/{a}/delete"), Some(&owner), &[]).await.status, StatusCode::SEE_OTHER);
    assert_eq!(s.post(&format!("/d/{b}/delete"), Some(&admin), &[]).await.status, StatusCode::SEE_OTHER);
    for id in [&a, &b] {
        let r = s.get(&format!("/d/{id}"), Some(&owner)).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
        assert!(r.body.contains("없는 문서"));
    }
    assert!(listed_titles(&s.get("/", Some(&owner)).await.body).is_empty());
    assert!(listed_titles(&s.get(&format!("/?q={}", enc("문서")), Some(&owner)).await.body).is_empty());
    // 기록과 파일은 남는다.
    assert_eq!(s.doc_count().await, 2);
    assert!(s.dir.join("files").join(format!("{a}.html")).exists());

    // 관리자 링크·화면: 일반 직원에게는 없다.
    assert!(!s.get("/", Some(&owner)).await.body.contains("/admin/hidden"));
    assert!(s.get("/", Some(&admin)).await.body.contains("/admin/hidden"));
    assert_eq!(s.get("/admin/hidden", Some(&owner)).await.status, StatusCode::FORBIDDEN);
    assert_eq!(s.post(&format!("/d/{a}/unhide"), Some(&owner), &[]).await.status, StatusCode::FORBIDDEN);

    // 관리자가 살리면 목록과 링크가 돌아온다 (D4-1).
    let hidden = s.get("/admin/hidden", Some(&admin)).await.body;
    assert!(hidden.contains("올린이 문서") && hidden.contains("남의 문서"));
    assert_eq!(s.post(&format!("/d/{a}/unhide"), Some(&admin), &[]).await.location(), "/admin/hidden");
    assert_eq!(s.get(&format!("/d/{a}"), Some(&owner)).await.status, StatusCode::OK);
    assert_eq!(listed_titles(&s.get("/", Some(&owner)).await.body), ["올린이 문서"]);
    assert_eq!(s.post(&format!("/d/{a}/unhide"), Some(&admin), &[]).await.status, StatusCode::NOT_FOUND, "숨기지 않은 문서");
}

// ---------- R2 새 버전 교체, 이력, 되돌리기 ----------

#[tokio::test]
async fn r2_새_버전_교체_이력_되돌리기() {
    let s = Server::start().await;
    let (owner, other) = (s.login(OWNER), s.login(OTHER));
    let id = s.upload(&owner, "버전 문서", "", "<p>v1 첫내용</p>").await;
    let replace = |cookie: &str, name: &str, html: &str| {
        let (cookie, name, html, id) = (cookie.to_owned(), name.to_owned(), html.to_owned(), id.clone());
        let s = &s;
        async move { s.multipart(&format!("/d/{id}/replace"), Some(&cookie), None, &[], Some((&name, html.as_bytes()))).await }
    };

    assert_eq!(replace(&other, "v.html", "<p>남이</p>").await.status, StatusCode::FORBIDDEN);
    assert_eq!(replace(&owner, "v.txt", "<p>x</p>").await.status, StatusCode::BAD_REQUEST);
    let r = replace(&owner, "v.html", "<p>v2 새내용</p>").await;
    assert_eq!(r.location(), format!("/d/{id}/history"));

    assert_eq!(s.get(&format!("/d/{id}"), Some(&other)).await.body, "<p>v2 새내용</p>", "링크는 그대로, 내용은 새 버전");
    assert_eq!(s.get(&format!("/d/{id}/v/1"), Some(&other)).await.body, "<p>v1 첫내용</p>");
    assert_eq!(s.get(&format!("/d/{id}/v/9"), Some(&other)).await.status, StatusCode::NOT_FOUND);
    let hist = s.get(&format!("/d/{id}/history"), Some(&other)).await.body;
    assert!(hist.contains(&format!("/d/{id}/v/1")) && hist.contains(&format!("/d/{id}/v/2")));

    // 본문 검색은 현재 버전만.
    assert!(listed_titles(&s.get(&format!("/?q={}", enc("첫내용")), Some(&owner)).await.body).is_empty());
    assert_eq!(listed_titles(&s.get(&format!("/?q={}", enc("새내용")), Some(&owner)).await.body), ["버전 문서"]);

    // 되돌리기: 올린 사람만, 옛 내용이 새 버전(3)으로 쌓인다.
    assert_eq!(s.post(&format!("/d/{id}/restore/1"), Some(&other), &[]).await.status, StatusCode::FORBIDDEN);
    assert_eq!(s.post(&format!("/d/{id}/restore/1"), Some(&owner), &[]).await.status, StatusCode::SEE_OTHER);
    assert_eq!(s.get(&format!("/d/{id}"), Some(&owner)).await.body, "<p>v1 첫내용</p>");
    assert!(s.get(&format!("/d/{id}/history"), Some(&owner)).await.body.contains(&format!("/d/{id}/v/3")));
    assert_eq!(s.post(&format!("/d/{id}/restore/9"), Some(&owner), &[]).await.status, StatusCode::NOT_FOUND);
}

// ---------- R2 개인 토큰과 API 업로드, 관리자 토큰 폐기 ----------

async fn issue_token(s: &Server, cookie: &str) -> String {
    let r = s.post("/tokens", Some(cookie), &[]).await;
    assert_eq!(r.status, StatusCode::OK);
    let start = r.body.find("hh_").expect("발급 화면에 토큰이 있어야 한다");
    r.body[start..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect()
}

#[tokio::test]
async fn r2_토큰으로_올리기() {
    let s = Server::start().await;
    let (owner, other) = (s.login(OWNER), s.login(OTHER));
    let html: &[u8] = b"<p>api</p>";
    let api = |token: Option<String>, title: &'static str, name: &'static str| {
        let s = &s;
        async move { s.multipart("/api/upload", None, token.as_deref(), &[("title", title), ("project", "API")], Some((name, html))).await }
    };

    let t1 = issue_token(&s, &owner).await;
    let r = api(Some(t1.clone()), "토큰 문서", "a.html").await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
    let id = v["id"].as_str().unwrap();
    assert_eq!(v["url"], format!("{}/d/{id}", s.base));
    assert!(s.get("/", Some(&other)).await.body.contains("<td>김올림</td>"), "토큰 주인 이름으로 기록");

    assert_eq!(api(None, "x", "a.html").await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(api(Some("hh_wrong".into()), "x", "a.html").await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(api(Some(t1.clone()), "", "a.html").await.status, StatusCode::BAD_REQUEST);
    assert_eq!(api(Some(t1.clone()), "x", "a.txt").await.status, StatusCode::BAD_REQUEST);

    // 토큰은 올리기 전용이다. 쿠키 없이 토큰만으로는 목록에 못 들어간다.
    let r = s.send(s.http.get(format!("{}/", s.base)).header(header::AUTHORIZATION, format!("Bearer {t1}")), None).await;
    assert!(r.location().starts_with("/login"));

    // 서버에는 해시만 저장한다.
    let stored: String = s.db().query_row("SELECT hash FROM api_tokens LIMIT 1", [], |r| r.get(0)).unwrap();
    assert!(!stored.contains(&t1) && stored.len() == 64);

    // 재발급하면 이전 것은 폐기, 직접 폐기하면 401.
    let t2 = issue_token(&s, &owner).await;
    assert_eq!(api(Some(t1).clone(), "x", "a.html").await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(api(Some(t2.clone()), "x", "a.html").await.status, StatusCode::CREATED);
    assert!(s.get("/tokens", Some(&owner)).await.body.contains("발급된 토큰이 있습니다"));
    assert_eq!(s.post("/tokens/revoke", Some(&owner), &[]).await.status, StatusCode::SEE_OTHER);
    assert_eq!(api(Some(t2), "x", "a.html").await.status, StatusCode::UNAUTHORIZED);

    // 비활성화된 직원의 토큰은 쓸 수 없다.
    let t3 = issue_token(&s, &other).await;
    s.db().execute("UPDATE users SET active = 0 WHERE email = ?1", [OTHER]).unwrap();
    assert_eq!(api(Some(t3), "x", "a.html").await.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn 관리자는_직원_토큰을_보고_폐기한다() {
    let s = Server::start().await;
    let (owner, admin) = (s.login(OWNER), s.login(ADMIN));
    let token = issue_token(&s, &owner).await;

    assert_eq!(s.get("/admin/tokens", Some(&owner)).await.status, StatusCode::FORBIDDEN);
    assert_eq!(s.post("/admin/tokens/revoke", Some(&owner), &[("email", OWNER)]).await.status, StatusCode::FORBIDDEN);
    assert!(s.get("/", Some(&admin)).await.body.contains("/admin/tokens"));

    let page = s.get("/admin/tokens", Some(&admin)).await.body;
    assert!(page.contains(OWNER) && !page.contains(&token), "토큰 값은 보이지 않는다");
    assert_eq!(s.post("/admin/tokens/revoke", Some(&admin), &[("email", OWNER)]).await.location(), "/admin/tokens");
    assert!(s.get("/admin/tokens", Some(&admin)).await.body.contains("발급된 토큰이 없습니다"));
    let r = s.multipart("/api/upload", None, Some(&token), &[("title", "x")], Some(("a.html", b"<p>x</p>"))).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

// ---------- R3 댓글 ----------

#[tokio::test]
async fn r3_댓글_쓰기와_지우기_규칙() {
    let s = Server::start().await;
    let (owner, other, admin) = (s.login(OWNER), s.login(OTHER), s.login(ADMIN));
    let id = s.upload(&owner, "댓글 문서", "", "<p>x</p>").await;
    let path = format!("/d/{id}/comments");

    assert_eq!(s.post(&path, Some(&other), &[("body", "<script>alert(1)</script>\n둘째 줄")]).await.status, StatusCode::SEE_OTHER);
    assert_eq!(s.post(&path, Some(&owner), &[("body", "올린이 댓글")]).await.status, StatusCode::SEE_OTHER);
    let page = s.get(&path, Some(&owner)).await.body;
    assert!(page.contains("&lt;script&gt;alert(1)&lt;/script&gt;") && !page.contains("<script>alert"), "이스케이프");
    assert!(page.find("&lt;script").unwrap() < page.find("올린이 댓글").unwrap(), "오래된 것부터");

    assert_eq!(s.post(&path, Some(&owner), &[("body", "   ")]).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(s.post(&path, Some(&owner), &[("body", &"가".repeat(2001))]).await.status, StatusCode::BAD_REQUEST);

    let ids: Vec<String> = page
        .split(&format!("action=\"/d/{id}/comments/"))
        .skip(1)
        .map(|s| s.split_once('/').unwrap().0.to_owned())
        .collect();
    // 올린이 화면에는 본인 댓글 삭제 단추만 있다.
    assert_eq!(ids.len(), 1);
    let (others_comment, owners_comment): (i64, i64) = s
        .db()
        .query_row(
            "SELECT (SELECT id FROM comments WHERE by_email = ?1), (SELECT id FROM comments WHERE by_email = ?2)",
            [OTHER, OWNER],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(ids[0], owners_comment.to_string());

    // 문서를 올린 사람이라도 남의 댓글은 못 지운다. 쓴 사람과 관리자만.
    let del = |cid: i64| format!("/d/{id}/comments/{cid}/delete");
    assert_eq!(s.post(&del(others_comment), Some(&owner), &[]).await.status, StatusCode::FORBIDDEN);
    assert_eq!(s.post(&del(owners_comment), Some(&owner), &[]).await.status, StatusCode::SEE_OTHER);
    assert_eq!(s.post(&del(others_comment), Some(&admin), &[]).await.status, StatusCode::SEE_OTHER);
    let page = s.get(&path, Some(&other)).await.body;
    assert!(!page.contains("올린이 댓글") && !page.contains("alert(1)"));
    let kept: i64 = s.db().query_row("SELECT COUNT(*) FROM comments WHERE deleted_at IS NOT NULL", [], |r| r.get(0)).unwrap();
    assert_eq!(kept, 2, "지운 댓글은 숨기고 기록은 남긴다");

    // 숨긴 문서의 댓글 화면은 없다.
    s.post(&format!("/d/{id}/delete"), Some(&owner), &[]).await;
    assert_eq!(s.get(&path, Some(&owner)).await.status, StatusCode::NOT_FOUND);
}
