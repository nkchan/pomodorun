use crate::model::*;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

pub struct Database {
    pub conn: Connection,
}
impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(3))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        let version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > 1 {
            return Err(AppError::new(
                "migration",
                "新しいバージョンで作成されたデータです。アプリを更新してください。",
                false,
            ));
        }
        if version == 0 {
            conn.execute_batch("BEGIN IMMEDIATE;
            CREATE TABLE tasks (id TEXT PRIMARY KEY, title TEXT NOT NULL, scheduled_date TEXT NOT NULL, estimated_pomodoros INTEGER NOT NULL, completed_at INTEGER, archived_at INTEGER, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
            CREATE TABLE sessions (id TEXT PRIMARY KEY, task_id TEXT REFERENCES tasks(id), phase TEXT NOT NULL, outcome TEXT NOT NULL, payload TEXT NOT NULL);
            CREATE INDEX sessions_task ON sessions(task_id,phase,outcome);
            CREATE TABLE brain_dumps (id TEXT PRIMARY KEY, text TEXT NOT NULL, captured_at INTEGER NOT NULL, session_id TEXT, converted_task_id TEXT REFERENCES tasks(id), archived_at INTEGER);
            CREATE TABLE timer_state (id INTEGER PRIMARY KEY CHECK(id=1), payload TEXT NOT NULL);
            CREATE TABLE settings (id INTEGER PRIMARY KEY CHECK(id=1), payload TEXT NOT NULL);
            CREATE TABLE slack_connection (id INTEGER PRIMARY KEY CHECK(id=1), team_id TEXT NOT NULL, user_id TEXT NOT NULL, credential_ref TEXT NOT NULL, last_verified_at INTEGER NOT NULL);
            CREATE TABLE slack_guard_journal (id INTEGER PRIMARY KEY CHECK(id=1), payload TEXT NOT NULL);
            CREATE TABLE entitlement_cache (id INTEGER PRIMARY KEY CHECK(id=1), tier TEXT NOT NULL, source TEXT NOT NULL, expires_at INTEGER, last_verified_at INTEGER);
            INSERT INTO entitlement_cache(id,tier,source) VALUES(1,'free','mvp');
            PRAGMA user_version=1; COMMIT;")?;
        }
        Ok(Self { conn })
    }
    pub fn load(&self) -> Result<(Settings, Timer)> {
        let settings: Settings = self.load_json("settings")?.unwrap_or_default();
        settings.validate()?;
        let timer = self.load_json("timer_state")?.unwrap_or_default();
        Ok((settings, timer))
    }
    pub fn load_json<T: serde::de::DeserializeOwned>(&self, table: &str) -> Result<Option<T>> {
        // Table names are internal constants, never IPC values.
        let s: Option<String> = self
            .conn
            .query_row(
                &format!("SELECT payload FROM {table} WHERE id=1"),
                [],
                |r| r.get(0),
            )
            .optional()?;
        s.map(|s| serde_json::from_str(&s).map_err(Into::into))
            .transpose()
    }
    pub fn save_json<T: serde::Serialize>(&self, table: &str, value: &T) -> Result<()> {
        self.conn.execute(&format!("INSERT INTO {table}(id,payload) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload"), [serde_json::to_string(value)?])?;
        Ok(())
    }
    pub fn commit_timer(&mut self, timer: &Timer, completed: &[Session]) -> Result<()> {
        let tx = self.conn.transaction()?;
        for s in completed.iter().chain(timer.session.iter()) {
            tx.execute("INSERT INTO sessions(id,task_id,phase,outcome,payload) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET outcome=excluded.outcome,payload=excluded.payload", params![s.id,s.task_id,serde_json::to_value(s.phase)?.as_str().unwrap(),s.outcome,serde_json::to_string(s)?])?;
        }
        tx.execute("INSERT INTO timer_state(id,payload) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload", [serde_json::to_string(timer)?])?;
        tx.commit()?;
        Ok(())
    }
    pub fn tasks(&self, date: &str) -> Result<Vec<Task>> {
        validate_date(date)?;
        let mut stmt = self.conn.prepare("SELECT t.id,t.title,t.scheduled_date,t.estimated_pomodoros,t.completed_at,t.archived_at,t.created_at,t.updated_at,(SELECT COUNT(*) FROM sessions s WHERE s.task_id=t.id AND s.phase='focus' AND s.outcome='completed') FROM tasks t WHERE t.archived_at IS NULL AND (t.scheduled_date=?1 OR (t.scheduled_date<?1 AND t.completed_at IS NULL)) ORDER BY t.completed_at IS NOT NULL,t.scheduled_date,t.created_at")?;
        let rows = stmt.query_map([date], |r| {
            Ok(Task {
                id: r.get(0)?,
                title: r.get(1)?,
                scheduled_date: r.get(2)?,
                estimated_pomodoros: r.get(3)?,
                completed_at: r.get(4)?,
                archived_at: r.get(5)?,
                created_at: r.get(6)?,
                updated_at: r.get(7)?,
                completed_pomodoros: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
    pub fn task(&self, id: &str) -> Result<Task> {
        self.conn.query_row("SELECT id,title,scheduled_date,estimated_pomodoros,completed_at,archived_at,created_at,updated_at FROM tasks WHERE id=?1 AND archived_at IS NULL",[id],|r| Ok(Task{id:r.get(0)?,title:r.get(1)?,scheduled_date:r.get(2)?,estimated_pomodoros:r.get(3)?,completed_at:r.get(4)?,archived_at:r.get(5)?,created_at:r.get(6)?,updated_at:r.get(7)?,completed_pomodoros:0})).optional()?.ok_or_else(|| AppError::invalid("タスクが見つかりません。"))
    }
    pub fn create_task(&self, title: &str, date: &str, estimate: u32, now: i64) -> Result<String> {
        let title = validate_text(title, 200)?;
        validate_date(date)?;
        if !(1..=99).contains(&estimate) {
            return Err(AppError::invalid("見積もりは1〜99回で指定してください。"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.conn.execute("INSERT INTO tasks(id,title,scheduled_date,estimated_pomodoros,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?5)",params![id,title,date,estimate,now])?;
        Ok(id)
    }
    pub fn update_task(&self, task: &Task) -> Result<()> {
        let title = validate_text(&task.title, 200)?;
        validate_date(&task.scheduled_date)?;
        if !(1..=99).contains(&task.estimated_pomodoros) {
            return Err(AppError::invalid("見積もりは1〜99回で指定してください。"));
        }
        self.task(&task.id)?;
        self.conn.execute("UPDATE tasks SET title=?2,scheduled_date=?3,estimated_pomodoros=?4,completed_at=?5,updated_at=?6 WHERE id=?1",params![task.id,title,task.scheduled_date,task.estimated_pomodoros,task.completed_at,crate::clock::utc_ms()])?;
        Ok(())
    }
    pub fn archive_task(&self, id: &str, now: i64) -> Result<()> {
        self.task(id)?;
        self.conn.execute(
            "UPDATE tasks SET archived_at=?2,updated_at=?2 WHERE id=?1",
            params![id, now],
        )?;
        Ok(())
    }
    pub fn add_dump(&self, text: &str, session: Option<&str>, now: i64) -> Result<String> {
        let text = validate_text(text, 4000)?;
        let id = uuid::Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO brain_dumps(id,text,captured_at,session_id) VALUES(?1,?2,?3,?4)",
            params![id, text, now, session],
        )?;
        Ok(id)
    }
    pub fn dumps(&self) -> Result<Vec<BrainDump>> {
        let mut stmt=self.conn.prepare("SELECT id,text,captured_at,session_id,converted_task_id,archived_at FROM brain_dumps WHERE archived_at IS NULL AND converted_task_id IS NULL ORDER BY captured_at DESC")?;
        let rows = stmt.query_map([], |r| {
            Ok(BrainDump {
                id: r.get(0)?,
                text: r.get(1)?,
                captured_at: r.get(2)?,
                session_id: r.get(3)?,
                converted_task_id: r.get(4)?,
                archived_at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
    pub fn archive_dump(&self, id: &str, now: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE brain_dumps SET archived_at=?2 WHERE id=?1",
            params![id, now],
        )?;
        Ok(())
    }
    pub fn convert_dump(&mut self, id: &str, date: &str, now: i64) -> Result<String> {
        validate_date(date)?;
        let tx = self.conn.transaction()?;
        let (text,converted):(String,Option<String>)=tx.query_row("SELECT text,converted_task_id FROM brain_dumps WHERE id=?1 AND archived_at IS NULL",[id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?.ok_or_else(||AppError::invalid("メモが見つかりません。"))?;
        if let Some(id) = converted {
            return Ok(id);
        }
        let task_id = uuid::Uuid::new_v4().to_string();
        let title: String = text
            .lines()
            .next()
            .unwrap_or(&text)
            .chars()
            .take(200)
            .collect();
        tx.execute("INSERT INTO tasks(id,title,scheduled_date,estimated_pomodoros,created_at,updated_at) VALUES(?1,?2,?3,1,?4,?4)",params![task_id,title,date,now])?;
        tx.execute(
            "UPDATE brain_dumps SET converted_task_id=?2 WHERE id=?1",
            params![id, task_id],
        )?;
        tx.commit()?;
        Ok(task_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn newer_schema_is_rejected_without_modification() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA user_version=99;").unwrap();
        drop(conn);
        assert!(matches!(Database::open(&path), Err(e) if e.code == "migration"));
        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            99
        );
    }
    #[test]
    fn carryover_respects_completion_and_local_date_boundary() {
        let db = Database::open(Path::new(":memory:")).unwrap();
        let id = db.create_task("yesterday", "2026-10-03", 2, 0).unwrap();
        db.create_task("tomorrow", "2026-10-05", 1, 0).unwrap();
        assert_eq!(db.tasks("2026-10-04").unwrap().len(), 1);
        let mut task = db.task(&id).unwrap();
        task.completed_at = Some(100);
        db.update_task(&task).unwrap();
        assert!(db.tasks("2026-10-04").unwrap().is_empty());
        assert_eq!(db.tasks("2026-10-03").unwrap().len(), 1);
    }
    #[test]
    fn migration_reopen_and_interrupted_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("app.db");
        let mut db = Database::open(&p).unwrap();
        let (s, mut t) = db.load().unwrap();
        t.start(None, &s, 0, 0).unwrap();
        db.commit_timer(&t, &[]).unwrap();
        drop(db);
        let mut db = Database::open(&p).unwrap();
        let (s, mut t) = db.load().unwrap();
        let done = t.recover(&s, 90_000).unwrap();
        db.commit_timer(&t, &[done]).unwrap();
        assert_eq!(db.load().unwrap().1.status, Status::Idle);
    }
    #[test]
    fn history_survives_archive_and_completion_is_idempotent() {
        let mut db = Database::open(Path::new(":memory:")).unwrap();
        let id = db.create_task("test", "2026-10-03", 1, 0).unwrap();
        let task = db.task(&id).unwrap();
        let mut t = Timer::default();
        let s = Settings::default();
        t.start(Some(&task), &s, 0, 0).unwrap();
        let done = t.advance(&s, 1_500_000, 1_500_000);
        db.commit_timer(&t, &done).unwrap();
        db.commit_timer(&t, &done).unwrap();
        assert_eq!(db.tasks("2026-10-04").unwrap()[0].completed_pomodoros, 1);
        db.archive_task(&id, 10).unwrap();
        assert!(db.tasks("2026-10-04").unwrap().is_empty());
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE outcome='completed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }
    #[test]
    fn conversion_atomic_and_idempotent() {
        let mut db = Database::open(Path::new(":memory:")).unwrap();
        let id = db.add_dump("Idea\nDetails", None, 0).unwrap();
        let task = db.convert_dump(&id, "2026-10-04", 0).unwrap();
        assert_eq!(db.convert_dump(&id, "2026-10-04", 0).unwrap(), task);
        assert!(db.dumps().unwrap().is_empty());
        assert_eq!(db.tasks("2026-10-04").unwrap().len(), 1);
    }
    #[test]
    fn failed_transaction_does_not_persist_timer_or_session() {
        let mut db = Database::open(Path::new(":memory:")).unwrap();
        db.conn.execute_batch("CREATE TRIGGER fail BEFORE INSERT ON timer_state BEGIN SELECT RAISE(ABORT,'full'); END;").unwrap();
        let mut t = Timer::default();
        t.start(None, &Settings::default(), 0, 0).unwrap();
        assert!(db.commit_timer(&t, &[]).is_err());
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}
