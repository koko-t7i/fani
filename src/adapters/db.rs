use crate::adapters::process::process_identity;
use crate::application::ports::{
    AttemptCandidateInput, AttemptInput, AttemptReceipt, CanonicalFile, CanonicalFileInput,
    CanonicalTranslationInput, FailedAttemptContext, FindingInput, OutboxEntry, OutboxKind,
    PublicationManifestInput, PullRequestStateInput, RecoveredAttempt, StateStore,
    StoredPullRequest, TranslationCandidate, TrustTranslationInput, UnitHistory,
};
use crate::domain::document::{UnitProvenance, validate_stored_translation};
use crate::domain::model::{CanonicalTransition, PublicationState};
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use rusqlite::{Connection, MAIN_DB, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

fn bound_request(
    conn: &Connection,
    work_item_id: i64,
    request: &str,
    policy: &str,
) -> Result<String> {
    let provenance = conn.query_row(
        "SELECT d.path,u.source_text,COALESCE(d.source_revision,''),u.context_json FROM work_items w JOIN units u ON u.id=w.unit_id JOIN documents d ON d.id=u.document_id WHERE w.id=?1",
        [work_item_id], |row| Ok(UnitProvenance {
            document_path: row.get(0)?, source: row.get(1)?, source_revision: row.get(2)?,
            context_json: row.get(3)?, policy_fingerprint: policy.into(),
        }))?;
    let mut value: serde_json::Value = serde_json::from_str(request)?;
    if let Some(object) = value.as_object_mut() {
        object.insert("_fani_unit".into(), serde_json::to_value(provenance)?);
    } else {
        bail!("stored Agent request must be an object");
    }
    Ok(serde_json::to_string(&value)?)
}

fn attempt_provenance(conn: &Connection, id: i64) -> Result<Option<UnitProvenance>> {
    let (request, policy, expected_revision): (String, String, Option<String>) = conn.query_row(
        "SELECT a.request_json,a.policy_fingerprint,json_extract(w.input_json,'$.source_revision') FROM attempts a JOIN work_items w ON w.id=a.work_item_id WHERE a.id=?1",
        [id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
    let value: serde_json::Value = serde_json::from_str(&request)?;
    if let Some(snapshot) = value.get("_fani_unit") {
        let parsed = serde_json::from_value::<UnitProvenance>(snapshot.clone()).ok();
        return Ok(parsed.filter(|p| {
            p.policy_fingerprint == policy
                && expected_revision
                    .as_ref()
                    .is_none_or(|rev| rev == &p.source_revision)
        }));
    }
    Ok(conn.query_row(
        "SELECT d.path,uv.source_text,uv.source_revision,uv.context_json FROM translation_versions tv JOIN unit_versions uv ON uv.id=tv.unit_version_id JOIN units u ON u.id=uv.unit_id JOIN documents d ON d.id=u.document_id WHERE tv.source_attempt_id=?1 ORDER BY tv.id LIMIT 1",
        [id], |row| Ok(UnitProvenance { document_path: row.get(0)?, source: row.get(1)?, source_revision: row.get(2)?, context_json: row.get(3)?, policy_fingerprint: policy.clone() }))
        .optional()?.filter(|p| expected_revision.as_ref().is_none_or(|rev| rev == &p.source_revision)))
}

fn checked_candidate_text(
    conn: &Connection,
    unit_id: i64,
    attempt_id: i64,
    text: String,
) -> Result<Option<String>> {
    let Some(bound) = attempt_provenance(conn, attempt_id)? else {
        return Ok(None);
    };
    let current: UnitProvenance = conn.query_row("SELECT d.path,u.source_text,COALESCE(d.source_revision,''),u.context_json FROM units u JOIN documents d ON d.id=u.document_id WHERE u.id=?1", [unit_id], |row| Ok(UnitProvenance { document_path: row.get(0)?, source: row.get(1)?, source_revision: row.get(2)?, context_json: row.get(3)?, policy_fingerprint: bound.policy_fingerprint.clone() }))?;
    let Some(unit) = crate::domain::document::stored_unit(&current) else {
        return Ok(None);
    };
    let output: Option<String> = conn.query_row(
        "SELECT json_extract(response_json,'$.output') FROM attempts WHERE id=?1",
        [attempt_id],
        |row| row.get(0),
    )?;
    Ok((output.as_deref() == Some(text.as_str())
        && crate::domain::document::validate_provenance(
            &current.document_path,
            &unit,
            &bound,
            &text,
        )
        .is_ok())
    .then_some(text))
}

fn metadata_from_key(key: &str, path: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(key).ok()?;
    let object = value.as_object()?;
    if object.len() != 4 || object.get("document")?.as_str()? != path {
        return None;
    }
    Some(
        serde_json::json!({"kind": object.get("kind")?, "context": object.get("context")?,
        "contract": object.get("contract")?, "memory_key": key})
        .to_string(),
    )
}

fn version_provenance(conn: &Connection, version_id: i64) -> Result<UnitProvenance> {
    let (mut provenance, attempt): (UnitProvenance, Option<i64>) = conn.query_row(
        "SELECT d.path,uv.source_text,uv.source_revision,uv.context_json,tv.policy_fingerprint,tv.source_attempt_id FROM translation_versions tv JOIN unit_versions uv ON uv.id=tv.unit_version_id JOIN units u ON u.id=uv.unit_id JOIN documents d ON d.id=u.document_id WHERE tv.id=?1",
        [version_id], |row| Ok((UnitProvenance { document_path: row.get(0)?, source: row.get(1)?, source_revision: row.get(2)?, context_json: row.get(3)?, policy_fingerprint: row.get(4)? }, row.get(5)?)))?;
    let version = provenance.clone();
    if let Some(id) = attempt {
        let snapshot = attempt_provenance(conn, id)?
            .ok_or_else(|| anyhow!("translation has incompatible attempt provenance"))?;
        if snapshot.source != provenance.source
            || snapshot.document_path != provenance.document_path
        {
            bail!("translation version source differs from attempt provenance");
        }
        let output_matches: bool = conn.query_row(
            "SELECT COALESCE(json_extract(a.response_json,'$.output')=tv.target_text,0) FROM translation_versions tv JOIN attempts a ON a.id=tv.source_attempt_id WHERE tv.id=?1",
            [version_id], |row| row.get(0))?;
        if !output_matches {
            bail!("translation version differs from durable attempt output");
        }
        provenance = snapshot;
    }
    let memory: Option<(String,String)> = conn.query_row("SELECT context_key,provenance FROM translation_memory_entries WHERE translation_version_id=?1 ORDER BY id LIMIT 1", [version_id], |row| Ok((row.get(0)?,row.get(1)?))).optional()?;
    if let Some((key, audit)) = memory {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&audit) {
            if let Some(snapshot) = value.get("_fani_unit") {
                let bound: UnitProvenance = serde_json::from_value(snapshot.clone())?;
                let unit = crate::domain::document::stored_unit(&bound)
                    .ok_or_else(|| anyhow!("invalid revalidated parser snapshot"))?;
                let text: String = conn.query_row(
                    "SELECT target_text FROM translation_versions WHERE id=?1",
                    [version_id],
                    |row| row.get(0),
                )?;
                if bound.source != version.source
                    || bound.source_revision != version.source_revision
                    || bound.document_path != version.document_path
                    || bound.policy_fingerprint != version.policy_fingerprint
                    || crate::domain::document::validate_provenance(
                        &bound.document_path,
                        &unit,
                        &provenance,
                        &text,
                    )
                    .is_err()
                {
                    bail!("revalidated snapshot differs from immutable translation provenance");
                }
                provenance = bound;
            }
        }
        bind_memory_metadata(&mut provenance, &key)?;
    }
    Ok(provenance)
}

fn current_memory_key(provenance: &UnitProvenance) -> Result<String> {
    let unit = crate::domain::document::stored_unit(provenance)
        .ok_or_else(|| anyhow!("incompatible stored source unit"))?;
    Ok(unit.memory_context_key(&provenance.document_path))
}

fn bind_memory_metadata(provenance: &mut UnitProvenance, key: &str) -> Result<()> {
    if crate::domain::document::compatible_metadata(provenance).is_none() {
        bail!("incompatible immutable unit context");
    }
    if let Some(metadata) = metadata_from_key(key, &provenance.document_path) {
        let old: serde_json::Value = serde_json::from_str(&provenance.context_json)?;
        let mut new: serde_json::Value = serde_json::from_str(&metadata)?;
        if let Some(source) = old.get("parser_source") {
            new["parser_source"] = source.clone();
        }
        provenance.context_json = new.to_string();
    } else {
        let metadata = crate::domain::document::compatible_metadata(provenance)
            .ok_or_else(|| anyhow!("incompatible stored memory context"))?;
        if key != metadata.kind.as_str() {
            bail!("trusted memory context has a different document identity or contract");
        }
    }
    Ok(())
}

const APPLICATION_ID: i64 = 0x4641_4e49;
const SCHEMA_VERSION: i64 = 3;
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);
static ID_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "0001_native_authority",
        sql: include_str!("../../migrations/0001_native_authority.sql"),
    },
    Migration {
        version: 2,
        name: "0002_orthogonal_translation_state",
        sql: include_str!("../../migrations/0002_orthogonal_translation_state.sql"),
    },
    Migration {
        version: 3,
        name: "0003_document_work_items",
        sql: include_str!("../../migrations/0003_document_work_items.sql"),
    },
];

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
    // DROP TABLE must not cascade through historical attempts and outboxes.
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let result = apply_migrations_exclusive(conn, migrations);
    conn.pragma_update(None, "foreign_keys", "ON")?;
    result
}

fn apply_migrations_exclusive(conn: &mut Connection, migrations: &[Migration]) -> Result<()> {
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

    let tx = conn.transaction_with_behavior(TransactionBehavior::Exclusive)?;
    if app_id == 0 {
        tx.pragma_update(None, "application_id", APPLICATION_ID)?;
        tx.execute_batch(MIGRATION_LEDGER_SQL)?;
    }

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

    if !applied.is_empty() && applied.len() < migrations.len() {
        validate_connection_version(&tx, &migrations[..applied.len()])?;
        require_idle_application(&tx)?;
        backup_before_upgrade(&tx, applied.len() as i64)?;
    }

    for (offset, migration) in migrations.iter().skip(applied.len()).enumerate() {
        if migration.version == 3 {
            validate_work_item_dependents(&tx)?;
        }
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
    validate_physical_integrity(&tx)?;
    tx.commit()?;
    Ok(())
}

fn require_idle_application(conn: &Connection) -> Result<()> {
    let mut statement = conn.prepare("SELECT resource_type,owner FROM leases")?;
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (kind, owner) = row?;
        if kind != "repository" || !dead_repository_owner(&owner) {
            bail!(
                "database upgrade requires all application leases released; stop all fani processes first"
            );
        }
    }
    let mut statement = conn.prepare(
        "SELECT owner FROM materialization_outbox WHERE state='processing' UNION ALL SELECT owner FROM publication_outbox WHERE state='processing'",
    )?;
    for owner in statement.query_map([], |row| row.get::<_, String>(0))? {
        if !dead_process_owner(&owner?) {
            bail!("database upgrade requires all outbox workers stopped");
        }
    }
    Ok(())
}

