use crate::{
    AppState,
    auth::{current_user, current_user_next, escape, page, shell},
};
use anyhow::Result;
use axum::{
    extract::{Multipart, Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use axum_extra::extract::cookie::PrivateCookieJar;
use openidconnect::CsrfToken;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use time::{OffsetDateTime, UtcOffset};

pub const MAX_BYTES: usize = 10 * 1024 * 1024;
const PAGE_SIZE: i64 = 50;

pub struct Comment {
    pub id: i64,
    pub by_email: String,
    pub by_name: String,
    pub body: String,
    pub created_at: i64,
}

/// 대시보드의 최근 댓글 한 줄.
pub struct RecentComment {
    pub doc_id: String,
    pub doc_title: String,
    pub by_name: String,
    pub body: String,
    pub created_at: i64,
}

/// 보이는 문서 수, 지난 7일 열람된 문서 수(북극성 지표), 이 사람이 올린 문서 수, 숨긴 문서 수, 보이는 댓글 수.
#[derive(Default)]
pub struct Summary {
    pub total: i64,
    pub viewed: i64,
    pub mine: i64,
    pub hidden: i64,
    pub comments: i64,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Sort {
    Recent,
    Views,
    Comments,
    Title,
}

impl Sort {
    pub fn parse(s: &str) -> Self {
        match s {
            "views" => Sort::Views,
            "comments" => Sort::Comments,
            "title" => Sort::Title,
            _ => Sort::Recent,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Sort::Recent => "recent",
            Sort::Views => "views",
            Sort::Comments => "comments",
            Sort::Title => "title",
        }
    }
}

/// 목록 조건. `project` 가 None 이면 모든 프로젝트, Some("") 이면 미분류만. `uploader` 가 비면 모든 사람.
pub struct Filter<'a> {
    pub q: &'a str,
    pub project: Option<&'a str>,
    pub uploader: &'a str,
    pub sort: Sort,
}

/// 대시보드의 "지난 7일" 기준.
const WEEK: i64 = 7 * 86400;

pub struct Version {
    pub n: i64,
    pub by_name: String,
    pub created_at: i64,
}

pub struct SurveyResponse {
    pub email: String,
    pub name: String,
    pub answers: String,
    pub updated_at: i64,
}

pub struct Doc {
    pub id: String,
    pub title: String,
    pub project: String,
    pub uploader_email: String,
    pub uploader_name: String,
    pub created_at: i64,
}

/// 문서 메타데이터(SQLite)와 파일(디스크)을 함께 다룬다.
/// 파일을 다루는 곳: put_file/read_file, replace 의 이름 바꾸기, purge 의 지우기와 purged.txt, ping 의 폴더 확인.
/// R2 로 옮길 때는 이 다섯 곳과 scripts/backup.sh 를 함께 바꾼다.
#[derive(Clone)]
pub struct Docs {
    db: Arc<Mutex<Connection>>,
    dir: PathBuf,
    /// 한도를 세는 것과 저장을 한 번에 한 요청씩 한다. 읽기는 막지 않는다.
    uploads: Arc<Mutex<()>>,
}

impl Docs {
    pub fn open(db_path: &str, dir: &str) -> Result<Self> {
        let conn = Connection::open(db_path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        // WAL: 쓰는 동안에도 읽기가 막히지 않는다. DB 파일에 남으므로 한 번 켜면 계속 간다.
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS documents (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                project TEXT NOT NULL,
                uploader_email TEXT NOT NULL,
                uploader_name TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                deleted_at INTEGER
            );
            -- 문서마다 버전이 쌓인다. 현재 버전은 번호가 가장 큰 것이다. 파일은 한 번 쓰면 바꾸지 않는다.
            CREATE TABLE IF NOT EXISTS versions (
                doc_id TEXT NOT NULL,
                n INTEGER NOT NULL,
                file TEXT NOT NULL,
                by_email TEXT NOT NULL,
                by_name TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                PRIMARY KEY (doc_id, n)
            );
            -- 버전 기능이 생기기 전에 올린 문서는 1번 버전으로 채운다.
            INSERT INTO versions (doc_id, n, file, by_email, by_name, created_at)
                SELECT id, 1, id || '.html', uploader_email, uploader_name, created_at FROM documents
                WHERE id NOT IN (SELECT doc_id FROM versions);
            -- 본문 검색용으로 문서마다 현재 버전의 보이는 글자만 둔다 (정책서 F2, D4-3).
            CREATE TABLE IF NOT EXISTS bodies (doc_id TEXT PRIMARY KEY, text TEXT NOT NULL);
            -- 댓글은 지우면 화면에서만 숨기고 기록은 남긴다.
            CREATE TABLE IF NOT EXISTS comments (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                doc_id TEXT NOT NULL,
                by_email TEXT NOT NULL,
                by_name TEXT NOT NULL,
                body TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                deleted_at INTEGER
            );
            -- 북극성 지표(주간 열람된 문서 수)용. 누가 봤는지는 남기지 않는다.
            CREATE TABLE IF NOT EXISTS views (doc_id TEXT NOT NULL, at INTEGER NOT NULL);
            -- 열람·댓글은 화면마다 센다. 쌓여도 느려지지 않게 색인을 둔다.
            CREATE INDEX IF NOT EXISTS views_doc_at ON views(doc_id, at);
            CREATE INDEX IF NOT EXISTS views_at ON views(at);
            CREATE INDEX IF NOT EXISTS comments_doc ON comments(doc_id);
            -- 설문 응답. 한 사람이 한 문서에 하나만 남기고, 다시 내면 덮어쓴다. answers 는 JSON 객체다.
            CREATE TABLE IF NOT EXISTS responses (
                doc_id TEXT NOT NULL,
                email TEXT NOT NULL,
                name TEXT NOT NULL,
                answers TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY (doc_id, email)
            );",
        )?;
        std::fs::create_dir_all(dir)?;
        let docs = Self {
            db: Arc::new(Mutex::new(conn)),
            dir: dir.into(),
            uploads: Default::default(),
        };
        docs.index_missing()?;
        Ok(docs)
    }

    /// 본문 색인이 없는 문서(색인 기능 이전에 올린 문서)를 현재 버전 파일로 채운다.
    fn index_missing(&self) -> Result<()> {
        let ids: Vec<String> = {
            let conn = self.db.lock().unwrap();
            let mut stmt = conn.prepare("SELECT id FROM documents WHERE id NOT IN (SELECT doc_id FROM bodies)")?;
            stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for id in ids {
            // 파일을 못 읽는 문서는 건너뛴다. 제목·올린 사람 검색에는 영향이 없다.
            if let Ok(bytes) = self.read(&id) {
                self.index_body(&id, &bytes)?;
            }
        }
        Ok(())
    }

    fn index_body(&self, id: &str, bytes: &[u8]) -> Result<()> {
        let text = visible_text(&String::from_utf8_lossy(bytes));
        self.db.lock().unwrap().execute(
            "INSERT INTO bodies (doc_id, text) VALUES (?1, ?2)
             ON CONFLICT(doc_id) DO UPDATE SET text = excluded.text",
            params![id, text],
        )?;
        Ok(())
    }

    /// 저장은 이미 끝났으므로 검색 색인이 실패해도 올리기는 성공으로 둔다 (실패로 답하면 다시 올려 중복이 생긴다).
    fn index_quietly(&self, id: &str, bytes: &[u8]) {
        if let Err(e) = self.index_body(id, bytes) {
            log!("본문 색인 실패 (검색에 옛 내용이 걸릴 수 있다): {id}: {e}");
        }
    }

    fn put_file(&self, name: &str, bytes: &[u8]) -> Result<()> {
        Ok(std::fs::write(self.dir.join(name), bytes)?)
    }

    fn read_file(&self, name: &str) -> Result<Vec<u8>> {
        Ok(std::fs::read(self.dir.join(name))?)
    }

    /// 새 문서를 저장하고 추측할 수 없는 번호를 돌려준다. 파일을 먼저 쓰고 기록을 남긴다.
    pub fn insert(&self, title: &str, project: &str, by: &crate::users::User, bytes: &[u8]) -> Result<String> {
        let id = CsrfToken::new_random().secret().clone();
        let file = format!("{id}.html");
        self.put_file(&file, bytes)?;
        let now = now_unix();
        let mut conn = self.db.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO documents (id, title, project, uploader_email, uploader_name, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, title, project, by.email, by.name, now],
        )?;
        tx.execute(
            "INSERT INTO versions (doc_id, n, file, by_email, by_name, created_at) VALUES (?1, 1, ?2, ?3, ?4, ?5)",
            params![id, file, by.email, by.name, now],
        )?;
        tx.commit()?;
        drop(conn);
        self.index_quietly(&id, bytes);
        Ok(id)
    }

    fn add_version(&self, id: &str, by: &crate::users::User, file_for: impl FnOnce(i64) -> Result<String>) -> Result<i64> {
        let conn = self.db.lock().unwrap();
        let n: i64 = conn.query_row("SELECT COALESCE(MAX(n), 0) + 1 FROM versions WHERE doc_id = ?1", [id], |r| r.get(0))?;
        let file = file_for(n)?;
        conn.execute(
            "INSERT INTO versions (doc_id, n, file, by_email, by_name, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, n, file, by.email, by.name, now_unix()],
        )?;
        Ok(n)
    }

    /// 같은 문서에 새 버전을 올린다. 이전 버전은 이력으로 남는다 (정책서 V1·V2).
    pub fn replace(&self, id: &str, by: &crate::users::User, bytes: &[u8]) -> Result<i64> {
        // 큰 파일은 DB 잠금 밖에서 먼저 쓰고, 잠금 안에서는 번호를 매겨 이름만 바꾼다.
        let tmp = format!("{id}-tmp-{}.html", CsrfToken::new_random().secret());
        self.put_file(&tmp, bytes)?;
        let added = self.add_version(id, by, |n| {
            let file = format!("{id}-{n}.html");
            std::fs::rename(self.dir.join(&tmp), self.dir.join(&file))?;
            Ok(file)
        });
        if added.is_err() {
            let _ = std::fs::remove_file(self.dir.join(&tmp));
        }
        let n = added?;
        self.index_quietly(id, bytes);
        Ok(n)
    }

    /// 이전 버전의 내용을 새 버전으로 한 번 더 저장한다. 사이의 버전은 지우지 않는다 (V8). 없는 버전이면 None.
    pub fn restore(&self, id: &str, n: i64, by: &crate::users::User) -> Result<Option<i64>> {
        let Some(file) = self.version_file(id, n)? else { return Ok(None) };
        // 파일이 없으면 버전을 만들기 전에 실패해야 현재 버전이 깨지지 않는다.
        let bytes = self.read_file(&file)?;
        // 파일은 바뀌지 않으므로 같은 파일을 새 버전이 가리키게 한다.
        let n = self.add_version(id, by, |_| Ok(file.clone()))?;
        self.index_quietly(id, &bytes);
        Ok(Some(n))
    }

    fn version_file(&self, id: &str, n: i64) -> Result<Option<String>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row("SELECT file FROM versions WHERE doc_id = ?1 AND n = ?2", params![id, n], |r| r.get(0))
            .optional()?)
    }

    /// 최신 버전이 먼저 온다.
    pub fn versions(&self, id: &str) -> Result<Vec<Version>> {
        let conn = self.db.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT n, by_name, created_at FROM versions WHERE doc_id = ?1 ORDER BY n DESC",
        )?;
        let rows = stmt.query_map([id], |r| Ok(Version { n: r.get(0)?, by_name: r.get(1)?, created_at: r.get(2)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 현재(가장 큰 번호) 버전의 내용.
    pub fn read(&self, id: &str) -> Result<Vec<u8>> {
        let file: String = self.db.lock().unwrap().query_row(
            "SELECT file FROM versions WHERE doc_id = ?1 ORDER BY n DESC LIMIT 1",
            [id],
            |r| r.get(0),
        )?;
        self.read_file(&file)
    }

    /// 특정 버전의 내용. 없는 버전이면 None.
    pub fn read_version(&self, id: &str, n: i64) -> Result<Option<Vec<u8>>> {
        match self.version_file(id, n)? {
            Some(file) => Ok(Some(self.read_file(&file)?)),
            None => Ok(None),
        }
    }

    pub fn add_comment(&self, id: &str, by: &crate::users::User, body: &str) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO comments (doc_id, by_email, by_name, body, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, by.email, by.name, body, now_unix()],
        )?;
        Ok(())
    }

    /// 제목·프로젝트를 고친다. 링크와 버전은 그대로다.
    pub fn set_meta(&self, id: &str, title: &str, project: &str) -> Result<()> {
        self.db.lock().unwrap().execute("UPDATE documents SET title = ?2, project = ?3 WHERE id = ?1", params![id, title, project])?;
        Ok(())
    }

    /// 최근 24시간 동안 그 사람이 올린 버전 수 (새 문서와 새 버전 모두).
    pub fn uploads_today(&self, email: &str) -> Result<i64> {
        Ok(self.db.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM versions WHERE by_email = ?1 AND created_at > ?2",
            params![email, now_unix() - 24 * 3600],
            |r| r.get(0),
        )?)
    }

    /// 오늘 한도가 남았을 때만 `save` 를 실행한다. 넘었으면 None.
    /// 셀 수 없으면 막지 않는다 (올리기를 멈추는 것보다 낫다).
    pub fn with_quota<T>(&self, email: &str, save: impl FnOnce() -> Result<T>) -> Result<Option<T>> {
        // 저장 중에 패닉이 나도 다음 올리기는 되게 한다 (지키는 값이 없는 잠금이다).
        let _one = self.uploads.lock().unwrap_or_else(|e| e.into_inner());
        if self.uploads_today(email).is_ok_and(|n| n >= DAILY_UPLOADS) {
            return Ok(None);
        }
        save().map(Some)
    }

    /// 상태 확인: DB 를 읽을 수 있고 문서 폴더가 있는가.
    pub fn ping(&self) -> Result<()> {
        // SELECT 1 은 DB 파일을 읽지 않는다. 실제 표를 읽는다.
        self.db.lock().unwrap().query_row("SELECT COUNT(*) FROM documents", [], |_| Ok(()))?;
        anyhow::ensure!(self.dir.is_dir(), "문서 폴더가 없다: {}", self.dir.display());
        Ok(())
    }

    pub fn save_response(&self, id: &str, user: &crate::users::User, answers: &str) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO responses (doc_id, email, name, answers, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT (doc_id, email) DO UPDATE SET name = excluded.name, answers = excluded.answers, updated_at = excluded.updated_at",
            params![id, user.email, user.name, answers, now_unix()],
        )?;
        Ok(())
    }

    /// 설문 응답을 먼저 낸 사람부터 돌려준다.
    pub fn responses(&self, id: &str) -> Result<Vec<SurveyResponse>> {
        let conn = self.db.lock().unwrap();
        let mut stmt = conn.prepare("SELECT email, name, answers, updated_at FROM responses WHERE doc_id = ?1 ORDER BY created_at, rowid")?;
        let rows = stmt.query_map([id], |r| {
            Ok(SurveyResponse { email: r.get(0)?, name: r.get(1)?, answers: r.get(2)?, updated_at: r.get(3)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn response_count(&self, id: &str) -> Result<i64> {
        Ok(self.db.lock().unwrap().query_row("SELECT COUNT(*) FROM responses WHERE doc_id = ?1", [id], |r| r.get(0))?)
    }

    /// 지우지 않은 댓글을 오래된 것부터 돌려준다.
    pub fn comments(&self, id: &str) -> Result<Vec<Comment>> {
        let conn = self.db.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, by_email, by_name, body, created_at FROM comments
             WHERE doc_id = ?1 AND deleted_at IS NULL ORDER BY id",
        )?;
        let rows = stmt.query_map([id], |r| {
            Ok(Comment { id: r.get(0)?, by_email: r.get(1)?, by_name: r.get(2)?, body: r.get(3)?, created_at: r.get(4)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 그 문서의 지우지 않은 댓글 하나.
    pub fn comment(&self, id: &str, comment_id: i64) -> Result<Option<Comment>> {
        Ok(self.comments(id)?.into_iter().find(|c| c.id == comment_id))
    }

    pub fn hide_comment(&self, comment_id: i64) -> Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE comments SET deleted_at = ?2 WHERE id = ?1 AND deleted_at IS NULL",
            params![comment_id, now_unix()],
        )?;
        Ok(())
    }

    /// 삭제(숨김)되지 않은 문서만 돌려준다.
    pub fn get(&self, id: &str) -> Result<Option<Doc>> {
        let conn = self.db.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT id, title, project, uploader_email, uploader_name, created_at
                 FROM documents WHERE id = ?1 AND deleted_at IS NULL",
                [id],
                row_to_doc,
            )
            .optional()?)
    }

    /// 제목, 올린 사람, 본문(현재 버전의 보이는 글자)에 검색어가 든 문서를 최신순으로 돌려준다. `project` 가 비어 있지 않으면 그 프로젝트만.
    /// LIKE 대신 instr 를 써서 검색어의 % 나 _ 가 특수문자로 해석되지 않게 한다.
    /// shortcut: 본문을 전부 훑는다. 문서가 수천 건을 넘어 느려지면 FTS5(trigram)로 바꾼다.
    /// `offset` 부터 한 쪽(PAGE_SIZE)을 돌려주되, 다음 쪽이 있는지 알 수 있게 한 개 더 가져온다.
    pub fn list(&self, f: &Filter, offset: i64) -> Result<Vec<Doc>> {
        // 정렬 기준은 정해진 목록에서만 고르므로 SQL 에 직접 넣어도 안전하다.
        let order = match f.sort {
            Sort::Recent => String::new(),
            Sort::Views => format!(
                "(SELECT COUNT(*) FROM views v WHERE v.doc_id = documents.id AND v.at >= {}) DESC,",
                now_unix() - WEEK
            ),
            Sort::Comments => {
                "(SELECT COUNT(*) FROM comments c WHERE c.doc_id = documents.id AND c.deleted_at IS NULL) DESC,".into()
            }
            Sort::Title => "title COLLATE NOCASE ASC,".into(),
        };
        let conn = self.db.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT id, title, project, uploader_email, uploader_name, created_at
             FROM documents
             WHERE deleted_at IS NULL
               AND (?1 = '' OR instr(lower(title), lower(?1)) > 0 OR instr(lower(uploader_name), lower(?1)) > 0
                    OR EXISTS (SELECT 1 FROM bodies b WHERE b.doc_id = documents.id AND instr(lower(b.text), lower(?1)) > 0))
               AND (?2 IS NULL OR project = ?2)
               AND (?3 = '' OR uploader_email = ?3)
             ORDER BY {order} created_at DESC, rowid DESC
             LIMIT ?4 OFFSET ?5"
        ))?;
        let rows = stmt.query_map(params![f.q, f.project, f.uploader, PAGE_SIZE + 1, offset], row_to_doc)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 프로젝트별 보이는 문서 수. 미분류("")는 맨 뒤.
    pub fn project_counts(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.db.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT project, COUNT(*) FROM documents WHERE deleted_at IS NULL
             GROUP BY project ORDER BY project = '', project",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 오늘을 포함한 최근 7일(한국 날짜)의 하루별 열람 수. (그날 0시의 유닉스 시각, 열람 수), 오래된 날부터.
    pub fn daily_views(&self) -> Result<Vec<(i64, i64)>> {
        const KST: i64 = 9 * 3600;
        let today = (now_unix() + KST).div_euclid(86400);
        let start = (today - 6) * 86400 - KST;
        let conn = self.db.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT (v.at + 32400) / 86400, COUNT(*) FROM views v JOIN documents d ON d.id = v.doc_id
             WHERE d.deleted_at IS NULL AND v.at >= ?1 GROUP BY 1",
        )?;
        let counts: std::collections::HashMap<i64, i64> =
            stmt.query_map([start], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        Ok((today - 6..=today).map(|day| (day * 86400 - KST, counts.get(&day).copied().unwrap_or(0))).collect())
    }

    /// 삭제되지 않은 문서에 쓰인 프로젝트 이름들.
    pub fn projects(&self) -> Result<Vec<String>> {
        let conn = self.db.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT project FROM documents
             WHERE deleted_at IS NULL AND project <> '' ORDER BY project",
        )?;
        Ok(stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    /// 화면에서만 숨긴다. 파일과 기록은 남긴다 (정책서 D4).
    pub fn hide(&self, id: &str) -> Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE documents SET deleted_at = ?2 WHERE id = ?1 AND deleted_at IS NULL",
            params![id, now_unix()],
        )?;
        Ok(())
    }

    /// 숨긴 문서를 최근에 숨긴 것부터 (D4-1, 관리자 화면용).
    pub fn hidden(&self) -> Result<Vec<Doc>> {
        let conn = self.db.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, title, project, uploader_email, uploader_name, created_at
             FROM documents WHERE deleted_at IS NOT NULL ORDER BY deleted_at DESC, rowid DESC",
        )?;
        let rows = stmt.query_map([], row_to_doc)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 숨긴 문서를 되살린다. 숨겨져 있지 않았으면 false.
    pub fn unhide(&self, id: &str) -> Result<bool> {
        let n = self.db.lock().unwrap().execute(
            "UPDATE documents SET deleted_at = NULL WHERE id = ?1 AND deleted_at IS NOT NULL",
            [id],
        )?;
        Ok(n > 0)
    }

    /// 숨긴 문서를 흔적까지 지운다 (정책서 D4 의 예외: 실수로 올린 민감 자료). 숨긴 문서만 되고, 숨기지 않았으면 false.
    /// 지운 파일 이름은 `purged.txt` 에 남겨 다음 백업 때 백업 폴더에서도 지우게 한다.
    pub fn purge(&self, id: &str) -> Result<bool> {
        let mut conn = self.db.lock().unwrap();
        let tx = conn.transaction()?;
        let hidden: Option<i64> = tx
            .query_row("SELECT 1 FROM documents WHERE id = ?1 AND deleted_at IS NOT NULL", [id], |r| r.get(0))
            .optional()?;
        if hidden.is_none() {
            return Ok(false);
        }
        let files: Vec<String> = {
            let mut stmt = tx.prepare("SELECT file FROM versions WHERE doc_id = ?1")?;
            stmt.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for table in ["versions", "bodies", "comments", "responses", "views"] {
            tx.execute(&format!("DELETE FROM {table} WHERE doc_id = ?1"), [id])?;
        }
        tx.execute("DELETE FROM documents WHERE id = ?1", [id])?;
        // 기록을 먼저 지운다(커밋). 목록을 먼저 쓰면 커밋이 실패했을 때 살아 있는 문서의 백업까지 지워진다.
        tx.commit()?;
        drop(conn);
        for f in &files {
            match std::fs::remove_file(self.dir.join(f)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => log!("완전 삭제: 파일을 못 지웠다 (손으로 지워야 한다): {f}: {e}"),
            }
        }
        use std::io::Write;
        let listed = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("..").join("purged.txt"))
            .and_then(|mut list| files.iter().try_for_each(|f| writeln!(list, "{f}")));
        if let Err(e) = listed {
            log!("완전 삭제: 백업 정리 목록을 못 썼다 (백업 폴더에서 손으로 지워야 한다): {files:?}: {e}");
        }
        Ok(true)
    }

    /// 본문(현재 버전)에서 검색어가 처음 나오는 곳 앞 40자부터 120자. 앞뒤가 잘렸으면 … 를 붙인다. 본문에 없으면 None.
    pub fn excerpt(&self, id: &str, q: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT CASE WHEN s > 1 THEN '…' ELSE '' END || substr(text, s, 120)
                        || CASE WHEN s + 120 <= length(text) THEN '…' ELSE '' END
                 FROM (SELECT text, max(instr(lower(text), lower(?2)) - 40, 1) AS s
                       FROM bodies WHERE doc_id = ?1 AND instr(lower(text), lower(?2)) > 0)",
                params![id, q],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// 대시보드와 메뉴에 쓰는 숫자들.
    pub fn summary(&self, email: &str) -> Result<Summary> {
        Ok(self.db.lock().unwrap().query_row(
            "SELECT (SELECT COUNT(*) FROM documents WHERE deleted_at IS NULL),
                    (SELECT COUNT(DISTINCT v.doc_id) FROM views v JOIN documents d ON d.id = v.doc_id
                      WHERE d.deleted_at IS NULL AND v.at >= ?2),
                    (SELECT COUNT(*) FROM documents WHERE deleted_at IS NULL AND uploader_email = ?1),
                    (SELECT COUNT(*) FROM documents WHERE deleted_at IS NOT NULL),
                    (SELECT COUNT(*) FROM comments c JOIN documents d ON d.id = c.doc_id
                      WHERE c.deleted_at IS NULL AND d.deleted_at IS NULL)",
            params![email, now_unix() - WEEK],
            |r| Ok(Summary { total: r.get(0)?, viewed: r.get(1)?, mine: r.get(2)?, hidden: r.get(3)?, comments: r.get(4)? }),
        )?)
    }

    /// 지난 7일 열람이 많은 문서 (번호, 제목, 열람 수).
    pub fn top_viewed(&self, limit: i64) -> Result<Vec<(String, String, i64)>> {
        let conn = self.db.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT d.id, d.title, COUNT(*) AS n FROM views v JOIN documents d ON d.id = v.doc_id
             WHERE d.deleted_at IS NULL AND v.at >= ?1
             GROUP BY d.id ORDER BY n DESC, MAX(v.at) DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![now_unix() - WEEK, limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 최근 댓글. 지운 댓글과 숨긴 문서의 댓글은 뺀다.
    pub fn recent_comments(&self, limit: i64) -> Result<Vec<RecentComment>> {
        let conn = self.db.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT c.doc_id, d.title, c.by_name, c.body, c.created_at FROM comments c JOIN documents d ON d.id = c.doc_id
             WHERE c.deleted_at IS NULL AND d.deleted_at IS NULL ORDER BY c.created_at DESC, c.id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |r| {
            Ok(RecentComment { doc_id: r.get(0)?, doc_title: r.get(1)?, by_name: r.get(2)?, body: r.get(3)?, created_at: r.get(4)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 문서별 (지난 7일 열람 수, 댓글 수). 목록 한 쪽(최대 50개)에만 쓴다.
    pub fn activity(&self, ids: &[String]) -> Result<std::collections::HashMap<String, (i64, i64)>> {
        if ids.is_empty() {
            return Ok(Default::default());
        }
        let marks = vec!["?"; ids.len()].join(",");
        let conn = self.db.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT d.id,
                    (SELECT COUNT(*) FROM views v WHERE v.doc_id = d.id AND v.at >= ?1),
                    (SELECT COUNT(*) FROM comments c WHERE c.doc_id = d.id AND c.deleted_at IS NULL)
             FROM documents d WHERE d.id IN ({marks})"
        ))?;
        let mut args: Vec<rusqlite::types::Value> = vec![(now_unix() - WEEK).into()];
        args.extend(ids.iter().map(|i| i.clone().into()));
        let rows = stmt.query_map(rusqlite::params_from_iter(args), |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn record_view(&self, id: &str) -> Result<()> {
        self.db
            .lock()
            .unwrap()
            .execute("INSERT INTO views (doc_id, at) VALUES (?1, ?2)", params![id, now_unix()])?;
        Ok(())
    }
}

/// HTML 에서 화면에 보이는 글자만 뽑는다 (정책서 F2). 태그, 주석, script·style 안의 내용은 뺀다.
fn visible_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase(); // 길이가 같아서 같은 위치로 자를 수 있다
    let (mut out, mut i) = (String::new(), 0);
    while i < html.len() {
        let rest = &lower[i..];
        let starts_tag = rest
            .strip_prefix('<')
            .and_then(|r| r.chars().next())
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '/' || c == '!');
        if rest.starts_with("<!--") {
            i += rest.find("-->").map_or(rest.len(), |e| e + 3);
        } else if rest.starts_with("<script") || rest.starts_with("<style") {
            let close = if rest.starts_with("<script") { "</script" } else { "</style" };
            i += rest
                .find(close)
                .and_then(|e| rest[e..].find('>').map(|g| e + g + 1))
                .unwrap_or(rest.len());
            out.push(' ');
        } else if starts_tag {
            i += rest.find('>').map_or(rest.len(), |e| e + 1);
            out.push(' ');
        } else {
            let ch = html[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    let out = out
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn row_to_doc(row: &rusqlite::Row) -> rusqlite::Result<Doc> {
    Ok(Doc {
        id: row.get(0)?,
        title: row.get(1)?,
        project: row.get(2)?,
        uploader_email: row.get(3)?,
        uploader_name: row.get(4)?,
        created_at: row.get(5)?,
    })
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

/// 한국 시간으로 "2026-11-25 14:03" 꼴로 보인다.
pub fn show_time(unix: i64) -> String {
    let Ok(t) = OffsetDateTime::from_unix_timestamp(unix) else {
        return String::new();
    };
    let t = t.to_offset(UtcOffset::from_hms(9, 0, 0).unwrap());
    format!("{} {:02}:{:02}", t.date(), t.hour(), t.minute())
}

/// 오늘은 "3시간 전", 어제는 "어제", 그 전은 날짜로 보인다 (한국 시각). 상대 시각만 쓰면 하루만 지나도 구분이 안 된다.
fn relative(unix: i64, now: i64) -> String {
    let kst = UtcOffset::from_hms(9, 0, 0).unwrap();
    let (Ok(t), Ok(n)) = (OffsetDateTime::from_unix_timestamp(unix), OffsetDateTime::from_unix_timestamp(now)) else {
        return String::new();
    };
    let (t, n) = (t.to_offset(kst), n.to_offset(kst));
    let d = (now - unix).max(0);
    if d < 60 {
        "방금".into()
    } else if d < 3600 {
        format!("{}분 전", d / 60)
    } else if t.date() >= n.date() {
        format!("{}시간 전", d / 3600)
    } else if n.date().previous_day() == Some(t.date()) {
        "어제".into()
    } else if t.year() == n.year() {
        format!("{}월 {}일", u8::from(t.month()), t.day())
    } else {
        format!("{}년 {}월 {}일", t.year(), u8::from(t.month()), t.day())
    }
}

/// 화면에 넣는 시각. 정확한 시각은 마우스를 올리면 보인다.
pub fn when(unix: i64) -> String {
    let iso = OffsetDateTime::from_unix_timestamp(unix)
        .ok()
        .and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok())
        .unwrap_or_default();
    format!("<time datetime=\"{iso}\" title=\"{}\">{}</time>", show_time(unix), relative(unix, now_unix()))
}

/// 격리된 문서는 출처가 없어(opaque origin) localStorage·sessionStorage 가 SecurityError 를 던진다.
/// 그러면 저장소를 쓰는 React 앱이 통째로 멈추므로, 막혔을 때만 메모리 저장소로 바꿔 끼운다 (새로고침하면 사라진다).
const STORAGE_SHIM: &str = "(function(){function m(){var d={};return{getItem:function(k){return Object.prototype.hasOwnProperty.call(d,k)?d[k]:null},setItem:function(k,v){d[k]=String(v)},removeItem:function(k){delete d[k]},clear:function(){d={}},key:function(i){return Object.keys(d)[i]||null},get length(){return Object.keys(d).length}}}['localStorage','sessionStorage'].forEach(function(n){try{window[n].getItem('_')}catch(e){try{Object.defineProperty(window,n,{value:m(),configurable:true})}catch(_){}}})})();";

/// 저장소 대체 코드를 문서 앞에 넣는다. `<!doctype ...>` 이 있으면 그 뒤에 넣는다 (앞에 넣으면 호환 모드로 그려진다).
/// 저장된 파일은 바꾸지 않고 내보낼 때만 붙인다. `extra` 는 같은 script 안에 이어 붙일 코드(설문 제출 함수)다.
fn with_storage_shim(bytes: Vec<u8>, extra: &str) -> Vec<u8> {
    let lead = bytes.iter().position(|b| !b" \t\r\n".contains(b) && !matches!(b, 0xEF | 0xBB | 0xBF)).unwrap_or(0);
    let at = match bytes.get(lead..lead + 9) {
        Some(head) if head.eq_ignore_ascii_case(b"<!doctype") => {
            bytes[lead..].iter().position(|&b| b == b'>').map_or(0, |end| lead + end + 1)
        }
        _ => 0,
    };
    let mut out = Vec::with_capacity(bytes.len() + STORAGE_SHIM.len() + extra.len() + 17);
    out.extend_from_slice(&bytes[..at]);
    out.extend_from_slice(format!("<script>{STORAGE_SHIM}{extra}</script>").as_bytes());
    out.extend_from_slice(&bytes[at..]);
    out
}

/// 올린 HTML 을 내보내는 응답. sandbox 로 앱과 분리해 실행해서 문서 안의 JS 가 로그인 세션을 쓰지 못하게 한다 (정책서 S1).
/// allow-same-origin 과 allow-top-navigation 은 주지 않는다. 새 창·알림·폼·다운로드는 출처가 없는 채로 허용해 Claude 결과물이 깨지지 않게 한다.
fn document_response(bytes: Vec<u8>, extra: &str) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (
                header::CONTENT_SECURITY_POLICY,
                "sandbox allow-scripts allow-popups allow-popups-to-escape-sandbox allow-modals allow-forms allow-downloads",
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            // no-transform: Cloudflare 가 분석·봇 탐지 스크립트를 문서에 끼워 넣지 않게 한다 (격리된 문서에서 오류를 낸다).
            (header::CACHE_CONTROL, "private, no-store, no-transform"),
        ],
        with_storage_shim(bytes, extra),
    )
        .into_response()
}

pub(crate) fn not_found() -> Response {
    (StatusCode::NOT_FOUND, page("없는 문서입니다. <a href=\"/\">목록으로</a>")).into_response()
}

pub(crate) fn server_error(what: &str, e: impl std::fmt::Display) -> Response {
    log!("{what} 실패: {e}");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

#[derive(Deserialize, Default)]
pub struct ListQuery {
    q: Option<String>,
    project: Option<String>,
    unfiled: Option<String>,
    mine: Option<String>,
    sort: Option<String>,
    page: Option<i64>,
}

/// 목록 주소. 검색어는 유지하고 쪽은 처음으로 돌린다. `project` 가 Some("") 이면 미분류.
fn list_url(q: &str, project: Option<&str>, mine: bool, sort: Sort) -> String {
    let enc = |s: &str| -> String { openidconnect::url::form_urlencoded::byte_serialize(s.as_bytes()).collect() };
    let mut p = Vec::new();
    if !q.is_empty() {
        p.push(format!("q={}", enc(q)));
    }
    match project {
        Some("") => p.push("unfiled=1".into()),
        Some(name) => p.push(format!("project={}", enc(name))),
        None => {}
    }
    if mine {
        p.push("mine=1".into());
    }
    if sort != Sort::Recent {
        p.push(format!("sort={}", sort.key()));
    }
    if p.is_empty() { "/".into() } else { format!("/?{}", p.join("&")) }
}

/// 비율 막대. 값이 있으면 너무 짧아 안 보이지 않게 최소 폭을 둔다.
fn meter(n: i64, max: i64) -> String {
    let pct = if max > 0 && n > 0 { (n * 100 / max).max(8) } else { 0 };
    format!("<span class=\"meter\" aria-hidden=\"true\"><i style=\"width:{pct}%\"></i></span>")
}

pub async fn index(
    State(app): State<AppState>,
    jar: PrivateCookieJar,
    Query(query): Query<ListQuery>,
) -> Response {
    if let Some(r) = crate::auth::landing(&jar) {
        return r;
    }
    let user = match current_user(&app, &jar) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let q = query.q.unwrap_or_default().trim().to_owned();
    let project_name = query.project.unwrap_or_default();
    let project: Option<&str> = if query.unfiled.as_deref() == Some("1") {
        Some("")
    } else if project_name.is_empty() {
        None
    } else {
        Some(&project_name)
    };
    let mine = query.mine.as_deref() == Some("1");
    let sort = Sort::parse(query.sort.as_deref().unwrap_or(""));
    // 큰 수를 넣어도 곱셈이 넘치지 않게 상한을 둔다.
    let page_no = query.page.unwrap_or(1).clamp(1, 1_000_000);
    let filter = Filter { q: &q, project, uploader: if mine { &user.email } else { "" }, sort };
    let mut docs = match app.docs.list(&filter, (page_no - 1) * PAGE_SIZE) {
        Ok(d) => d,
        Err(e) => return server_error("목록 조회", e),
    };
    let has_next = docs.len() as i64 > PAGE_SIZE;
    docs.truncate(PAGE_SIZE as usize);
    // 숫자·막대·옆 패널은 보조 정보라, 못 읽어도 목록은 보여 준다.
    let sum = app.docs.summary(&user.email).unwrap_or_default();
    let counts = app.docs.project_counts().unwrap_or_default();
    let daily = app.docs.daily_views().unwrap_or_default();
    let ids: Vec<String> = docs.iter().map(|d| d.id.clone()).collect();
    let activity = app.docs.activity(&ids).unwrap_or_default();

    // 요약: 전체 문서, 최근 7일 열람(하루별 막대), 내가 올린 문서.
    let named = counts.iter().filter(|(p, _)| !p.is_empty()).count();
    let unfiled_n = counts.iter().find(|(p, _)| p.is_empty()).map_or(0, |c| c.1);
    let unfiled_note = if unfiled_n > 0 {
        format!("<span class=\"unfiled-note\"><span class=\"sep\" aria-hidden=\"true\">·</span>미분류 {unfiled_n}</span>")
    } else {
        String::new()
    };
    let day_max = daily.iter().map(|d| d.1).max().unwrap_or(0);
    let week_views: i64 = daily.iter().map(|d| d.1).sum();
    let bars: String = daily
        .iter()
        .enumerate()
        .map(|(i, (start, n))| {
            let h = if day_max > 0 { (n * 100 / day_max).max(if *n > 0 { 12 } else { 0 }) } else { 0 };
            let class = match (*n > 0, i == daily.len() - 1) {
                (true, true) => "on today",
                (true, false) => "on",
                _ => "",
            };
            format!("<span class=\"{class}\" style=\"height:{h}%\" title=\"{} {n}회\"></span>", show_time(*start).split(' ').next().unwrap_or(""))
        })
        .collect();
    let first_day = daily.first().map(|(s, _)| show_time(*s)).unwrap_or_default();
    let first_day = first_day.get(8..10).map(|d| format!("{}일", d.trim_start_matches('0'))).unwrap_or_default();
    let mine_link = if mine {
        format!("<a href=\"{}\">모든 문서 보기</a>", escape(&list_url(&q, project, false, sort)))
    } else {
        format!("<a href=\"{}\">내 문서만 보기</a>", escape(&list_url(&q, project, true, sort)))
    };
    let stats = format!(
        "<dl class=\"stats\">\
         <div><dt>전체 문서</dt><dd>{total}<span class=\"sub\"><span>프로젝트 {named}개{unfiled_note}</span></span></dd></div>\
         <div class=\"with-spark\"><dt><span class=\"long\">최근 </span>7일 열람된 문서</dt><dd>{viewed}<small> / {total}</small>\
         <span class=\"sub\"><span>열람 {week_views}회</span></span>\
         <span class=\"spark-box\"><span class=\"spark\" role=\"img\" aria-label=\"최근 7일 하루별 열람 수\">{bars}</span>\
         <span class=\"spark-axis\"><span>{first_day}</span><span>오늘</span></span></span></dd></div>\
         <div><dt>내가 올린 문서</dt><dd>{mine_n}<span class=\"sub\">{mine_link}</span></dd></div></dl>",
        total = sum.total,
        viewed = sum.viewed,
        mine_n = sum.mine,
    );

    // 프로젝트 칩: 문서 수를 붙이고, 미분류도 고를 수 있다.
    let chip = |p: Option<&str>, label: &str, n: i64| {
        let cur = if p == project { " aria-current=\"true\"" } else { "" };
        format!(
            "<a href=\"{}\"{cur}>{}<span class=\"n\">{n}</span></a>",
            escape(&list_url(&q, p, mine, sort)),
            escape(label)
        )
    };
    let chips: String = std::iter::once(chip(None, "모든 프로젝트", sum.total))
        .chain(counts.iter().map(|(p, n)| chip(Some(p), if p.is_empty() { "미분류" } else { p }, *n)))
        .collect();
    let hidden_keep = format!(
        "<input type=\"hidden\" name=\"q\" value=\"{}\">{}{}",
        escape(&q),
        match project {
            Some("") => "<input type=\"hidden\" name=\"unfiled\" value=\"1\">".to_owned(),
            Some(p) => format!("<input type=\"hidden\" name=\"project\" value=\"{}\">", escape(p)),
            None => String::new(),
        },
        if mine { "<input type=\"hidden\" name=\"mine\" value=\"1\">" } else { "" },
    );
    let sort_opts: String = [(Sort::Recent, "최근 올린 순"), (Sort::Views, "많이 본 순"), (Sort::Comments, "댓글 많은 순"), (Sort::Title, "제목 순")]
        .iter()
        .map(|(s, label)| {
            let sel = if *s == sort { " selected" } else { "" };
            format!("<option value=\"{}\"{sel}>{label}</option>", s.key())
        })
        .collect();
    let toolbar = format!(
        "<div class=\"toolbar\"><nav class=\"chips\" aria-label=\"프로젝트\">{chips}</nav>\
         <form class=\"sort\" method=\"get\" action=\"/\">{hidden_keep}<label for=\"sort\">정렬</label>\
         <select id=\"sort\" name=\"sort\" onchange=\"this.form.submit()\">{sort_opts}</select>\
         <noscript><button class=\"btn quiet sm\">적용</button></noscript></form></div>"
    );

    // 검색어·프로젝트·정렬을 숨은 칸으로 넘겨 브라우저가 주소를 인코딩하게 한다.
    let nav_button = |n: i64, label: &str| {
        format!(
            "<form method=\"get\" action=\"/\">{hidden_keep}<input type=\"hidden\" name=\"sort\" value=\"{}\">\
             <button name=\"page\" value=\"{n}\">{label}</button></form>",
            sort.key()
        )
    };
    let pager = if page_no > 1 || has_next {
        format!(
            "<nav class=\"pager\" aria-label=\"쪽\">{}<span>{page_no}쪽</span>{}</nav>",
            if page_no > 1 { nav_button(page_no - 1, "이전") } else { String::new() },
            if has_next { nav_button(page_no + 1, "다음") } else { String::new() },
        )
    } else {
        String::new()
    };

    let view_max = activity.values().map(|a| a.0).max().unwrap_or(0);
    let rows: String = docs
        .iter()
        .map(|d| {
            let id = escape(&d.id);
            let title = escape(&d.title);
            let confirm = "onsubmit=\"return confirm('이 문서를 삭제할까요? 목록과 검색에서 사라지고, 관리자가 다시 살릴 수 있습니다.')\"";
            let (delete, delete_menu) = if can_modify(&user, d) {
                (
                    format!("<form method=\"post\" action=\"/d/{id}/delete\" {confirm}><button class=\"link danger\">삭제</button></form>"),
                    format!("<form method=\"post\" action=\"/d/{id}/delete\" {confirm}><button class=\"danger\">삭제…</button></form>"),
                )
            } else {
                // 삭제가 없는 줄도 작업 열 끝이 맞게 자리만 둔다.
                ("<span class=\"slot\"></span>".to_owned(), String::new())
            };
            // 본문에서 걸렸으면 왜 걸렸는지 보이게 주변 글자를 붙인다. 발췌를 못 읽어도 목록은 보여 준다.
            let excerpt = match q.as_str() {
                "" => String::new(),
                t => match app.docs.excerpt(&d.id, t) {
                    Ok(Some(e)) => format!("<span class=\"excerpt\">{}</span>", escape(&e)),
                    _ => String::new(),
                },
            };
            let tag = if d.project.is_empty() {
                "<span class=\"tag none\">미분류</span>".to_owned()
            } else {
                format!("<span class=\"tag\">{}</span>", escape(&d.project))
            };
            let (views, comments) = activity.get(&d.id).copied().unwrap_or_default();
            format!(
                "<tr><td class=\"t\"><a class=\"doc-title\" href=\"/d/{id}\" title=\"{title}\">{title}</a>{excerpt}</td>\
                 <td class=\"g\">{tag}</td><td class=\"by\">{}{}</td>\
                 <td class=\"num v{}\" data-label=\"7일 열람\">{}{views}</td><td class=\"num c{}\" data-label=\"댓글\">{comments}</td>\
                 <td class=\"m\"><div class=\"acts\"><a href=\"/d/{id}/history\">이력</a><a href=\"/d/{id}/comments\">댓글</a>{delete}</div>\
                 <details class=\"rowmenu\"><summary aria-label=\"{title} 작업\">···</summary>\
                 <div><a href=\"/d/{id}/history\">이력 보기</a><a href=\"/d/{id}/comments\">댓글 보기</a>{delete_menu}</div></details></td></tr>",
                escape(&d.uploader_name),
                when(d.created_at),
                if views == 0 { " zero" } else { "" },
                meter(views, view_max),
                if comments == 0 { " zero" } else { "" },
            )
        })
        .collect();

    let filtering = !q.is_empty() || project.is_some() || mine;
    let scope = {
        let mut parts = vec![match project {
            Some("") => "미분류".to_owned(),
            Some(p) => escape(p),
            None => "모든 프로젝트".to_owned(),
        }];
        if mine {
            parts.push("내 문서".into());
        }
        if !q.is_empty() {
            parts.push(format!("'{}' 검색", escape(&q)));
        }
        parts.join(", ")
    };
    let shown = if has_next || page_no > 1 { format!("{PAGE_SIZE}개 넘게") } else { format!("{}개", docs.len()) };
    let result_line = if filtering && !docs.is_empty() {
        format!("<p class=\"result\">찾은 문서 {shown}. <a href=\"/\">조건 지우기</a></p>")
    } else {
        String::new()
    };
    // 머리글을 누르면 그 기준으로 정렬한다. 지금 기준은 화살표와 aria-sort 로 알린다.
    let sorted = |s: Sort, label: &str, class: &str| {
        let (arrow, aria) = if s == sort {
            if s == Sort::Title { (" ↑", " aria-sort=\"ascending\"") } else { (" ↓", " aria-sort=\"descending\"") }
        } else {
            ("", "")
        };
        format!("<th{class}{aria}><a class=\"sort-link\" href=\"{}\">{label}{arrow}</a></th>", escape(&list_url(&q, project, mine, s)))
    };
    let board = if docs.is_empty() {
        let msg = if filtering {
            "<p class=\"empty\">찾는 문서가 없습니다. 검색어를 바꾸거나 <a href=\"/\">조건 지우기</a>로 넓혀 보세요.</p>"
        } else {
            "<p class=\"empty\">아직 올라온 문서가 없습니다. <a href=\"/upload\">첫 HTML 올리기</a></p>"
        };
        format!("<div class=\"board\">{msg}</div>")
    } else {
        format!(
            "<div class=\"board\"><div class=\"mhead\"><form method=\"get\" action=\"/\">{hidden_keep}\
             <select name=\"sort\" aria-label=\"정렬\" onchange=\"this.form.submit()\">{sort_opts}</select>\
             <noscript><button>적용</button></noscript></form><span>열람</span><span>댓글</span><span></span></div>\
             <table><thead><tr>{}<th>프로젝트</th>{}{}{}\
             <th><span class=\"visually-hidden\">작업</span></th></tr></thead><tbody>{rows}</tbody></table>\
             <div class=\"board-foot{}\"><span>{scope}, {}</span>{pager}</div></div>",
            sorted(Sort::Title, "제목", ""),
            sorted(Sort::Recent, "올린 사람, 시각", ""),
            sorted(Sort::Views, "7일 열람", " class=\"num\""),
            sorted(Sort::Comments, "댓글", " class=\"num\""),
            // 아무 조건도 없을 때는 휴대폰에서 이 줄을 숨긴다 (목록 머리의 정렬과 겹친다).
            if filtering { "" } else { " plain" },
            match sort {
                Sort::Recent => "최근 올린 순",
                Sort::Views => "많이 본 순",
                Sort::Comments => "댓글 많은 순",
                Sort::Title => "제목 순",
            },
        )
    };

    // 옆: 많이 본 문서(순위와 막대), 최근 댓글.
    let top = app.docs.top_viewed(5).unwrap_or_default();
    let top_max = top.first().map_or(0, |t| t.2);
    let top: String = top
        .iter()
        .enumerate()
        .map(|(i, (id, title, n))| {
            format!(
                "<li><span class=\"no\">{}</span><a href=\"/d/{}\" title=\"{t}\">{t}</a><span class=\"n\">{n}회</span>{}</li>",
                i + 1,
                escape(id),
                meter(*n, top_max),
                t = escape(title),
            )
        })
        .collect();
    let top = if top.is_empty() { "<p class=\"quiet\">지난 7일 동안 열린 문서가 없습니다.</p>".to_owned() } else { format!("<ol class=\"rank\">{top}</ol>") };
    let recent: String = app
        .docs
        .recent_comments(5)
        .unwrap_or_default()
        .iter()
        .map(|c| {
            let body: String = c.body.chars().take(80).collect();
            let more = if c.body.chars().count() > 80 { "…" } else { "" };
            let initial: String = c.by_name.chars().next().map(String::from).unwrap_or_default();
            format!(
                "<li><span class=\"avatar sm\" aria-hidden=\"true\">{}</span><div><b>{}</b>{}<p>{}{more}</p>\
                 <a href=\"/d/{}/comments\">{}</a></div></li>",
                escape(&initial),
                escape(&c.by_name),
                when(c.created_at),
                escape(&body),
                escape(&c.doc_id),
                escape(&c.doc_title),
            )
        })
        .collect();
    let recent = if recent.is_empty() { "<p class=\"quiet\">아직 댓글이 없습니다.</p>".to_owned() } else { format!("<ul class=\"cmts\">{recent}</ul>") };

    shell(
        &app,
        &user,
        "/",
        &format!(
            "<div class=\"head dash\"><div><h1>문서<small>{total}개</small></h1>\
             <p class=\"lede\">팀이 Claude로 만든 HTML 결과물을 링크로 열고 찾습니다.</p></div>\
             <a class=\"btn\" href=\"/upload\">HTML 올리기</a></div>\
             <form class=\"search\" method=\"get\" action=\"/\" role=\"search\"><span class=\"mag\" aria-hidden=\"true\"></span>\
             <input type=\"search\" id=\"q\" name=\"q\" aria-keyshortcuts=\"/\" value=\"{}\" placeholder=\"제목, 올린 사람, 본문으로 찾기\" aria-label=\"문서 찾기\">\
             <kbd aria-hidden=\"true\">/</kbd><button class=\"btn quiet sm\">찾기</button></form>\
             <div class=\"grid\"><section aria-label=\"문서\">{stats}{toolbar}{result_line}{board}</section>\
             <aside class=\"side\" aria-label=\"많이 본 문서와 최근 댓글\"><section><h2>많이 본 문서<small>최근 7일</small></h2>{top}</section>\
             <section><h2>최근 댓글<small>전체 {}개</small></h2>{recent}</section></aside></div>\
             <script>document.addEventListener('keydown',e=>{{if(e.key==='/'&&!/INPUT|TEXTAREA|SELECT/.test(document.activeElement.tagName)){{e.preventDefault();document.getElementById('q').focus()}}}})</script>",
            escape(&q),
            sum.comments,
            total = sum.total,
        ),
    )
    .into_response()
}

fn error_box(error: &str) -> String {
    if error.is_empty() { String::new() } else { format!("<p class=\"error\" role=\"alert\">{}</p>", escape(error)) }
}

/// `values` 는 오류로 다시 보여 줄 때 입력해 둔 (제목, 프로젝트) 다.
fn upload_form(app: &AppState, user: &crate::users::User, error: &str, values: (&str, &str)) -> Response {
    let known: String = app
        .docs
        .projects()
        .unwrap_or_default()
        .iter()
        .map(|p| format!("<option value=\"{}\">", escape(p)))
        .collect();
    let status = if error.is_empty() { StatusCode::OK } else { StatusCode::BAD_REQUEST };
    (
        status,
        shell(
            &app,
            user,
            "/upload",
            &format!(
                "<div class=\"head\"><div><h1>HTML 올리기</h1>\
                 <p class=\"lede\">올린 문서는 등록된 전 직원이 볼 수 있습니다. 링크를 받아 공유하세요.</p></div></div>\
                 <form class=\"form\" method=\"post\" action=\"/upload\" enctype=\"multipart/form-data\">{}\
                 <div class=\"field\"><label for=\"title\">제목</label>\
                 <input id=\"title\" name=\"title\" required maxlength=\"200\" value=\"{}\"></div>\
                 <div class=\"field\"><label for=\"project\">프로젝트</label>\
                 <input id=\"project\" name=\"project\" list=\"projects\" maxlength=\"100\" value=\"{}\">\
                 <datalist id=\"projects\">{known}</datalist>\
                 <span class=\"hint\">비워 두면 미분류로 올라갑니다. 기존 프로젝트는 입력하면 제안됩니다.</span></div>\
                 <div class=\"field\"><label for=\"file\">파일</label>\
                 <input id=\"file\" type=\"file\" name=\"file\" accept=\"{accept}\" required>\
                 <span class=\"hint\">HTML, Markdown(.md), SVG, Mermaid(.mmd), React(.jsx) · 10MB 이하</span></div>\
                 <button class=\"btn\">올리기</button></form>\
                 <p class=\"lede\" style=\"margin-top:24px\">Claude에서 바로 올리려면 <a href=\"/tokens\">내 토큰</a>을 발급받으세요.</p>\
                 <p class=\"lede\" style=\"margin-top:8px\">설문도 됩니다. 문서 스크립트에서 <code>window.htmlhub.submit({{질문: 답}})</code> 을 부르면 \
                 보는 사람이 확인한 뒤 그 사람 이름으로 응답이 모이고, 올린 사람이 문서의 '응답' 탭에서 봅니다.</p>",
                error_box(error),
                escape(values.0),
                escape(values.1),
                accept = crate::convert::ACCEPT,
            ),
        ),
    )
        .into_response()
}

pub async fn upload_page(State(app): State<AppState>, jar: PrivateCookieJar) -> Response {
    match current_user(&app, &jar) {
        Ok(user) => upload_form(&app, &user, "", ("", "")),
        Err(r) => r,
    }
}

const BAD_FILE: &str = "파일이 너무 크거나 올바르지 않습니다. 10MB 이하 파일만 올릴 수 있습니다.";

pub struct Form {
    pub title: String,
    pub project: String,
    pub file: Option<(String, axum::body::Bytes)>,
}

pub async fn read_form(mut form: Multipart) -> Result<Form, &'static str> {
    let mut out = Form { title: String::new(), project: String::new(), file: None };
    while let Some(field) = form.next_field().await.map_err(|_| BAD_FILE)? {
        let name = field.name().unwrap_or_default().to_owned();
        let filename = field.file_name().unwrap_or_default().to_owned();
        let bytes = field.bytes().await.map_err(|_| BAD_FILE)?;
        match name.as_str() {
            "title" => out.title = String::from_utf8_lossy(&bytes).trim().to_owned(),
            "project" => out.project = String::from_utf8_lossy(&bytes).trim().to_owned(),
            "file" => out.file = Some((filename, bytes)),
            _ => {}
        }
    }
    Ok(out)
}

/// 한 사람이 하루(24시간)에 올릴 수 있는 버전 수. 새어 나간 토큰이나 폭주한 에이전트가 디스크를 채우지 못하게 한다 (가정: 200).
pub const DAILY_UPLOADS: i64 = 200;
pub const OVER_QUOTA: &str = "하루 올리기 한도(200건)를 넘었습니다. 내일 다시 하거나 관리자에게 문의하세요.";

/// 제목(필수, 200자)·프로젝트(100자) 규칙.
pub fn check_meta(title: &str, project: &str) -> Result<(), &'static str> {
    if title.is_empty() {
        return Err("제목을 입력해 주세요.");
    }
    if title.chars().count() > 200 || project.chars().count() > 100 {
        return Err("제목은 200자, 프로젝트는 100자 이하로 입력해 주세요.");
    }
    Ok(())
}

/// 올린 파일이 규칙(U3·U4)에 맞는지 본다. 맞으면 저장할 HTML 을 돌려준다 (Markdown·SVG·Mermaid·JSX 는 HTML 로 감싼다).
pub fn check_file(file: Option<(String, axum::body::Bytes)>) -> Result<Vec<u8>, &'static str> {
    let Some((filename, bytes)) = file.filter(|(_, b)| !b.is_empty()) else {
        return Err("파일을 선택해 주세요.");
    };
    if bytes.len() > MAX_BYTES {
        return Err("파일이 10MB 를 넘습니다.");
    }
    crate::convert::to_html(&filename, &bytes).unwrap_or(Err(".html, .md, .svg, .mmd, .jsx 파일만 올릴 수 있습니다."))
}

pub async fn upload(State(app): State<AppState>, jar: PrivateCookieJar, form: Multipart) -> Response {
    let user = match current_user(&app, &jar) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let form = match read_form(form).await {
        Ok(f) => f,
        Err(msg) => return upload_form(&app, &user, msg, ("", "")),
    };
    if let Err(msg) = check_meta(&form.title, &form.project) {
        return upload_form(&app, &user, msg, (&form.title, &form.project));
    }
    let bytes = match check_file(form.file) {
        Ok(b) => b,
        Err(msg) => return upload_form(&app, &user, msg, (&form.title, &form.project)),
    };
    match app.docs.with_quota(&user.email, || app.docs.insert(&form.title, &form.project, &user, &bytes)) {
        Ok(Some(id)) => Redirect::to(&format!("/uploaded/{id}")).into_response(),
        Ok(None) => upload_form(&app, &user, OVER_QUOTA, (&form.title, &form.project)),
        Err(e) => server_error("문서 저장", e),
    }
}

pub(crate) fn can_modify(user: &crate::users::User, doc: &Doc) -> bool {
    doc.uploader_email == user.email || user.is_admin()
}

/// 로그인한 직원과 삭제되지 않은 문서를 함께 돌려준다. 안 되면 그대로 돌려줄 응답을 에러로 준다.
pub(crate) fn user_and_doc(
    app: &AppState,
    jar: &PrivateCookieJar,
    id: &str,
) -> Result<(crate::users::User, Doc), Response> {
    let user = current_user_next(app, jar, &format!("/d/{id}"))?;
    match app.docs.get(id) {
        Ok(Some(doc)) => Ok((user, doc)),
        Ok(None) => Err(not_found()),
        Err(e) => Err(server_error("문서 조회", e)),
    }
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, page("올린 사람이나 관리자만 할 수 있습니다.")).into_response()
}

/// 문서 하나에 딸린 화면(이력, 댓글, 새 버전)의 머리말. 이동 경로, 메타 줄, 개수가 붙은 탭. 지금 탭은 링크 대신 글자로 둔다.
pub(crate) fn doc_header(app: &AppState, doc: &Doc, here: &str) -> String {
    let id = escape(&doc.id);
    let versions = app.docs.versions(&doc.id).map(|v| v.len()).unwrap_or(0);
    let (views, comments) = app.docs.activity(std::slice::from_ref(&doc.id)).unwrap_or_default().get(&doc.id).copied().unwrap_or_default();
    let tab = |href: String, label: &str, n: usize| {
        if label == here {
            format!("<span aria-current=\"page\">{label} <span class=\"n\">{n}</span></span>")
        } else {
            format!("<a href=\"{href}\">{label} <span class=\"n\">{n}</span></a>")
        }
    };
    let project = if doc.project.is_empty() { "미분류".to_owned() } else { escape(&doc.project) };
    let sep = "<span class=\"sep\" aria-hidden=\"true\">·</span>";
    let crumb_project = if doc.project.is_empty() {
        String::new()
    } else {
        format!(" / <a href=\"{}\">{project}</a>", escape(&list_url("", Some(&doc.project), false, Sort::Recent)))
    };
    format!(
        "<nav class=\"crumb\" aria-label=\"이동 경로\"><a href=\"/\">문서</a>{crumb_project}</nav>\
         <div class=\"head doc-head\"><div><h1>{}</h1><p class=\"lede\">{project}{sep}{}{sep}{}{sep}7일 열람 {views}</p></div>\
         <a class=\"btn line\" href=\"/d/{id}\">문서 열기</a></div>\
         <nav class=\"tabs\" aria-label=\"문서 메뉴\">{}{}{}</nav>",
        escape(&doc.title),
        escape(&doc.uploader_name),
        when(doc.created_at),
        tab(format!("/d/{id}/history"), "이력", versions),
        tab(format!("/d/{id}/comments"), "댓글", comments as usize),
        // 설문 응답이 있는 문서에만 보인다. 내용은 올린 사람과 관리자만 본다.
        match app.docs.response_count(&doc.id).unwrap_or(0) {
            0 if here != "응답" => String::new(),
            n => tab(format!("/d/{id}/responses"), "응답", n as usize),
        },
    )
}

fn replace_form(app: &AppState, user: &crate::users::User, doc: &Doc, error: &str) -> Response {
    let status = if error.is_empty() { StatusCode::OK } else { StatusCode::BAD_REQUEST };
    (
        status,
        shell(
            &app,
            user,
            "/",
            &format!(
                "{}{}<h2>제목·프로젝트 고치기</h2>\
                 <form class=\"form\" method=\"post\" action=\"/d/{id}/meta\" style=\"margin-bottom:32px\">\
                 <div class=\"field\"><label for=\"m-title\">제목</label>\
                 <input id=\"m-title\" name=\"title\" required maxlength=\"200\" value=\"{title}\"></div>\
                 <div class=\"field\"><label for=\"m-project\">프로젝트</label>\
                 <input id=\"m-project\" name=\"project\" maxlength=\"100\" value=\"{project}\">\
                 <span class=\"hint\">비워 두면 미분류. 링크와 이력은 그대로입니다.</span></div>\
                 <button class=\"btn line\">저장</button></form>\
                 <h2>새 버전 올리기</h2>\
                 <p class=\"lede\" style=\"margin-bottom:16px\">링크는 그대로이고 내용만 새 파일로 바뀝니다. 이전 버전은 이력에 남습니다.</p>\
                 <form class=\"form\" method=\"post\" action=\"/d/{id}/replace\" enctype=\"multipart/form-data\">\
                 <div class=\"field\"><label for=\"file\">파일</label>\
                 <input id=\"file\" type=\"file\" name=\"file\" accept=\"{accept}\" required>\
                 <span class=\"hint\">HTML, Markdown(.md), SVG, Mermaid(.mmd), React(.jsx) · 10MB 이하</span></div>\
                 <button class=\"btn\">새 버전 올리기</button></form>",
                doc_header(app, doc, ""),
                error_box(error),
                id = escape(&doc.id),
                title = escape(&doc.title),
                project = escape(&doc.project),
                accept = crate::convert::ACCEPT,
            ),
        ),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct MetaForm {
    title: String,
    #[serde(default)]
    project: String,
}

/// 제목·프로젝트 고치기. 올린 사람과 관리자만.
pub async fn set_meta(
    State(app): State<AppState>,
    jar: PrivateCookieJar,
    Path(id): Path<String>,
    axum::Form(f): axum::Form<MetaForm>,
) -> Response {
    let (user, doc) = match user_and_doc(&app, &jar, &id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if !can_modify(&user, &doc) {
        return forbidden();
    }
    let (title, project) = (f.title.trim(), f.project.trim());
    if let Err(msg) = check_meta(title, project) {
        return replace_form(&app, &user, &doc, msg);
    }
    match app.docs.set_meta(&id, title, project) {
        Ok(()) => Redirect::to(&format!("/d/{id}/history")).into_response(),
        Err(e) => server_error("제목 고치기", e),
    }
}

pub async fn replace_page(State(app): State<AppState>, jar: PrivateCookieJar, Path(id): Path<String>) -> Response {
    match user_and_doc(&app, &jar, &id) {
        Ok((user, doc)) if can_modify(&user, &doc) => replace_form(&app, &user, &doc, ""),
        Ok(_) => forbidden(),
        Err(r) => r,
    }
}

pub async fn replace(
    State(app): State<AppState>,
    jar: PrivateCookieJar,
    Path(id): Path<String>,
    form: Multipart,
) -> Response {
    let (user, doc) = match user_and_doc(&app, &jar, &id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if !can_modify(&user, &doc) {
        return forbidden();
    }
    let form = match read_form(form).await {
        Ok(f) => f,
        Err(msg) => return replace_form(&app, &user, &doc, msg),
    };
    let bytes = match check_file(form.file) {
        Ok(b) => b,
        Err(msg) => return replace_form(&app, &user, &doc, msg),
    };
    match app.docs.with_quota(&user.email, || app.docs.replace(&id, &user, &bytes)) {
        Ok(Some(_)) => Redirect::to(&format!("/d/{id}/history")).into_response(),
        Ok(None) => replace_form(&app, &user, &doc, OVER_QUOTA),
        Err(e) => server_error("새 버전 저장", e),
    }
}

pub async fn history(State(app): State<AppState>, jar: PrivateCookieJar, Path(id): Path<String>) -> Response {
    let (user, doc) = match user_and_doc(&app, &jar, &id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let versions = match app.docs.versions(&id) {
        Ok(v) => v,
        Err(e) => return server_error("이력 조회", e),
    };
    let modify = can_modify(&user, &doc);
    let latest = versions.first().map(|v| v.n);
    let rows: String = versions
        .iter()
        .map(|v| {
            let current = if Some(v.n) == latest { " <span class=\"tag\">현재</span>" } else { "" };
            let restore = if modify && Some(v.n) != latest {
                format!(
                    "<form method=\"post\" action=\"/d/{}/restore/{}\" \
                     onsubmit=\"return confirm('{}번 버전 내용을 새 버전으로 다시 올릴까요?')\">\
                     <button class=\"link\">이 버전으로 되돌리기</button></form>",
                    escape(&doc.id),
                    v.n,
                    v.n,
                )
            } else {
                String::new()
            };
            format!(
                "<tr><td>{}번{current}</td><td>{}</td><td class=\"when\">{}</td>\
                 <td class=\"actions\"><a href=\"/d/{}/v/{}\">열기</a>{restore}</td></tr>",
                v.n,
                escape(&v.by_name),
                when(v.created_at),
                escape(&doc.id),
                v.n,
            )
        })
        .collect();
    let replace_link = if modify {
        format!("<p><a class=\"btn\" href=\"/d/{}/replace\">새 버전 올리기</a></p>", escape(&doc.id))
    } else {
        String::new()
    };
    shell(
        &app,
        &user,
        "/",
        &format!(
            "{}<h2>버전 이력</h2>\
             <p class=\"lede\" style=\"margin-bottom:12px\">되돌리면 옛 내용이 새 버전으로 올라가고, 중간 버전도 그대로 남습니다.</p>\
             <div class=\"table-wrap narrow\"><table><thead><tr><th>버전</th><th>올린 사람</th><th>올린 시각</th>\
             <th><span class=\"visually-hidden\">할 일</span></th></tr></thead><tbody>{rows}</tbody></table></div>{replace_link}",
            doc_header(&app, &doc, "이력"),
        ),
    )
    .into_response()
}

const MAX_COMMENT: usize = 2000;

fn comments_page(app: &AppState, doc: &Doc, user: &crate::users::User, comments: &[Comment], error: &str) -> Response {
    let status = if error.is_empty() { StatusCode::OK } else { StatusCode::BAD_REQUEST };
    let id = escape(&doc.id);
    let items: String = comments
        .iter()
        .map(|c| {
            let delete = if c.by_email == user.email || user.is_admin() {
                format!(
                    "<form method=\"post\" action=\"/d/{id}/comments/{}/delete\" \
                     onsubmit=\"return confirm('이 댓글을 지울까요?')\"><button class=\"ghost danger\">삭제</button></form>",
                    c.id
                )
            } else {
                String::new()
            };
            let initial: String = c.by_name.chars().next().map(String::from).unwrap_or_default();
            format!(
                "<li><span class=\"avatar sm\" aria-hidden=\"true\">{}</span><div><div class=\"who-line\"><b>{}</b>{}{delete}</div>\
                 <div class=\"body\">{}</div></div></li>",
                escape(&initial),
                escape(&c.by_name),
                when(c.created_at),
                escape(&c.body)
            )
        })
        .collect();
    let list = if comments.is_empty() {
        "<p class=\"empty\">아직 댓글이 없습니다. 문서를 보고 의견을 남겨 주세요.</p>".to_owned()
    } else {
        format!("<ul class=\"comments\">{items}</ul>")
    };
    (
        status,
        shell(
            &app,
            user,
            "/",
            &format!(
                "{}<h2 class=\"visually-hidden\">댓글 {}개</h2>{list}\
                 <form class=\"reply\" method=\"post\" action=\"/d/{id}/comments\">{}\
                 <label for=\"body\">댓글</label>\
                 <textarea id=\"body\" name=\"body\" rows=\"4\" maxlength=\"{MAX_COMMENT}\" required></textarea>\
                 <div class=\"reply-foot\"><span class=\"hint\">2,000자까지. 쓴 사람과 관리자만 지울 수 있습니다.</span>\
                 <button class=\"btn\">댓글 남기기</button></div></form>",
                doc_header(app, doc, "댓글"),
                comments.len(),
                error_box(error),
            ),
        ),
    )
        .into_response()
}

pub async fn comments_view(State(app): State<AppState>, jar: PrivateCookieJar, Path(id): Path<String>) -> Response {
    let (user, doc) = match user_and_doc(&app, &jar, &id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    match app.docs.comments(&id) {
        Ok(list) => comments_page(&app, &doc, &user, &list, ""),
        Err(e) => server_error("댓글 조회", e),
    }
}

#[derive(Deserialize)]
pub struct CommentForm {
    body: String,
}

pub async fn comment_add(
    State(app): State<AppState>,
    jar: PrivateCookieJar,
    Path(id): Path<String>,
    axum::Form(form): axum::Form<CommentForm>,
) -> Response {
    let (user, doc) = match user_and_doc(&app, &jar, &id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let body = form.body.trim();
    let list = || app.docs.comments(&id).unwrap_or_default();
    if body.is_empty() {
        return comments_page(&app, &doc, &user, &list(), "댓글 내용을 입력해 주세요.");
    }
    if body.chars().count() > MAX_COMMENT {
        return comments_page(&app, &doc, &user, &list(), "댓글은 2,000자 이하로 입력해 주세요.");
    }
    match app.docs.add_comment(&id, &user, body) {
        Ok(()) => Redirect::to(&format!("/d/{id}/comments")).into_response(),
        Err(e) => server_error("댓글 저장", e),
    }
}

pub async fn comment_delete(
    State(app): State<AppState>,
    jar: PrivateCookieJar,
    Path((id, cid)): Path<(String, i64)>,
) -> Response {
    let (user, _) = match user_and_doc(&app, &jar, &id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let c = match app.docs.comment(&id, cid) {
        Ok(Some(c)) => c,
        Ok(None) => return not_found(),
        Err(e) => return server_error("댓글 조회", e),
    };
    if c.by_email != user.email && !user.is_admin() {
        return (StatusCode::FORBIDDEN, page("쓴 사람이나 관리자만 지울 수 있습니다.")).into_response();
    }
    match app.docs.hide_comment(cid) {
        Ok(()) => Redirect::to(&format!("/d/{id}/comments")).into_response(),
        Err(e) => server_error("댓글 삭제", e),
    }
}

pub async fn view_version(
    State(app): State<AppState>,
    jar: PrivateCookieJar,
    Path((id, n)): Path<(String, i64)>,
) -> Response {
    if let Err(r) = user_and_doc(&app, &jar, &id) {
        return r;
    }
    match app.docs.read_version(&id, n) {
        Ok(Some(bytes)) => document_response(bytes, ""),
        Ok(None) => not_found(),
        Err(e) => server_error("문서 파일 읽기", e),
    }
}

pub async fn restore(
    State(app): State<AppState>,
    jar: PrivateCookieJar,
    Path((id, n)): Path<(String, i64)>,
) -> Response {
    let (user, doc) = match user_and_doc(&app, &jar, &id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if !can_modify(&user, &doc) {
        return forbidden();
    }
    match app.docs.restore(&id, n, &user) {
        Ok(Some(_)) => Redirect::to(&format!("/d/{id}/history")).into_response(),
        Ok(None) => not_found(),
        Err(e) => server_error("되돌리기", e),
    }
}

/// 올린 직후 링크를 보여 준다.
pub async fn uploaded(State(app): State<AppState>, jar: PrivateCookieJar, Path(id): Path<String>) -> Response {
    let user = match current_user(&app, &jar) {
        Ok(u) => u,
        Err(r) => return r,
    };
    match app.docs.get(&id) {
        Ok(Some(d)) => shell(
            &app,
            &user,
            "/upload",
            &format!(
                "<div class=\"head\"><div><h1>올렸습니다</h1><p class=\"lede\">{}</p></div></div>\
                 <p>이 링크를 보내면 등록된 직원 누구나 로그인 후 볼 수 있습니다.</p>\
                 <div class=\"copy\"><input id=\"link\" readonly value=\"{base}/d/{id}\" onfocus=\"this.select()\" aria-label=\"공유 링크\">\
                 <button class=\"btn\" type=\"button\" onclick=\"navigator.clipboard.writeText(link.value);this.textContent='복사됨'\">링크 복사</button></div>\
                 <p class=\"actions\"><a href=\"/d/{id}\">문서 열기</a><a href=\"/upload\">하나 더 올리기</a><a href=\"/\">문서 목록</a></p>",
                escape(&d.title),
                id = escape(&d.id),
                base = escape(app.base_url.trim_end_matches('/')),
            ),
        )
        .into_response(),
        Ok(None) => not_found(),
        Err(e) => server_error("문서 조회", e),
    }
}

/// 문서 링크(`/d/<번호>`)는 앱 틀이다. 위에 제목·댓글·이력 줄을 두고, 문서는 격리된 iframe(`/raw`)으로 넣는다 (정책서 M4).
/// 설문 문서가 응답을 내면 틀이 보는 사람에게 확인을 받은 뒤 로그인한 이름으로 낸다.
pub async fn view(State(app): State<AppState>, jar: PrivateCookieJar, Path(id): Path<String>) -> Response {
    let (user, doc) = match user_and_doc(&app, &jar, &id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    // 기록이 실패해도 문서는 보여 준다.
    if let Err(e) = app.docs.record_view(&id) {
        log!("열람 기록 실패: {e}");
    }
    let (_, comments) = app.docs.activity(std::slice::from_ref(&doc.id)).unwrap_or_default().get(&doc.id).copied().unwrap_or_default();
    let responses = match app.docs.response_count(&id).unwrap_or(0) {
        n if n > 0 && can_modify(&user, &doc) => format!("<a href=\"/d/{0}/responses\">응답<span class=\"n\">{n}</span></a>", escape(&id)),
        _ => String::new(),
    };
    let (id, title) = (escape(&id), escape(&doc.title));
    crate::auth::document_titled(
        &doc.title,
        &format!(
            "<div class=\"viewer\"><header class=\"vbar\"><a class=\"brand\" href=\"/\">html-hub</a>\
             <h1 class=\"vtitle\" title=\"{title}\">{title}</h1><nav aria-label=\"문서 메뉴\">\
             <a href=\"/d/{id}/comments\">댓글<span class=\"n\">{comments}</span></a><a href=\"/d/{id}/history\">이력</a>{responses}\
             <a class=\"opt\" href=\"/d/{id}/raw\" target=\"_blank\" rel=\"noopener\">새 창</a></nav></header>\
             <main class=\"vmain\" id=\"main\"><iframe id=\"doc\" src=\"/d/{id}/raw\" title=\"{title}\" \
             sandbox=\"allow-scripts allow-popups allow-popups-to-escape-sandbox allow-modals allow-forms allow-downloads\" \
             allow=\"fullscreen; clipboard-write\"></iframe></main></div>{}",
            crate::survey::confirm_dialog(&user.name)
        ),
    )
    .into_response()
}

/// 문서 원본. 앱 틀의 iframe 이 부르고, "새 창" 으로 따로 열 수도 있다. 격리 헤더는 그대로다.
pub async fn view_raw(State(app): State<AppState>, jar: PrivateCookieJar, Path(id): Path<String>) -> Response {
    if let Err(r) = user_and_doc(&app, &jar, &id) {
        return r;
    }
    match app.docs.read(&id) {
        Ok(bytes) => document_response(bytes, crate::survey::BRIDGE),
        Err(e) => server_error("문서 파일 읽기", e),
    }
}

pub async fn delete(State(app): State<AppState>, jar: PrivateCookieJar, Path(id): Path<String>) -> Response {
    let user = match current_user(&app, &jar) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let doc = match app.docs.get(&id) {
        Ok(Some(d)) => d,
        Ok(None) => return not_found(),
        Err(e) => return server_error("문서 조회", e),
    };
    if !can_modify(&user, &doc) {
        return (StatusCode::FORBIDDEN, page("올린 사람이나 관리자만 삭제할 수 있습니다.")).into_response();
    }
    match app.docs.hide(&id) {
        Ok(()) => Redirect::to("/").into_response(),
        Err(e) => server_error("문서 삭제", e),
    }
}

fn admin_only() -> Response {
    (StatusCode::FORBIDDEN, page("관리자만 할 수 있습니다. <a href=\"/\">목록으로</a>")).into_response()
}

/// 숨긴 문서 목록 (정책서 D4-1: 살리는 일은 관리자만).
pub async fn hidden_list(State(app): State<AppState>, jar: PrivateCookieJar) -> Response {
    let user = match current_user(&app, &jar) {
        Ok(u) if u.is_admin() => u,
        Ok(_) => return admin_only(),
        Err(r) => return r,
    };
    let docs = match app.docs.hidden() {
        Ok(d) => d,
        Err(e) => return server_error("숨긴 문서 조회", e),
    };
    let rows: String = docs
        .iter()
        .map(|d| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td class=\"when\">{}</td><td class=\"actions\">\
                 <form method=\"post\" action=\"/d/{id}/unhide\"><button class=\"link\">살리기</button></form>\
                 <form method=\"post\" action=\"/d/{id}/purge\" onsubmit=\"return confirm('이 문서의 모든 버전·댓글·응답을 완전히 지웁니다. 되돌릴 수 없습니다. 계속할까요?')\">\
                 <button class=\"link danger\">완전 삭제</button></form></td></tr>",
                escape(&d.title),
                if d.project.is_empty() { "<span class=\"tag none\">미분류</span>".into() } else { format!("<span class=\"tag\">{}</span>", escape(&d.project)) },
                escape(&d.uploader_name),
                when(d.created_at),
                id = escape(&d.id),
            )
        })
        .collect();
    let body = if docs.is_empty() {
        "<p class=\"empty\">숨긴 문서가 없습니다.</p>".to_owned()
    } else {
        format!(
            "<div class=\"table-wrap narrow\"><table><thead><tr><th>제목</th><th>프로젝트</th><th>올린 사람</th><th>올린 시각</th>\
             <th><span class=\"visually-hidden\">할 일</span></th></tr></thead><tbody>{rows}</tbody></table></div>"
        )
    };
    shell(
        &app,
        &user,
        "/admin/hidden",
        &format!(
            "<div class=\"head\"><div><h1>숨긴 문서</h1>\
             <p class=\"lede\">목록에서 지운 문서입니다. 살리면 목록과 검색에 다시 나오고 링크도 열립니다. \
             완전 삭제는 실수로 올린 민감 자료용으로, 모든 버전·댓글·응답을 지우고 다음 백업 때 백업 폴더의 파일도 지웁니다 (DB 백업에는 14일 동안 남습니다).</p></div></div>{body}"
        ),
    )
    .into_response()
}

pub async fn unhide(State(app): State<AppState>, jar: PrivateCookieJar, Path(id): Path<String>) -> Response {
    match current_user(&app, &jar) {
        Ok(u) if u.is_admin() => {}
        Ok(_) => return admin_only(),
        Err(r) => return r,
    }
    match app.docs.unhide(&id) {
        Ok(true) => Redirect::to("/admin/hidden").into_response(),
        Ok(false) => not_found(),
        Err(e) => server_error("문서 살리기", e),
    }
}

/// 숨긴 문서를 완전히 지운다. 관리자만.
pub async fn purge(State(app): State<AppState>, jar: PrivateCookieJar, Path(id): Path<String>) -> Response {
    let user = match current_user(&app, &jar) {
        Ok(u) if u.is_admin() => u,
        Ok(_) => return admin_only(),
        Err(r) => return r,
    };
    match app.docs.purge(&id) {
        Ok(true) => {
            log!("문서 완전 삭제: {id} (관리자 {})", user.email);
            Redirect::to("/admin/hidden").into_response()
        }
        Ok(false) => (StatusCode::NOT_FOUND, page("숨긴 문서만 완전히 지울 수 있습니다. 먼저 삭제(숨기기)하세요. <a href=\"/admin/hidden\">숨긴 문서</a>")).into_response(),
        Err(e) => server_error("문서 완전 삭제", e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::users::User;
    use axum::http::HeaderValue;

    fn user(email: &str, name: &str) -> User {
        User { email: email.into(), name: name.into(), role: "member".into() }
    }

    /// 예전 시험들이 쓰던 (검색어, 프로젝트) 목록 조회. 프로젝트가 비면 전체.
    fn ls(d: &Docs, q: &str, p: &str, offset: i64) -> Result<Vec<Doc>> {
        d.list(&Filter { q, project: (!p.is_empty()).then_some(p), uploader: "", sort: Sort::Recent }, offset)
    }

    #[test]
    fn 내_문서_미분류_정렬로_목록을_거른다() {
        let (d, dir) = docs();
        let (a, b) = (user("a@x.im", "가나"), user("b@x.im", "다라"));
        let x = d.insert("나 제목", "A", &a, b"x").unwrap();
        let y = d.insert("가 제목", "", &b, b"y").unwrap();
        let z = d.insert("다 제목", "A", &a, b"z").unwrap();
        for _ in 0..3 {
            d.record_view(&x).unwrap();
        }
        d.record_view(&y).unwrap();
        d.add_comment(&y, &a, "1").unwrap();
        d.add_comment(&y, &a, "2").unwrap();
        d.add_comment(&z, &a, "3").unwrap();
        let titles = |f: Filter| d.list(&f, 0).unwrap().into_iter().map(|x| x.title).collect::<Vec<_>>();
        let f = |project, uploader, sort| Filter { q: "", project, uploader, sort };
        assert_eq!(titles(f(None, "a@x.im", Sort::Recent)), ["다 제목", "나 제목"], "내 문서만");
        assert_eq!(titles(f(Some(""), "", Sort::Recent)), ["가 제목"], "미분류만");
        assert_eq!(titles(f(None, "", Sort::Views)), ["나 제목", "가 제목", "다 제목"], "7일 열람 많은 순");
        assert_eq!(titles(f(None, "", Sort::Comments)), ["가 제목", "다 제목", "나 제목"], "댓글 많은 순");
        assert_eq!(titles(f(None, "", Sort::Title)), ["가 제목", "나 제목", "다 제목"], "제목 순");
        assert_eq!(Sort::parse("views"), Sort::Views);
        assert_eq!(Sort::parse("엉뚱한 값"), Sort::Recent);
        assert_eq!(d.project_counts().unwrap(), [("A".to_owned(), 2), ("".to_owned(), 1)], "미분류는 맨 뒤");
        let daily = d.daily_views().unwrap();
        assert_eq!(daily.len(), 7);
        assert_eq!(daily.iter().map(|(_, n)| n).sum::<i64>(), 4);
        assert_eq!(daily[6].1, 4, "오늘 칸에 쌓인다");
        assert!(daily.windows(2).all(|w| w[1].0 - w[0].0 == 86400), "하루씩");
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn docs() -> (Docs, PathBuf) {
        let dir = std::env::temp_dir().join(format!("html-hub-test-{}", CsrfToken::new_random().secret()));
        (Docs::open(":memory:", dir.to_str().unwrap()).unwrap(), dir)
    }

    #[test]
    fn 올린_문서를_번호로_읽는다() {
        let (d, dir) = docs();
        let id = d.insert("보고서", "A", &user("a@x.im", "가나"), b"<p>hi</p>").unwrap();
        assert_eq!(d.get(&id).unwrap().unwrap().title, "보고서");
        assert_eq!(d.read(&id).unwrap(), b"<p>hi</p>");
        assert!(id.len() >= 22, "번호가 너무 짧아 추측될 수 있다");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 제목과_올린_사람으로_찾고_프로젝트로_거른다() {
        let (d, dir) = docs();
        d.insert("월간 보고", "A", &user("a@x.im", "가나"), b"1").unwrap();
        d.insert("설계 메모", "B", &user("b@x.im", "다라"), b"2").unwrap();
        assert_eq!(ls(&d, "", "", 0).unwrap().len(), 2);
        assert_eq!(ls(&d, "보고", "", 0).unwrap()[0].title, "월간 보고");
        assert_eq!(ls(&d, "다라", "", 0).unwrap()[0].title, "설계 메모");
        assert_eq!(ls(&d, "", "A", 0).unwrap().len(), 1);
        assert_eq!(ls(&d, "%", "", 0).unwrap().len(), 0, "% 는 특수문자가 아니라 글자로 찾는다");
        assert_eq!(d.projects().unwrap(), ["A", "B"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 최신_문서가_먼저_나온다() {
        let (d, dir) = docs();
        d.insert("첫째", "", &user("a@x.im", "가나"), b"1").unwrap();
        d.insert("둘째", "", &user("a@x.im", "가나"), b"2").unwrap();
        assert_eq!(ls(&d, "", "", 0).unwrap()[0].title, "둘째");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 숨긴_문서는_조회와_목록에서_빠지지만_파일은_남는다() {
        let (d, dir) = docs();
        let id = d.insert("지울 것", "A", &user("a@x.im", "가나"), b"x").unwrap();
        d.hide(&id).unwrap();
        assert!(d.get(&id).unwrap().is_none());
        assert!(ls(&d, "", "", 0).unwrap().is_empty());
        assert!(d.projects().unwrap().is_empty());
        assert_eq!(d.read(&id).unwrap(), b"x");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 새_버전을_올려도_이전_버전이_남고_현재_버전이_바뀐다() {
        let (d, dir) = docs();
        let (a, b) = (user("a@x.im", "가나"), user("b@x.im", "다라"));
        let id = d.insert("문서", "", &a, b"v1").unwrap();
        assert_eq!(d.replace(&id, &b, b"v2").unwrap(), 2);
        assert_eq!(d.read(&id).unwrap(), b"v2");
        assert_eq!(d.read_version(&id, 1).unwrap().unwrap(), b"v1");
        let v = d.versions(&id).unwrap();
        assert_eq!((v[0].n, v[0].by_name.as_str(), v[1].n), (2, "다라", 1));
        assert!(d.read_version(&id, 9).unwrap().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 되돌리면_옛_내용이_새_버전으로_저장되고_중간_버전은_남는다() {
        let (d, dir) = docs();
        let a = user("a@x.im", "가나");
        let id = d.insert("문서", "", &a, b"v1").unwrap();
        d.replace(&id, &a, b"v2").unwrap();
        assert_eq!(d.restore(&id, 1, &a).unwrap(), Some(3));
        assert_eq!(d.read(&id).unwrap(), b"v1");
        assert_eq!(d.read_version(&id, 2).unwrap().unwrap(), b"v2");
        assert_eq!(d.versions(&id).unwrap().len(), 3);
        assert_eq!(d.restore(&id, 9, &a).unwrap(), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 버전_기능_이전에_올린_문서는_1번_버전으로_채운다() {
        let dir = std::env::temp_dir().join(format!("html-hub-test-{}", CsrfToken::new_random().secret()));
        let db = dir.join("h.db");
        std::fs::create_dir_all(&dir).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE documents (id TEXT PRIMARY KEY, title TEXT NOT NULL, project TEXT NOT NULL,
               uploader_email TEXT NOT NULL, uploader_name TEXT NOT NULL, created_at INTEGER NOT NULL, deleted_at INTEGER);
             INSERT INTO documents VALUES ('old1', '옛 문서', '', 'a@x.im', '가나', 100, NULL);",
        )
        .unwrap();
        drop(conn);
        std::fs::write(dir.join("old1.html"), b"legacy").unwrap();
        // 옛 파일이 있는 폴더를 파일 폴더로 쓴다.
        let d = Docs::open(db.to_str().unwrap(), dir.to_str().unwrap()).unwrap();
        assert_eq!(d.read("old1").unwrap(), b"legacy");
        assert_eq!(d.versions("old1").unwrap().len(), 1);
        let a = user("a@x.im", "가나");
        assert_eq!(d.replace("old1", &a, b"new").unwrap(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 보이는_글자만_뽑는다() {
        let t = visible_text("<html><head><title>제목</title><style>.a{color:red}</style></head><body><!-- 숨김 -->\
            <h1 class=\"x\">안녕&nbsp;하세요</h1><script>var secret = 'zzz';</script><p>a &lt; b &amp; c</p>3 < 4</body></html>");
        assert_eq!(t, "제목 안녕 하세요 a < b & c 3 < 4");
        assert!(!t.contains("secret") && !t.contains("color") && !t.contains("숨김"));
        assert_eq!(visible_text("<SCRIPT>x</SCRIPT>끝"), "끝");
        assert_eq!(visible_text("열린 <b"), "열린");
    }

    #[test]
    fn 본문으로_찾고_교체_되돌리기_숨기기를_따른다() {
        let (d, dir) = docs();
        let a = user("a@x.im", "가나");
        let id = d.insert("보고", "", &a, "<p>분기 매출 요약</p><script>토큰</script>".as_bytes()).unwrap();
        assert_eq!(ls(&d, "매출", "", 0).unwrap().len(), 1);
        assert_eq!(ls(&d, "토큰", "", 0).unwrap().len(), 0, "script 안의 글자는 찾지 않는다");
        d.replace(&id, &a, "<p>새 내용</p>".as_bytes()).unwrap();
        assert_eq!(ls(&d, "매출", "", 0).unwrap().len(), 0, "이전 버전 본문은 색인하지 않는다");
        assert_eq!(ls(&d, "새 내용", "", 0).unwrap().len(), 1);
        d.restore(&id, 1, &a).unwrap();
        assert_eq!(ls(&d, "매출", "", 0).unwrap().len(), 1);
        d.hide(&id).unwrap();
        assert_eq!(ls(&d, "매출", "", 0).unwrap().len(), 0, "숨긴 문서는 본문으로도 안 나온다");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 댓글은_오래된_것부터_나오고_지우면_숨겨진다() {
        let (d, dir) = docs();
        let (a, b) = (user("a@x.im", "가나"), user("b@x.im", "다라"));
        let id = d.insert("문서", "", &a, b"x").unwrap();
        d.add_comment(&id, &a, "첫 댓글").unwrap();
        d.add_comment(&id, &b, "둘째").unwrap();
        let list = d.comments(&id).unwrap();
        assert_eq!((list[0].body.as_str(), list[1].by_name.as_str()), ("첫 댓글", "다라"));
        d.hide_comment(list[0].id).unwrap();
        assert_eq!(d.comments(&id).unwrap().len(), 1);
        assert!(d.comment(&id, list[0].id).unwrap().is_none());
        assert!(d.comment("다른문서", list[1].id).unwrap().is_none(), "다른 문서의 댓글 번호로는 못 찾는다");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 목록은_쪽마다_나뉘고_다음_쪽이_있는지_한_개_더_알려준다() {
        let (d, dir) = docs();
        let a = user("a@x.im", "가나");
        for i in 0..PAGE_SIZE + 3 {
            d.insert(&format!("문서{i}"), "", &a, b"x").unwrap();
        }
        let first = ls(&d, "", "", 0).unwrap();
        assert_eq!(first.len() as i64, PAGE_SIZE + 1, "다음 쪽이 있으면 한 개 더 온다");
        assert_eq!(first[0].title, format!("문서{}", PAGE_SIZE + 2));
        let second = ls(&d, "", "", PAGE_SIZE).unwrap();
        assert_eq!(second.len(), 3);
        assert_eq!(second[2].title, "문서0");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 숨긴_문서만_따로_보이고_살리면_목록으로_돌아온다() {
        let (d, dir) = docs();
        let a = user("a@x.im", "가나");
        let id = d.insert("되살릴 것", "A", &a, "<p>본문글</p>".as_bytes()).unwrap();
        d.insert("그대로", "A", &a, b"x").unwrap();
        assert!(d.hidden().unwrap().is_empty());
        d.hide(&id).unwrap();
        assert_eq!(d.hidden().unwrap()[0].title, "되살릴 것");
        assert!(d.unhide(&id).unwrap());
        assert!(d.hidden().unwrap().is_empty());
        assert!(d.get(&id).unwrap().is_some());
        assert_eq!(ls(&d, "본문글", "", 0).unwrap().len(), 1, "본문 검색에도 다시 걸린다");
        assert!(!d.unhide(&id).unwrap(), "숨기지 않은 문서는 바뀌지 않는다");
        assert!(!d.unhide("없는번호").unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 시각은_가까우면_상대로_멀면_날짜로_보인다() {
        let now = 1_795_000_000; // 2026-11-18 20:06 (한국 시각)
        assert_eq!(relative(now - 10, now), "방금");
        assert_eq!(relative(now - 5 * 60, now), "5분 전");
        assert_eq!(relative(now - 3 * 3600, now), "3시간 전");
        assert_eq!(relative(now - 21 * 3600, now), "어제", "날짜가 바뀌었으면 시간이 아니라 어제");
        assert_eq!(relative(now - 2 * 86400, now), "11월 16일", "그 전은 날짜");
        assert_eq!(relative(now - 30 * 86400, now), "10월 19일");
        assert_eq!(relative(now - 400 * 86400, now), "2025년 10월 14일");
        assert_eq!(relative(now + 30, now), "방금", "서버 시계가 조금 어긋나도 미래로 보이지 않는다");
        let html = when(now);
        assert!(html.starts_with("<time datetime=\"") && html.contains(&format!("title=\"{}\"", show_time(now))));
    }

    #[test]
    fn 대시보드_숫자와_활동을_숨긴_것_빼고_센다() {
        let (d, dir) = docs();
        let (a, b) = (user("a@x.im", "가나"), user("b@x.im", "다라"));
        let x = d.insert("많이 본", "", &a, b"x").unwrap();
        let y = d.insert("조금 본", "", &b, b"y").unwrap();
        let z = d.insert("숨길 것", "", &a, b"z").unwrap();
        for _ in 0..3 {
            d.record_view(&x).unwrap();
        }
        d.record_view(&y).unwrap();
        d.record_view(&z).unwrap();
        // 8일 전 열람은 지난 7일에 들지 않는다.
        d.db.lock().unwrap().execute("INSERT INTO views (doc_id, at) VALUES (?1, ?2)", params![y, now_unix() - 8 * 86400]).unwrap();
        d.add_comment(&y, &a, "첫 댓글").unwrap();
        d.add_comment(&z, &a, "숨긴 문서 댓글").unwrap();
        d.add_comment(&x, &b, "최근 댓글").unwrap();
        d.hide(&z).unwrap();

        let sum = d.summary("a@x.im").unwrap();
        assert_eq!((sum.total, sum.viewed, sum.mine, sum.hidden, sum.comments), (2, 2, 1, 1, 2), "숨긴 문서의 댓글은 세지 않는다");
        let top = d.top_viewed(5).unwrap();
        assert_eq!(top.iter().map(|t| (t.1.as_str(), t.2)).collect::<Vec<_>>(), [("많이 본", 3), ("조금 본", 1)]);
        let recent = d.recent_comments(5).unwrap();
        assert_eq!(recent.iter().map(|c| c.body.as_str()).collect::<Vec<_>>(), ["최근 댓글", "첫 댓글"]);
        assert_eq!(recent[0].doc_title, "많이 본");
        let act = d.activity(&[x.clone(), y.clone()]).unwrap();
        assert_eq!((act[&x], act[&y]), ((3, 1), (1, 1)));
        assert!(d.activity(&[]).unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 본문에서_찾은_검색어_주변을_발췌한다() {
        let (d, dir) = docs();
        let a = user("a@x.im", "가나");
        let long = format!("<p>{}분기 매출 요약{}</p>", "앞".repeat(100), "뒤".repeat(100));
        let id = d.insert("보고", "", &a, long.as_bytes()).unwrap();
        let e = d.excerpt(&id, "매출").unwrap().unwrap();
        assert!(e.contains("분기 매출 요약") && e.starts_with('…') && e.ends_with('…'), "앞뒤가 잘렸으면 … 를 붙인다");
        assert!(e.chars().count() <= 122, "발췌는 짧게");
        let start = d.insert("짧은", "", &a, "<p>Revenue 맨 앞</p>".as_bytes()).unwrap();
        assert_eq!(d.excerpt(&start, "revenue").unwrap().unwrap(), "Revenue 맨 앞", "대소문자 무시, 잘리지 않으면 … 없음");
        assert_eq!(d.excerpt(&id, "없는말").unwrap(), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 열람을_문서번호와_시각만으로_기록한다() {
        let (d, dir) = docs();
        let id = d.insert("문서", "", &user("a@x.im", "가나"), b"x").unwrap();
        d.record_view(&id).unwrap();
        d.record_view(&id).unwrap();
        let conn = d.db.lock().unwrap();
        let (rows, docs): (i64, i64) = conn
            .query_row("SELECT COUNT(*), COUNT(DISTINCT doc_id) FROM views", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!((rows, docs), (2, 1));
        let cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('views')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(cols, ["doc_id", "at"], "누가 봤는지는 남기지 않는다");
        drop(conn);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 문서_응답은_격리_헤더를_붙인다() {
        let res = document_response(b"<script>1</script>".to_vec(), "");
        let h = res.headers();
        let csp = h[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
        assert!(csp.starts_with("sandbox allow-scripts"));
        for allowed in ["allow-popups", "allow-modals", "allow-forms", "allow-downloads"] {
            assert!(csp.contains(allowed), "{allowed}");
        }
        assert!(!csp.contains("allow-same-origin") && !csp.contains("allow-top-navigation"), "격리는 그대로");
        assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], HeaderValue::from_static("nosniff"));
    }

    #[test]
    fn 저장소_대체_코드는_doctype_뒤에_들어간다() {
        let out = String::from_utf8(with_storage_shim(b"<!DOCTYPE html>\n<p>a</p>".to_vec(), "")).unwrap();
        assert!(out.starts_with("<!DOCTYPE html><script>") && out.ends_with("\n<p>a</p>"), "doctype 이 맨 앞이어야 표준 모드로 그려진다");
        let out = String::from_utf8(with_storage_shim("\u{feff}  <!doctype html><p>b</p>".as_bytes().to_vec(), "")).unwrap();
        assert!(out.starts_with("\u{feff}  <!doctype html><script>") && out.ends_with("<p>b</p>"));
        let out = String::from_utf8(with_storage_shim(b"<p>c</p>".to_vec(), "")).unwrap();
        assert!(out.starts_with("<script>") && out.ends_with("</script><p>c</p>"), "doctype 이 없으면 맨 앞");
        let out = with_storage_shim(b"<!doctype".to_vec(), "");
        assert!(out.ends_with(b"<!doctype"), "닫히지 않은 doctype 은 건드리지 않고 앞에 붙인다");
    }
}
