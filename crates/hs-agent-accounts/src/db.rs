use crate::model::*;
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    path::Path,
    sync::{Mutex, PoisonError},
};

pub struct Database(Mutex<Connection>);
fn error(_: impl std::fmt::Display) -> Failure {
    Failure::new("database_failure", "Local database operation failed")
}
impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path).map_err(error)?;
        db.busy_timeout(std::time::Duration::from_secs(3)).map_err(error)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")
            .map_err(error)?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(error)?;
        if version > 1 {
            return Err(Failure::new(
                "schema_version",
                "Database schema is newer than this application",
            ));
        }
        if version == 0 {
            // Owner-authorized, transactional SQLite schema migration.
            db.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE profiles(id TEXT PRIMARY KEY, data TEXT NOT NULL);
                CREATE TABLE history(id INTEGER PRIMARY KEY, profile_id TEXT NOT NULL REFERENCES profiles(id) ON DELETE CASCADE, observed_at INTEGER NOT NULL, data TEXT NOT NULL);
                CREATE INDEX history_profile ON history(profile_id, observed_at);
                CREATE TABLE runtimes(provider TEXT PRIMARY KEY, data TEXT NOT NULL);
                CREATE TABLE operations(id TEXT PRIMARY KEY, summary TEXT NOT NULL);
                PRAGMA user_version=1; COMMIT;").map_err(error)?;
        }
        Ok(Self(Mutex::new(db)))
    }
    pub fn ready(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .query_row("SELECT COUNT(*) FROM profiles", [], |r| r.get::<_, i64>(0))
            .is_ok()
    }
    pub fn profiles(&self) -> Result<Vec<Profile>> {
        let db = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let mut q = db.prepare("SELECT data FROM profiles ORDER BY id").map_err(error)?;
        let rows = q.query_map([], |r| r.get::<_, String>(0)).map_err(error)?;
        rows.map(|s| serde_json::from_str(&s.map_err(error)?).map_err(error))
            .collect()
    }
    pub fn profile(&self, id: &str) -> Result<Profile> {
        let s: Option<String> = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .query_row("SELECT data FROM profiles WHERE id=?", [id], |r| r.get(0))
            .optional()
            .map_err(error)?;
        serde_json::from_str(&s.ok_or_else(|| Failure::new("not_found", "Profile not found"))?).map_err(error)
    }
    pub fn save(&self, p: &Profile, snapshot: Option<&Snapshot>) -> Result<()> {
        let mut db = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let tx = db.transaction().map_err(error)?;
        tx.execute(
            "INSERT INTO profiles(id,data) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET data=excluded.data",
            params![p.id, serde_json::to_string(p).map_err(error)?],
        )
        .map_err(error)?;
        if let Some(s) = snapshot {
            tx.execute(
                "INSERT INTO history(profile_id,observed_at,data) VALUES(?1,?2,?3)",
                params![p.id, s.observed_at, serde_json::to_string(s).map_err(error)?],
            )
            .map_err(error)?;
        }
        tx.commit().map_err(error)
    }
    pub fn delete(&self, id: &str) -> Result<()> {
        let mut db = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let tx = db.transaction().map_err(error)?;
        tx.execute(
            "DELETE FROM operations WHERE json_extract(summary,'$.profile_id')=?",
            [id],
        )
        .map_err(error)?;
        tx.execute("DELETE FROM profiles WHERE id=?", [id]).map_err(error)?;
        tx.commit().map_err(error)?;
        Ok(())
    }
    pub fn history(&self, id: &str) -> Result<Vec<Snapshot>> {
        let db = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let mut q = db
            .prepare("SELECT data FROM history WHERE profile_id=? ORDER BY id DESC LIMIT 100")
            .map_err(error)?;
        q.query_map([id], |r| r.get::<_, String>(0))
            .map_err(error)?
            .map(|s| serde_json::from_str(&s.map_err(error)?).map_err(error))
            .collect()
    }
    pub fn runtime(&self, provider: &str) -> Result<Option<Runtime>> {
        let s: Option<String> = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .query_row("SELECT data FROM runtimes WHERE provider=?", [provider], |r| r.get(0))
            .optional()
            .map_err(error)?;
        s.map(|s| serde_json::from_str(&s).map_err(error)).transpose()
    }
    pub fn save_runtime(&self, r: &Runtime) -> Result<()> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).execute("INSERT INTO runtimes(provider,data) VALUES(?1,?2) ON CONFLICT(provider) DO UPDATE SET data=excluded.data", params![r.provider,serde_json::to_string(r).map_err(error)?]).map_err(error)?;
        Ok(())
    }
    pub fn operation(&self, op: &Operation) -> Result<()> {
        // Authorization URLs/codes are transient and never persisted.
        let mut safe = serde_json::to_value(op).map_err(error)?;
        safe["authorization_url"] = serde_json::Value::Null;
        safe["user_code"] = serde_json::Value::Null;
        let db = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        db.execute("INSERT OR REPLACE INTO operations(id,summary) SELECT ?1,?2 WHERE ?3 IS NULL OR EXISTS(SELECT 1 FROM profiles WHERE id=?3)", params![op.id,safe.to_string(),op.profile_id]).map_err(error)?;
        Ok(())
    }
    pub fn read_operation(&self, id: &str) -> Result<Operation> {
        let s: Option<String> = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .query_row("SELECT summary FROM operations WHERE id=?", [id], |r| r.get(0))
            .optional()
            .map_err(error)?;
        serde_json::from_str(&s.ok_or_else(|| Failure::new("not_found", "Operation not found"))?).map_err(error)
    }
    pub fn interrupt_operations(&self) -> Result<()> {
        let db = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let mut q = db
            .prepare("SELECT summary FROM operations WHERE json_extract(summary,'$.finished_at') IS NULL")
            .map_err(error)?;
        let rows = q
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(error)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(error)?;
        drop(q);
        for row in rows {
            let mut op: Operation = serde_json::from_str(&row).map_err(error)?;
            op.status = "interrupted".into();
            op.finished_at = Some(now());
            op.message = "Service restarted; unverified candidates were not activated; inspect the active pin".into();
            op.error = Some(Failure::new("interrupted", "Operation interrupted by service restart"));
            db.execute(
                "UPDATE operations SET summary=?2 WHERE id=?1",
                params![op.id, serde_json::to_string(&op).map_err(error)?],
            )
            .map_err(error)?;
            if op.kind == "upgrade" {
                let data: Option<String> = db
                    .query_row("SELECT data FROM runtimes WHERE provider=?", [&op.provider], |r| {
                        r.get(0)
                    })
                    .optional()
                    .map_err(error)?;
                if let Some(data) = data {
                    let mut runtime: Runtime = serde_json::from_str(&data).map_err(error)?;
                    runtime.last_error = op.error.clone();
                    runtime.last_result = Some(op.message.clone());
                    db.execute(
                        "UPDATE runtimes SET data=?2 WHERE provider=?1",
                        params![runtime.provider, serde_json::to_string(&runtime).map_err(error)?],
                    )
                    .map_err(error)?;
                }
            }
        }
        Ok(())
    }

    pub fn activate_runtime(&self, runtime: &Runtime, profile: &Profile, snapshot: &Snapshot) -> Result<()> {
        let mut p = profile.clone();
        p.snapshot = Some(snapshot.clone());
        p.account = Some(snapshot.account.clone());
        p.last_success = Some(snapshot.observed_at);
        p.last_attempt = Some(now());
        p.updated_at = now();
        p.error = None;
        p.failures = 0;
        let mut db = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let tx = db.transaction().map_err(error)?;
        tx.execute(
            "UPDATE runtimes SET data=?2 WHERE provider=?1",
            params![runtime.provider, serde_json::to_string(runtime).map_err(error)?],
        )
        .map_err(error)?;
        tx.execute(
            "UPDATE profiles SET data=?2 WHERE id=?1",
            params![p.id, serde_json::to_string(&p).map_err(error)?],
        )
        .map_err(error)?;
        tx.execute(
            "INSERT INTO history(profile_id,observed_at,data) VALUES(?1,?2,?3)",
            params![
                p.id,
                snapshot.observed_at,
                serde_json::to_string(snapshot).map_err(error)?
            ],
        )
        .map_err(error)?;
        tx.commit().map_err(error)
    }
}
