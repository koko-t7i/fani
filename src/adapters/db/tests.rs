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
    let migrations = &MIGRATIONS[..2];
    apply_migrations(&mut conn, migrations).unwrap();
    conn.execute("DELETE FROM schema_migrations WHERE version=1", [])
        .unwrap();

    let error = apply_migrations(&mut conn, migrations).unwrap_err();
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
    assert_eq!(rows(&conn, "schema_migrations").len(), 6);
}

#[test]
fn publication_authorization_upgrade_preserves_snapshots_and_rolls_back_faults() {
    for old_version in 2..=5 {
        for fault in [
            None,
            Some(
                "CREATE TRIGGER reject_authorization_upgrade BEFORE UPDATE ON state_schema WHEN NEW.version=6 BEGIN SELECT RAISE(ABORT,'injected authorization failure'); END;",
            ),
            Some(
                "CREATE TRIGGER corrupt_authorization_marker AFTER UPDATE ON state_schema WHEN NEW.version=6 BEGIN UPDATE state_schema SET version=5 WHERE singleton=1; END;",
            ),
            Some(
                "CREATE TRIGGER corrupt_authorization_ledger AFTER INSERT ON schema_migrations WHEN NEW.version=6 BEGIN UPDATE schema_migrations SET checksum=lower(hex(zeroblob(32))) WHERE version=1; END;",
            ),
            Some(
                "CREATE TRIGGER corrupt_authorization_fk AFTER UPDATE ON state_schema WHEN NEW.version=6 BEGIN UPDATE publication_manifest_files SET canonical_content_version_id=9999; END;",
            ),
            Some(
                "CREATE TABLE custom_manifest_child(id INTEGER REFERENCES publication_manifests(id));",
            ),
            Some("CREATE INDEX custom_manifest_index ON publication_manifests(state);"),
            Some(
                "CREATE TRIGGER custom_manifest_trigger AFTER UPDATE ON publication_manifests BEGIN SELECT 1; END;",
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("old.db");
            let mut conn = schema_two_fixture(&path);
            apply_migrations(&mut conn, &MIGRATIONS[..old_version]).unwrap();
            conn.execute_batch("INSERT INTO publication_manifests VALUES (151,11,'old-run','fr','rev','commit',lower(hex(zeroblob(32))),'commit_created',1,NULL); INSERT INTO publication_manifest_files VALUES (151,141,131,'hash'); UPDATE publication_outbox SET payload_json='{\"commit\":\"commit\",\"run_id\":\"old-run\"}' WHERE id=91;").unwrap();
            if let Some(sql) = fault {
                conn.execute_batch(sql).unwrap();
            }
            let schema = rows(&conn, "sqlite_schema");
            let tables = conn
                .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")
                .unwrap()
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            let before = tables
                .iter()
                .map(|table| rows(&conn, table))
                .collect::<Vec<_>>();
            let result = apply_migrations(&mut conn, MIGRATIONS);
            if fault.is_some() {
                assert!(result.is_err(), "schema {old_version}: {fault:?}");
                validate_connection_version(&conn, &MIGRATIONS[..old_version]).unwrap();
                assert_eq!(rows(&conn, "sqlite_schema"), schema);
            } else {
                result.unwrap();
                validate_existing_connection(&conn).unwrap();
                assert_eq!(
                    conn.query_row(
                        "SELECT authorization_key FROM publication_manifests WHERE id=151",
                        [],
                        |r| r.get::<_, String>(0)
                    )
                    .unwrap(),
                    "publish"
                );
                let error = conn.execute("UPDATE publication_manifest_files SET content_hash='wrong-hash' WHERE manifest_id=151", []).unwrap_err();
                assert!(error.to_string().contains("FOREIGN KEY"), "{error}");
            }
            let backup = Connection::open(
                temp.path()
                    .join(format!("old.db.pre-schema-{old_version}.bak")),
            )
            .unwrap();
            validate_connection_version(&backup, &MIGRATIONS[..old_version]).unwrap();
            for (table, expected) in tables.iter().zip(before) {
                assert_eq!(rows(&backup, table), expected, "backup {table}");
                if fault.is_some()
                    || !matches!(table.as_str(), "schema_migrations" | "state_schema")
                {
                    let mut actual = rows(&conn, table);
                    if fault.is_none() && table == "publication_manifests" {
                        for row in &mut actual {
                            row.pop();
                        }
                    }
                    if fault.is_none() && old_version == 2 && table == "work_items" {
                        for row in &mut actual {
                            row.remove(3);
                        }
                    }
                    assert_eq!(actual, expected, "schema {old_version}: {table}");
                }
            }
            assert_eq!(
                conn.query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                1
            );
        }
    }
}

#[test]
fn schema_four_intent_selection_upgrade_preserves_history_and_rolls_back() {
    for fault in [
        None,
        Some(
            "CREATE TRIGGER corrupt_selection_upgrade AFTER UPDATE ON state_schema WHEN NEW.version=5 BEGIN UPDATE state_schema SET version=4 WHERE singleton=1; END;",
        ),
        Some(
            "CREATE TRIGGER corrupt_selection_ledger AFTER INSERT ON schema_migrations WHEN NEW.version=5 BEGIN UPDATE schema_migrations SET checksum=lower(hex(zeroblob(32))) WHERE version=1; END;",
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("old.db");
        let mut conn = schema_two_fixture(&path);
        apply_migrations(&mut conn, &MIGRATIONS[..4]).unwrap();
        conn.execute_batch("INSERT INTO canonical_document_intents(id,canonical_content_version_id,identity_json,created_at) SELECT 1,id,'{\"identity\":\"A\"}',1 FROM canonical_content_versions LIMIT 1; INSERT INTO canonical_document_intents(id,canonical_content_version_id,identity_json,created_at) SELECT 2,id,'{\"identity\":\"B\"}',2 FROM canonical_content_versions LIMIT 1;").unwrap();
        if let Some(sql) = fault {
            conn.execute_batch(sql).unwrap();
        }
        let schema = rows(&conn, "sqlite_schema");
        let tables = conn
            .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let before = tables
            .iter()
            .map(|table| rows(&conn, table))
            .collect::<Vec<_>>();
        let result = apply_migrations(&mut conn, MIGRATIONS);
        if fault.is_some() {
            assert!(result.is_err());
            validate_connection_version(&conn, &MIGRATIONS[..4]).unwrap();
            assert_eq!(rows(&conn, "sqlite_schema"), schema);
        } else {
            result.unwrap();
            validate_existing_connection(&conn).unwrap();
            assert_eq!(
                conn.query_row(
                    "SELECT intent_id FROM canonical_document_intent_selection",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                2
            );
        }
        let backup = Connection::open(temp.path().join("old.db.pre-schema-4.bak")).unwrap();
        validate_connection_version(&backup, &MIGRATIONS[..4]).unwrap();
        for (table, expected) in tables.iter().zip(before) {
            assert_eq!(rows(&backup, table), expected, "backup {table}");
            if fault.is_some() || !matches!(table.as_str(), "schema_migrations" | "state_schema") {
                assert_eq!(rows(&conn, table), expected, "current {table}");
            }
        }
        assert_eq!(
            conn.query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}

#[test]
fn schema_three_revision_upgrade_preserves_ids_and_rolls_back_faults() {
    for fault in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("old.db");
        let mut conn = schema_two_fixture(&path);
        apply_migrations(&mut conn, &MIGRATIONS[..3]).unwrap();
        if fault {
            conn.execute_batch("CREATE TRIGGER corrupt_revision_upgrade AFTER UPDATE ON state_schema WHEN NEW.version=4 BEGIN UPDATE state_schema SET version=3 WHERE singleton=1; END;").unwrap();
        }
        let tables = [
            "canonical_files",
            "canonical_content_versions",
            "canonical_file_translations",
            "publication_manifests",
            "publication_manifest_files",
            "work_items",
            "materialization_outbox",
            "publication_outbox",
        ];
        let before = tables.map(|table| rows(&conn, table));
        let result = apply_migrations(&mut conn, MIGRATIONS);
        if fault {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("invalid state_schema marker")
            );
            validate_connection_version(&conn, &MIGRATIONS[..3]).unwrap();
        } else {
            result.unwrap();
            validate_existing_connection(&conn).unwrap();
        }
        for (table, previous) in tables.into_iter().zip(before) {
            assert_eq!(rows(&conn, table), previous, "{table}");
        }
        let backup = Connection::open(temp.path().join("old.db.pre-schema-3.bak")).unwrap();
        validate_connection_version(&backup, &MIGRATIONS[..3]).unwrap();
        assert_eq!(
            conn.query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
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
fn final_schema_contract_faults_roll_back_the_complete_upgrade() {
    for (trigger, message) in [
        (
            "CREATE TRIGGER corrupt_marker AFTER UPDATE ON state_schema WHEN NEW.version=3 BEGIN UPDATE state_schema SET version=2 WHERE singleton=1; END;",
            "invalid state_schema marker",
        ),
        (
            "CREATE TRIGGER corrupt_ledger AFTER INSERT ON schema_migrations WHEN NEW.version=3 BEGIN UPDATE schema_migrations SET checksum=lower(hex(zeroblob(32))) WHERE version=1; END;",
            "checksum/name mismatch",
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("old.db");
        let mut conn = schema_two_fixture(&path);
        conn.execute_batch(trigger).unwrap();
        let schema = rows(&conn, "sqlite_schema");
        let error = apply_migrations(&mut conn, MIGRATIONS).unwrap_err();
        assert!(error.to_string().contains(message), "{error:#}");
        validate_connection_version(&conn, &MIGRATIONS[..2]).unwrap();
        assert_eq!(rows(&conn, "sqlite_schema"), schema);
        assert_eq!(
            conn.query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        let backup = Connection::open(temp.path().join("old.db.pre-schema-2.bak")).unwrap();
        validate_connection_version(&backup, &MIGRATIONS[..2]).unwrap();
        let mut statement = backup
            .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")
            .unwrap();
        let tables = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        for table in tables {
            assert_eq!(rows(&conn, &table), rows(&backup, &table), "{table}");
        }
    }
}

#[test]
fn upgrade_rejects_live_owners_with_unreadable_identity() {
    std::thread::spawn(|| {
        let pid = nix::unistd::gettid().as_raw() as u32;
        let (_, started) = process_identity(pid).unwrap();
        nix::sys::prctl::set_name(c"\xff").unwrap();
        assert!(process_identity(pid).is_none());
        assert!(fs::metadata(format!("/proc/{pid}")).is_ok());
        for kind in ["repository", "materialize", "publish"] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("old.db");
            let mut conn = schema_two_fixture(&path);
            let identity = format!("{pid}:{started}:run");
            if kind == "repository" {
                conn.execute(
                    "INSERT INTO leases VALUES ('repository','repo',?1,1,1,2)",
                    [&identity],
                )
                .unwrap();
            } else {
                let table = if kind == "materialize" {
                    "materialization_outbox"
                } else {
                    "publication_outbox"
                };
                conn.execute(
                    &format!("UPDATE {table} SET state='processing',owner=?1,lease_expires_at=2"),
                    [format!("{kind}:{identity}")],
                )
                .unwrap();
            }
            let message = if kind == "repository" {
                "all application leases released"
            } else {
                "outbox workers stopped"
            };
            assert!(
                apply_migrations(&mut conn, MIGRATIONS)
                    .unwrap_err()
                    .to_string()
                    .contains(message)
            );
            validate_connection_version(&conn, &MIGRATIONS[..2]).unwrap();
            assert!(!temp.path().join("old.db.pre-schema-2.bak").exists());
        }
    })
    .join()
    .unwrap();
}

#[test]
fn migration_guard_accepts_confirmed_dead_and_reused_owners() {
    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let pid = child.id();
    let identity = process_identity(pid);
    child.kill().unwrap();
    child.wait().unwrap();
    let (_, started) = identity.unwrap();
    let dead = format!("{pid}:{started}:run");
    assert!(migration_owner_is_dead(&dead));
    let (pid, started) = process_identity(std::process::id()).unwrap();
    assert!(!migration_owner_is_dead(&format!("{pid}:{started}:run")));
    let reused = format!("{pid}:{}:run", started + 1);
    assert!(migration_owner_is_dead(&reused));
    assert!(!migration_owner_is_dead("unknown"));
    let temp = tempfile::tempdir().unwrap();
    let mut conn = schema_two_fixture(&temp.path().join("old.db"));
    conn.execute(
        "INSERT INTO leases VALUES ('repository','repo',?1,1,1,2)",
        [&dead],
    )
    .unwrap();
    conn.execute(
        "UPDATE materialization_outbox SET state='processing',owner=?1,lease_expires_at=2",
        [format!("materialize:{reused}")],
    )
    .unwrap();
    conn.execute(
        "UPDATE publication_outbox SET state='processing',owner=?1,lease_expires_at=2",
        [format!("publish:{dead}")],
    )
    .unwrap();
    apply_migrations(&mut conn, MIGRATIONS).unwrap();
    validate_existing_connection(&conn).unwrap();
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
