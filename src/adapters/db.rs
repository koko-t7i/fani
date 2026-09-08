mod schema;
mod verification;
use schema::*;
mod attempts;
mod canonical;
mod documents;
mod effects;
mod leases;
mod legacy;
mod preparation;
mod publication;
#[cfg(test)]
mod tests;
mod workflows;
use crate::application::ports::*;

use crate::adapters::process::process_identity;

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

struct StoredMaterializationIntent {
    work_item_id: i64,
    state: String,
    repository_id: i64,
    document_id: Option<i64>,
    locale: String,
    payload_json: String,
}

fn enqueue_document_work_item_in_transaction(
    conn: &Connection,
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
               ON CONFLICT(run_id,document_id,locale,kind,COALESCE(json_extract(input_json, '$.effect_key'), '')) WHERE document_id IS NOT NULL DO UPDATE SET
                 priority=excluded.priority,
                 input_json=excluded.input_json,
                 policy_fingerprint=excluded.policy_fingerprint,
                 updated_at=excluded.updated_at"#,
            params![run_id, document_id, locale, kind, priority, input_json, now_ms()],
        )?;
    Ok(conn.query_row(
            "SELECT id FROM work_items WHERE run_id=?1 AND document_id=?2 AND locale=?3 AND kind=?4 AND COALESCE(json_extract(input_json, '$.effect_key'), '')=COALESCE(json_extract(?5, '$.effect_key'), '')",
            params![run_id, document_id, locale, kind, input_json],
            |row| row.get(0),
        )?)
}

