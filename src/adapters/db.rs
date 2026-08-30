use crate::adapters::process::process_identity;
use crate::application::ports::{
    AttemptCandidateInput, AttemptInput, AttemptReceipt, CanonicalFile, CanonicalFileInput,
    FindingInput, OutboxEntry, OutboxKind, PullRequestStateInput, RecoveredAttempt, StateStore,
    StoredPullRequest, TrustTranslationInput, UnitHistory,
};
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const APPLICATION_ID: i64 = 0x4641_4e49;
const SCHEMA_VERSION: i64 = 1;
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);
static ID_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "0001_native_authority",
    sql: include_str!("../../migrations/0001_native_authority.sql"),
}];

const MIGRATION_LEDGER_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY CHECK (version > 0),
    name TEXT NOT NULL UNIQUE,
    checksum TEXT NOT NULL CHECK (length(checksum) = 64),
    applied_at INTEGER NOT NULL
) STRICT;
"#;

fn migration_checksum(sql: &str) -> String {
    format!("{:x}", Sha256::digest(sql.as_bytes()))
}

fn configure_connection(conn: &Connection) -> Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "journal_mode", "DELETE")?;
    Ok(())
}

fn open_connection(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)
        .with_context(|| format!("cannot open SQLite database {}", path.display()))?;
    configure_connection(&conn)?;
    Ok(conn)
}

fn application_id(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA application_id", [], |row| row.get(0))?)
}

fn user_table_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?)
}

fn has_table(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |row| row.get(0),
    )?)
}

fn validate_migration_list(migrations: &[Migration]) -> Result<()> {
    for (index, migration) in migrations.iter().enumerate() {
        let expected = index as i64 + 1;
        if migration.version != expected {
            bail!(
                "migration {} has version {}; expected contiguous version {expected}",
                migration.name,
                migration.version
            );
        }
        if !migration
            .name
            .starts_with(&format!("{:04}_", migration.version))
        {
            bail!(
                "migration name {:?} must start with {:04}_",
                migration.name,
                migration.version
            );
        }
    }
    Ok(())
}

