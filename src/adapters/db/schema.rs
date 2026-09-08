use super::*;

pub(super) const APPLICATION_ID: i64 = 0x4641_4e49;
pub(super) const SCHEMA_VERSION: i64 = 6;
pub(super) const BUSY_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) static ID_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
pub(super) struct Migration {
    pub(super) version: i64,
    pub(super) name: &'static str,
    pub(super) sql: &'static str,
}

pub(super) const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "0001_native_authority",
        sql: include_str!("../../../migrations/0001_native_authority.sql"),
    },
    Migration {
        version: 2,
        name: "0002_orthogonal_translation_state",
        sql: include_str!("../../../migrations/0002_orthogonal_translation_state.sql"),
    },
    Migration {
        version: 3,
        name: "0003_document_work_items",
        sql: include_str!("../../../migrations/0003_document_work_items.sql"),
    },
    Migration {
        version: 4,
        name: "0004_revision_bound_canonical_content",
        sql: include_str!("../../../migrations/0004_revision_bound_canonical_content.sql"),
    },
    Migration {
        version: 5,
        name: "0005_current_intent_effects",
        sql: include_str!("../../../migrations/0005_current_intent_effects.sql"),
    },
    Migration {
        version: 6,
        name: "0006_publication_authorizations",
        sql: include_str!("../../../migrations/0006_publication_authorizations.sql"),
    },
];

pub(super) const MIGRATION_LEDGER_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY CHECK (version > 0),
    name TEXT NOT NULL UNIQUE,
    checksum TEXT NOT NULL CHECK (length(checksum) = 64),
    applied_at INTEGER NOT NULL
) STRICT;
"#;

pub(super) fn migration_checksum(sql: &str) -> String {
    format!("{:x}", Sha256::digest(sql.as_bytes()))
}

pub(super) fn configure_connection(conn: &Connection) -> Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "journal_mode", "DELETE")?;
    Ok(())
}

pub(super) fn open_connection(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)
        .with_context(|| format!("cannot open SQLite database {}", path.display()))?;
    configure_connection(&conn)?;
    Ok(conn)
}

pub(super) fn application_id(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA application_id", [], |row| row.get(0))?)
}

pub(super) fn user_table_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?)
}

pub(super) fn has_table(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |row| row.get(0),
    )?)
}

pub(super) fn validate_migration_list(migrations: &[Migration]) -> Result<()> {
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

pub(super) fn apply_migrations(conn: &mut Connection, migrations: &[Migration]) -> Result<()> {
    // DROP TABLE must not cascade through historical attempts and outboxes.
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let result = apply_migrations_exclusive(conn, migrations);
    conn.pragma_update(None, "foreign_keys", "ON")?;
    result
}

pub(super) fn apply_migrations_exclusive(
    conn: &mut Connection,
    migrations: &[Migration],
) -> Result<()> {
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
        if migration.version == 4 {
            validate_canonical_content_dependents(&tx)?;
        }
        if migration.version == 6 {
            validate_publication_manifest_dependents(&tx)?;
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
        validate_connection_version(&tx, &migrations[..migration.version as usize])?;
    }

    validate_connection_version(&tx, migrations)?;
    tx.commit()?;
    Ok(())
}

pub(super) fn migration_owner_is_dead(owner: &str) -> bool {
    let mut parts = owner.split(':');
    let Some((pid, started_at)) = parts
        .next()
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|pid| *pid > 0)
        .zip(parts.next().and_then(|value| value.parse::<u64>().ok()))
    else {
        return false;
    };
    match process_identity(pid as u32) {
        Some(identity) => identity.1 != started_at,
        // Signal zero confirms absence without treating unreadable /proc data as death.
        None => matches!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
            Err(nix::errno::Errno::ESRCH)
        ),
    }
}