fn supersede_materializations_in_transaction(
    conn: &Connection,
    repository_id: i64,
    locale: &str,
    path: &str,
    active_dedupe_key: &str,
) -> Result<usize> {
    let now = now_ms();
    let owners = {
        let mut statement = conn.prepare(
                r#"SELECT owner FROM materialization_outbox
                   WHERE dedupe_key<>?3 AND state='processing'
                     AND json_extract(payload_json,'$.locale')=?1
                     AND json_extract(payload_json,'$.path')=?2
                     AND work_item_id IN (SELECT w.id FROM work_items w JOIN runs r ON r.id=w.run_id WHERE r.repository_id=?4)"#,
            )?;
        statement
            .query_map(
                params![locale, path, active_dedupe_key, repository_id],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    if owners.iter().any(|owner| !dead_process_owner(owner)) {
        bail!("older materialization for {path} is still owned by a live worker");
    }
    conn.execute(
            "UPDATE work_items SET status='cancelled',updated_at=?4 WHERE id IN (SELECT o.work_item_id FROM materialization_outbox o JOIN work_items w ON w.id=o.work_item_id JOIN runs r ON r.id=w.run_id WHERE r.repository_id=?5 AND o.dedupe_key<>?3 AND o.state<>'done' AND json_extract(o.payload_json,'$.locale')=?1 AND json_extract(o.payload_json,'$.path')=?2)",
            params![locale, path, active_dedupe_key, now, repository_id],
        )?;
    let changed = conn.execute(
            r#"UPDATE materialization_outbox
               SET state='done',owner=NULL,lease_expires_at=NULL,
                   last_error='superseded by newer canonical content',completed_at=?4
               WHERE dedupe_key<>?3 AND state<>'done'
                 AND json_extract(payload_json,'$.locale')=?1
                 AND json_extract(payload_json,'$.path')=?2
                 AND work_item_id IN (SELECT w.id FROM work_items w JOIN runs r ON r.id=w.run_id WHERE r.repository_id=?5)"#,
            params![locale, path, active_dedupe_key, now, repository_id],
        )?;
    Ok(changed)
}

fn enqueue_materialization_in_transaction(
    conn: &Connection,
    work_item_id: i64,
    dedupe_key: &str,
    payload_json: &str,
    now: i64,
) -> Result<i64> {
    require_json(payload_json)?;
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

fn bind_document_intent_in_transaction(
    tx: &Connection,
    content_version_id: i64,
    identity_json: &str,
) -> Result<()> {
    tx.execute(
            "INSERT OR IGNORE INTO canonical_document_intents(canonical_content_version_id,identity_json,created_at) VALUES (?1,?2,?3)",
            params![content_version_id, identity_json, now_ms()],
        )?;
    let compatible: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM canonical_content_versions v JOIN canonical_files f ON f.id=v.canonical_file_id WHERE v.id=?1 AND v.source_revision=json_extract(?2,'$.source_revision') AND f.path=json_extract(?2,'$.target_path') AND f.locale=json_extract(?2,'$.locale'))",
            params![content_version_id, identity_json], |row| row.get(0),
        )?;
    if !compatible {
        bail!("canonical document identity conflicts with immutable content");
    }
    tx.execute(
            "INSERT INTO canonical_document_intent_selection(canonical_content_version_id,intent_id) SELECT canonical_content_version_id,id FROM canonical_document_intents WHERE canonical_content_version_id=?1 AND identity_json=?2 ON CONFLICT(canonical_content_version_id) DO UPDATE SET intent_id=excluded.intent_id",
            params![content_version_id, identity_json],
        )?;
    Ok(())
}

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
        let document: Option<String> = conn.query_row(
            "SELECT json_extract(input_json,'$.document_identity') FROM work_items WHERE id=?1",
            [work_item_id],
            |row| row.get(0),
        )?;
        if let Some(document) = document {
            object.insert("_fani_document".into(), serde_json::from_str(&document)?);
        }
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

fn refresh_publication_states(conn: &Connection, manifest_id: i64, now: i64) -> Result<()> {
    // Shared content keeps the strongest remaining authorization, not the last cancelled effect.
    conn.execute(
        r#"UPDATE canonical_content_versions AS cv SET publication_state=(
             SELECT pm.state FROM publication_manifests pm JOIN publication_manifest_files f ON f.manifest_id=pm.id
             WHERE f.canonical_content_version_id=cv.id
             ORDER BY CASE pm.state WHEN 'merged' THEN 5 WHEN 'pr_open' THEN 4 WHEN 'push_pending' THEN 3 WHEN 'commit_created' THEN 2 ELSE 1 END DESC,pm.id DESC LIMIT 1)
           WHERE cv.id IN (SELECT canonical_content_version_id FROM publication_manifest_files WHERE manifest_id=?1)
             AND cv.publication_state<>'merged'"#,
        [manifest_id],
    )?;
    conn.execute(
        r#"UPDATE canonical_files AS cf SET
             publication_state=(SELECT publication_state FROM canonical_content_versions WHERE id=cf.current_content_version_id),
             state=CASE WHEN (SELECT publication_state FROM canonical_content_versions WHERE id=cf.current_content_version_id)='pr_open' THEN 'published' ELSE state END,
             updated_at=?2
           WHERE current_content_version_id IN (SELECT canonical_content_version_id FROM publication_manifest_files WHERE manifest_id=?1)
             AND publication_state<>'merged'"#,
        params![manifest_id,now],
    )?;
    conn.execute(
        r#"UPDATE translation_versions AS tv SET publication_state=(
             SELECT pm.state FROM publication_manifests pm JOIN publication_manifest_files f ON f.manifest_id=pm.id
             JOIN canonical_file_translations cft ON cft.canonical_content_version_id=f.canonical_content_version_id
             WHERE cft.translation_version_id=tv.id
             ORDER BY CASE pm.state WHEN 'merged' THEN 5 WHEN 'pr_open' THEN 4 WHEN 'push_pending' THEN 3 WHEN 'commit_created' THEN 2 ELSE 1 END DESC,pm.id DESC LIMIT 1)
           WHERE tv.id IN (SELECT cft.translation_version_id FROM publication_manifest_files f JOIN canonical_file_translations cft ON cft.canonical_content_version_id=f.canonical_content_version_id WHERE f.manifest_id=?1)
             AND tv.publication_state<>'merged'"#,
        [manifest_id],
    )?;
    Ok(())
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
    fn planning(&self) -> &dyn PlanningStore {
        self
    }
    fn runs(&self) -> &dyn RunStore {
        self
    }
    fn verification(&self) -> &dyn VerificationStore {
        self
    }
    fn repositories(&self) -> &dyn RepositoryStore {
        self
    }
    fn preparation(&self) -> &dyn PreparationStore {
        self
    }
    fn pipeline(&self) -> &dyn PipelineStore {
        self
    }
    fn materialization(&self) -> &dyn MaterializationStore {
        self
    }
    fn publication(&self) -> &dyn PublicationWorkflowStore {
        self
    }
    fn reconciliation(&self) -> &dyn ReconciliationStore {
        self
    }
}

impl PlanningStore for Database {
    fn repository_id(&self, repository_key: &str) -> Result<Option<i64>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT id FROM repositories WHERE repository_key=?1",
                [repository_key],
                |row| row.get(0),
            )
            .optional()?)
    }
    fn document_id(&self, repository_id: i64, path: &str) -> Result<Option<i64>> {
        Database::document_id(self, repository_id, path)
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
    fn canonical_file(
        &self,
        repository_id: i64,
        locale: &str,
        path: &str,
    ) -> Result<Option<CanonicalFile>> {
        Database::canonical_file(self, repository_id, locale, path)
    }
}

impl RepositoryStore for Database {
    fn upsert_repository(
        &self,
        repository_key: &str,
        root_path: &Path,
        default_branch: Option<&str>,
        remote_url: Option<&str>,
    ) -> Result<i64> {
        Database::upsert_repository(self, repository_key, root_path, default_branch, remote_url)
    }
    fn repository_id(&self, repository_key: &str) -> Result<Option<i64>> {
        PlanningStore::repository_id(self, repository_key)
    }
}

impl RunStore for Database {
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
}
