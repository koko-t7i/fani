use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const SCHEMA_VERSION: i64 = 1;
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);
static ID_SEQUENCE: AtomicU64 = AtomicU64::new(1);

const SCHEMA: &str = r#"
CREATE TABLE state_schema (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    generation TEXT NOT NULL,
    version INTEGER NOT NULL CHECK (version > 0),
    installed_at INTEGER NOT NULL
) STRICT;

CREATE TABLE repositories (
    id INTEGER PRIMARY KEY,
    repository_key TEXT NOT NULL UNIQUE,
    root_path TEXT NOT NULL UNIQUE,
    default_branch TEXT,
    remote_url TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE documents (
    id INTEGER PRIMARY KEY,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    source_revision TEXT,
    content_hash TEXT NOT NULL,
    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
    deleted_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(repository_id, path)
) STRICT;
CREATE INDEX documents_repository ON documents(repository_id, deleted_at);

CREATE TABLE units (
    id INTEGER PRIMARY KEY,
    document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    unit_key TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    source_text TEXT NOT NULL,
    source_hash TEXT NOT NULL,
    context_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(context_json)),
    active INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(document_id, unit_key)
) STRICT;
CREATE INDEX units_source_hash ON units(source_hash);

CREATE TABLE trusted_translation_memory (
    id INTEGER PRIMARY KEY,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    unit_id INTEGER REFERENCES units(id) ON DELETE SET NULL,
    locale TEXT NOT NULL,
    source_hash TEXT NOT NULL,
    context_key TEXT NOT NULL DEFAULT '',
    target_text TEXT NOT NULL,
    provenance TEXT NOT NULL,
    trusted_at INTEGER NOT NULL,
    superseded_at INTEGER,
    UNIQUE(repository_id, locale, source_hash, context_key)
) STRICT;
CREATE INDEX trusted_tm_lookup ON trusted_translation_memory(repository_id, locale, source_hash, superseded_at);

CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    repository_id INTEGER REFERENCES repositories(id) ON DELETE RESTRICT,
    invocation_key TEXT UNIQUE,
    config_path TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'running' CHECK (status IN ('running','ok','partial','needs_human','error','cancelled')),
    started_at INTEGER NOT NULL,
    heartbeat_at INTEGER NOT NULL,
    finished_at INTEGER,
    exit_code INTEGER,
    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
    CHECK ((finished_at IS NULL AND exit_code IS NULL) OR finished_at IS NOT NULL)
) STRICT;
CREATE INDEX runs_repository_status ON runs(repository_id, status, started_at);

CREATE TABLE work_items (
    id INTEGER PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    unit_id INTEGER NOT NULL REFERENCES units(id) ON DELETE RESTRICT,
    locale TEXT NOT NULL,
    kind TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','running','succeeded','failed','cancelled')),
    priority INTEGER NOT NULL DEFAULT 0,
    input_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(input_json)),
    result_json TEXT CHECK (result_json IS NULL OR json_valid(result_json)),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(run_id, unit_id, locale, kind)
) STRICT;
CREATE INDEX work_items_claim ON work_items(run_id, status, priority DESC, id);

CREATE TABLE attempts (
    id INTEGER PRIMARY KEY,
    work_item_id INTEGER NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    dedupe_key TEXT NOT NULL,
    attempt_no INTEGER NOT NULL CHECK (attempt_no > 0),
    agent TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('started','succeeded','failed','timed_out','cancelled')),
    request_json TEXT NOT NULL CHECK (json_valid(request_json)),
    response_json TEXT CHECK (response_json IS NULL OR json_valid(response_json)),
    error TEXT,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    UNIQUE(work_item_id, dedupe_key),
    UNIQUE(work_item_id, attempt_no)
) STRICT;
CREATE INDEX attempts_work_item ON attempts(work_item_id, id);

CREATE TABLE findings (
    id INTEGER PRIMARY KEY,
    work_item_id INTEGER NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    attempt_id INTEGER REFERENCES attempts(id) ON DELETE CASCADE,
    fingerprint TEXT NOT NULL,
    severity TEXT NOT NULL CHECK (severity IN ('info','warning','error','blocking')),
    code TEXT NOT NULL,
    message TEXT NOT NULL,
    details_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(details_json)),
    resolved_at INTEGER,
    created_at INTEGER NOT NULL,
    UNIQUE(work_item_id, fingerprint)
) STRICT;