fn apply_migrations(conn: &mut Connection, migrations: &[Migration]) -> Result<()> {
    validate_migration_list(migrations)?;
    let app_id = application_id(conn)?;
    let tables = user_table_count(conn)?;
    if app_id == 0 {
        if tables != 0 {
            bail!(
                "unsupported pre-native SQLite database; reset it explicitly to initialize the native schema"
            );
        }
    } else if app_id != APPLICATION_ID {
        bail!(
            "SQLite application_id {app_id:#x} does not identify a fani database ({APPLICATION_ID:#x})"
        );
    } else if tables != 0 && !has_table(conn, "schema_migrations")? {
        bail!("fani database is missing the schema_migrations ledger; reset it explicitly");
    }

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if app_id == 0 {
        tx.pragma_update(None, "application_id", APPLICATION_ID)?;
    }
    tx.execute_batch(MIGRATION_LEDGER_SQL)?;

    let applied = {
        let mut statement =
            tx.prepare("SELECT version,name,checksum FROM schema_migrations ORDER BY version")?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };

    for (index, (version, name, checksum)) in applied.iter().enumerate() {
        let expected_version = index as i64 + 1;
        if *version != expected_version {
            bail!(
                "database migration history is not contiguous: found version {version}; expected {expected_version}"
            );
        }
        let migration = migrations
            .get(index)
            .filter(|migration| migration.version == *version)
            .ok_or_else(|| anyhow!("database contains unknown migration {version} ({name})"))?;
        let expected_checksum = migration_checksum(migration.sql);
        if name != migration.name || checksum != &expected_checksum {
            bail!(
                "migration {version} checksum/name mismatch: database has {name} {checksum}, binary expects {} {expected_checksum}",
                migration.name
            );
        }
    }

    for (offset, migration) in migrations.iter().skip(applied.len()).enumerate() {
        let expected_version = applied.len() as i64 + offset as i64 + 1;
        if migration.version != expected_version {
            bail!("database migration history is not contiguous at version {expected_version}");
        }
        tx.execute_batch(migration.sql)
            .with_context(|| format!("migration {} failed", migration.name))?;
        tx.execute(
            "INSERT INTO schema_migrations(version,name,checksum,applied_at) VALUES (?1,?2,?3,?4)",
            params![
                migration.version,
                migration.name,
                migration_checksum(migration.sql),
                Utc::now().timestamp_millis()
            ],
        )?;
        tx.pragma_update(None, "user_version", migration.version)?;
    }

    let latest = migrations.last().map_or(0, |migration| migration.version);
    let user_version: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if user_version != latest {
        bail!("SQLite user_version is {user_version}; expected migration version {latest}");
    }
    tx.commit()?;
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Database {
    path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Lease {
    pub resource_type: String,
    pub resource_key: String,
    pub owner: String,
    pub fencing_token: i64,
    pub acquired_at: i64,
    pub expires_at: i64,
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
        let conn = open_connection(&self.path)?;
        let actual = application_id(&conn)?;
        if actual != APPLICATION_ID {
            bail!(
                "SQLite application_id {actual:#x} does not identify a fani database ({APPLICATION_ID:#x})"
            );
        }
        Ok(conn)
    }

    pub fn snapshot(source: &Path, destination: &Path) -> Result<()> {
        if destination.exists() {
            fs::remove_file(destination).with_context(|| {
                format!("cannot replace database snapshot {}", destination.display())
            })?;
        }
        let conn = Connection::open_with_flags(
            source,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("cannot open SQLite database {} read-only", source.display()))?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let actual = application_id(&conn)?;
        if actual != APPLICATION_ID {
            bail!(
                "cannot snapshot SQLite application_id {actual:#x}; expected fani {APPLICATION_ID:#x}"
            );
        }
        conn.execute("VACUUM INTO ?1", [destination.to_string_lossy().as_ref()])
            .with_context(|| {
                format!(
                    "cannot snapshot database {} to {}",
                    source.display(),
                    destination.display()
                )
            })?;
        Ok(())
    }

    pub fn migrate(&self) -> Result<()> {
        let mut conn = open_connection(&self.path)?;
        apply_migrations(&mut conn, MIGRATIONS)?;
        let marker: Option<(String, i64)> = conn
            .query_row(
                "SELECT generation,version FROM state_schema WHERE singleton=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match marker {
            Some((generation, SCHEMA_VERSION)) if generation == "native-authoritative" => Ok(()),
            Some((generation, version)) => bail!(
                "unsupported database schema {generation} version {version}; reset it explicitly"
            ),
            None => bail!("database has an invalid state_schema marker"),
        }
    }

    pub fn reset(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if path.exists() {
            let conn = open_connection(&path)?;
            let app_id = application_id(&conn)?;
            let tables = user_table_count(&conn)?;
            if app_id != APPLICATION_ID && !(app_id == 0 && tables == 0) {
                bail!(
                    "refusing to reset SQLite database with application_id {app_id:#x}; expected {APPLICATION_ID:#x}"
                );
            }
            drop(conn);
            for candidate in [
                path.clone(),
                PathBuf::from(format!("{}-wal", path.display())),
                PathBuf::from(format!("{}-shm", path.display())),
            ] {
                match fs::remove_file(&candidate) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("cannot reset fani database {}", candidate.display())
                        });
                    }
                }
            }
        }
        Self::open(path)
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self.connect()?.query_row(
            "SELECT version FROM state_schema WHERE singleton=1",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn sqlite_version(&self) -> Result<String> {
        Ok(self
            .connect()?
            .query_row("SELECT sqlite_version()", [], |row| row.get(0))?)
    }

    pub fn integrity_check(&self) -> Result<()> {
        let conn = self.connect()?;
        let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            bail!("SQLite integrity check failed: {integrity}");
        }
        let violations: i64 =
            conn.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if violations != 0 {
            bail!("SQLite foreign key check found {violations} violation(s)");
        }
        Ok(())
    }

    pub fn upsert_repository(
        &self,
        repository_key: &str,
        root_path: &Path,
        default_branch: Option<&str>,
        remote_url: Option<&str>,
    ) -> Result<i64> {
        let now = now_ms();
        let root = root_path.display().to_string();
        let conn = self.connect()?;
        conn.execute(
            r#"INSERT INTO repositories(repository_key,root_path,default_branch,remote_url,created_at,updated_at)
               VALUES (?1,?2,?3,?4,?5,?5)
               ON CONFLICT(repository_key) DO UPDATE SET
                 root_path=excluded.root_path,
                 default_branch=excluded.default_branch,
                 remote_url=excluded.remote_url,
                 updated_at=excluded.updated_at"#,
            params![repository_key, root, default_branch, remote_url, now],
        )?;
        Ok(conn.query_row(
            "SELECT id FROM repositories WHERE repository_key=?1",
            [repository_key],
            |row| row.get(0),
        )?)
    }

    pub fn upsert_document(
        &self,
        repository_id: i64,
        path: &str,
        source_revision: Option<&str>,
        content_hash: &str,
        metadata_json: &str,
    ) -> Result<i64> {
        require_json(metadata_json)?;
        let now = now_ms();
        let conn = self.connect()?;
        conn.execute(
            r#"INSERT INTO documents(repository_id,path,source_revision,content_hash,metadata_json,created_at,updated_at)
               VALUES (?1,?2,?3,?4,?5,?6,?6)
               ON CONFLICT(repository_id,path) DO UPDATE SET
                 source_revision=excluded.source_revision,
                 content_hash=excluded.content_hash,
                 metadata_json=excluded.metadata_json,
                 deleted_at=NULL,
                 updated_at=excluded.updated_at"#,
            params![repository_id, path, source_revision, content_hash, metadata_json, now],
        )?;
        Ok(conn.query_row(
            "SELECT id FROM documents WHERE repository_id=?1 AND path=?2",
            params![repository_id, path],
            |row| row.get(0),
        )?)
    }

    pub fn upsert_unit(
        &self,
        document_id: i64,
        unit_key: &str,
        ordinal: i64,
        source_text: &str,
        source_hash: &str,
        context_json: &str,
    ) -> Result<i64> {
        require_json(context_json)?;
        let now = now_ms();
        let conn = self.connect()?;
        conn.execute(
            r#"INSERT INTO units(document_id,unit_key,ordinal,source_text,source_hash,context_json,created_at,updated_at)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?7)
               ON CONFLICT(document_id,unit_key) DO UPDATE SET
                 ordinal=excluded.ordinal,
                 source_text=excluded.source_text,
                 source_hash=excluded.source_hash,
                 context_json=excluded.context_json,
                 active=1,
                 updated_at=excluded.updated_at"#,
            params![document_id, unit_key, ordinal, source_text, source_hash, context_json, now],
        )?;
        Ok(conn.query_row(
            "SELECT id FROM units WHERE document_id=?1 AND unit_key=?2",
            params![document_id, unit_key],
            |row| row.get(0),
        )?)
    }

    pub fn unit_history(&self, document_id: i64, locale: &str) -> Result<Vec<UnitHistory>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare(
            r#"SELECT u.id,u.unit_key,u.ordinal,u.source_text,u.source_hash,u.context_json,
                      COALESCE(t.target_text,c.target_text),CASE WHEN t.id IS NULL THEN 0 ELSE 1 END
               FROM units u
               LEFT JOIN canonical_candidates c ON c.unit_id=u.id AND c.locale=?2 AND c.selected=1
               LEFT JOIN trusted_translation_memory t ON t.unit_id=u.id AND t.locale=?2 AND t.superseded_at IS NULL
               WHERE u.document_id=?1 AND u.active=1 ORDER BY u.ordinal,u.id"#,
        )?;
        let rows = statement.query_map(params![document_id, locale], |row| {
            Ok(UnitHistory {
                id: row.get(0)?,
                unit_key: row.get(1)?,
                ordinal: row.get::<_, i64>(2)? as usize,
                source_text: row.get(3)?,
                source_hash: row.get(4)?,
                context_json: row.get(5)?,
                translation: row.get(6)?,
                trusted: row.get::<_, i64>(7)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn trust_translation(&self, input: TrustTranslationInput<'_>) -> Result<i64> {
        let TrustTranslationInput {
            repository_id,
            unit_id,
            locale,
            source_hash,
            context_key,
            target_text,
            provenance,
        } = input;
        let conn = self.connect()?;
        conn.execute(
            r#"INSERT INTO trusted_translation_memory(
                   repository_id,unit_id,locale,source_hash,context_key,target_text,provenance,trusted_at)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
               ON CONFLICT(repository_id,locale,source_hash,context_key) DO UPDATE SET
                 unit_id=excluded.unit_id,
                 target_text=excluded.target_text,
                 provenance=excluded.provenance,
                 trusted_at=excluded.trusted_at,
                 superseded_at=NULL"#,
            params![repository_id, unit_id, locale, source_hash, context_key, target_text, provenance, now_ms()],
        )?;
        Ok(conn.query_row(
            "SELECT id FROM trusted_translation_memory WHERE repository_id=?1 AND locale=?2 AND source_hash=?3 AND context_key=?4",
            params![repository_id, locale, source_hash, context_key],
            |row| row.get(0),
        )?)
    }

    pub fn begin_run(
        &self,
        repository_id: i64,
        invocation_key: &str,
        config_path: &Path,
        metadata_json: &str,
    ) -> Result<String> {
        require_json(metadata_json)?;
        let conn = self.connect()?;
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM runs WHERE invocation_key=?1",
                [invocation_key],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            let now = now_ms();
            conn.execute(
                "UPDATE runs SET status='running',heartbeat_at=?2,finished_at=NULL WHERE id=?1",
                params![id, now],
            )?;
            return Ok(id);
        }
        let id = new_id("run");
        let now = now_ms();
        conn.execute(
            "INSERT INTO runs(id,repository_id,invocation_key,config_path,started_at,heartbeat_at,metadata_json) VALUES (?1,?2,?3,?4,?5,?5,?6)",
            params![id, repository_id, invocation_key, config_path.display().to_string(), now, metadata_json],
        )?;
        Ok(id)
    }

    pub fn finish_run(&self, run_id: &str, status: &str) -> Result<bool> {
        let now = now_ms();
        Ok(self.connect()?.execute(
            "UPDATE runs SET status=?2,heartbeat_at=?3,finished_at=?3 WHERE id=?1",
            params![run_id, status, now],
        )? == 1)
    }

    pub fn enqueue_work_item(
        &self,
        run_id: &str,
        unit_id: i64,
        locale: &str,
        kind: &str,
        priority: i64,
        input_json: &str,
    ) -> Result<i64> {
        require_json(input_json)?;
        let now = now_ms();
        let conn = self.connect()?;
        conn.execute(
            r#"INSERT INTO work_items(run_id,unit_id,locale,kind,priority,input_json,created_at,updated_at)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?7)
               ON CONFLICT(run_id,unit_id,locale,kind) DO UPDATE SET
                 priority=excluded.priority,
                 input_json=excluded.input_json,
                 updated_at=excluded.updated_at"#,
            params![run_id, unit_id, locale, kind, priority, input_json, now],
        )?;
        Ok(conn.query_row(
            "SELECT id FROM work_items WHERE run_id=?1 AND unit_id=?2 AND locale=?3 AND kind=?4",
            params![run_id, unit_id, locale, kind],
            |row| row.get(0),
        )?)
    }

    pub fn record_attempt(&self, input: AttemptInput<'_>) -> Result<AttemptReceipt> {
        require_json(input.request_json)?;
        if let Some(response) = input.response_json {
            require_json(response)?;
        }
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(id) = tx
            .query_row(
                "SELECT id FROM attempts WHERE work_item_id=?1 AND dedupe_key=?2",
                params![input.work_item_id, input.dedupe_key],
                |row| row.get(0),
            )
            .optional()?
        {
            tx.commit()?;
            return Ok(AttemptReceipt {
                id,
                inserted: false,
            });
        }
        let attempt_no: i64 = tx.query_row(
            "SELECT COALESCE(MAX(attempt_no),0)+1 FROM attempts WHERE work_item_id=?1",
            [input.work_item_id],
            |row| row.get(0),
        )?;
        let now = now_ms();
        tx.execute(
            r#"INSERT INTO attempts(work_item_id,dedupe_key,attempt_no,agent,status,request_json,response_json,error,started_at,finished_at)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,CASE WHEN ?5='started' THEN NULL ELSE ?9 END)"#,
            params![input.work_item_id, input.dedupe_key, attempt_no, input.agent, input.status, input.request_json, input.response_json, input.error, now],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(AttemptReceipt { id, inserted: true })
    }

    pub fn attempt_status(&self, work_item_id: i64, dedupe_key: &str) -> Result<Option<String>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT status FROM attempts WHERE work_item_id=?1 AND dedupe_key=?2",
                params![work_item_id, dedupe_key],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn successful_attempt(
        &self,
        work_item_id: i64,
        dedupe_key: &str,
    ) -> Result<Option<RecoveredAttempt>> {
        let row: Option<(i64, String)> = self
            .connect()?
            .query_row(
                "SELECT id,response_json FROM attempts WHERE work_item_id=?1 AND dedupe_key=?2 AND status='succeeded' AND response_json IS NOT NULL",
                params![work_item_id, dedupe_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        row.map(|(id, response)| {
            let value: serde_json::Value = serde_json::from_str(&response)
                .context("durable Agent response is not valid JSON")?;
            let output = value
                .get("output")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow!("durable Agent response has no output string"))?;
            Ok(RecoveredAttempt {
                id,
                output: output.to_owned(),
            })
        })
        .transpose()
    }

    pub fn record_attempt_candidate(
        &self,
        input: AttemptCandidateInput<'_>,
    ) -> Result<AttemptReceipt> {
        if input.attempt.status != "succeeded" {
            bail!("only a successful attempt can select a canonical candidate");
        }
        require_json(input.attempt.request_json)?;
        let response = input
            .attempt
            .response_json
            .ok_or_else(|| anyhow!("successful Agent attempt requires a response"))?;
        require_json(response)?;
        let persisted_output = serde_json::from_str::<serde_json::Value>(response)?
            .get("output")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("successful Agent response has no output string"))?
            .to_owned();
        if persisted_output != input.target_text {
            bail!("canonical candidate differs from the durable Agent response");
        }

        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(i64, String, String)> = tx
            .query_row(
                "SELECT id,status,response_json FROM attempts WHERE work_item_id=?1 AND dedupe_key=?2",
                params![input.attempt.work_item_id, input.attempt.dedupe_key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let (attempt_id, inserted) = if let Some((id, status, stored_response)) = existing {
            if status != "succeeded" || stored_response != response {
                bail!("durable Agent attempt conflicts with canonical candidate selection");
            }
            (id, false)
        } else {
            let attempt_no: i64 = tx.query_row(
                "SELECT COALESCE(MAX(attempt_no),0)+1 FROM attempts WHERE work_item_id=?1",
                [input.attempt.work_item_id],
                |row| row.get(0),
            )?;
            let now = now_ms();
            tx.execute(
                r#"INSERT INTO attempts(work_item_id,dedupe_key,attempt_no,agent,status,request_json,response_json,error,started_at,finished_at)
                   VALUES (?1,?2,?3,?4,'succeeded',?5,?6,NULL,?7,?7)"#,
                params![
                    input.attempt.work_item_id,
                    input.attempt.dedupe_key,
                    attempt_no,
                    input.attempt.agent,
                    input.attempt.request_json,
                    response,
                    now,
                ],
            )?;
            (tx.last_insert_rowid(), true)
        };
        tx.execute(
            "UPDATE canonical_candidates SET selected=0 WHERE unit_id=?1 AND locale=?2 AND selected=1",
            params![input.unit_id, input.locale],
        )?;
        tx.execute(
            r#"INSERT INTO canonical_candidates(unit_id,locale,candidate_key,target_text,source_attempt_id,score,selected,created_at)
               VALUES (?1,?2,?3,?4,?5,?6,1,?7)
               ON CONFLICT(unit_id,locale,candidate_key) DO UPDATE SET
                 target_text=excluded.target_text,source_attempt_id=excluded.source_attempt_id,
                 score=excluded.score,selected=1"#,
            params![
                input.unit_id,
                input.locale,
                input.candidate_key,
                input.target_text,
                attempt_id,
                input.score,
                now_ms(),
            ],
        )?;
        tx.commit()?;
        Ok(AttemptReceipt {
            id: attempt_id,
            inserted,
        })
    }

    pub fn record_finding(&self, input: FindingInput<'_>) -> Result<i64> {
        let FindingInput {
            work_item_id,
            attempt_id,
            finding_key: fingerprint,
            severity,
            code,
            message,
            details_json,
        } = input;
        require_json(details_json)?;
        let conn = self.connect()?;
        conn.execute(
            r#"INSERT INTO findings(work_item_id,attempt_id,fingerprint,severity,code,message,details_json,created_at)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
               ON CONFLICT(work_item_id,fingerprint) DO UPDATE SET
                 attempt_id=excluded.attempt_id,
                 severity=excluded.severity,
                 code=excluded.code,
                 message=excluded.message,
                 details_json=excluded.details_json,
                 resolved_at=NULL"#,
            params![work_item_id, attempt_id, fingerprint, severity, code, message, details_json, now_ms()],
        )?;
        Ok(conn.query_row(
            "SELECT id FROM findings WHERE work_item_id=?1 AND fingerprint=?2",
            params![work_item_id, fingerprint],
            |row| row.get(0),
        )?)
    }

    pub fn select_canonical_candidate(
        &self,
        unit_id: i64,
        locale: &str,
        candidate_key: &str,
        target_text: &str,
        source_attempt_id: Option<i64>,
        score: Option<f64>,
    ) -> Result<i64> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE canonical_candidates SET selected=0 WHERE unit_id=?1 AND locale=?2 AND selected=1",
            params![unit_id, locale],
        )?;
        tx.execute(
            r#"INSERT INTO canonical_candidates(unit_id,locale,candidate_key,target_text,source_attempt_id,score,selected,created_at)
               VALUES (?1,?2,?3,?4,?5,?6,1,?7)
               ON CONFLICT(unit_id,locale,candidate_key) DO UPDATE SET
                 target_text=excluded.target_text,
                 source_attempt_id=excluded.source_attempt_id,
                 score=excluded.score,
                 selected=1"#,
            params![unit_id, locale, candidate_key, target_text, source_attempt_id, score, now_ms()],
        )?;
        let id = tx.query_row(
            "SELECT id FROM canonical_candidates WHERE unit_id=?1 AND locale=?2 AND candidate_key=?3",
            params![unit_id, locale, candidate_key],
            |row| row.get(0),
        )?;
        tx.commit()?;
        Ok(id)
    }

    pub fn supersede_materializations(
        &self,
        locale: &str,
        path: &str,
        active_dedupe_key: &str,
    ) -> Result<usize> {
        let now = now_ms();
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let owners = {
            let mut statement = tx.prepare(
                r#"SELECT owner FROM materialization_outbox
                   WHERE dedupe_key<>?3 AND state='processing'
                     AND json_extract(payload_json,'$.locale')=?1
                     AND json_extract(payload_json,'$.path')=?2"#,
            )?;
            statement
                .query_map(params![locale, path, active_dedupe_key], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if owners.iter().any(|owner| !dead_process_owner(owner)) {
            bail!("older materialization for {path} is still owned by a live worker");
        }
        let changed = tx.execute(
            r#"UPDATE materialization_outbox
               SET state='done',owner=NULL,lease_expires_at=NULL,
                   last_error='superseded by newer canonical content',completed_at=?4
               WHERE dedupe_key<>?3 AND state<>'done'
                 AND json_extract(payload_json,'$.locale')=?1
                 AND json_extract(payload_json,'$.path')=?2"#,
            params![locale, path, active_dedupe_key, now],
        )?;
        tx.commit()?;
        Ok(changed)
    }

    pub fn enqueue_materialization(
        &self,
        work_item_id: i64,
        dedupe_key: &str,
        payload_json: &str,
    ) -> Result<i64> {
        require_json(payload_json)?;
        let now = now_ms();
        let conn = self.connect()?;
        conn.execute(
            "INSERT OR IGNORE INTO materialization_outbox(work_item_id,dedupe_key,payload_json,available_at,created_at) VALUES (?1,?2,?3,?4,?4)",
            params![work_item_id, dedupe_key, payload_json, now],
        )?;
        Ok(conn.query_row(
            "SELECT id FROM materialization_outbox WHERE dedupe_key=?1",
            [dedupe_key],
            |row| row.get(0),
        )?)
    }

    pub fn enqueue_publication(
        &self,
        repository_id: i64,
        run_id: Option<&str>,
        locale: &str,
        dedupe_key: &str,
        payload_json: &str,
    ) -> Result<i64> {
        require_json(payload_json)?;
        let now = now_ms();
        let conn = self.connect()?;
        conn.execute(
            "INSERT OR IGNORE INTO publication_outbox(repository_id,run_id,locale,dedupe_key,payload_json,available_at,created_at) VALUES (?1,?2,?3,?4,?5,?6,?6)",
            params![repository_id, run_id, locale, dedupe_key, payload_json, now],
        )?;
        Ok(conn.query_row(
            "SELECT id FROM publication_outbox WHERE dedupe_key=?1",
            [dedupe_key],
            |row| row.get(0),
        )?)
    }

    pub fn update_outbox_payload(
        &self,
        kind: OutboxKind,
        id: i64,
        owner: &str,
        payload_json: &str,
    ) -> Result<bool> {
        require_json(payload_json)?;
        let table = outbox_table(kind);
        let conn = self.connect()?;
        Ok(conn.execute(
            &format!(
                "UPDATE {table} SET payload_json=?3 WHERE id=?1 AND state='processing' AND owner=?2"
            ),
            params![id, owner, payload_json],
        )? == 1)
    }

    pub fn claim_outbox_key(
        &self,
        kind: OutboxKind,
        dedupe_key: &str,
        owner: &str,
        now: i64,
        lease_ms: i64,
    ) -> Result<Option<OutboxEntry>> {
        if lease_ms <= 0 {
            bail!("outbox lease must be positive");
        }
        let table = outbox_table(kind);
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let processing_owner: Option<String> = tx
            .query_row(
                &format!("SELECT owner FROM {table} WHERE dedupe_key=?1 AND state='processing'"),
                [dedupe_key],
                |row| row.get(0),
            )
            .optional()?;
        if processing_owner.as_deref().is_some_and(dead_process_owner) {
            tx.execute(
                &format!("UPDATE {table} SET state='pending',owner=NULL,lease_expires_at=NULL,last_error='worker process exited' WHERE dedupe_key=?1 AND state='processing'"),
                [dedupe_key],
            )?;
        }
        tx.execute(
            &format!("UPDATE {table} SET state='pending',owner=NULL,lease_expires_at=NULL,last_error=COALESCE(last_error,'worker lease expired') WHERE dedupe_key=?1 AND state='processing' AND lease_expires_at<=?2"),
            params![dedupe_key, now],
        )?;
        let row: Option<(i64, String, String, i64)> = tx.query_row(
            &format!("SELECT id,dedupe_key,payload_json,attempt_count FROM {table} WHERE dedupe_key=?1 AND state='pending' AND available_at<=?2"),
            params![dedupe_key, now],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).optional()?;
        let Some((id, dedupe_key, payload_json, attempt_count)) = row else {
            tx.commit()?;
            return Ok(None);
        };
        tx.execute(
            &format!("UPDATE {table} SET state='processing',owner=?2,lease_expires_at=?3,attempt_count=attempt_count+1 WHERE id=?1 AND state='pending'"),
            params![id, owner, now + lease_ms],
        )?;
        tx.commit()?;
        Ok(Some(OutboxEntry {
            id,
            dedupe_key,
            payload_json,
            attempt_count: attempt_count + 1,
        }))
    }

    pub fn claim_publication_locale(
        &self,
        locale: &str,
        owner: &str,
        now: i64,
        lease_ms: i64,
    ) -> Result<Option<OutboxEntry>> {
        if lease_ms <= 0 {
            bail!("outbox lease must be positive");
        }
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let dead_ids = {
            let mut statement = tx.prepare(
                "SELECT id,owner FROM publication_outbox WHERE locale=?1 AND state='processing'",
            )?;
            statement
                .query_map([locale], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter_map(|(id, owner)| dead_process_owner(&owner).then_some(id))
                .collect::<Vec<_>>()
        };
        for id in dead_ids {
            tx.execute(
                "UPDATE publication_outbox SET state='pending',owner=NULL,lease_expires_at=NULL,last_error='worker process exited' WHERE id=?1 AND state='processing'",
                [id],
            )?;
        }
        tx.execute(
            "UPDATE publication_outbox SET state='pending',owner=NULL,lease_expires_at=NULL,last_error=COALESCE(last_error,'worker lease expired') WHERE locale=?1 AND state='processing' AND lease_expires_at<=?2",
            params![locale, now],
        )?;
        let row: Option<(i64, String, String, i64)> = tx
            .query_row(
                "SELECT id,dedupe_key,payload_json,attempt_count FROM publication_outbox WHERE locale=?1 AND state='pending' AND available_at<=?2 ORDER BY available_at,id LIMIT 1",
                params![locale, now],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((id, dedupe_key, payload_json, attempt_count)) = row else {
            tx.commit()?;
            return Ok(None);
        };
        tx.execute(
            "UPDATE publication_outbox SET state='processing',owner=?2,lease_expires_at=?3,attempt_count=attempt_count+1 WHERE id=?1 AND state='pending'",
            params![id, owner, now + lease_ms],
        )?;
        tx.commit()?;
        Ok(Some(OutboxEntry {
            id,
            dedupe_key,
            payload_json,
            attempt_count: attempt_count + 1,
        }))
    }

    pub fn claim_outbox(
        &self,
        kind: OutboxKind,
        owner: &str,
        now: i64,
        lease_ms: i64,
    ) -> Result<Option<OutboxEntry>> {
        if lease_ms <= 0 {
            bail!("outbox lease must be positive");
        }
        let table = outbox_table(kind);
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            &format!("UPDATE {table} SET state='pending',owner=NULL,lease_expires_at=NULL,last_error=COALESCE(last_error,'worker lease expired') WHERE state='processing' AND lease_expires_at<=?1"),
            [now],
        )?;
        let row: Option<(i64, String, String, i64)> = tx
            .query_row(
                &format!("SELECT id,dedupe_key,payload_json,attempt_count FROM {table} WHERE state='pending' AND available_at<=?1 ORDER BY available_at,id LIMIT 1"),
                [now],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((id, dedupe_key, payload_json, attempt_count)) = row else {
            tx.commit()?;
            return Ok(None);
        };
        tx.execute(
            &format!("UPDATE {table} SET state='processing',owner=?2,lease_expires_at=?3,attempt_count=attempt_count+1 WHERE id=?1 AND state='pending'"),
            params![id, owner, now + lease_ms],
        )?;
        tx.commit()?;
        Ok(Some(OutboxEntry {
            id,
            dedupe_key,
            payload_json,
            attempt_count: attempt_count + 1,
        }))
    }

    pub fn complete_outbox(&self, kind: OutboxKind, id: i64, owner: &str) -> Result<bool> {
        let table = outbox_table(kind);
        Ok(self.connect()?.execute(
            &format!("UPDATE {table} SET state='done',owner=NULL,lease_expires_at=NULL,completed_at=?3 WHERE id=?1 AND state='processing' AND owner=?2"),
            params![id, owner, now_ms()],
        )? == 1)
    }

    pub fn retry_outbox(
        &self,
        kind: OutboxKind,
        id: i64,
        owner: &str,
        error: &str,
        available_at: i64,
    ) -> Result<bool> {
        let table = outbox_table(kind);
        Ok(self.connect()?.execute(
            &format!("UPDATE {table} SET state='pending',owner=NULL,lease_expires_at=NULL,last_error=?3,available_at=?4 WHERE id=?1 AND state='processing' AND owner=?2"),
            params![id, owner, error, available_at],
        )? == 1)
    }

    pub fn record_pr_state(&self, input: PullRequestStateInput<'_>) -> Result<i64> {
        let PullRequestStateInput {
            repository_id,
            provider,
            external_id,
            number,
            branch,
            url,
            state,
            head_revision,
            event_key,
            payload_json,
        } = input;
        require_json(payload_json)?;
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<String> = tx
            .query_row(
                "SELECT state FROM pull_requests WHERE repository_id=?1 AND provider=?2 AND external_id=?3",
                params![repository_id, provider, external_id],
                |row| row.get(0),
            )
            .optional()?;
        let now = now_ms();
        tx.execute(
            r#"INSERT INTO pull_requests(repository_id,provider,external_id,number,branch,url,state,head_revision,opened_at,updated_at,closed_at)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?9,CASE WHEN ?7 IN ('merged','closed') THEN ?9 ELSE NULL END)
               ON CONFLICT(repository_id,provider,external_id) DO UPDATE SET
                 number=excluded.number,branch=excluded.branch,url=excluded.url,state=excluded.state,
                 head_revision=excluded.head_revision,updated_at=excluded.updated_at,closed_at=excluded.closed_at"#,
            params![repository_id, provider, external_id, number, branch, url, state, head_revision, now],
        )?;
        let pr_id: i64 = tx.query_row(
            "SELECT id FROM pull_requests WHERE repository_id=?1 AND provider=?2 AND external_id=?3",
            params![repository_id, provider, external_id],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO pr_events(pull_request_id,event_key,from_state,to_state,payload_json,occurred_at) VALUES (?1,?2,?3,?4,?5,?6)",
            params![pr_id, event_key, previous, state, payload_json, now],
        )?;
        tx.commit()?;
        Ok(pr_id)
    }

    pub fn pull_request_for_branch(
        &self,
        repository_id: i64,
        provider: &str,
        branch: &str,
    ) -> Result<Option<StoredPullRequest>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT external_id,number,branch,url,state,head_revision FROM pull_requests WHERE repository_id=?1 AND provider=?2 AND branch=?3 ORDER BY updated_at DESC,id DESC LIMIT 1",
                params![repository_id, provider, branch],
                |row| {
                    Ok(StoredPullRequest {
                        external_id: row.get(0)?,
                        number: row.get(1)?,
                        branch: row.get(2)?,
                        url: row.get(3)?,
                        state: row.get(4)?,
                        head_revision: row.get(5)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn promote_merged_locale(
        &self,
        repository_id: i64,
        locale: &str,
        provenance: &str,
    ) -> Result<usize> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        let promoted = tx.execute(
            r#"INSERT INTO trusted_translation_memory(
                   repository_id,unit_id,locale,source_hash,context_key,target_text,provenance,trusted_at)
               SELECT d.repository_id,u.id,c.locale,u.source_hash,
                      COALESCE(json_extract(u.context_json,'$.kind'),''),c.target_text,?3,?4
               FROM canonical_candidates c
               JOIN units u ON u.id=c.unit_id
               JOIN documents d ON d.id=u.document_id
               WHERE d.repository_id=?1 AND c.locale=?2 AND c.selected=1 AND u.active=1
               ON CONFLICT(repository_id,locale,source_hash,context_key) DO UPDATE SET
                 unit_id=excluded.unit_id,target_text=excluded.target_text,
                 provenance=excluded.provenance,trusted_at=excluded.trusted_at,superseded_at=NULL"#,
            params![repository_id, locale, provenance, now],
        )?;
        tx.execute(
            "UPDATE canonical_files SET state='merged',updated_at=?3 WHERE repository_id=?1 AND locale=?2",
            params![repository_id, locale, now],
        )?;
        tx.commit()?;
        Ok(promoted)
    }

    pub fn acquire_lease(
        &self,
        resource_type: &str,
        resource_key: &str,
        owner: &str,
        now: i64,
        ttl_ms: i64,
    ) -> Result<Option<Lease>> {
        if ttl_ms <= 0 {
            bail!("lease TTL must be positive");
        }
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<Lease> = tx
            .query_row(
                "SELECT resource_type,resource_key,owner,fencing_token,acquired_at,expires_at FROM leases WHERE resource_type=?1 AND resource_key=?2",
                params![resource_type, resource_key],
                lease_from_row,
            )
            .optional()?;
        let lease = match current {
            Some(current)
                if current.owner != owner
                    && current.expires_at > now
                    && !(resource_type == "repository"
                        && dead_repository_owner(&current.owner)) =>
            {
                tx.commit()?;
                return Ok(None);
            }
            Some(current) => Lease {
                resource_type: resource_type.into(),
                resource_key: resource_key.into(),
                owner: owner.into(),
                fencing_token: if current.owner == owner {
                    current.fencing_token
                } else {
                    current.fencing_token + 1
                },
                acquired_at: now,
                expires_at: now + ttl_ms,
            },
            None => Lease {
                resource_type: resource_type.into(),
                resource_key: resource_key.into(),
                owner: owner.into(),
                fencing_token: 1,
                acquired_at: now,
                expires_at: now + ttl_ms,
            },
        };
        tx.execute(
            r#"INSERT INTO leases(resource_type,resource_key,owner,fencing_token,acquired_at,expires_at)
               VALUES (?1,?2,?3,?4,?5,?6)
               ON CONFLICT(resource_type,resource_key) DO UPDATE SET
                 owner=excluded.owner,fencing_token=excluded.fencing_token,
                 acquired_at=excluded.acquired_at,expires_at=excluded.expires_at"#,
            params![lease.resource_type, lease.resource_key, lease.owner, lease.fencing_token, lease.acquired_at, lease.expires_at],
        )?;
        tx.commit()?;
        Ok(Some(lease))
    }

    pub fn renew_lease(&self, lease: &Lease, now: i64, ttl_ms: i64) -> Result<Option<Lease>> {
        if ttl_ms <= 0 {
            bail!("lease TTL must be positive");
        }
        let expires_at = now + ttl_ms;
        let changed = self.connect()?.execute(
            "UPDATE leases SET expires_at=?6 WHERE resource_type=?1 AND resource_key=?2 AND owner=?3 AND fencing_token=?4 AND expires_at>?5",
            params![lease.resource_type, lease.resource_key, lease.owner, lease.fencing_token, now, expires_at],
        )?;
        Ok((changed == 1).then(|| Lease {
            expires_at,
            ..lease.clone()
        }))
    }

    pub fn release_lease(&self, lease: &Lease) -> Result<bool> {
        Ok(self.connect()?.execute(
            "DELETE FROM leases WHERE resource_type=?1 AND resource_key=?2 AND owner=?3 AND fencing_token=?4",
            params![lease.resource_type, lease.resource_key, lease.owner, lease.fencing_token],
        )? == 1)
    }

    pub fn trusted_translation(
        &self,
        repository_id: i64,
        locale: &str,
        source_hash: &str,
        context_key: &str,
    ) -> Result<Option<String>> {
        Ok(self.connect()?.query_row(
            "SELECT target_text FROM trusted_translation_memory WHERE repository_id=?1 AND locale=?2 AND source_hash=?3 AND context_key=?4 AND superseded_at IS NULL",
            params![repository_id, locale, source_hash, context_key],
            |row| row.get(0),
        ).optional()?)
    }

    pub fn selected_candidate(&self, unit_id: i64, locale: &str) -> Result<Option<String>> {
        Ok(self.connect()?.query_row(
            "SELECT target_text FROM canonical_candidates WHERE unit_id=?1 AND locale=?2 AND selected=1",
            params![unit_id, locale],
            |row| row.get(0),
        ).optional()?)
    }

    pub fn upsert_canonical_file(&self, input: CanonicalFileInput<'_>) -> Result<i64> {
        let CanonicalFileInput {
            repository_id,
            locale,
            path,
            source_revision,
            content,
            content_hash,
            materialized_hash,
            state,
        } = input;
        let conn = self.connect()?;
        conn.execute(
            r#"INSERT INTO canonical_files(repository_id,locale,path,source_revision,content,content_hash,materialized_hash,state,updated_at)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
               ON CONFLICT(repository_id,locale,path) DO UPDATE SET
                 source_revision=excluded.source_revision,content=excluded.content,content_hash=excluded.content_hash,
                 materialized_hash=excluded.materialized_hash,state=excluded.state,updated_at=excluded.updated_at"#,
            params![repository_id, locale, path, source_revision, content, content_hash, materialized_hash, state, now_ms()],
        )?;
        Ok(conn.query_row(
            "SELECT id FROM canonical_files WHERE repository_id=?1 AND locale=?2 AND path=?3",
            params![repository_id, locale, path],
            |row| row.get(0),
        )?)
    }

    pub fn canonical_file(
        &self,
        repository_id: i64,
        locale: &str,
        path: &str,
    ) -> Result<Option<CanonicalFile>> {
        Ok(self.connect()?.query_row(
            "SELECT id,source_revision,content,content_hash,materialized_hash,state FROM canonical_files WHERE repository_id=?1 AND locale=?2 AND path=?3",
            params![repository_id, locale, path],
            |row| Ok(CanonicalFile {
                id: row.get(0)?,
                source_revision: row.get(1)?,
                content: row.get(2)?,
                content_hash: row.get(3)?,
                materialized_hash: row.get(4)?,
                state: row.get(5)?,
            }),
        ).optional()?)
    }

    pub fn set_canonical_file_state(
        &self,
        id: i64,
        state: &str,
        materialized_hash: Option<&str>,
    ) -> Result<()> {
        let changed = self.connect()?.execute(
            "UPDATE canonical_files SET state=?2,materialized_hash=?3,updated_at=?4 WHERE id=?1",
            params![id, state, materialized_hash, now_ms()],
        )?;
        if changed != 1 {
            bail!("canonical file {id} does not exist");
        }
        Ok(())
    }
}

fn process_owner_is_dead(pid: Option<&str>, started_at: Option<&str>) -> bool {
    let Some((pid, started_at)) = pid
        .and_then(|value| value.parse::<u32>().ok())
        .zip(started_at.and_then(|value| value.parse::<u64>().ok()))
    else {
        return false;
    };
    process_identity(pid).is_none_or(|identity| identity.1 != started_at)
}

fn dead_repository_owner(owner: &str) -> bool {
    let mut parts = owner.split(':');
    process_owner_is_dead(parts.next(), parts.next())
}

fn dead_process_owner(owner: &str) -> bool {
    let mut parts = owner.split(':');
    let Some(kind) = parts.next() else {
        return false;
    };
    if !matches!(kind, "materialize" | "publish") {
        return false;
    }
    process_owner_is_dead(parts.next(), parts.next())
}

fn outbox_table(kind: OutboxKind) -> &'static str {
    match kind {
        OutboxKind::Materialization => "materialization_outbox",
        OutboxKind::Publication => "publication_outbox",
    }
}

fn lease_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Lease> {
    Ok(Lease {
        resource_type: row.get(0)?,
        resource_key: row.get(1)?,
        owner: row.get(2)?,
        fencing_token: row.get(3)?,
        acquired_at: row.get(4)?,
        expires_at: row.get(5)?,
    })
}

fn require_json(value: &str) -> Result<()> {
    serde_json::from_str::<serde_json::Value>(value)
        .map(|_| ())
        .map_err(|error| anyhow!("invalid JSON payload: {error}"))
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn new_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}-{}",
        now_ms(),
        std::process::id(),
        ID_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

impl StateStore for Database {
    fn upsert_repository(
        &self,
        repository_key: &str,
        root_path: &Path,
        default_branch: Option<&str>,
        remote_url: Option<&str>,
    ) -> Result<i64> {
        Database::upsert_repository(self, repository_key, root_path, default_branch, remote_url)
    }

    fn upsert_document(
        &self,
        repository_id: i64,
        path: &str,
        source_revision: Option<&str>,
        content_hash: &str,
        metadata_json: &str,
    ) -> Result<i64> {
        Database::upsert_document(
            self,
            repository_id,
            path,
            source_revision,
            content_hash,
            metadata_json,
        )
    }

    fn upsert_unit(
        &self,
        document_id: i64,
        unit_key: &str,
        ordinal: i64,
        source_text: &str,
        source_hash: &str,
        context_json: &str,
    ) -> Result<i64> {
        Database::upsert_unit(
            self,
            document_id,
            unit_key,
            ordinal,
            source_text,
            source_hash,
            context_json,
        )
    }

    fn unit_history(&self, document_id: i64, locale: &str) -> Result<Vec<UnitHistory>> {
        Database::unit_history(self, document_id, locale)
    }

    fn trusted_translation(
        &self,
        repository_id: i64,
        locale: &str,
        source_hash: &str,
        context_key: &str,
    ) -> Result<Option<String>> {
        Database::trusted_translation(self, repository_id, locale, source_hash, context_key)
    }

    fn trust_translation(&self, input: TrustTranslationInput<'_>) -> Result<i64> {
        Database::trust_translation(self, input)
    }

    fn begin_run(
        &self,
        repository_id: i64,
        invocation_key: &str,
        config_path: &Path,
        metadata_json: &str,
    ) -> Result<String> {
        Database::begin_run(
            self,
            repository_id,
            invocation_key,
            config_path,
            metadata_json,
        )
    }

    fn finish_run(&self, run_id: &str, status: &str) -> Result<bool> {
        Database::finish_run(self, run_id, status)
    }

    fn enqueue_work_item(
        &self,
        run_id: &str,
        unit_id: i64,
        locale: &str,
        kind: &str,
        priority: i64,
        input_json: &str,
    ) -> Result<i64> {
        Database::enqueue_work_item(self, run_id, unit_id, locale, kind, priority, input_json)
    }

    fn successful_attempt(
        &self,
        work_item_id: i64,
        dedupe_key: &str,
    ) -> Result<Option<RecoveredAttempt>> {
        Database::successful_attempt(self, work_item_id, dedupe_key)
    }

    fn attempt_status(&self, work_item_id: i64, dedupe_key: &str) -> Result<Option<String>> {
        Database::attempt_status(self, work_item_id, dedupe_key)
    }

    fn record_attempt(&self, input: AttemptInput<'_>) -> Result<AttemptReceipt> {
        Database::record_attempt(self, input)
    }

    fn record_attempt_candidate(&self, input: AttemptCandidateInput<'_>) -> Result<AttemptReceipt> {
        Database::record_attempt_candidate(self, input)
    }

    fn select_canonical_candidate(
        &self,
        unit_id: i64,
        locale: &str,
        candidate_key: &str,
        target_text: &str,
        source_attempt_id: Option<i64>,
        score: Option<f64>,
    ) -> Result<i64> {
        Database::select_canonical_candidate(
            self,
            unit_id,
            locale,
            candidate_key,
            target_text,
            source_attempt_id,
            score,
        )
    }

    fn record_finding(&self, input: FindingInput<'_>) -> Result<i64> {
        Database::record_finding(self, input)
    }

    fn canonical_file(
        &self,
        repository_id: i64,
        locale: &str,
        path: &str,
    ) -> Result<Option<CanonicalFile>> {
        Database::canonical_file(self, repository_id, locale, path)
    }

    fn upsert_canonical_file(&self, input: CanonicalFileInput<'_>) -> Result<i64> {
        Database::upsert_canonical_file(self, input)
    }

    fn set_canonical_file_state(
        &self,
        id: i64,
        state: &str,
        materialized_hash: Option<&str>,
    ) -> Result<()> {
        Database::set_canonical_file_state(self, id, state, materialized_hash)
    }

    fn supersede_materializations(
        &self,
        locale: &str,
        path: &str,
        active_dedupe_key: &str,
    ) -> Result<usize> {
        Database::supersede_materializations(self, locale, path, active_dedupe_key)
    }

    fn enqueue_materialization(
        &self,
        work_item_id: i64,
        dedupe_key: &str,
        payload_json: &str,
    ) -> Result<i64> {
        Database::enqueue_materialization(self, work_item_id, dedupe_key, payload_json)
    }

    fn claim_outbox_key(
        &self,
        kind: OutboxKind,
        dedupe_key: &str,
        owner: &str,
        now: i64,
        lease_ms: i64,
    ) -> Result<Option<OutboxEntry>> {
        Database::claim_outbox_key(self, kind, dedupe_key, owner, now, lease_ms)
    }

    fn retry_outbox(
        &self,
        kind: OutboxKind,
        id: i64,
        owner: &str,
        error: &str,
        available_at: i64,
    ) -> Result<bool> {
        Database::retry_outbox(self, kind, id, owner, error, available_at)
    }

    fn complete_outbox(&self, kind: OutboxKind, id: i64, owner: &str) -> Result<bool> {
        Database::complete_outbox(self, kind, id, owner)
    }

    fn enqueue_publication(
        &self,
        repository_id: i64,
        run_id: Option<&str>,
        locale: &str,
        dedupe_key: &str,
        payload_json: &str,
    ) -> Result<i64> {
        Database::enqueue_publication(
            self,
            repository_id,
            run_id,
            locale,
            dedupe_key,
            payload_json,
        )
    }

    fn claim_publication_locale(
        &self,
        locale: &str,
        owner: &str,
        now: i64,
        lease_ms: i64,
    ) -> Result<Option<OutboxEntry>> {
        Database::claim_publication_locale(self, locale, owner, now, lease_ms)
    }

    fn update_outbox_payload(
        &self,
        kind: OutboxKind,
        id: i64,
        owner: &str,
        payload_json: &str,
    ) -> Result<bool> {
        Database::update_outbox_payload(self, kind, id, owner, payload_json)
    }

    fn pull_request_for_branch(
        &self,
        repository_id: i64,
        provider: &str,
        branch: &str,
    ) -> Result<Option<StoredPullRequest>> {
        Database::pull_request_for_branch(self, repository_id, provider, branch)
    }

    fn record_pr_state(&self, input: PullRequestStateInput<'_>) -> Result<i64> {
        Database::record_pr_state(self, input)
    }

    fn promote_merged_locale(
        &self,
        repository_id: i64,
        locale: &str,
        provenance: &str,
    ) -> Result<usize> {
        Database::promote_merged_locale(self, repository_id, locale, provenance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_migration_rolls_back_schema_ledger_and_header_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rollback.db");
        let mut conn = open_connection(&path).unwrap();
        let migrations = [Migration {
            version: 1,
            name: "0001_invalid",
            sql: "CREATE TABLE partially_applied(id INTEGER); INVALID SQL;",
        }];

        let error = apply_migrations(&mut conn, &migrations).unwrap_err();
        assert!(error.to_string().contains("migration 0001_invalid failed"));
        assert_eq!(application_id(&conn).unwrap(), 0);
        assert_eq!(user_table_count(&conn).unwrap(), 0);
        assert!(!has_table(&conn, "schema_migrations").unwrap());
        assert!(!has_table(&conn, "partially_applied").unwrap());
        let user_version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(user_version, 0);
    }

    #[test]
    fn migration_history_gaps_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("gap.db");
        let mut conn = open_connection(&path).unwrap();
        let migrations = [
            Migration {
                version: 1,
                name: "0001_first",
                sql: "CREATE TABLE first(id INTEGER);",
            },
            Migration {
                version: 2,
                name: "0002_second",
                sql: "CREATE TABLE second(id INTEGER);",
            },
        ];
        apply_migrations(&mut conn, &migrations).unwrap();
        conn.execute("DELETE FROM schema_migrations WHERE version=1", [])
            .unwrap();

        let error = apply_migrations(&mut conn, &migrations).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("database migration history is not contiguous")
        );
    }

    #[test]
    fn process_owners_detect_pid_reuse_by_start_time() {
        let (pid, started_at) = process_identity(std::process::id()).unwrap();
        assert!(!dead_repository_owner(&format!("{pid}:{started_at}:run")));
        assert!(dead_repository_owner(&format!(
            "{pid}:{}:run",
            started_at.saturating_add(1)
        )));
        assert!(!dead_process_owner(&format!(
            "materialize:{pid}:{started_at}:run"
        )));
        assert!(dead_process_owner(&format!(
            "publish:{pid}:{}:run",
            started_at.saturating_add(1)
        )));
    }
}