pub(super) fn require_idle_application(conn: &Connection) -> Result<()> {
    let mut statement = conn.prepare("SELECT resource_type,owner FROM leases")?;
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (kind, owner) = row?;
        if kind != "repository" || !migration_owner_is_dead(&owner) {
            bail!(
                "database upgrade requires all application leases released; stop all fani processes first"
            );
        }
    }
    let mut statement = conn.prepare(
        "SELECT owner FROM materialization_outbox WHERE state='processing' UNION ALL SELECT owner FROM publication_outbox WHERE state='processing'",
    )?;
    for owner in statement.query_map([], |row| row.get::<_, String>(0))? {
        let owner = owner?;
        let identity = owner
            .strip_prefix("materialize:")
            .or_else(|| owner.strip_prefix("publish:"));
        if !identity.is_some_and(migration_owner_is_dead) {
            bail!("database upgrade requires all outbox workers stopped");
        }
    }
    Ok(())
}

pub(super) fn backup_before_upgrade(conn: &Connection, version: i64) -> Result<()> {
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

pub(super) fn validate_work_item_dependents(conn: &Connection) -> Result<()> {
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

pub(super) fn validate_publication_manifest_dependents(conn: &Connection) -> Result<()> {
    let mut statement = conn.prepare(
        "SELECT m.name,f.\"from\",f.\"to\",f.on_delete FROM sqlite_schema m JOIN pragma_foreign_key_list(m.name) f WHERE m.type='table' AND f.\"table\"='publication_manifests' ORDER BY 1,2,3,4",
    )?;
    let actual = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if actual
        != [(
            "publication_manifest_files".into(),
            "manifest_id".into(),
            "id".into(),
            "CASCADE".into(),
        )]
    {
        bail!("unexpected publication manifest foreign keys; refusing table rebuild");
    }
    let extensions: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_schema WHERE tbl_name='publication_manifests' AND (type='trigger' OR (type='index' AND sql IS NOT NULL))",
        [], |row| row.get(0),
    )?;
    if extensions != 0 {
        bail!("unexpected publication manifest indexes or triggers; refusing table rebuild");
    }
    Ok(())
}

pub(super) fn validate_canonical_content_dependents(conn: &Connection) -> Result<()> {
    let mut statement = conn.prepare(
        "SELECT m.name,f.\"from\",f.\"to\",f.on_delete FROM sqlite_schema m JOIN pragma_foreign_key_list(m.name) f WHERE m.type='table' AND f.\"table\"='canonical_content_versions' ORDER BY 1,2,3,4",
    )?;
    let actual = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let expected = [
        (
            "canonical_file_translations",
            "canonical_content_version_id",
            "id",
            "CASCADE",
        ),
        (
            "canonical_files",
            "current_content_version_id",
            "id",
            "RESTRICT",
        ),
        (
            "publication_manifest_files",
            "canonical_content_version_id",
            "id",
            "RESTRICT",
        ),
        (
            "publication_manifest_files",
            "canonical_file_id",
            "canonical_file_id",
            "RESTRICT",
        ),
        (
            "publication_manifest_files",
            "content_hash",
            "content_hash",
            "RESTRICT",
        ),
    ]
    .map(|(a, b, c, d)| (a.into(), b.into(), c.into(), d.into()));
    if actual != expected {
        bail!("unexpected canonical content foreign keys; refusing table rebuild");
    }
    let extensions: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_schema WHERE tbl_name='canonical_content_versions' AND (type='trigger' OR (type='index' AND sql IS NOT NULL))",
        [], |row| row.get(0),
    )?;
    if extensions != 0 {
        bail!("unexpected canonical content indexes or triggers; refusing table rebuild");
    }
    Ok(())
}

pub(super) fn validate_physical_integrity(conn: &Connection) -> Result<()> {
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

pub(super) fn validate_existing_connection(conn: &Connection) -> Result<()> {
    validate_connection_version(conn, MIGRATIONS)
}

pub(super) fn validate_connection_version(
    conn: &Connection,
    migrations: &[Migration],
) -> Result<()> {
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