CREATE TABLE canonical_candidates (
    id INTEGER PRIMARY KEY,
    unit_id INTEGER NOT NULL REFERENCES units(id) ON DELETE CASCADE,
    locale TEXT NOT NULL,
    candidate_key TEXT NOT NULL,
    target_text TEXT NOT NULL,
    source_attempt_id INTEGER REFERENCES attempts(id) ON DELETE SET NULL,
    score REAL,
    selected INTEGER NOT NULL DEFAULT 0 CHECK (selected IN (0, 1)),
    created_at INTEGER NOT NULL,
    UNIQUE(unit_id, locale, candidate_key)
) STRICT;
CREATE UNIQUE INDEX one_selected_candidate ON canonical_candidates(unit_id, locale) WHERE selected = 1;

CREATE TABLE canonical_files (
    id INTEGER PRIMARY KEY,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    locale TEXT NOT NULL,
    path TEXT NOT NULL,
    source_revision TEXT NOT NULL,
    content BLOB NOT NULL,
    content_hash TEXT NOT NULL,
    materialized_hash TEXT,
    state TEXT NOT NULL DEFAULT 'candidate' CHECK (state IN ('candidate','materialized','human_edit','adopted','published','merged')),
    updated_at INTEGER NOT NULL,
    UNIQUE(repository_id, locale, path)
) STRICT;
CREATE INDEX canonical_files_state ON canonical_files(repository_id, locale, state);

CREATE TABLE materialization_outbox (
    id INTEGER PRIMARY KEY,
    work_item_id INTEGER NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    dedupe_key TEXT NOT NULL UNIQUE,
    payload_json TEXT NOT NULL CHECK (json_valid(payload_json)),
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','processing','done')),
    available_at INTEGER NOT NULL,
    owner TEXT,
    lease_expires_at INTEGER,
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    last_error TEXT,
    created_at INTEGER NOT NULL,
    completed_at INTEGER,
    CHECK ((state = 'processing') = (owner IS NOT NULL AND lease_expires_at IS NOT NULL)),
    CHECK ((state = 'done') = (completed_at IS NOT NULL))
) STRICT;
CREATE INDEX materialization_ready ON materialization_outbox(state, available_at, id);

CREATE TABLE publication_outbox (
    id INTEGER PRIMARY KEY,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    run_id TEXT REFERENCES runs(id) ON DELETE SET NULL,
    locale TEXT NOT NULL,
    dedupe_key TEXT NOT NULL UNIQUE,
    payload_json TEXT NOT NULL CHECK (json_valid(payload_json)),
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','processing','done')),
    available_at INTEGER NOT NULL,
    owner TEXT,
    lease_expires_at INTEGER,
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    last_error TEXT,
    created_at INTEGER NOT NULL,
    completed_at INTEGER,
    CHECK ((state = 'processing') = (owner IS NOT NULL AND lease_expires_at IS NOT NULL)),
    CHECK ((state = 'done') = (completed_at IS NOT NULL))
) STRICT;
CREATE INDEX publication_ready ON publication_outbox(state, available_at, id);

CREATE TABLE pull_requests (
    id INTEGER PRIMARY KEY,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    external_id TEXT NOT NULL,
    number INTEGER,
    branch TEXT NOT NULL,
    url TEXT,
    state TEXT NOT NULL CHECK (state IN ('draft','open','merged','closed')),
    head_revision TEXT,
    opened_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    closed_at INTEGER,
    UNIQUE(repository_id, provider, external_id)
) STRICT;

CREATE TABLE pr_events (
    id INTEGER PRIMARY KEY,
    pull_request_id INTEGER NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE,
    event_key TEXT NOT NULL,
    from_state TEXT,
    to_state TEXT NOT NULL CHECK (to_state IN ('draft','open','merged','closed')),
    payload_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(payload_json)),
    occurred_at INTEGER NOT NULL,
    UNIQUE(pull_request_id, event_key)
) STRICT;

