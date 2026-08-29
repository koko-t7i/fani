use crate::model::{LangOutcome, Published, TaskOutcome};
use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

const MIGRATIONS: &[(i64, &str)] = &[
    (
        1,
        r#"
        CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS runs (
            id TEXT PRIMARY KEY,
            config_path TEXT NOT NULL,
            started_at TEXT NOT NULL,
            finished_at TEXT,
            status TEXT,
            exit_code INTEGER
        );
        CREATE TABLE IF NOT EXISTS language_runs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            repo TEXT NOT NULL,
            lang TEXT NOT NULL,
            status TEXT NOT NULL,
            skill_run_id TEXT NOT NULL,
            message TEXT NOT NULL,
            written_json TEXT NOT NULL,
            conflicts_json TEXT NOT NULL,
            findings_json TEXT NOT NULL,
            published_json TEXT NOT NULL,
            repair_rounds INTEGER NOT NULL,
            remaining_tasks INTEGER NOT NULL,
            fuzzy_matched INTEGER NOT NULL,
            duration_ms INTEGER NOT NULL,
            transitions_json TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS agent_calls (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            repo TEXT NOT NULL,
            lang TEXT NOT NULL,
            stage TEXT NOT NULL,
            task_id TEXT NOT NULL,
            agent TEXT NOT NULL,
            ok INTEGER NOT NULL,
            code TEXT,
            attempts INTEGER NOT NULL,
            duration_ms INTEGER NOT NULL,
            message TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_agent_calls_run ON agent_calls(run_id, repo, lang);
        CREATE TABLE IF NOT EXISTS repo_locks (
            repo TEXT PRIMARY KEY,
            pid INTEGER NOT NULL,
            started_at INTEGER NOT NULL
        );
    "#,
    ),
    (
        2,
        r#"
        CREATE TABLE IF NOT EXISTS legacy_imports (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            source_path TEXT NOT NULL,
            kind TEXT NOT NULL,
            sha256 TEXT NOT NULL,
            content_json TEXT NOT NULL,
            imported_at TEXT NOT NULL,
            UNIQUE(source_path, sha256)
        );
        CREATE INDEX IF NOT EXISTS idx_legacy_imports_kind ON legacy_imports(kind);
    "#,
    ),
];

#[derive(Clone, Debug)]
pub struct Database {
    path: PathBuf,
}

impl Database {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let db = Self { path: path.into() };
        if let Some(parent) = db.path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("cannot create database directory {}", parent.display())
            })?;
        }
        db.migrate()?;
        Ok(db)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn connect(&self) -> Result<Connection> {
        let conn = Connection::open(&self.path)
            .with_context(|| format!("cannot open SQLite database {}", self.path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        Ok(conn)
    }

    pub fn migrate(&self) -> Result<()> {
        let conn = self.connect()?;
        conn.pragma_update(None, "journal_mode", "DELETE")?;
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);")?;
            for (version, sql) in MIGRATIONS {
                let applied: Option<i64> = conn
                    .query_row(
                        "SELECT version FROM schema_migrations WHERE version = ?1",
                        [version],
                        |row| row.get(0),
                    )
                    .optional()?;
                if applied.is_none() {
                    conn.execute_batch(sql)?;
                    conn.execute(
                        "INSERT INTO schema_migrations(version, applied_at) VALUES (?1, ?2)",
                        params![version, Utc::now().to_rfc3339()],
                    )?;
                }
            }
            Ok(())
        })();
        match result {
            Ok(()) => conn.execute_batch("COMMIT")?,
            Err(err) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(err);
            }
        }
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self.connect()?.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |r| r.get(0),
        )?)
    }

    pub fn sqlite_version(&self) -> Result<String> {
        Ok(self
            .connect()?
            .query_row("SELECT sqlite_version()", [], |r| r.get(0))?)
    }

    pub fn start_run(&self, config_path: &Path) -> Result<String> {
        let id = format!(
            "{}-{}",
            Utc::now().format("%Y%m%dT%H%M%S%.9fZ"),
            std::process::id()
        );
        self.connect()?.execute(
            "INSERT INTO runs(id, config_path, started_at) VALUES (?1, ?2, ?3)",
            params![
                id,
                config_path.display().to_string(),
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(id)
    }

    pub fn finish_run(&self, run_id: &str, status: &str, exit_code: i32) -> Result<()> {
        self.connect()?.execute(
            "UPDATE runs SET finished_at=?2, status=?3, exit_code=?4 WHERE id=?1",
            params![run_id, Utc::now().to_rfc3339(), status, exit_code],
        )?;
        Ok(())
    }

    pub fn recover_incomplete_runs(&self, current_run_id: &str) -> Result<usize> {
        Ok(self.connect()?.execute(
            "UPDATE runs SET finished_at=?2, status='error', exit_code=2 WHERE finished_at IS NULL AND id<>?1",
            params![current_run_id, Utc::now().to_rfc3339()],
        )?)
    }

    pub fn pending_push(&self, repo: &str, lang: &str) -> Result<Option<Published>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare(
            "SELECT published_json FROM language_runs WHERE repo=?1 AND lang=?2 ORDER BY id DESC",
        )?;
        let rows = statement.query_map(params![repo, lang], |row| row.get::<_, String>(0))?;
        for row in rows {
            let published: Published = serde_json::from_str(&row?)?;
            if published.commit.is_empty() {
                continue;
            }
            return Ok((!published.pushed).then_some(published));
        }
        Ok(None)
    }

    pub fn record_language(&self, run_id: &str, out: &LangOutcome) -> Result<()> {
        self.connect()?.execute(
            r#"INSERT INTO language_runs(
                run_id, repo, lang, status, skill_run_id, message, written_json,
                conflicts_json, findings_json, published_json, repair_rounds,
                remaining_tasks, fuzzy_matched, duration_ms, transitions_json
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)"#,
            params![
                run_id,
                out.repo,
                out.lang,
                out.status.as_str(),
                out.run_id,
                out.message,
                serde_json::to_string(&out.written)?,
                serde_json::to_string(&out.conflicts)?,
                serde_json::to_string(&out.findings)?,
                serde_json::to_string(&out.published)?,
                out.repair_rounds as i64,
                out.remaining_tasks as i64,
                out.fuzzy_matched as i64,
                (out.duration_s * 1000.0) as i64,
                serde_json::to_string(&out.transitions)?,
            ],
        )?;
        Ok(())
    }

    pub fn record_agent_calls(
        &self,
        run_id: &str,
        repo: &str,
        lang: &str,
        stage: &str,
        agent: &str,
        outcomes: &[TaskOutcome],
    ) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        insert_agent_calls(&tx, run_id, repo, lang, stage, agent, outcomes)?;
        tx.commit()?;
        Ok(())
    }

    pub fn import_legacy_json(&self, path: &Path, kind: &str) -> Result<bool> {
        if !path.is_file() {
            return Ok(false);
        }
        let content = fs::read_to_string(path)
            .with_context(|| format!("cannot read legacy state {}", path.display()))?;
        let value: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("legacy state is not valid JSON: {}", path.display()))?;
        let canonical = serde_json::to_string(&value)?;
        let digest = format!("{:x}", Sha256::digest(content.as_bytes()));
        let changed = self.connect()?.execute(
            "INSERT OR IGNORE INTO legacy_imports(source_path, kind, sha256, content_json, imported_at) VALUES (?1,?2,?3,?4,?5)",
            params![path.display().to_string(), kind, digest, canonical, Utc::now().to_rfc3339()],
        )?;
        Ok(changed == 1)
    }

    pub fn import_legacy_jsonl(&self, path: &Path, kind: &str) -> Result<usize> {
        if !path.is_file() {
            return Ok(0);
        }
        let bytes = fs::read(path)?;
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let mut imported = 0;
        for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let parsed = std::str::from_utf8(line)
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok());
            let (value, row_kind) = match parsed {
                Some(value) => (value, kind.to_string()),
                None => (
                    serde_json::json!({
                        "invalid_jsonl": String::from_utf8_lossy(line),
                        "raw_hex": line.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
                        "line_number": index + 1,
                    }),
                    format!("{kind}-invalid"),
                ),
            };
            let canonical = serde_json::to_string(&value)?;
            let digest = format!("{:x}", Sha256::digest(line));
            imported += tx.execute(
                "INSERT OR IGNORE INTO legacy_imports(source_path, kind, sha256, content_json, imported_at) VALUES (?1,?2,?3,?4,?5)",
                params![format!("{}#{}", path.display(), index + 1), row_kind, digest, canonical, Utc::now().to_rfc3339()],
            )?;
        }
        tx.commit()?;
        Ok(imported)
    }

    pub fn legacy_count(&self) -> Result<i64> {
        Ok(self
            .connect()?
            .query_row("SELECT COUNT(*) FROM legacy_imports", [], |r| r.get(0))?)
    }
}

