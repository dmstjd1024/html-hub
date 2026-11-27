use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct Users(Arc<Mutex<Connection>>);

pub struct User {
    pub email: String,
    pub name: String,
    pub role: String,
}

impl User {
    pub fn is_admin(&self) -> bool {
        self.role == "admin"
    }
}

// 구글 관리 콘솔 "사용자 다운로드" JSON 형식 그대로다.
#[derive(Deserialize)]
struct Export {
    users: Vec<ExportUser>,
}

#[derive(Deserialize)]
struct ExportUser {
    #[serde(rename = "First Name [Required]")]
    first: String,
    #[serde(rename = "Last Name [Required]")]
    last: String,
    #[serde(rename = "Email Address [Required]")]
    email: String,
    #[serde(rename = "Status [READ ONLY]")]
    status: String,
}

impl Users {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        // 같은 DB 를 docs·tokens 도 따로 연다. 쓰는 순간 바로 실패하지 않게 기다린다.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS users (
                email TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                department TEXT,
                role TEXT NOT NULL DEFAULT 'member',
                active INTEGER NOT NULL
            )",
        )?;
        Ok(Self(Arc::new(Mutex::new(conn))))
    }

    /// 내보내기 JSON 을 반영하고 반영한 사람 수를 돌려준다.
    /// 소속·역할은 건드리지 않고, 파일에 없는 사람은 그대로 둔다.
    pub fn import(&self, json: &str) -> Result<usize> {
        let export: Export = serde_json::from_str(json)?;
        let conn = self.0.lock().unwrap();
        let mut stmt = conn.prepare(
            "INSERT INTO users (email, name, active) VALUES (?1, ?2, ?3)
             ON CONFLICT(email) DO UPDATE SET name = excluded.name, active = excluded.active",
        )?;
        for u in &export.users {
            // 한국어 이름은 성+이름 순서로 붙인다.
            let name = format!("{}{}", u.last.trim(), u.first.trim());
            stmt.execute(params![
                u.email.trim().to_lowercase(),
                name,
                u.status == "Active"
            ])?;
        }
        Ok(export.users.len())
    }

    /// 내보내기 파일에 없는 활성 계정을 비활성화하고 그 이메일들을 돌려준다 (구글에서 지운 퇴사자는 파일에 안 나온다).
    /// 파일 밖에서 손으로 넣은 계정도 꺼지므로 `import-users --deactivate-missing` 으로만 부른다.
    pub fn deactivate_missing(&self, json: &str) -> Result<Vec<String>> {
        let export: Export = serde_json::from_str(json)?;
        let keep: Vec<String> = export.users.iter().map(|u| u.email.trim().to_lowercase()).collect();
        let conn = self.0.lock().unwrap();
        let mut stmt = conn.prepare("SELECT email FROM users WHERE active = 1 ORDER BY email")?;
        let gone: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|e| !keep.contains(e))
            .collect();
        for e in &gone {
            conn.execute("UPDATE users SET active = 0 WHERE email = ?1", [e])?;
        }
        Ok(gone)
    }

    /// 한 사람의 역할을 바꾸거나(admin·member) 끈다(off). 없는 사람은 이름을 주면 새로 넣는다.
    pub fn set(&self, email: &str, role: &str, name: Option<&str>) -> Result<()> {
        let email = email.trim().to_lowercase();
        let conn = self.0.lock().unwrap();
        let changed = match role {
            "off" => conn.execute("UPDATE users SET active = 0 WHERE email = ?1", [&email])?,
            "admin" | "member" => match name {
                Some(n) => conn.execute(
                    "INSERT INTO users (email, name, role, active) VALUES (?1, ?2, ?3, 1)
                     ON CONFLICT(email) DO UPDATE SET name = excluded.name, role = excluded.role, active = 1",
                    params![email, n, role],
                )?,
                None => conn.execute("UPDATE users SET role = ?2, active = 1 WHERE email = ?1", params![email, role])?,
            },
            _ => anyhow::bail!("역할은 admin, member, off 중 하나다"),
        };
        anyhow::ensure!(changed == 1, "{email} 이 명단에 없다. 새로 넣으려면 이름을 함께 준다");
        Ok(())
    }

    /// 등록되어 있고 활성인 사람만 돌려준다.
    pub fn find_active(&self, email: &str) -> Result<Option<User>> {
        let conn = self.0.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT email, name, role FROM users WHERE email = ?1 AND active = 1",
                [email.trim().to_lowercase()],
                |row| {
                    Ok(User {
                        email: row.get(0)?,
                        name: row.get(1)?,
                        role: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 실제 직원이 아닌 가짜 데이터다.
    const SAMPLE: &str = r#"{"users":[
        {"First Name [Required]":"길동","Last Name [Required]":"홍","Email Address [Required]":"Hong@Example.com","Status [READ ONLY]":"Active"},
        {"First Name [Required]":"철수","Last Name [Required]":"김","Email Address [Required]":"kim@example.com","Status [READ ONLY]":"Suspended"}
    ]}"#;

    fn db() -> Users {
        Users::open(":memory:").unwrap()
    }

    #[test]
    fn active_user_is_found_case_insensitively() {
        let users = db();
        assert_eq!(users.import(SAMPLE).unwrap(), 2);
        let u = users.find_active("HONG@example.com").unwrap().unwrap();
        assert_eq!(u.name, "홍길동");
        assert_eq!(u.email, "hong@example.com");
    }

    #[test]
    fn suspended_and_unknown_users_are_rejected() {
        let users = db();
        users.import(SAMPLE).unwrap();
        assert!(users.find_active("kim@example.com").unwrap().is_none());
        assert!(users.find_active("nobody@example.com").unwrap().is_none());
    }

    #[test]
    fn 파일에_없는_계정을_끄고_역할을_바꾼다() {
        let users = db();
        users.import(SAMPLE).unwrap();
        users.set("Me@Gmail.com", "admin", Some("나")).unwrap();
        assert!(users.find_active("me@gmail.com").unwrap().unwrap().is_admin());
        assert_eq!(users.deactivate_missing(SAMPLE).unwrap(), ["me@gmail.com"]);
        assert!(users.find_active("me@gmail.com").unwrap().is_none());
        assert!(users.find_active("hong@example.com").unwrap().is_some(), "파일에 있는 사람은 그대로");
        users.set("me@gmail.com", "member", None).unwrap();
        assert!(!users.find_active("me@gmail.com").unwrap().unwrap().is_admin());
        users.set("hong@example.com", "off", None).unwrap();
        assert!(users.find_active("hong@example.com").unwrap().is_none());
        assert!(users.set("nobody@x.com", "member", None).is_err(), "없는 사람은 이름이 필요하다");
        assert!(users.set("hong@example.com", "boss", None).is_err());
    }

    #[test]
    fn reimport_keeps_department_and_role() {
        let users = db();
        users.import(SAMPLE).unwrap();
        users
            .0
            .lock()
            .unwrap()
            .execute(
                "UPDATE users SET department = 'IT', role = 'admin' WHERE email = 'hong@example.com'",
                [],
            )
            .unwrap();
        users.import(SAMPLE).unwrap();
        let (dept, role): (String, String) = users
            .0
            .lock()
            .unwrap()
            .query_row(
                "SELECT department, role FROM users WHERE email = 'hong@example.com'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((dept.as_str(), role.as_str()), ("IT", "admin"));
    }
}