fn backup_before_upgrade(conn: &Connection, version: i64) -> Result<()> {
    let journal: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    if journal != "delete" {
        bail!("pre-upgrade backup requires DELETE journal mode");
    }
    let source = conn
        .path()
        .ok_or_else(|| anyhow!("upgrade requires a file-backed database"))?;
    let source = Path::new(source);
    let parent = source.parent().unwrap_or_else(|| Path::new("."));
    let backup = PathBuf::from(format!("{}.pre-schema-{version}.bak", source.display()));
    let temporary = tempfile::NamedTempFile::new_in(parent)?;
    // The exclusive DELETE-journal transaction has not written any pages yet.
    fs::copy(source, temporary.path())?;
    temporary.as_file().sync_all()?;
    let snapshot = Connection::open(temporary.path())?;
    validate_connection_version(&snapshot, &MIGRATIONS[..version as usize])?;
    drop(snapshot);
    temporary.persist_noclobber(&backup).map_err(|error| error.error)
        .with_context(|| format!("cannot preserve pre-upgrade backup {}; move any existing backup aside before retrying", backup.display()))?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn validate_work_item_dependents(conn: &Connection) -> Result<()> {
    let mut statement = conn.prepare(
        "WITH RECURSIVE dependents(name) AS (VALUES ('work_items') UNION SELECT m.name FROM sqlite_schema m JOIN pragma_foreign_key_list(m.name) f JOIN dependents d ON f.\"table\"=d.name WHERE m.type='table') SELECT m.name,f.\"from\",f.\"table\",f.\"to\",f.on_delete FROM sqlite_schema m JOIN pragma_foreign_key_list(m.name) f WHERE m.type='table' AND f.\"table\" IN (SELECT name FROM dependents) ORDER BY 1,2,3,4,5"
    )?;
    let actual = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let expected = [
        ("attempts", "work_item_id", "work_items", "id", "CASCADE"),
        (
            "canonical_candidates",
            "source_attempt_id",
            "attempts",
            "id",
            "SET NULL",
        ),
        (
            "canonical_file_translations",
            "translation_version_id",
            "translation_versions",
            "id",
            "RESTRICT",
        ),
        ("findings", "attempt_id", "attempts", "id", "CASCADE"),
        ("findings", "work_item_id", "work_items", "id", "CASCADE"),
        (
            "materialization_outbox",
            "work_item_id",
            "work_items",
            "id",
            "CASCADE",
        ),
        (
            "translation_memory_entries",
            "translation_version_id",
            "translation_versions",
            "id",
            "SET NULL",
        ),
        (
            "translation_versions",
            "source_attempt_id",
            "attempts",
            "id",
            "SET NULL",
        ),
    ]
    .map(|(a, b, c, d, e)| (a.into(), b.into(), c.into(), d.into(), e.into()));
    if actual != expected {
        bail!(
            "unexpected work_items reverse foreign-key graph; refusing table rebuild: {actual:?}"
        );
    }
    let extensions: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_schema WHERE tbl_name='work_items' AND (type='trigger' OR (type='index' AND sql IS NOT NULL AND name!='work_items_claim'))",
        [], |row| row.get(0))?;
    if extensions != 0 {
        bail!("unexpected work_items indexes or triggers; refusing table rebuild");
    }
    Ok(())
}