CREATE TABLE leases (
    resource_type TEXT NOT NULL,
    resource_key TEXT NOT NULL,
    owner TEXT NOT NULL,
    fencing_token INTEGER NOT NULL CHECK (fencing_token > 0),
    acquired_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    PRIMARY KEY(resource_type, resource_key),
    CHECK (expires_at > acquired_at)
) WITHOUT ROWID, STRICT;
CREATE INDEX leases_expiry ON leases(expires_at);

INSERT INTO state_schema(singleton, generation, version, installed_at)
VALUES (1, 'native-authoritative', 1, unixepoch('subsec') * 1000);
PRAGMA user_version = 1;
"#;

#[derive(Clone, Debug)]
pub struct Database {
    path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttemptReceipt {
    pub id: i64,
    pub inserted: bool,
}

#[derive(Clone, Debug)]
pub struct AttemptInput<'a> {
    pub work_item_id: i64,
    pub dedupe_key: &'a str,
    pub agent: &'a str,
    pub status: &'a str,
    pub request_json: &'a str,
    pub response_json: Option<&'a str>,
    pub error: Option<&'a str>,
}

#[derive(Clone, Debug)]
pub struct TrustTranslationInput<'a> {
    pub repository_id: i64,
    pub unit_id: Option<i64>,
    pub locale: &'a str,
    pub source_hash: &'a str,
    pub context_key: &'a str,
    pub target_text: &'a str,
    pub provenance: &'a str,
}

#[derive(Clone, Debug)]
pub struct FindingInput<'a> {
    pub work_item_id: i64,
    pub attempt_id: Option<i64>,
    pub finding_key: &'a str,
    pub severity: &'a str,
    pub code: &'a str,
    pub message: &'a str,
    pub details_json: &'a str,
}

#[derive(Clone, Debug)]
pub struct PullRequestStateInput<'a> {
    pub repository_id: i64,
    pub provider: &'a str,
    pub external_id: &'a str,
    pub number: Option<i64>,
    pub branch: &'a str,
    pub url: Option<&'a str>,
    pub state: &'a str,
    pub head_revision: Option<&'a str>,
    pub event_key: &'a str,
    pub payload_json: &'a str,
}

#[derive(Clone, Debug)]
pub struct CanonicalFileInput<'a> {
    pub repository_id: i64,
    pub locale: &'a str,
    pub path: &'a str,
    pub source_revision: &'a str,
    pub content: &'a [u8],
    pub content_hash: &'a str,
    pub materialized_hash: Option<&'a str>,
    pub state: &'a str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutboxKind {
    Materialization,
    Publication,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutboxEntry {
    pub id: i64,
    pub dedupe_key: String,
    pub payload_json: String,
    pub attempt_count: i64,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalFile {
    pub id: i64,
    pub source_revision: String,
    pub content: Vec<u8>,
    pub content_hash: String,
    pub materialized_hash: Option<String>,
    pub state: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredPullRequest {
    pub external_id: String,
    pub number: Option<i64>,
    pub branch: String,
    pub url: Option<String>,
    pub state: String,
    pub head_revision: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnitHistory {
    pub id: i64,
    pub unit_key: String,
    pub ordinal: usize,
    pub source_text: String,
    pub source_hash: String,
    pub context_json: String,
    pub translation: Option<String>,
    pub trusted: bool,
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
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "journal_mode", "DELETE")?;
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
        let mut conn = self.connect()?;
        let initialized: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='state_schema')",
            [],
            |row| row.get(0),
        )?;
        if initialized {
            let marker: Option<(String, i64)> = conn
                .query_row(
                    "SELECT generation, version FROM state_schema WHERE singleton=1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            match marker {
                Some((generation, SCHEMA_VERSION)) if generation == "native-authoritative" => {
                    return Ok(());
                }
                Some((generation, version)) => {
                    bail!(
                        "unsupported database schema {generation} version {version}; remove the experimental database to initialize the native schema"
                    )
                }
                None => bail!("database has an invalid state_schema marker"),
            }
        }
        let existing_tables: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        if existing_tables != 0 {
            bail!(
                "unsupported pre-native SQLite database; remove it to initialize the native schema"
            );
        }

        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(SCHEMA)?;
        tx.commit()?;
        Ok(())
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
            Some(current) if current.owner != owner && current.expires_at > now => {
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