fn insert_agent_calls(
    tx: &Transaction<'_>,
    run_id: &str,
    repo: &str,
    lang: &str,
    stage: &str,
    agent: &str,
    outcomes: &[TaskOutcome],
) -> Result<()> {
    let mut stmt = tx.prepare(
        r#"INSERT INTO agent_calls(
        run_id, repo, lang, stage, task_id, agent, ok, code, attempts,
        duration_ms, message, created_at
    ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)"#,
    )?;
    for out in outcomes {
        stmt.execute(params![
            run_id,
            repo,
            lang,
            stage,
            out.task_id,
            agent,
            out.ok as i64,
            out.code,
            out.attempts as i64,
            (out.duration_s * 1000.0) as i64,
            out.message,
            Utc::now().to_rfc3339(),
        ])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn connection_setup_honors_busy_timeout_before_pragmas() {
        let tmp = tempdir().unwrap();
        let db = Database::open(tmp.path().join("fani.db")).unwrap();
        let blocker = db.connect().unwrap();
        blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let waiting = db.clone();
        let handle = std::thread::spawn(move || waiting.schema_version());
        std::thread::sleep(std::time::Duration::from_millis(50));
        blocker.execute_batch("ROLLBACK").unwrap();
        assert_eq!(handle.join().unwrap().unwrap(), 2);
    }

    #[test]
    fn migrations_are_repeatable_and_persist() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("fani.db");
        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 2);
        let conn = db.connect().unwrap();
        let journal: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        let synchronous: i64 = conn
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .unwrap();
        let foreign_keys: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(journal.to_ascii_lowercase(), "delete");
        assert_eq!(synchronous, 2);
        assert_eq!(foreign_keys, 1);
        drop(conn);
        drop(db);
        let reopened = Database::open(&path).unwrap();
        assert_eq!(reopened.schema_version().unwrap(), 2);
    }

    #[test]
    fn migration_from_v1_preserves_existing_data() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("fani.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATIONS[0].1).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations(version, applied_at) VALUES (1, ?1)",
            ["2026-01-01T00:00:00Z"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO runs(id, config_path, started_at, finished_at, status, exit_code) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                "existing-run",
                "fani.toml",
                "2026-01-01T00:00:00Z",
                "2026-01-01T00:00:01Z",
                "partial",
                3
            ],
        )
        .unwrap();
        conn.execute(
            r#"INSERT INTO language_runs(
                run_id, repo, lang, status, skill_run_id, message, written_json,
                conflicts_json, findings_json, published_json, repair_rounds,
                remaining_tasks, fuzzy_matched, duration_ms, transitions_json
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)"#,
            params![
                "existing-run",
                "docs",
                "zh-CN",
                "partial",
                "skill-run-1",
                "preserved",
                r#"["docs/zh-CN/guide.md"]"#,
                "[]",
                r#"[{"severity":"warning"}]"#,
                "[]",
                1,
                2,
                3,
                450,
                r#"["plan","dispatch","verify"]"#
            ],
        )
        .unwrap();
        conn.execute(
            r#"INSERT INTO agent_calls(
                run_id, repo, lang, stage, task_id, agent, ok, code, attempts,
                duration_ms, message, created_at
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)"#,
            params![
                "existing-run",
                "docs",
                "zh-CN",
                "dispatch",
                "task-1",
                "fixture-agent",
                1,
                Option::<String>::None,
                1,
                120,
                "done",
                "2026-01-01T00:00:00Z"
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO repo_locks(repo, pid, started_at) VALUES (?1, ?2, ?3)",
            params!["docs", 4242, 1_767_225_600_i64],
        )
        .unwrap();
        drop(conn);

        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 2);
        let conn = db.connect().unwrap();
        let run: (String, String, i64) = conn
            .query_row(
                "SELECT config_path, status, exit_code FROM runs WHERE id='existing-run'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(run, ("fani.toml".into(), "partial".into(), 3));
        let language: (String, String, i64, i64) = conn
            .query_row(
                "SELECT message, written_json, repair_rounds, remaining_tasks FROM language_runs WHERE run_id='existing-run'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            language,
            (
                "preserved".into(),
                r#"["docs/zh-CN/guide.md"]"#.into(),
                1,
                2
            )
        );
        let agent: (String, String, i64) = conn
            .query_row(
                "SELECT task_id, message, duration_ms FROM agent_calls WHERE run_id='existing-run'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(agent, ("task-1".into(), "done".into(), 120));
        let lock: (i64, i64) = conn
            .query_row(
                "SELECT pid, started_at FROM repo_locks WHERE repo='docs'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(lock, (4242, 1_767_225_600));
        conn.execute(
            "INSERT INTO legacy_imports(source_path, kind, sha256, content_json, imported_at) VALUES (?1,?2,?3,?4,?5)",
            params!["state.json", "state", "abc", "{}", "2026-01-01T00:00:02Z"],
        )
        .unwrap();
        drop(conn);
        drop(db);

        let reopened = Database::open(&path).unwrap();
        assert_eq!(reopened.schema_version().unwrap(), 2);
        let conn = reopened.connect().unwrap();
        let versions: Vec<i64> = conn
            .prepare("SELECT version FROM schema_migrations ORDER BY version")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(versions, vec![1, 2]);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM runs", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM legacy_imports", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn imports_legacy_json_once_without_losing_content() {
        let tmp = tempdir().unwrap();
        let old = tmp.path().join("state.json");
        fs::write(&old, r#"{"files":{"docs/a.md":{"hash":"abc"}}}"#).unwrap();
        let db = Database::open(tmp.path().join("fani.db")).unwrap();
        assert!(db.import_legacy_json(&old, "external-skill-state").unwrap());
        assert!(!db.import_legacy_json(&old, "external-skill-state").unwrap());
        assert_eq!(db.legacy_count().unwrap(), 1);
        let content: String = db
            .connect()
            .unwrap()
            .query_row("SELECT content_json FROM legacy_imports", [], |r| r.get(0))
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(value["files"]["docs/a.md"]["hash"], "abc");
    }

    #[test]
    fn imports_legacy_jsonl_idempotently() {
        let tmp = tempdir().unwrap();
        let old = tmp.path().join("dispatch.jsonl");
        fs::write(
            &old,
            "{\"task_id\":\"a\",\"ok\":true}\n{\"task_id\":\"b\",\"ok\":false}\n",
        )
        .unwrap();
        let db = Database::open(tmp.path().join("fani.db")).unwrap();
        assert_eq!(
            db.import_legacy_jsonl(&old, "python-dispatch-record")
                .unwrap(),
            2
        );
        assert_eq!(
            db.import_legacy_jsonl(&old, "python-dispatch-record")
                .unwrap(),
            0
        );
        assert_eq!(db.legacy_count().unwrap(), 2);
    }

    #[test]
    fn imports_valid_jsonl_records_around_a_truncated_line() {
        let tmp = tempdir().unwrap();
        let old = tmp.path().join("dispatch.jsonl");
        fs::write(
            &old,
            "{\"task_id\":\"a\",\"ok\":true}\n{\"task_id\":\n{\"task_id\":\"b\",\"ok\":false}\n",
        )
        .unwrap();
        let db = Database::open(tmp.path().join("fani.db")).unwrap();
        assert_eq!(
            db.import_legacy_jsonl(&old, "python-dispatch-record")
                .unwrap(),
            3
        );
        assert_eq!(
            db.import_legacy_jsonl(&old, "python-dispatch-record")
                .unwrap(),
            0
        );
        let conn = db.connect().unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM legacy_imports WHERE kind='python-dispatch-record'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            2
        );
        let invalid: String = conn
            .query_row(
                "SELECT content_json FROM legacy_imports WHERE kind='python-dispatch-record-invalid'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let invalid: serde_json::Value = serde_json::from_str(&invalid).unwrap();
        assert_eq!(invalid["invalid_jsonl"], "{\"task_id\":");
        assert_eq!(invalid["line_number"], 2);
    }

    #[test]
    fn imports_valid_jsonl_records_around_invalid_utf8() {
        let tmp = tempdir().unwrap();
        let old = tmp.path().join("dispatch.jsonl");
        let mut bytes = b"{\"task_id\":\"a\",\"ok\":true}\n{\"text\":\"".to_vec();
        bytes.push(0xe4);
        bytes.extend_from_slice(b"\n{\"task_id\":\"b\",\"ok\":false}\n");
        fs::write(&old, bytes).unwrap();
        let db = Database::open(tmp.path().join("fani.db")).unwrap();
        assert_eq!(
            db.import_legacy_jsonl(&old, "python-dispatch-record")
                .unwrap(),
            3
        );
        let conn = db.connect().unwrap();
        let invalid: String = conn
            .query_row(
                "SELECT content_json FROM legacy_imports WHERE kind='python-dispatch-record-invalid'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let invalid: serde_json::Value = serde_json::from_str(&invalid).unwrap();
        assert!(invalid["raw_hex"].as_str().unwrap().ends_with("e4"));
    }

    #[test]
    fn recovers_previous_incomplete_run_without_touching_current_run() {
        let tmp = tempdir().unwrap();
        let db = Database::open(tmp.path().join("fani.db")).unwrap();
        let previous = db.start_run(Path::new("old.toml")).unwrap();
        let current = db.start_run(Path::new("fani.toml")).unwrap();
        assert_eq!(db.recover_incomplete_runs(&current).unwrap(), 1);
        let conn = db.connect().unwrap();
        let previous_row: (String, i64, bool) = conn
            .query_row(
                "SELECT status, exit_code, finished_at IS NOT NULL FROM runs WHERE id=?1",
                [previous],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(previous_row, ("error".into(), 2, true));
        let current_finished: bool = conn
            .query_row(
                "SELECT finished_at IS NOT NULL FROM runs WHERE id=?1",
                [current],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!current_finished);
    }

    #[test]
    fn pending_push_tracks_only_the_latest_publication_attempt() {
        let tmp = tempdir().unwrap();
        let db = Database::open(tmp.path().join("fani.db")).unwrap();
        let run = db.start_run(Path::new("fani.toml")).unwrap();
        let mut failed = LangOutcome::new(Path::new("repo"), "zh-CN");
        failed.status = crate::model::Status::NeedsHuman;
        failed.published = Published {
            branch: "i18n/zh-CN".into(),
            commit: "abc123".into(),
            error: "could not push".into(),
            ..Published::default()
        };
        db.record_language(&run, &failed).unwrap();
        assert_eq!(
            db.pending_push("repo", "zh-CN").unwrap().unwrap().commit,
            "abc123"
        );

        let mut unrelated = LangOutcome::new(Path::new("repo"), "zh-CN");
        unrelated.status = crate::model::Status::Error;
        db.record_language(&run, &unrelated).unwrap();
        assert!(db.pending_push("repo", "zh-CN").unwrap().is_some());

        let mut local_only = LangOutcome::new(Path::new("repo"), "zh-CN");
        local_only.published = Published {
            branch: "i18n/zh-CN".into(),
            commit: "def456".into(),
            ..Published::default()
        };
        db.record_language(&run, &local_only).unwrap();
        assert_eq!(
            db.pending_push("repo", "zh-CN").unwrap().unwrap().commit,
            "def456"
        );

        let mut pushed = LangOutcome::new(Path::new("repo"), "zh-CN");
        pushed.published = Published {
            branch: "i18n/zh-CN".into(),
            commit: "def456".into(),
            pushed: true,
            ..Published::default()
        };
        db.record_language(&run, &pushed).unwrap();
        assert!(db.pending_push("repo", "zh-CN").unwrap().is_none());
    }

    #[test]
    fn run_history_survives_restart() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("fani.db");
        let db = Database::open(&path).unwrap();
        let id = db.start_run(Path::new("fani.toml")).unwrap();
        db.finish_run(&id, "ok", 0).unwrap();
        drop(db);
        let conn = Database::open(path).unwrap().connect().unwrap();
        let row: (String, i64) = conn
            .query_row(
                "SELECT status, exit_code FROM runs WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, ("ok".into(), 0));
    }
}
