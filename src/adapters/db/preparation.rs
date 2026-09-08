use super::*;

pub(super) fn upsert_document_in_transaction(
    conn: &Connection,
    repository_id: i64,
    path: &str,
    source_revision: Option<&str>,
    content_hash: &str,
    metadata_json: &str,
) -> Result<i64> {
    require_json(metadata_json)?;
    let now = now_ms();
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

pub(super) fn upsert_unit_in_transaction(
    conn: &Connection,
    document_id: i64,
    unit_key: &str,
    ordinal: i64,
    source_text: &str,
    source_hash: &str,
    context_json: &str,
) -> Result<i64> {
    require_json(context_json)?;
    let now = now_ms();
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

pub(super) fn enqueue_work_item_in_transaction(
    conn: &Connection,
    run_id: &str,
    unit_id: i64,
    locale: &str,
    kind: &str,
    priority: i64,
    input_json: &str,
) -> Result<i64> {
    require_json(input_json)?;
    let now = now_ms();
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

pub(super) fn quarantine_incompatible_translations_in_transaction(
    conn: &Connection,
    document_id: i64,
    locale: &str,
) -> Result<()> {
    let versions = {
        let mut statement = conn.prepare("SELECT tv.id,tv.target_text FROM translation_versions tv JOIN unit_versions uv ON uv.id=tv.unit_version_id JOIN units u ON u.id=uv.unit_id WHERE u.document_id=?1 AND tv.locale=?2 AND tv.superseded_at IS NULL")?;
        statement
            .query_map(params![document_id, locale], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, text) in versions {
        if version_provenance(conn, id)
            .as_ref()
            .is_ok_and(|bound| validate_stored_translation(bound, &text))
        {
            continue;
        }
        conn.execute("UPDATE translation_memory_entries SET tier='history',superseded_at=?2 WHERE translation_version_id=?1 AND superseded_at IS NULL", params![id,now_ms()])?;
        conn.execute("UPDATE translation_versions SET validation_state='quarantined',superseded_at=?2 WHERE id=?1", params![id,now_ms()])?;
    }

    Ok(())
}

pub(super) fn revalidate_candidate_in_transaction(
    conn: &Connection,
    unit_id: i64,
    locale: &str,
    candidate: &TranslationCandidate,
) -> Result<()> {
    let metadata = crate::domain::document::compatible_metadata(&candidate.provenance)
        .ok_or_else(|| anyhow!("incompatible candidate metadata"))?;
    if !metadata.needs_markdown_snapshot_upgrade() {
        return Ok(());
    }
    let (version_id, source_hash, bound): (i64,String,UnitProvenance) = conn.query_row(
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
    let attempt: i64 = conn.query_row("SELECT a.id FROM canonical_candidates c JOIN attempts a ON a.id=c.source_attempt_id WHERE c.unit_id=?1 AND c.locale=?2 AND c.selected=1 AND c.target_text=?3 AND a.status='succeeded' AND json_extract(a.response_json,'$.output')=c.target_text", params![unit_id,locale,candidate.text], |row| row.get(0))?;
    let original = attempt_provenance(conn, attempt)?
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
    conn.execute("INSERT INTO translation_versions(unit_version_id,locale,target_text,target_hash,freshness,provenance,validation_state,review_state,publication_state,policy_fingerprint,source_attempt_id,created_at) VALUES (?1,?2,?3,?4,'exact','candidate_tm','passed','unreviewed','candidate',?5,?6,?7)", params![version_id,locale,candidate.text,migration_checksum(&candidate.text),bound.policy_fingerprint,attempt,now])?;
    let translation_id = conn.last_insert_rowid();
    conn.execute("UPDATE translation_memory_entries SET tier='history',superseded_at=?4 WHERE repository_id=(SELECT d.repository_id FROM units u JOIN documents d ON d.id=u.document_id WHERE u.id=?1) AND source_hash=(SELECT source_hash FROM units WHERE id=?1) AND locale=?2 AND context_key=?3 AND tier='candidate' AND superseded_at IS NULL", params![unit_id,locale,key,now])?;
    let audit = serde_json::json!({"origin":"compatible_candidate","_fani_unit":bound}).to_string();
    conn.execute("INSERT INTO translation_memory_entries(repository_id,unit_id,translation_version_id,locale,source_hash,source_revision,context_key,target_text,tier,provenance,policy_fingerprint,created_at) SELECT d.repository_id,?1,?2,?3,?4,?5,?6,?7,'candidate',?8,?9,?10 FROM units u JOIN documents d ON d.id=u.document_id WHERE u.id=?1", params![unit_id,translation_id,locale,source_hash,bound.source_revision,key,candidate.text,audit,bound.policy_fingerprint,now])?;

    Ok(())
}

impl Database {
    pub fn revalidate_candidate(
        &self,
        unit_id: i64,
        locale: &str,
        candidate: &TranslationCandidate,
    ) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        preparation::revalidate_candidate_in_transaction(&tx, unit_id, locale, candidate)?;
        tx.commit()?;
        Ok(())
    }
}