fn validate_physical_integrity(conn: &Connection) -> Result<()> {
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

fn validate_existing_connection(conn: &Connection) -> Result<()> {
    validate_connection_version(conn, MIGRATIONS)
}

fn validate_connection_version(conn: &Connection, migrations: &[Migration]) -> Result<()> {
    let expected_version = migrations.last().map_or(0, |migration| migration.version);
    let actual = application_id(conn)?;
    if actual != APPLICATION_ID {
        bail!(
            "SQLite application_id {actual:#x} does not identify a fani database ({APPLICATION_ID:#x})"
        );
    }
    validate_physical_integrity(conn)?;
    let user_version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if user_version != expected_version {
        bail!(
            "SQLite user_version is {user_version}; expected migration version {expected_version}"
        );
    }
    let marker: Option<(String, i64)> = conn
        .query_row(
            "SELECT generation,version FROM state_schema WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if marker.as_ref() != Some(&("native-authoritative".to_owned(), expected_version)) {
        bail!("database has an invalid state_schema marker");
    }
    let applied = {
        let mut statement =
            conn.prepare("SELECT version,name,checksum FROM schema_migrations ORDER BY version")?;
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
    if applied.len() != migrations.len() {
        bail!(
            "database has {} migrations; expected {}",
            applied.len(),
            migrations.len()
        );
    }
    for (migration, (version, name, checksum)) in migrations.iter().zip(applied) {
        let expected_checksum = migration_checksum(migration.sql);
        if version != migration.version || name != migration.name || checksum != expected_checksum {
            bail!(
                "migration {} checksum/name mismatch: database has {name} {checksum}, binary expects {} {expected_checksum}",
                migration.version,
                migration.name
            );
        }
    }
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
        db.integrity_check()?;
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
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version != SCHEMA_VERSION {
            bail!("SQLite user_version is {version}; expected migration version {SCHEMA_VERSION}");
        }
        Ok(conn)
    }

    pub fn snapshot(source: &Path, destination: &Path) -> Result<()> {
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create snapshot directory {}", parent.display()))?;
        let conn = Connection::open_with_flags(
            source,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("cannot open SQLite database {} read-only", source.display()))?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        validate_existing_connection(&conn)?;

        let temporary = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("cannot create temporary snapshot in {}", parent.display()))?;
        conn.backup(MAIN_DB, temporary.path(), None)
            .with_context(|| {
                format!(
                    "cannot snapshot database {} to {}",
                    source.display(),
                    destination.display()
                )
            })?;
        temporary.as_file().sync_all()?;
        let snapshot = Connection::open_with_flags(
            temporary.path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        validate_existing_connection(&snapshot)?;
        drop(snapshot);
        temporary
            .persist(destination)
            .map_err(|error| error.error)
            .with_context(|| {
                format!("cannot install database snapshot {}", destination.display())
            })?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    }

    pub fn restore(source: &Path, destination: &Path) -> Result<Self> {
        Self::snapshot(source, destination).with_context(|| {
            format!(
                "cannot restore database {} from {}",
                destination.display(),
                source.display()
            )
        })?;
        Self::open(destination)
    }

    pub fn migrate(&self) -> Result<()> {
        let mut conn = open_connection(&self.path)?;
        validate_physical_integrity(&conn)?;
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
        validate_existing_connection(&self.connect()?)
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

    pub fn document_id(&self, repository_id: i64, path: &str) -> Result<Option<i64>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT id FROM documents WHERE repository_id=?1 AND path=?2 AND deleted_at IS NULL",
                params![repository_id, path],
                |row| row.get(0),
            )
            .optional()?)
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
        let unit_id = conn.query_row(
            "SELECT id FROM units WHERE document_id=?1 AND unit_key=?2",
            params![document_id, unit_key],
            |row| row.get(0),
        )?;
        let source_revision: String = conn.query_row(
            "SELECT COALESCE(source_revision,'') FROM documents WHERE id=?1",
            [document_id],
            |row| row.get(0),
        )?;
        conn.execute(
            r#"INSERT OR IGNORE INTO unit_versions(
                   unit_id,source_revision,source_text,source_hash,context_json,created_at)
               VALUES (?1,?2,?3,?4,?5,?6)"#,
            params![
                unit_id,
                source_revision,
                source_text,
                source_hash,
                context_json,
                now
            ],
        )?;
        Ok(unit_id)
    }

    pub fn unit_history(&self, document_id: i64, locale: &str) -> Result<Vec<UnitHistory>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare(
            r#"SELECT u.id,u.unit_key,u.ordinal,COALESCE(uv.source_text,u.source_text),
                      COALESCE(uv.source_hash,u.source_hash),COALESCE(uv.context_json,u.context_json),
                      CASE WHEN uv.id IS NOT NULL THEN t.target_text END,
                      CASE WHEN uv.id IS NULL THEN 0 ELSE 1 END
               FROM units u
               LEFT JOIN translation_memory_entries t ON t.id=(
                 SELECT m.id FROM translation_memory_entries m WHERE m.unit_id=u.id AND m.locale=?2
                   AND m.tier='trusted' AND m.superseded_at IS NULL ORDER BY m.id DESC LIMIT 1)
               LEFT JOIN translation_versions tv ON tv.id=t.translation_version_id
               LEFT JOIN unit_versions uv ON uv.id=COALESCE(tv.unit_version_id,
                 (SELECT v.id FROM unit_versions v WHERE v.unit_id=t.unit_id AND v.source_hash=t.source_hash
                    AND (t.source_revision='' OR v.source_revision=t.source_revision) ORDER BY v.id LIMIT 1))
                 AND uv.source_hash=t.source_hash
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
        require_fingerprint(input.policy_fingerprint, "translation policy")?;
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let unit_id = input
            .unit_id
            .ok_or_else(|| anyhow!("trusted memory requires a source unit"))?;
        let (unit_version_id, mut bound): (i64, UnitProvenance) = tx.query_row(
            "SELECT uv.id,d.path,uv.source_text,uv.source_revision,uv.context_json FROM unit_versions uv JOIN units u ON u.id=uv.unit_id JOIN documents d ON d.id=u.document_id WHERE u.id=?1 AND d.repository_id=?2 AND uv.source_hash=?3 ORDER BY uv.id DESC LIMIT 1",
            params![unit_id,input.repository_id,input.source_hash], |row| Ok((row.get(0)?, UnitProvenance {
                document_path: row.get(1)?, source: row.get(2)?, source_revision: row.get(3)?, context_json: row.get(4)?, policy_fingerprint: input.policy_fingerprint.into(),
            })))?;
        let original = crate::domain::document::compatible_metadata(&bound)
            .ok_or_else(|| anyhow!("incompatible immutable unit context"))?;
        if original.parser_source.is_none() {
            let current: Option<String> = tx.query_row("SELECT u.context_json FROM units u JOIN documents d ON d.id=u.document_id WHERE u.id=?1 AND u.source_text=?2 AND d.source_revision=?3", params![unit_id,bound.source,bound.source_revision], |row| row.get(0)).optional()?;
            if let Some(current) = current {
                let mut snapshot = bound.clone();
                snapshot.context_json = current;
                if crate::domain::document::compatible_metadata(&snapshot)
                    .is_some_and(|metadata| metadata.kind == original.kind)
                {
                    bound = snapshot;
                }
            }
        }
        bind_memory_metadata(&mut bound, input.context_key)?;
        if !validate_stored_translation(&bound, input.target_text) {
            bail!("trusted translation failed document compatibility validation");
        }
        let key = current_memory_key(&bound)?;
        if let Some(id) = tx.query_row(
            "SELECT id FROM translation_memory_entries WHERE repository_id=?1 AND locale=?2 AND source_hash=?3 AND context_key=?4 AND target_text=?5 AND tier='trusted' AND superseded_at IS NULL",
            params![input.repository_id,input.locale,input.source_hash,key,input.target_text], |row| row.get(0)).optional()? {
            return Ok(id);
        }
        let now = now_ms();
        tx.execute("UPDATE translation_memory_entries SET tier='history',superseded_at=?5 WHERE repository_id=?1 AND locale=?2 AND source_hash=?3 AND context_key=?4 AND tier='trusted' AND superseded_at IS NULL",
            params![input.repository_id,input.locale,input.source_hash,key,now])?;
        tx.execute("INSERT INTO translation_versions(unit_version_id,locale,target_text,target_hash,freshness,provenance,validation_state,review_state,publication_state,policy_fingerprint,created_at) VALUES (?1,?2,?3,?4,'exact','trusted_tm','passed','approved','candidate',?5,?6)",
            params![unit_version_id,input.locale,input.target_text,migration_checksum(input.target_text),input.policy_fingerprint,now])?;
        let version_id = tx.last_insert_rowid();
        let audit =
            serde_json::json!({"origin": input.provenance, "_fani_unit": bound}).to_string();
        tx.execute("INSERT INTO translation_memory_entries(repository_id,unit_id,translation_version_id,locale,source_hash,source_revision,context_key,target_text,tier,provenance,policy_fingerprint,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'trusted',?9,?10,?11)",
            params![input.repository_id,unit_id,version_id,input.locale,input.source_hash,bound.source_revision,key,input.target_text,audit,input.policy_fingerprint,now])?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    pub fn begin_run(
        &self,
        repository_id: i64,
        invocation_key: &str,
        config_path: &Path,
        metadata_json: &str,
        policy_fingerprint: &str,
    ) -> Result<String> {
        require_json(metadata_json)?;
        require_fingerprint(policy_fingerprint, "run policy")?;
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
            "INSERT INTO runs(id,repository_id,invocation_key,config_path,started_at,heartbeat_at,metadata_json,policy_fingerprint) VALUES (?1,?2,?3,?4,?5,?5,?6,?7)",
            params![id, repository_id, invocation_key, config_path.display().to_string(), now, metadata_json, policy_fingerprint],
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
            r#"INSERT INTO work_items(
                   run_id,unit_id,locale,kind,priority,input_json,created_at,updated_at,policy_fingerprint)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?7,
                       (SELECT policy_fingerprint FROM runs WHERE id=?1))
               ON CONFLICT(run_id,unit_id,locale,kind) WHERE unit_id IS NOT NULL DO UPDATE SET
                 priority=excluded.priority,
                 input_json=excluded.input_json,
                 policy_fingerprint=excluded.policy_fingerprint,
                 updated_at=excluded.updated_at"#,
            params![run_id, unit_id, locale, kind, priority, input_json, now],
        )?;
        Ok(conn.query_row(
            "SELECT id FROM work_items WHERE run_id=?1 AND unit_id=?2 AND locale=?3 AND kind=?4",
            params![run_id, unit_id, locale, kind],
            |row| row.get(0),
        )?)
    }

    pub fn enqueue_document_work_item(
        &self,
        run_id: &str,
        document_id: i64,
        locale: &str,
        kind: &str,
        priority: i64,
        input_json: &str,
    ) -> Result<i64> {
        require_json(input_json)?;
        if !matches!(kind, "assembly" | "materialization" | "project_check") {
            bail!("document work must be assembly, materialization, or project_check");
        }
        let conn = self.connect()?;
        let same_repository: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM documents d JOIN runs r ON r.repository_id=d.repository_id WHERE d.id=?1 AND r.id=?2)",
            params![document_id, run_id], |row| row.get(0))?;
        if !same_repository {
            bail!("document work must belong to the run's repository");
        }
        conn.execute(
            r#"INSERT INTO work_items(
                   run_id,document_id,locale,kind,priority,input_json,created_at,updated_at,policy_fingerprint)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?7,
                       (SELECT policy_fingerprint FROM runs WHERE id=?1))
               ON CONFLICT(run_id,document_id,locale,kind) WHERE document_id IS NOT NULL DO UPDATE SET
                 priority=excluded.priority,
                 input_json=excluded.input_json,
                 policy_fingerprint=excluded.policy_fingerprint,
                 updated_at=excluded.updated_at"#,
            params![run_id, document_id, locale, kind, priority, input_json, now_ms()],
        )?;
        Ok(conn.query_row(
            "SELECT id FROM work_items WHERE run_id=?1 AND document_id=?2 AND locale=?3 AND kind=?4",
            params![run_id, document_id, locale, kind],
            |row| row.get(0),
        )?)
    }

    pub fn record_attempt(&self, input: AttemptInput<'_>) -> Result<AttemptReceipt> {
        require_attempt_provenance(&input)?;
        require_json(input.request_json)?;
        let request_json = bound_request(
            &self.connect()?,
            input.work_item_id,
            input.request_json,
            input.policy_fingerprint,
        )?;
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
            r#"INSERT INTO attempts(
                   work_item_id,dedupe_key,attempt_no,agent,provider,model,adapter,
                   provider_fingerprint,prompt_version,prompt_hash,policy_fingerprint,status,
                   request_json,response_json,error,started_at,finished_at)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,
                       CASE WHEN ?12='started' THEN NULL ELSE ?16 END)"#,
            params![
                input.work_item_id,
                input.dedupe_key,
                attempt_no,
                input.agent,
                input.provider,
                input.model,
                input.adapter,
                input.provider_fingerprint,
                input.prompt_version,
                input.prompt_hash,
                input.policy_fingerprint,
                input.status,
                request_json,
                input.response_json,
                input.error,
                now
            ],
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

    pub fn failed_attempt_context(
        &self,
        work_item_id: i64,
    ) -> Result<Option<FailedAttemptContext>> {
        let conn = self.connect()?;
        let attempt: Option<(i64, Option<String>, Option<String>)> = conn
            .query_row(
                r#"SELECT id,response_json,error FROM attempts
                   WHERE work_item_id=?1 AND status!='succeeded'
                   ORDER BY id DESC LIMIT 1"#,
                [work_item_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((attempt_id, response_json, error)) = attempt else {
            return Ok(None);
        };
        let output = if let Some(response) = response_json {
            let value: serde_json::Value = serde_json::from_str(&response)
                .context("failed Agent response is not valid JSON")?;
            value
                .get("output")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        } else {
            None
        };
        Ok(Some(FailedAttemptContext {
            attempt_id,
            output,
            error,
        }))
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
                .unwrap_or("");
            Ok(RecoveredAttempt {
                id,
                dedupe_key: dedupe_key.to_owned(),
                output: output.to_owned(),
                request_json: self.connect()?.query_row(
                    "SELECT request_json FROM attempts WHERE id=?1",
                    [id],
                    |row| row.get(0),
                )?,
                provenance: attempt_provenance(&self.connect()?, id)?,
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
        require_fingerprint(input.policy_fingerprint, "translation policy")?;
        require_attempt_provenance(&input.attempt)?;
        require_json(input.attempt.request_json)?;
        let request_json = bound_request(
            &self.connect()?,
            input.attempt.work_item_id,
            input.attempt.request_json,
            input.attempt.policy_fingerprint,
        )?;
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
                r#"INSERT INTO attempts(
                       work_item_id,dedupe_key,attempt_no,agent,provider,model,adapter,
                       provider_fingerprint,prompt_version,prompt_hash,policy_fingerprint,status,
                       request_json,response_json,error,started_at,finished_at)
                   VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'succeeded',?12,?13,NULL,?14,?14)"#,
                params![
                    input.attempt.work_item_id,
                    input.attempt.dedupe_key,
                    attempt_no,
                    input.attempt.agent,
                    input.attempt.provider,
                    input.attempt.model,
                    input.attempt.adapter,
                    input.attempt.provider_fingerprint,
                    input.attempt.prompt_version,
                    input.attempt.prompt_hash,
                    input.attempt.policy_fingerprint,
                    request_json,
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
        let (unit_version_id, repository_id, source_hash, source_revision, context_key): (
            i64,
            i64,
            String,
            String,
            String,
        ) = tx.query_row(
            r#"SELECT uv.id,d.repository_id,uv.source_hash,uv.source_revision,
                          COALESCE(json_extract(u.context_json,'$.memory_key'),json_extract(u.context_json,'$.kind'),'')
                   FROM units u
                   JOIN documents d ON d.id=u.document_id
                   JOIN unit_versions uv ON uv.unit_id=u.id AND uv.source_hash=u.source_hash
                   WHERE u.id=?1 ORDER BY uv.id DESC LIMIT 1"#,
            [input.unit_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
        let translation_version_id = if let Some(id) = tx
            .query_row(
                "SELECT id FROM translation_versions WHERE source_attempt_id=?1",
                [attempt_id],
                |row| row.get(0),
            )
            .optional()?
        {
            id
        } else {
            tx.execute(
                r#"INSERT INTO translation_versions(
                       unit_version_id,locale,target_text,target_hash,freshness,provenance,
                       validation_state,review_state,publication_state,policy_fingerprint,
                       source_attempt_id,created_at)
                   VALUES (?1,?2,?3,?4,'exact',?5,'passed','unreviewed','candidate',?6,?7,?8)"#,
                params![
                    unit_version_id,
                    input.locale,
                    input.target_text,
                    migration_checksum(input.target_text),
                    input.provenance.as_str(),
                    input.policy_fingerprint,
                    attempt_id,
                    now_ms()
                ],
            )?;
            tx.last_insert_rowid()
        };
        let updated = tx.execute(
            r#"UPDATE translation_memory_entries
               SET unit_id=?2,translation_version_id=?3,source_revision=?6,target_text=?8,provenance=?9,
                   policy_fingerprint=?10,created_at=?11
               WHERE repository_id=?1 AND locale=?4 AND source_hash=?5 AND context_key=?7
                 AND tier='candidate' AND superseded_at IS NULL"#,
            params![
                repository_id,
                input.unit_id,
                translation_version_id,
                input.locale,
                source_hash,
                source_revision,
                context_key,
                input.target_text,
                input.provenance.as_str(),
                input.policy_fingerprint,
                now_ms()
            ],
        )?;
        if updated == 0 {
            tx.execute(
                r#"INSERT INTO translation_memory_entries(
                       repository_id,unit_id,translation_version_id,locale,source_hash,source_revision,context_key,
                       target_text,tier,provenance,policy_fingerprint,created_at)
                   VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'candidate',?9,?10,?11)"#,
                params![
                    repository_id,
                    input.unit_id,
                    translation_version_id,
                    input.locale,
                    source_hash,
                    source_revision,
                    context_key,
                    input.target_text,
                    input.provenance.as_str(),
                    input.policy_fingerprint,
                    now_ms()
                ],
            )?;
        }
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
        if let Some(attempt) = source_attempt_id {
            if let Some(provenance) = attempt_provenance(&self.connect()?, attempt)? {
                if crate::domain::document::compatible_metadata(&provenance)
                    .is_some_and(|metadata| metadata.parser_source.is_none())
                {
                    self.revalidate_candidate(
                        unit_id,
                        locale,
                        &TranslationCandidate {
                            unit_id,
                            text: target_text.into(),
                            provenance,
                            trusted: false,
                            run_id: None,
                            invocation_key: None,
                            deterministic_model: None,
                        },
                    )?;
                }
            }
        }
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

    pub fn persist_canonical_file(
        &self,
        input: CanonicalFileInput<'_>,
        translations: &[CanonicalTranslationInput<'_>],
    ) -> Result<CanonicalFile> {
        let CanonicalFileInput {
            repository_id,
            locale,
            path,
            source_revision,
            content,
            content_hash,
            materialized_hash,
            freshness,
            provenance,
            validation,
            review,
            publication,
            trust_tier,
            policy_fingerprint,
        } = input;
        require_fingerprint(policy_fingerprint, "translation policy")?;
        let state = if trust_tier.as_str() == "trusted" {
            "adopted"
        } else {
            "candidate"
        };
        let now = now_ms();
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            r#"INSERT INTO canonical_files(
                   repository_id,locale,path,source_revision,content,content_hash,materialized_hash,
                   state,freshness,provenance,validation_state,review_state,publication_state,
                   trust_tier,policy_fingerprint,updated_at)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
               ON CONFLICT(repository_id,locale,path) DO UPDATE SET
                 source_revision=excluded.source_revision,content=excluded.content,
                 content_hash=excluded.content_hash,materialized_hash=excluded.materialized_hash,
                 state=excluded.state,freshness=excluded.freshness,provenance=excluded.provenance,
                 validation_state=excluded.validation_state,review_state=excluded.review_state,
                 publication_state=excluded.publication_state,trust_tier=excluded.trust_tier,
                 policy_fingerprint=excluded.policy_fingerprint,updated_at=excluded.updated_at"#,
            params![
                repository_id,
                locale,
                path,
                source_revision,
                content,
                content_hash,
                materialized_hash,
                state,
                freshness.as_str(),
                provenance.as_str(),
                validation.as_str(),
                review.as_str(),
                publication.as_str(),
                trust_tier.as_str(),
                policy_fingerprint,
                now
            ],
        )?;
        let canonical_file_id: i64 = tx.query_row(
            "SELECT id FROM canonical_files WHERE repository_id=?1 AND locale=?2 AND path=?3",
            params![repository_id, locale, path],
            |row| row.get(0),
        )?;
        let existing_content: Option<(i64, String, Vec<u8>)> = tx
            .query_row(
                "SELECT id,source_revision,content FROM canonical_content_versions WHERE canonical_file_id=?1 AND content_hash=?2",
                params![canonical_file_id, content_hash],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((_, stored_revision, stored_content)) = &existing_content {
            if stored_revision != source_revision || stored_content != content {
                bail!("canonical content hash conflicts with immutable durable content");
            }
        }
        tx.execute(
            r#"INSERT OR IGNORE INTO canonical_content_versions(
                   canonical_file_id,source_revision,content,content_hash,publication_state,created_at)
               VALUES (?1,?2,?3,?4,?5,?6)"#,
            params![
                canonical_file_id,
                source_revision,
                content,
                content_hash,
                publication.as_str(),
                now
            ],
        )?;
        let content_version_id: i64 = tx.query_row(
            "SELECT id FROM canonical_content_versions WHERE canonical_file_id=?1 AND content_hash=?2",
            params![canonical_file_id, content_hash],
            |row| row.get(0),
        )?;
        tx.execute(
            "UPDATE canonical_files SET current_content_version_id=?2 WHERE id=?1",
            params![canonical_file_id, content_version_id],
        )?;
        crate::adapters::failpoint::reach("canonical_content_before_translation_links");
        let mut translation_version_ids = Vec::with_capacity(translations.len());
        for translation in translations {
            let unit_version: (i64, String, String) = tx
                .query_row(
                    r#"SELECT uv.id,uv.source_hash,
                              COALESCE(json_extract(uv.context_json,'$.memory_key'),json_extract(uv.context_json,'$.kind'),'')
                       FROM unit_versions uv
                       JOIN units u ON u.id=uv.unit_id
                       JOIN documents d ON d.id=u.document_id
                       WHERE uv.unit_id=?1 AND uv.source_revision=?2
                         AND d.repository_id=?3
                       ORDER BY uv.id DESC LIMIT 1"#,
                    params![translation.unit_id, source_revision, repository_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?
                .ok_or_else(|| {
                    anyhow!(
                        "unit {} has no immutable source version for revision {}",
                        translation.unit_id,
                        source_revision
                    )
                })?;
            let translation_version_id: Option<i64> = tx
                .query_row(
                    r#"SELECT tv.id
                       FROM translation_versions tv
                       WHERE tv.unit_version_id=?1 AND tv.locale=?2 AND tv.target_text=?3
                         AND tv.superseded_at IS NULL AND tv.validation_state='passed'
                       ORDER BY CASE WHEN EXISTS(SELECT 1 FROM canonical_file_translations cft WHERE cft.canonical_content_version_id=?4 AND cft.translation_version_id=tv.id) THEN 0 ELSE 1 END,tv.id DESC LIMIT 1"#,
                    params![unit_version.0, locale, translation.target_text, content_version_id],
                    |row| row.get(0),
                )
                .optional()?;
            let translation_version_id = if let Some(id) = translation_version_id {
                id
            } else {
                let tm_tier: Option<String> = tx
                    .query_row(
                        r#"SELECT tier FROM translation_memory_entries
                           WHERE repository_id=?1 AND unit_id=?2 AND locale=?3
                             AND source_hash=?4 AND context_key=?5 AND target_text=?6
                             AND superseded_at IS NULL
                           ORDER BY CASE tier WHEN 'trusted' THEN 0 ELSE 1 END,id DESC LIMIT 1"#,
                        params![
                            repository_id,
                            translation.unit_id,
                            locale,
                            unit_version.1,
                            unit_version.2,
                            translation.target_text
                        ],
                        |row| row.get(0),
                    )
                    .optional()?;
                let version_provenance = match tm_tier.as_deref() {
                    Some("trusted") => "trusted_tm",
                    Some("candidate") => "candidate_tm",
                    _ => provenance.as_str(),
                };
                tx.execute(
                    r#"INSERT INTO translation_versions(
                           unit_version_id,locale,target_text,target_hash,freshness,provenance,
                           validation_state,review_state,publication_state,policy_fingerprint,created_at)
                       VALUES (?1,?2,?3,?4,'exact',?5,'passed',?6,?7,?8,?9)"#,
                    params![
                        unit_version.0,
                        locale,
                        translation.target_text,
                        migration_checksum(translation.target_text),
                        version_provenance,
                        review.as_str(),
                        publication.as_str(),
                        policy_fingerprint,
                        now
                    ],
                )?;
                tx.last_insert_rowid()
            };
            let bound = version_provenance(&tx, translation_version_id)?;
            if !validate_stored_translation(&bound, translation.target_text) {
                bail!("canonical translation failed document compatibility validation");
            }
            translation_version_ids.push(translation_version_id);
        }
        if !translation_version_ids.is_empty() {
            translation_version_ids.sort_unstable();
            translation_version_ids.dedup();
            let stored = {
                let mut statement = tx.prepare(
                    "SELECT translation_version_id FROM canonical_file_translations WHERE canonical_content_version_id=?1 ORDER BY translation_version_id",
                )?;
                statement
                    .query_map([content_version_id], |row| row.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            if stored.is_empty() {
                for translation_version_id in &translation_version_ids {
                    tx.execute(
                        "INSERT INTO canonical_file_translations(canonical_content_version_id,translation_version_id) VALUES (?1,?2)",
                        params![content_version_id, translation_version_id],
                    )?;
                }
            } else if stored != translation_version_ids {
                let mut statement = tx.prepare("SELECT cft.translation_version_id FROM canonical_file_translations cft JOIN translation_versions tv ON tv.id=cft.translation_version_id WHERE cft.canonical_content_version_id=?1 AND tv.superseded_at IS NULL ORDER BY cft.translation_version_id")?;
                let active = statement
                    .query_map([content_version_id], |row| row.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                if active.len() == stored.len()
                    || active
                        .iter()
                        .any(|id| !translation_version_ids.contains(id))
                {
                    bail!(
                        "canonical content translation set conflicts with immutable durable links"
                    );
                }
                // Revalidated identical bytes retain quarantined links as history.
                for id in &translation_version_ids {
                    tx.execute("INSERT OR IGNORE INTO canonical_file_translations(canonical_content_version_id,translation_version_id) VALUES (?1,?2)", params![content_version_id,id])?;
                }
            }
        }
        tx.commit()?;
        Ok(CanonicalFile {
            id: canonical_file_id,
            content_version_id,
            source_revision: source_revision.to_owned(),
            content: content.to_vec(),
            content_hash: content_hash.to_owned(),
            materialized_hash: materialized_hash.map(str::to_owned),
            state: state.to_owned(),
        })
    }

    pub fn record_canonical_file_translations(
        &self,
        canonical_file_id: i64,
        translations: &[CanonicalTranslationInput<'_>],
        locale: &str,
    ) -> Result<usize> {
        let conn = self.connect()?;
        let row: (i64, String, String, Vec<u8>, String, Option<String>, String) = conn
            .query_row(
                r#"SELECT repository_id,path,source_revision,content,content_hash,
                          materialized_hash,policy_fingerprint
                   FROM canonical_files WHERE id=?1 AND locale=?2"#,
                params![canonical_file_id, locale],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| {
                anyhow!("canonical file {canonical_file_id} does not exist for {locale}")
            })?;
        drop(conn);
        self.persist_canonical_file(
            CanonicalFileInput {
                repository_id: row.0,
                locale,
                path: &row.1,
                source_revision: &row.2,
                content: &row.3,
                content_hash: &row.4,
                materialized_hash: row.5.as_deref(),
                freshness: crate::domain::model::Freshness::Exact,
                provenance: crate::domain::model::TranslationProvenance::Ai,
                validation: crate::domain::model::ValidationState::Passed,
                review: crate::domain::model::ReviewState::Unreviewed,
                publication: PublicationState::Candidate,
                trust_tier: crate::domain::model::MemoryTier::Candidate,
                policy_fingerprint: &row.6,
            },
            translations,
        )?;
        Ok(translations.len())
    }

    pub fn record_publication_manifest(&self, input: PublicationManifestInput<'_>) -> Result<i64> {
        let PublicationManifestInput {
            repository_id,
            run_id,
            locale,
            source_revision,
            candidate_commit,
            policy_fingerprint,
            files,
        } = input;
        require_fingerprint(policy_fingerprint, "publication policy")?;
        if candidate_commit.is_empty() {
            bail!("publication candidate commit cannot be empty");
        }
        if files.is_empty() {
            bail!("publication manifest must contain at least one canonical file");
        }
        let mut requested = files
            .iter()
            .map(|file| {
                (
                    file.canonical_content_version_id,
                    file.canonical_file_id,
                    file.content_hash.clone(),
                )
            })
            .collect::<Vec<_>>();
        requested.sort();
        requested.dedup();
        if requested.len() != files.len() {
            bail!("publication manifest contains duplicate canonical content versions");
        }
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run: Option<(i64, String)> = tx
            .query_row(
                "SELECT repository_id,policy_fingerprint FROM runs WHERE id=?1",
                [run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if run.as_ref() != Some(&(repository_id, policy_fingerprint.to_owned())) {
            bail!("publication run does not match repository and policy fingerprint");
        }
        let now = now_ms();
        let inserted = tx.execute(
            r#"INSERT OR IGNORE INTO publication_manifests(
                   repository_id,run_id,locale,source_revision,candidate_commit,
                   policy_fingerprint,state,created_at)
               VALUES (?1,?2,?3,?4,?5,?6,'commit_created',?7)"#,
            params![
                repository_id,
                run_id,
                locale,
                source_revision,
                candidate_commit,
                policy_fingerprint,
                now
            ],
        )? == 1;
        let manifest: (i64, String, String, String) = tx.query_row(
            r#"SELECT id,run_id,source_revision,policy_fingerprint
               FROM publication_manifests
               WHERE repository_id=?1 AND locale=?2 AND candidate_commit=?3"#,
            params![repository_id, locale, candidate_commit],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        if manifest.1 != run_id || manifest.2 != source_revision || manifest.3 != policy_fingerprint
        {
            bail!("publication candidate commit conflicts with its durable manifest");
        }
        for file in files {
            let valid = tx.query_row(
                r#"SELECT EXISTS(
                       SELECT 1
                       FROM canonical_content_versions ccv
                       JOIN canonical_files cf ON cf.id=ccv.canonical_file_id
                       WHERE ccv.id=?1 AND ccv.canonical_file_id=?2 AND ccv.content_hash=?3
                         AND ccv.source_revision=?4 AND cf.repository_id=?5 AND cf.locale=?6)"#,
                params![
                    file.canonical_content_version_id,
                    file.canonical_file_id,
                    file.content_hash,
                    source_revision,
                    repository_id,
                    locale
                ],
                |row| row.get::<_, i64>(0),
            )? != 0;
            if !valid {
                bail!(
                    "canonical content version {} does not match publication manifest content",
                    file.canonical_content_version_id
                );
            }
        }
        let stored = {
            let mut statement = tx.prepare(
                "SELECT canonical_content_version_id,canonical_file_id,content_hash FROM publication_manifest_files WHERE manifest_id=?1 ORDER BY canonical_content_version_id,canonical_file_id,content_hash",
            )?;
            statement
                .query_map([manifest.0], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if inserted {
            for file in files {
                tx.execute(
                    "INSERT INTO publication_manifest_files(manifest_id,canonical_content_version_id,canonical_file_id,content_hash) VALUES (?1,?2,?3,?4)",
                    params![manifest.0, file.canonical_content_version_id, file.canonical_file_id, file.content_hash],
                )?;
            }
        } else if stored != requested {
            bail!("publication manifest file set conflicts with durable state");
        }
        tx.execute(
            r#"UPDATE canonical_content_versions SET publication_state=
                   CASE WHEN publication_state='merged' THEN 'merged' ELSE 'commit_created' END
               WHERE id IN (SELECT canonical_content_version_id FROM publication_manifest_files WHERE manifest_id=?1)"#,
            [manifest.0],
        )?;
        tx.execute(
            r#"UPDATE canonical_files SET publication_state=
                   CASE WHEN publication_state='merged' THEN 'merged' ELSE 'commit_created' END,
                   updated_at=?2
               WHERE current_content_version_id IN (
                   SELECT canonical_content_version_id FROM publication_manifest_files WHERE manifest_id=?1)"#,
            params![manifest.0, now],
        )?;
        tx.execute(
            r#"UPDATE translation_versions SET publication_state=
                   CASE WHEN publication_state='merged' THEN 'merged' ELSE 'commit_created' END
               WHERE id IN (
                   SELECT cft.translation_version_id
                   FROM publication_manifest_files pmf
                   JOIN canonical_file_translations cft
                     ON cft.canonical_content_version_id=pmf.canonical_content_version_id
                   WHERE pmf.manifest_id=?1)"#,
            [manifest.0],
        )?;
        tx.commit()?;
        Ok(manifest.0)
    }

    pub fn transition_publication_manifest(
        &self,
        repository_id: i64,
        locale: &str,
        candidate_commit: &str,
        state: PublicationState,
    ) -> Result<()> {
        let state = state.as_str();
        if !matches!(
            state,
            "commit_created" | "push_pending" | "pr_open" | "superseded"
        ) {
            bail!("publication manifest cannot transition directly to {state}");
        }
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let manifest: (i64, String) = tx
            .query_row(
                "SELECT id,state FROM publication_manifests WHERE repository_id=?1 AND locale=?2 AND candidate_commit=?3",
                params![repository_id, locale, candidate_commit],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| anyhow!("publication manifest for commit {candidate_commit} does not exist"))?;
        let effective = if manifest.1 == "merged"
            || manifest.1 == "superseded"
            || publication_rank(&manifest.1) >= publication_rank(state)
        {
            manifest.1.as_str()
        } else {
            state
        };
        let now = now_ms();
        tx.execute(
            "UPDATE publication_manifests SET state=?2 WHERE id=?1",
            params![manifest.0, effective],
        )?;
        tx.execute(
            r#"UPDATE canonical_content_versions SET publication_state=?2
               WHERE id IN (SELECT canonical_content_version_id FROM publication_manifest_files WHERE manifest_id=?1)
                 AND publication_state<>'merged'"#,
            params![manifest.0, effective],
        )?;
        tx.execute(
            r#"UPDATE canonical_files SET
                   state=CASE WHEN ?2='pr_open' THEN 'published' ELSE state END,
                   publication_state=?2,updated_at=?3
               WHERE current_content_version_id IN (
                   SELECT canonical_content_version_id FROM publication_manifest_files WHERE manifest_id=?1)
                 AND publication_state<>'merged'"#,
            params![manifest.0, effective, now],
        )?;
        tx.execute(
            r#"UPDATE translation_versions SET publication_state=?2
               WHERE id IN (
                   SELECT cft.translation_version_id
                   FROM publication_manifest_files pmf
                   JOIN canonical_file_translations cft
                     ON cft.canonical_content_version_id=pmf.canonical_content_version_id
                   WHERE pmf.manifest_id=?1)
                 AND publication_state<>'merged'"#,
            params![manifest.0, effective],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn canonical_compatible(&self, content_version_id: i64) -> Result<bool> {
        let conn = self.connect()?;
        let mut statement = conn.prepare("SELECT tv.id,tv.target_text FROM canonical_file_translations cft JOIN translation_versions tv ON tv.id=cft.translation_version_id WHERE cft.canonical_content_version_id=?1 AND tv.superseded_at IS NULL")?;
        let versions = statement
            .query_map([content_version_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if versions.is_empty() && conn.query_row("SELECT COUNT(*) FROM canonical_file_translations WHERE canonical_content_version_id=?1", [content_version_id], |row| row.get::<_,i64>(0))? > 0 { return Ok(false); }
        for (id, text) in versions {
            let Ok(provenance) = version_provenance(&conn, id) else {
                return Ok(false);
            };
            if !validate_stored_translation(&provenance, &text) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn publication_snapshot(
        &self,
        repository_id: i64,
        locale: &str,
        commit: &str,
    ) -> Result<Vec<crate::application::ports::CanonicalSnapshot>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare("SELECT cv.id,cv.source_revision,cf.path,cv.content FROM publication_manifests pm JOIN publication_manifest_files pmf ON pmf.manifest_id=pm.id JOIN canonical_content_versions cv ON cv.id=pmf.canonical_content_version_id JOIN canonical_files cf ON cf.id=cv.canonical_file_id WHERE pm.repository_id=?1 AND pm.locale=?2 AND pm.candidate_commit=?3 AND pm.state<>'superseded' ORDER BY cf.path")?;
        Ok(statement
            .query_map(params![repository_id, locale, commit], |row| {
                Ok(crate::application::ports::CanonicalSnapshot {
                    content_version_id: row.get(0)?,
                    source_revision: row.get(1)?,
                    path: row.get(2)?,
                    content: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn promote_merged_publication(
        &self,
        repository_id: i64,
        locale: &str,
        candidate_commit: &str,
        provenance: &str,
    ) -> Result<usize> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let manifest_id: i64 = tx
            .query_row(
                "SELECT id FROM publication_manifests WHERE repository_id=?1 AND locale=?2 AND candidate_commit=?3 AND state<>'superseded'",
                params![repository_id, locale, candidate_commit],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| anyhow!("verified merged commit has no publication manifest"))?;
        let mut translations = {
            let mut statement = tx.prepare(
                r#"SELECT DISTINCT tv.id,uv.unit_id,uv.source_hash,uv.source_revision,
                          COALESCE(json_extract(uv.context_json,'$.memory_key'),json_extract(uv.context_json,'$.kind'),''),tv.target_text,
                          tv.policy_fingerprint
                   FROM publication_manifest_files pmf
                   JOIN canonical_file_translations cft
                     ON cft.canonical_content_version_id=pmf.canonical_content_version_id
                   JOIN translation_versions tv ON tv.id=cft.translation_version_id
                   JOIN unit_versions uv ON uv.id=tv.unit_version_id
                   JOIN units u ON u.id=uv.unit_id
                   JOIN documents d ON d.id=u.document_id
                   WHERE pmf.manifest_id=?1 AND d.repository_id=?2 AND tv.locale=?3 AND tv.superseded_at IS NULL AND tv.validation_state='passed'"#,
            )?;
            statement
                .query_map(params![manifest_id, repository_id, locale], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if translations.is_empty() {
            bail!("publication manifest contains no exact translation versions");
        }
        for translation in &mut translations {
            let bound = version_provenance(&tx, translation.0);
            if !bound
                .as_ref()
                .is_ok_and(|bound| validate_stored_translation(bound, &translation.5))
            {
                tx.execute(
                    "UPDATE publication_manifests SET state='superseded' WHERE id=?1",
                    [manifest_id],
                )?;
                tx.commit()?;
                return Ok(0);
            }
            translation.4 = current_memory_key(&bound?)?;
        }
        let now = now_ms();
        for (
            version_id,
            unit_id,
            source_hash,
            source_revision,
            context_key,
            target_text,
            fingerprint,
        ) in &translations
        {
            let provenance = serde_json::json!({"origin": provenance, "_fani_unit": version_provenance(&tx, *version_id)?}).to_string();
            tx.execute("UPDATE translation_memory_entries SET tier='history',superseded_at=?7 WHERE repository_id=?1 AND locale=?2 AND source_hash=?3 AND context_key=?4 AND tier='trusted' AND superseded_at IS NULL AND (translation_version_id IS NOT ?5 OR target_text<>?6)", params![repository_id,locale,source_hash,context_key,version_id,target_text,now])?;
            let updated = tx.execute(
                r#"UPDATE translation_memory_entries
                   SET unit_id=?2,translation_version_id=?3,source_revision=?6,target_text=?8,provenance=?9,
                       policy_fingerprint=?10,created_at=?11,superseded_at=NULL
                   WHERE repository_id=?1 AND locale=?4 AND source_hash=?5 AND context_key=?7
                     AND tier='trusted' AND superseded_at IS NULL"#,
                params![
                    repository_id,
                    unit_id,
                    version_id,
                    locale,
                    source_hash,
                    source_revision,
                    context_key,
                    target_text,
                    provenance,
                    fingerprint,
                    now
                ],
            )?;
            if updated == 0 {
                tx.execute(
                    r#"INSERT INTO translation_memory_entries(
                           repository_id,unit_id,translation_version_id,locale,source_hash,
                           source_revision,context_key,target_text,tier,provenance,policy_fingerprint,created_at)
                       VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'trusted',?9,?10,?11)"#,
                    params![
                        repository_id,
                        unit_id,
                        version_id,
                        locale,
                        source_hash,
                        source_revision,
                        context_key,
                        target_text,
                        provenance,
                        fingerprint,
                        now
                    ],
                )?;
            }
        }
        tx.execute(
            "UPDATE translation_versions SET publication_state='merged',review_state='approved' WHERE superseded_at IS NULL AND validation_state='passed' AND id IN (SELECT cft.translation_version_id FROM publication_manifest_files pmf JOIN canonical_file_translations cft ON cft.canonical_content_version_id=pmf.canonical_content_version_id WHERE pmf.manifest_id=?1)",
            [manifest_id],
        )?;
        tx.execute(
            "UPDATE canonical_content_versions SET publication_state='merged' WHERE id IN (SELECT canonical_content_version_id FROM publication_manifest_files WHERE manifest_id=?1)",
            [manifest_id],
        )?;
        tx.execute(
            r#"UPDATE canonical_files SET state='merged',publication_state='merged',
                   review_state='approved',trust_tier='trusted',updated_at=?2
               WHERE current_content_version_id IN (
                   SELECT canonical_content_version_id FROM publication_manifest_files WHERE manifest_id=?1)"#,
            params![manifest_id, now],
        )?;
        tx.execute(
            "UPDATE publication_manifests SET state='merged',merged_at=?2 WHERE id=?1",
            params![manifest_id, now],
        )?;
        tx.commit()?;
        Ok(translations.len())
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

    pub fn unchanged_document_unit_keys(
        &self,
        repository_id: i64,
        path: &str,
        content_hash: &str,
    ) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare(
            r#"SELECT u.unit_key
               FROM units u
               JOIN documents d ON d.id=u.document_id
               WHERE d.repository_id=?1 AND d.path=?2 AND d.content_hash=?3
                 AND d.deleted_at IS NULL AND u.active=1
                 AND EXISTS (
                     SELECT 1 FROM unit_versions uv
                     WHERE uv.unit_id=u.id
                       AND uv.source_revision=COALESCE(d.source_revision,'')
                       AND uv.source_hash=u.source_hash
                       AND uv.source_text=u.source_text
                 )
               ORDER BY u.ordinal,u.id"#,
        )?;
        let rows =
            statement.query_map(params![repository_id, path, content_hash], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn trusted_translation(
        &self,
        repository_id: i64,
        locale: &str,
        source_hash: &str,
        context_key: &str,
    ) -> Result<Option<String>> {
        let row: Option<(i64, i64, String)> = self.connect()?.query_row(
            "SELECT u.document_id,t.unit_id,t.target_text FROM translation_memory_entries t JOIN units u ON u.id=t.unit_id WHERE t.repository_id=?1 AND t.locale=?2 AND t.source_hash=?3 AND t.context_key=?4 AND t.tier='trusted' AND t.superseded_at IS NULL",
            params![repository_id,locale,source_hash,context_key], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
        let Some((document_id, unit_id, text)) = row else {
            return Ok(None);
        };
        Ok(self
            .translation_candidates(document_id, locale)?
            .into_iter()
            .find(|candidate| {
                candidate.trusted
                    && candidate.unit_id == unit_id
                    && candidate.text == text
                    && current_memory_key(&candidate.provenance).ok().as_deref()
                        == Some(context_key)
                    && validate_stored_translation(&candidate.provenance, &candidate.text)
            })
            .map(|candidate| candidate.text))
    }

    pub fn translation_candidates(
        &self,
        document_id: i64,
        locale: &str,
    ) -> Result<Vec<TranslationCandidate>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare(
            r#"SELECT t.unit_id,t.target_text,t.tier,d.path,uv.source_text,uv.source_revision,
                      uv.context_json,t.policy_fingerprint,w.run_id,r.invocation_key,
                      CASE WHEN a.agent='fani' AND a.provider='deterministic' AND a.adapter='native' THEN a.model END,
                      a.id,tv.id,t.context_key
               FROM (
                 SELECT id,unit_id,target_text,tier,policy_fingerprint,translation_version_id,locale,superseded_at,source_hash,source_revision,context_key
                 FROM translation_memory_entries
                 UNION ALL
                 SELECT -tv.id,uv.unit_id,tv.target_text,'candidate',tv.policy_fingerprint,tv.id,tv.locale,tv.superseded_at,uv.source_hash,uv.source_revision,
                        COALESCE(json_extract(uv.context_json,'$.memory_key'),json_extract(uv.context_json,'$.kind'),'')
                 FROM translation_versions tv JOIN unit_versions uv ON uv.id=tv.unit_version_id
                 WHERE tv.source_attempt_id IS NOT NULL
                   AND NOT EXISTS(SELECT 1 FROM translation_memory_entries m WHERE m.translation_version_id=tv.id AND m.superseded_at IS NULL)
               ) t
               JOIN units u ON u.id=t.unit_id
               JOIN documents d ON d.id=u.document_id
               LEFT JOIN translation_versions tv ON tv.id=t.translation_version_id
               JOIN unit_versions uv ON uv.id=COALESCE(tv.unit_version_id,
                 (SELECT v.id FROM unit_versions v WHERE v.unit_id=t.unit_id AND v.source_hash=t.source_hash
                   AND (t.source_revision='' OR v.source_revision=t.source_revision) ORDER BY v.id LIMIT 1))
               LEFT JOIN attempts a ON a.id=tv.source_attempt_id
               LEFT JOIN work_items w ON w.id=a.work_item_id
               LEFT JOIN runs r ON r.id=w.run_id
               WHERE d.id=?1 AND t.locale=?2 AND t.superseded_at IS NULL
                 AND t.source_hash=uv.source_hash AND uv.unit_id=t.unit_id
                 AND (tv.id IS NULL OR (tv.locale=t.locale AND tv.target_text=t.target_text AND tv.superseded_at IS NULL))
                 AND (t.tier='trusted' OR (t.tier='candidate' AND a.status='succeeded'
                   AND EXISTS(SELECT 1 FROM canonical_candidates c WHERE c.source_attempt_id=a.id AND c.selected=1 AND c.target_text=t.target_text)))
               ORDER BY CASE t.tier WHEN 'trusted' THEN 0 ELSE 1 END,t.id DESC"#)?;
        let rows = statement
            .query_map(params![document_id, locale], |row| {
                Ok((
                    TranslationCandidate {
                        unit_id: row.get(0)?,
                        text: row.get(1)?,
                        trusted: row.get::<_, String>(2)? == "trusted",
                        provenance: UnitProvenance {
                            document_path: row.get(3)?,
                            source: row.get(4)?,
                            source_revision: row.get(5)?,
                            context_json: row.get(6)?,
                            policy_fingerprint: row.get(7)?,
                        },
                        run_id: row.get(8)?,
                        invocation_key: row.get(9)?,
                        deterministic_model: row.get(10)?,
                    },
                    row.get::<_, Option<i64>>(11)?,
                    row.get::<_, Option<i64>>(12)?,
                    row.get::<_, String>(13)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut candidates = Vec::new();
        for (mut candidate, _attempt_id, version_id, key) in rows {
            if let Some(id) = version_id {
                let Ok(snapshot) = version_provenance(&conn, id) else {
                    continue;
                };
                if snapshot.source != candidate.provenance.source
                    || snapshot.document_path != candidate.provenance.document_path
                {
                    continue;
                }
                candidate.provenance = snapshot;
            }
            let Some(metadata) =
                crate::domain::document::compatible_metadata(&candidate.provenance)
            else {
                continue;
            };
            if key != metadata.kind.as_str()
                && metadata.memory_key.as_deref() != Some(key.as_str())
                && current_memory_key(&candidate.provenance).ok().as_deref() != Some(key.as_str())
            {
                continue;
            }
            candidates.push(candidate);
        }
        Ok(candidates)
    }

    pub fn quarantine_incompatible_translations(
        &self,
        document_id: i64,
        locale: &str,
    ) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let versions = {
            let mut statement = tx.prepare("SELECT tv.id,tv.target_text FROM translation_versions tv JOIN unit_versions uv ON uv.id=tv.unit_version_id JOIN units u ON u.id=uv.unit_id WHERE u.document_id=?1 AND tv.locale=?2 AND tv.superseded_at IS NULL")?;
            statement
                .query_map(params![document_id, locale], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (id, text) in versions {
            if version_provenance(&tx, id)
                .as_ref()
                .is_ok_and(|bound| validate_stored_translation(bound, &text))
            {
                continue;
            }
            tx.execute("UPDATE translation_memory_entries SET tier='history',superseded_at=?2 WHERE translation_version_id=?1 AND superseded_at IS NULL", params![id,now_ms()])?;
            tx.execute("UPDATE translation_versions SET validation_state='quarantined',superseded_at=?2 WHERE id=?1", params![id,now_ms()])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn retire_attempt(&self, attempt_id: i64) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("UPDATE attempts SET dedupe_key=dedupe_key||':superseded:'||id,status='cancelled',error='stored output rejected by current document contract' WHERE id=?1 AND status='succeeded'", [attempt_id])?;
        tx.execute("UPDATE canonical_candidates SET selected=0,candidate_key=candidate_key||':superseded:'||id WHERE source_attempt_id=?1", [attempt_id])?;
        tx.execute("UPDATE translation_memory_entries SET tier='history',superseded_at=?2 WHERE translation_version_id IN (SELECT id FROM translation_versions WHERE source_attempt_id=?1) AND superseded_at IS NULL", params![attempt_id,now_ms()])?;
        tx.execute("UPDATE translation_versions SET validation_state='quarantined',superseded_at=?2 WHERE source_attempt_id=?1", params![attempt_id,now_ms()])?;
        tx.commit()?;
        Ok(())
    }

    pub fn selected_candidate(&self, unit_id: i64, locale: &str) -> Result<Option<String>> {
        Ok(self.connect()?.query_row(
            "SELECT target_text FROM canonical_candidates WHERE unit_id=?1 AND locale=?2 AND selected=1",
            params![unit_id, locale],
            |row| row.get(0),
        ).optional()?)
    }

    pub fn recoverable_candidate(
        &self,
        run_id: &str,
        unit_id: i64,
        locale: &str,
        policy_fingerprint: &str,
        deterministic_repair_version: &str,
    ) -> Result<Option<String>> {
        require_fingerprint(policy_fingerprint, "translation policy")?;
        let conn = self.connect()?;
        let row = conn.query_row(
                r#"SELECT c.target_text,a.id
                   FROM canonical_candidates c
                   JOIN attempts a ON a.id=c.source_attempt_id AND a.status='succeeded'
                   JOIN work_items w ON w.id=a.work_item_id
                   WHERE w.run_id=?1 AND c.unit_id=?2 AND c.locale=?3 AND c.selected=1
                     AND a.policy_fingerprint=?4
                     AND (a.agent<>'fani' OR a.provider<>'deterministic' OR a.adapter<>'native' OR a.model=?5)"#,
                params![
                    run_id,
                    unit_id,
                    locale,
                    policy_fingerprint,
                    deterministic_repair_version
                ],
                |row| Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?)),
            )
            .optional()?;
        match row {
            Some((text, id)) => checked_candidate_text(&conn, unit_id, id, text),
            None => Ok(None),
        }
    }

    pub fn recoverable_invocation_candidate(
        &self,
        invocation_key: &str,
        unit_id: i64,
        locale: &str,
        policy_fingerprint: &str,
        deterministic_repair_version: &str,
    ) -> Result<Option<String>> {
        require_fingerprint(policy_fingerprint, "translation policy")?;
        let conn = self.connect()?;
        let row = conn.query_row(
                r#"SELECT c.target_text,a.id
                   FROM canonical_candidates c
                   JOIN attempts a ON a.id=c.source_attempt_id AND a.status='succeeded'
                   JOIN work_items w ON w.id=a.work_item_id
                   JOIN runs r ON r.id=w.run_id
                   WHERE r.invocation_key=?1 AND c.unit_id=?2 AND c.locale=?3 AND c.selected=1
                     AND a.policy_fingerprint=?4
                     AND (a.agent<>'fani' OR a.provider<>'deterministic' OR a.adapter<>'native' OR a.model=?5)"#,
                params![
                    invocation_key,
                    unit_id,
                    locale,
                    policy_fingerprint,
                    deterministic_repair_version
                ],
                |row| Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?)),
            )
            .optional()?;
        match row {
            Some((text, id)) => checked_candidate_text(&conn, unit_id, id, text),
            None => Ok(None),
        }
    }

    pub fn recoverable_unit_candidate(
        &self,
        unit_id: i64,
        locale: &str,
        policy_fingerprint: &str,
        deterministic_repair_version: &str,
    ) -> Result<Option<String>> {
        require_fingerprint(policy_fingerprint, "translation policy")?;
        let conn = self.connect()?;
        let row = conn.query_row(
                r#"SELECT c.target_text,a.id
                   FROM canonical_candidates c
                   JOIN attempts a ON a.id=c.source_attempt_id AND a.status='succeeded'
                   JOIN work_items w ON w.id=a.work_item_id
                   WHERE c.unit_id=?1 AND c.locale=?2 AND c.selected=1
                     AND a.policy_fingerprint=?3
                     AND (a.agent<>'fani' OR a.provider<>'deterministic' OR a.adapter<>'native' OR a.model=?4)"#,
                params![
                    unit_id,
                    locale,
                    policy_fingerprint,
                    deterministic_repair_version
                ],
                |row| Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?)),
            )
            .optional()?;
        match row {
            Some((text, id)) => checked_candidate_text(&conn, unit_id, id, text),
            None => Ok(None),
        }
    }

    pub fn upsert_canonical_file(&self, input: CanonicalFileInput<'_>) -> Result<i64> {
        Ok(self.persist_canonical_file(input, &[])?.id)
    }

    pub fn canonical_file(
        &self,
        repository_id: i64,
        locale: &str,
        path: &str,
    ) -> Result<Option<CanonicalFile>> {
        Ok(self.connect()?.query_row(
            "SELECT id,current_content_version_id,source_revision,content,content_hash,materialized_hash,state FROM canonical_files WHERE repository_id=?1 AND locale=?2 AND path=?3",
            params![repository_id, locale, path],
            |row| Ok(CanonicalFile {
                id: row.get(0)?,
                content_version_id: row.get(1)?,
                source_revision: row.get(2)?,
                content: row.get(3)?,
                content_hash: row.get(4)?,
                materialized_hash: row.get(5)?,
                state: row.get(6)?,
            }),
        ).optional()?)
    }

    pub fn transition_canonical_file(
        &self,
        id: i64,
        transition: CanonicalTransition,
        materialized_hash: Option<&str>,
    ) -> Result<()> {
        let conn = self.connect()?;
        let changed = match transition {
            CanonicalTransition::Materialized => conn.execute(
                "UPDATE canonical_files SET state='materialized',materialized_hash=?2,updated_at=?3 WHERE id=?1",
                params![id, materialized_hash, now_ms()],
            )?,
            CanonicalTransition::HumanEdit => conn.execute(
                "UPDATE canonical_files SET state='human_edit',materialized_hash=?2,review_state='needs_review',updated_at=?3 WHERE id=?1",
                params![id, materialized_hash, now_ms()],
            )?,
            CanonicalTransition::Adopted => conn.execute(
                "UPDATE canonical_files SET state='adopted',materialized_hash=?2,freshness='exact',provenance='human',validation_state='passed',review_state='approved',trust_tier='trusted',updated_at=?3 WHERE id=?1",
                params![id, materialized_hash, now_ms()],
            )?,
            CanonicalTransition::CommitCreated => conn.execute(
                "UPDATE canonical_files SET publication_state='commit_created',updated_at=?2 WHERE id=?1",
                params![id, now_ms()],
            )?,
            CanonicalTransition::PushPending => conn.execute(
                "UPDATE canonical_files SET publication_state='push_pending',updated_at=?2 WHERE id=?1",
                params![id, now_ms()],
            )?,
            CanonicalTransition::PrOpen => conn.execute(
                "UPDATE canonical_files SET state='published',publication_state='pr_open',updated_at=?2 WHERE id=?1",
                params![id, now_ms()],
            )?,
            CanonicalTransition::Merged => conn.execute(
                "UPDATE canonical_files SET state='merged',publication_state='merged',review_state='approved',trust_tier='trusted',updated_at=?2 WHERE id=?1",
                params![id, now_ms()],
            )?,
            CanonicalTransition::Superseded => conn.execute(
                "UPDATE canonical_files SET publication_state='superseded',trust_tier='history',updated_at=?2 WHERE id=?1",
                params![id, now_ms()],
            )?,
        };
        if changed != 1 {
            bail!("canonical file {id} does not exist");
        }
        Ok(())
    }
}

fn publication_rank(state: &str) -> u8 {
    match state {
        "candidate" => 0,
        "commit_created" => 1,
        "push_pending" => 2,
        "pr_open" => 3,
        "merged" | "superseded" => 4,
        _ => 0,
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

fn require_attempt_provenance(input: &AttemptInput<'_>) -> Result<()> {
    for (label, value) in [
        ("Agent", input.agent),
        ("provider", input.provider),
        ("model", input.model),
        ("adapter", input.adapter),
        ("prompt version", input.prompt_version),
    ] {
        if value.trim().is_empty() {
            bail!("{label} identity must not be empty");
        }
    }
    require_fingerprint(input.provider_fingerprint, "provider")?;
    require_fingerprint(input.prompt_hash, "prompt")?;
    require_fingerprint(input.policy_fingerprint, "policy")
}

fn require_json(value: &str) -> Result<()> {
    serde_json::from_str::<serde_json::Value>(value)
        .map(|_| ())
        .map_err(|error| anyhow!("invalid JSON payload: {error}"))
}

fn require_fingerprint(value: &str, label: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("{label} fingerprint must be exactly 64 hexadecimal characters");
    }
    Ok(())
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

    fn document_id(&self, repository_id: i64, path: &str) -> Result<Option<i64>> {
        Database::document_id(self, repository_id, path)
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

    fn unchanged_document_unit_keys(
        &self,
        repository_id: i64,
        path: &str,
        content_hash: &str,
    ) -> Result<Vec<String>> {
        Database::unchanged_document_unit_keys(self, repository_id, path, content_hash)
    }

    fn translation_candidates(
        &self,
        document_id: i64,
        locale: &str,
    ) -> Result<Vec<TranslationCandidate>> {
        Database::translation_candidates(self, document_id, locale)
    }

    fn review_attempts(
        &self,
        unit_id: i64,
        locale: &str,
        run_or_invocation: &str,
    ) -> Result<Vec<RecoveredAttempt>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare("SELECT a.work_item_id,a.dedupe_key FROM attempts a JOIN work_items w ON w.id=a.work_item_id JOIN runs r ON r.id=w.run_id WHERE w.unit_id=?1 AND w.locale=?2 AND (r.id=?3 OR r.invocation_key=?3) AND a.status='succeeded' AND (a.dedupe_key LIKE '%:revision' OR a.dedupe_key LIKE '%:proofread')")?;
        let keys = statement
            .query_map(params![unit_id, locale, run_or_invocation], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut receipts = Vec::new();
        for (work, key) in keys {
            if let Some(receipt) = self.successful_attempt(work, &key)? {
                receipts.push(receipt);
            }
        }
        Ok(receipts)
    }

    fn revalidate_candidate(
        &self,
        unit_id: i64,
        locale: &str,
        candidate: &TranslationCandidate,
    ) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (version_id, source_hash, bound): (i64,String,UnitProvenance) = tx.query_row(
            "SELECT uv.id,uv.source_hash,d.path,uv.source_text,uv.source_revision,u.context_json FROM units u JOIN documents d ON d.id=u.document_id JOIN unit_versions uv ON uv.unit_id=u.id AND uv.source_revision=d.source_revision AND uv.source_hash=u.source_hash AND uv.source_text=u.source_text WHERE u.id=?1",
            [unit_id], |row| Ok((row.get(0)?,row.get(1)?,UnitProvenance { document_path:row.get(2)?,source:row.get(3)?,source_revision:row.get(4)?,context_json:row.get(5)?,policy_fingerprint:candidate.provenance.policy_fingerprint.clone() })))?;
        let unit = crate::domain::document::stored_unit(&bound)
            .ok_or_else(|| anyhow!("current source has no parser snapshot"))?;
        crate::domain::document::validate_provenance(
            &bound.document_path,
            &unit,
            &candidate.provenance,
            &candidate.text,
        )
        .map_err(|_| anyhow!("legacy candidate failed current validation"))?;
        let attempt: i64 = tx.query_row("SELECT a.id FROM canonical_candidates c JOIN attempts a ON a.id=c.source_attempt_id WHERE c.unit_id=?1 AND c.locale=?2 AND c.selected=1 AND c.target_text=?3 AND a.status='succeeded' AND json_extract(a.response_json,'$.output')=c.target_text", params![unit_id,locale,candidate.text], |row| row.get(0))?;
        let original = attempt_provenance(&tx, attempt)?
            .ok_or_else(|| anyhow!("legacy candidate has no immutable attempt source"))?;
        crate::domain::document::validate_provenance(
            &bound.document_path,
            &unit,
            &original,
            &candidate.text,
        )
        .map_err(|_| anyhow!("legacy attempt failed current validation"))?;
        let now = now_ms();
        let key = unit.memory_context_key(&bound.document_path);
        tx.execute("INSERT INTO translation_versions(unit_version_id,locale,target_text,target_hash,freshness,provenance,validation_state,review_state,publication_state,policy_fingerprint,source_attempt_id,created_at) VALUES (?1,?2,?3,?4,'exact','candidate_tm','passed','unreviewed','candidate',?5,?6,?7)", params![version_id,locale,candidate.text,migration_checksum(&candidate.text),bound.policy_fingerprint,attempt,now])?;
        let translation_id = tx.last_insert_rowid();
        tx.execute("UPDATE translation_memory_entries SET tier='history',superseded_at=?4 WHERE repository_id=(SELECT d.repository_id FROM units u JOIN documents d ON d.id=u.document_id WHERE u.id=?1) AND source_hash=(SELECT source_hash FROM units WHERE id=?1) AND locale=?2 AND context_key=?3 AND tier='candidate' AND superseded_at IS NULL", params![unit_id,locale,key,now])?;
        let audit =
            serde_json::json!({"origin":"compatible_candidate","_fani_unit":bound}).to_string();
        tx.execute("INSERT INTO translation_memory_entries(repository_id,unit_id,translation_version_id,locale,source_hash,source_revision,context_key,target_text,tier,provenance,policy_fingerprint,created_at) SELECT d.repository_id,?1,?2,?3,?4,?5,?6,?7,'candidate',?8,?9,?10 FROM units u JOIN documents d ON d.id=u.document_id WHERE u.id=?1", params![unit_id,translation_id,locale,source_hash,bound.source_revision,key,candidate.text,audit,bound.policy_fingerprint,now])?;
        tx.commit()?;
        Ok(())
    }

    fn retire_attempt(&self, attempt_id: i64) -> Result<()> {
        Database::retire_attempt(self, attempt_id)
    }

    fn quarantine_incompatible_translations(&self, document_id: i64, locale: &str) -> Result<()> {
        Database::quarantine_incompatible_translations(self, document_id, locale)
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
        policy_fingerprint: &str,
    ) -> Result<String> {
        Database::begin_run(
            self,
            repository_id,
            invocation_key,
            config_path,
            metadata_json,
            policy_fingerprint,
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

    fn enqueue_document_work_item(
        &self,
        run_id: &str,
        document_id: i64,
        locale: &str,
        kind: &str,
        priority: i64,
        input_json: &str,
    ) -> Result<i64> {
        Database::enqueue_document_work_item(
            self,
            run_id,
            document_id,
            locale,
            kind,
            priority,
            input_json,
        )
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

    fn failed_attempt_context(&self, work_item_id: i64) -> Result<Option<FailedAttemptContext>> {
        Database::failed_attempt_context(self, work_item_id)
    }

    fn recoverable_candidate(
        &self,
        run_id: &str,
        unit_id: i64,
        locale: &str,
        policy_fingerprint: &str,
        deterministic_repair_version: &str,
    ) -> Result<Option<String>> {
        Database::recoverable_candidate(
            self,
            run_id,
            unit_id,
            locale,
            policy_fingerprint,
            deterministic_repair_version,
        )
    }

    fn recoverable_invocation_candidate(
        &self,
        invocation_key: &str,
        unit_id: i64,
        locale: &str,
        policy_fingerprint: &str,
        deterministic_repair_version: &str,
    ) -> Result<Option<String>> {
        Database::recoverable_invocation_candidate(
            self,
            invocation_key,
            unit_id,
            locale,
            policy_fingerprint,
            deterministic_repair_version,
        )
    }

    fn recoverable_unit_candidate(
        &self,
        unit_id: i64,
        locale: &str,
        policy_fingerprint: &str,
        deterministic_repair_version: &str,
    ) -> Result<Option<String>> {
        Database::recoverable_unit_candidate(
            self,
            unit_id,
            locale,
            policy_fingerprint,
            deterministic_repair_version,
        )
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

    fn persist_canonical_file(
        &self,
        input: CanonicalFileInput<'_>,
        translations: &[CanonicalTranslationInput<'_>],
    ) -> Result<CanonicalFile> {
        Database::persist_canonical_file(self, input, translations)
    }

    fn record_canonical_file_translations(
        &self,
        canonical_file_id: i64,
        translations: &[CanonicalTranslationInput<'_>],
        locale: &str,
    ) -> Result<usize> {
        Database::record_canonical_file_translations(self, canonical_file_id, translations, locale)
    }

    fn transition_canonical_file(
        &self,
        id: i64,
        transition: CanonicalTransition,
        materialized_hash: Option<&str>,
    ) -> Result<()> {
        Database::transition_canonical_file(self, id, transition, materialized_hash)
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

    fn record_publication_manifest(&self, input: PublicationManifestInput<'_>) -> Result<i64> {
        Database::record_publication_manifest(self, input)
    }

    fn transition_publication_manifest(
        &self,
        repository_id: i64,
        locale: &str,
        candidate_commit: &str,
        state: PublicationState,
    ) -> Result<()> {
        Database::transition_publication_manifest(
            self,
            repository_id,
            locale,
            candidate_commit,
            state,
        )
    }

    fn publication_snapshot(
        &self,
        repository_id: i64,
        locale: &str,
        commit: &str,
    ) -> Result<Vec<crate::application::ports::CanonicalSnapshot>> {
        Database::publication_snapshot(self, repository_id, locale, commit)
    }

    fn canonical_compatible(&self, content_version_id: i64) -> Result<bool> {
        Database::canonical_compatible(self, content_version_id)
    }

    fn promote_merged_publication(
        &self,
        repository_id: i64,
        locale: &str,
        candidate_commit: &str,
        provenance: &str,
    ) -> Result<usize> {
        Database::promote_merged_publication(
            self,
            repository_id,
            locale,
            candidate_commit,
            provenance,
        )
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

    fn schema_two_fixture(path: &Path) -> Connection {
        assert_eq!(
            migration_checksum(MIGRATIONS[0].sql),
            "737fa3494a4038d6e21897de7bcca5090453fde831a5582b14209b1685c40ee3"
        );
        assert_eq!(
            migration_checksum(MIGRATIONS[1].sql),
            "af24c32a6f31dbfb8f6d35ee40ca079e28e6aa98c90bdd29b86673f3c00f4692"
        );
        let mut conn = open_connection(path).unwrap();
        apply_migrations(&mut conn, &MIGRATIONS[..2]).unwrap();
        conn.execute_batch(r#"
            INSERT INTO repositories VALUES (11,'repo','/repo',NULL,NULL,1,1);
            INSERT INTO documents VALUES (21,11,'old.md','rev','hash','{}',NULL,1,1);
            INSERT INTO units VALUES (31,21,'old',0,'source','hash','{}',1,1,1);
            INSERT INTO runs(id,repository_id,config_path,started_at,heartbeat_at) VALUES ('old-run',11,'fani.toml',1,1);
            INSERT INTO work_items(id,run_id,unit_id,locale,kind,created_at,updated_at) VALUES (41,'old-run',31,'fr','materialization',1,1);
            INSERT INTO attempts(id,work_item_id,dedupe_key,attempt_no,agent,status,request_json,response_json,started_at,finished_at) VALUES (51,41,'attempt',1,'translator','succeeded','{}','{}',1,2);
            INSERT INTO findings VALUES (61,41,51,'finding','warning','OLD','history','{}',NULL,1);
            INSERT INTO canonical_candidates VALUES (71,31,'fr','candidate','target',51,NULL,1,1);
            INSERT INTO materialization_outbox(id,work_item_id,dedupe_key,payload_json,available_at,created_at) VALUES (81,41,'materialize','{"path":"old.fr.md"}',1,1);
            INSERT INTO publication_outbox(id,repository_id,run_id,locale,dedupe_key,payload_json,available_at,created_at) VALUES (91,11,'old-run','fr','publish','{}',1,1);
            INSERT INTO unit_versions VALUES (101,31,'rev','source','hash','{}',1);
            INSERT INTO translation_versions VALUES (111,101,'fr','target','hash','exact','ai','passed','approved','candidate',lower(hex(zeroblob(32))),51,1,NULL);
            INSERT INTO translation_memory_entries(id,repository_id,unit_id,translation_version_id,locale,source_hash,target_text,tier,provenance,policy_fingerprint,created_at) VALUES (121,11,31,111,'fr','hash','target','history','old',lower(hex(zeroblob(32))),1);
            INSERT INTO canonical_files(id,repository_id,locale,path,source_revision,content,content_hash,updated_at) VALUES (131,11,'fr','old.fr.md','rev',X'746172676574','hash',1);
            INSERT INTO canonical_content_versions VALUES (141,131,'rev',X'746172676574','hash','candidate',1);
            UPDATE canonical_files SET current_content_version_id=141 WHERE id=131;
            INSERT INTO canonical_file_translations VALUES (141,111);
        "#).unwrap();
        validate_connection_version(&conn, &MIGRATIONS[..2]).unwrap();
        conn
    }

    fn rows(conn: &Connection, table: &str) -> Vec<Vec<rusqlite::types::Value>> {
        let mut statement = conn
            .prepare(&format!("SELECT * FROM {table} ORDER BY 1"))
            .unwrap();
        let columns = statement.column_count();
        statement
            .query_map([], |row| (0..columns).map(|i| row.get(i)).collect())
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn schema_two_upgrade_preserves_every_child_and_pending_outbox() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("old.db");
        let mut conn = schema_two_fixture(&path);
        let tables = [
            "repositories",
            "documents",
            "units",
            "runs",
            "attempts",
            "findings",
            "canonical_candidates",
            "materialization_outbox",
            "publication_outbox",
            "unit_versions",
            "translation_versions",
            "translation_memory_entries",
            "canonical_files",
            "canonical_content_versions",
            "canonical_file_translations",
        ];
        let before: Vec<_> = tables.iter().map(|table| rows(&conn, table)).collect();
        let ledger = rows(&conn, "schema_migrations");
        let old_work = rows(&conn, "work_items");
        apply_migrations(&mut conn, MIGRATIONS).unwrap();
        validate_existing_connection(&conn).unwrap();
        for (table, expected) in tables.iter().zip(before) {
            assert_eq!(rows(&conn, table), expected, "{table}");
        }
        let mut migrated_work = rows(&conn, "work_items");
        for row in &mut migrated_work {
            assert_eq!(row.remove(3), rusqlite::types::Value::Null);
        }
        assert_eq!(migrated_work, old_work);
        assert_eq!(&rows(&conn, "schema_migrations")[..2], ledger);
        assert_eq!(
            conn.query_row("SELECT id,unit_id,document_id FROM work_items", [], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            })
            .unwrap(),
            (41, 31, None)
        );
        let backup = Connection::open(temp.path().join("old.db.pre-schema-2.bak")).unwrap();
        validate_connection_version(&backup, &MIGRATIONS[..2]).unwrap();
        assert_eq!(
            rows(&backup, "materialization_outbox"),
            rows(&conn, "materialization_outbox")
        );
        assert!(
            apply_migrations(&mut conn, &MIGRATIONS[..2])
                .unwrap_err()
                .to_string()
                .contains("unknown migration 3")
        );
        apply_migrations(&mut conn, MIGRATIONS).unwrap();
        assert_eq!(rows(&conn, "schema_migrations").len(), 3);
    }

    #[test]
    fn schema_two_rebuild_failure_rolls_back_children_and_restores_foreign_keys() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("old.db");
        let mut conn = schema_two_fixture(&path);
        conn.execute_batch("CREATE TRIGGER reject_upgrade BEFORE UPDATE ON state_schema WHEN NEW.version=3 BEGIN SELECT RAISE(ABORT,'injected migration failure'); END;").unwrap();
        let before = rows(&conn, "work_items");
        let error = apply_migrations(&mut conn, MIGRATIONS).unwrap_err();
        assert!(format!("{error:#}").contains("injected migration failure"));
        validate_connection_version(&conn, &MIGRATIONS[..2]).unwrap();
        assert_eq!(rows(&conn, "work_items"), before);
        assert_eq!(
            conn.query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        let backup = Connection::open(temp.path().join("old.db.pre-schema-2.bak")).unwrap();
        for table in [
            "attempts",
            "findings",
            "canonical_candidates",
            "materialization_outbox",
            "publication_outbox",
            "translation_versions",
            "translation_memory_entries",
            "canonical_file_translations",
            "schema_migrations",
        ] {
            assert_eq!(rows(&conn, table), rows(&backup, table), "{table}");
        }
        assert!(!has_table(&conn, "work_items_new").unwrap());
        conn.execute_batch("DROP TRIGGER reject_upgrade").unwrap();
        assert!(
            format!("{:#}", apply_migrations(&mut conn, MIGRATIONS).unwrap_err())
                .contains("move any existing backup aside")
        );
        validate_connection_version(&conn, &MIGRATIONS[..2]).unwrap();
    }

    #[test]
    fn post_rebuild_foreign_key_violation_aborts_before_commit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("old.db");
        let mut conn = schema_two_fixture(&path);
        conn.execute_batch("CREATE TRIGGER corrupt_upgrade AFTER UPDATE ON state_schema WHEN NEW.version=3 BEGIN UPDATE findings SET attempt_id=9999 WHERE id=61; END;").unwrap();
        let before = rows(&conn, "findings");
        assert!(
            apply_migrations(&mut conn, MIGRATIONS)
                .unwrap_err()
                .to_string()
                .contains("foreign key check")
        );
        validate_connection_version(&conn, &MIGRATIONS[..2]).unwrap();
        assert_eq!(rows(&conn, "findings"), before);
        assert_eq!(
            conn.query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn upgrade_rejects_live_even_expired_application_lease() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("old.db");
        let mut conn = schema_two_fixture(&path);
        let (pid, started) = process_identity(std::process::id()).unwrap();
        conn.execute(
            "INSERT INTO leases VALUES ('repository','repo',?1,1,1,2)",
            [format!("{pid}:{started}:run")],
        )
        .unwrap();
        assert!(
            apply_migrations(&mut conn, MIGRATIONS)
                .unwrap_err()
                .to_string()
                .contains("all application leases released")
        );
        validate_connection_version(&conn, &MIGRATIONS[..2]).unwrap();
        assert!(!temp.path().join("old.db.pre-schema-2.bak").exists());
    }

    #[test]
    fn upgrade_rejects_live_outbox_worker_without_repository_lease() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("old.db");
        let mut conn = schema_two_fixture(&path);
        let (pid, started) = process_identity(std::process::id()).unwrap();
        conn.execute("UPDATE materialization_outbox SET state='processing',owner=?1,lease_expires_at=2 WHERE id=81", [format!("materialize:{pid}:{started}:run")]).unwrap();
        assert!(
            apply_migrations(&mut conn, MIGRATIONS)
                .unwrap_err()
                .to_string()
                .contains("outbox workers stopped")
        );
        validate_connection_version(&conn, &MIGRATIONS[..2]).unwrap();
        assert!(!temp.path().join("old.db.pre-schema-2.bak").exists());
    }

    #[test]
    fn upgrade_requires_exclusive_database_access_before_backup() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("old.db");
        let mut conn = schema_two_fixture(&path);
        conn.busy_timeout(Duration::ZERO).unwrap();
        let reader = open_connection(&path).unwrap();
        reader
            .execute_batch("BEGIN; SELECT * FROM work_items;")
            .unwrap();
        assert!(
            apply_migrations(&mut conn, MIGRATIONS)
                .unwrap_err()
                .to_string()
                .contains("locked")
        );
        assert!(!temp.path().join("old.db.pre-schema-2.bak").exists());
        reader.execute_batch("ROLLBACK").unwrap();
        validate_connection_version(&conn, &MIGRATIONS[..2]).unwrap();
        apply_migrations(&mut conn, MIGRATIONS).unwrap();
        validate_existing_connection(&conn).unwrap();
    }

    #[test]
    fn upgrade_rejects_unexpected_recursive_foreign_key_dependency() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("old.db");
        let mut conn = schema_two_fixture(&path);
        conn.execute_batch("CREATE TABLE extra_history(id INTEGER PRIMARY KEY, finding_id INTEGER REFERENCES findings(id) ON DELETE CASCADE); INSERT INTO extra_history VALUES (1,61);").unwrap();
        assert!(
            apply_migrations(&mut conn, MIGRATIONS)
                .unwrap_err()
                .to_string()
                .contains("reverse foreign-key graph")
        );
        validate_connection_version(&conn, &MIGRATIONS[..2]).unwrap();
        assert_eq!(rows(&conn, "extra_history").len(), 1);
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
