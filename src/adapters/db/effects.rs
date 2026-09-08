use super::*;

pub(super) fn effect_key_in_transaction(
    conn: &Connection,
    kind: OutboxKind,
    base: &str,
) -> Result<String> {
    let mut key = base.to_owned();
    loop {
        let row: Option<(i64, String, String)> = conn
            .query_row(
                &format!(
                    "SELECT id,state,payload_json FROM {} WHERE dedupe_key=?1",
                    outbox_table(kind)
                ),
                [&key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((id, state, payload)) = row else {
            return Ok(key);
        };
        if state != "done" {
            return Ok(key);
        }
        if matches!(kind, OutboxKind::Publication) {
            let payload: serde_json::Value = serde_json::from_str(&payload)?;
            let mut authorized = payload.get("superseded_reason").is_none();
            if let Some(commit) = payload.get("commit").and_then(|value| value.as_str()) {
                authorized &= conn.query_row(
                        "SELECT EXISTS(SELECT 1 FROM publication_manifests m JOIN publication_outbox o ON o.repository_id=m.repository_id AND o.locale=m.locale AND o.dedupe_key=m.authorization_key WHERE o.id=?1 AND m.candidate_commit=?2 AND m.state<>'superseded')",
                        params![id,commit], |row| row.get::<_, bool>(0),
                    )?;
            }
            if authorized {
                return Ok(key);
            }
        }
        key = format!("{base}:after:{id}");
    }
}

pub(super) fn schedule_materialization_in_transaction(
    conn: &Connection,
    input: MaterializationIntentInput<'_>,
) -> Result<MaterializationReceipt> {
    require_json(input.work_input_json)?;
    require_json(input.payload_json)?;
    let existing = conn.query_row(
            "SELECT o.work_item_id,o.state,r.repository_id,w.document_id,w.locale,o.payload_json FROM materialization_outbox o JOIN work_items w ON w.id=o.work_item_id JOIN runs r ON r.id=w.run_id WHERE o.dedupe_key=?1",
            [input.dedupe_key],
            |row| Ok(StoredMaterializationIntent {
                work_item_id: row.get(0)?, state: row.get(1)?, repository_id: row.get(2)?,
                document_id: row.get(3)?, locale: row.get(4)?, payload_json: row.get(5)?,
            }),
        ).optional()?;
    if let Some(existing) = &existing {
        if existing.repository_id != input.repository_id
            || existing.document_id != Some(input.document_id)
            || existing.locale != input.locale
            || serde_json::from_str::<serde_json::Value>(&existing.payload_json)?
                .get("path")
                .and_then(serde_json::Value::as_str)
                != Some(input.path)
        {
            bail!("materialization effect key conflicts with an existing durable intent");
        }
    }
    if existing
        .as_ref()
        .is_some_and(|existing| existing.state == "done")
    {
        bail!("materialization effect key is already terminal; resolve a new effect key");
    }
    let compatible: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM documents d JOIN runs r ON r.repository_id=d.repository_id WHERE d.id=?1 AND r.id=?2 AND r.repository_id=?3) AND json_extract(?4,'$.repository_id')=?3 AND json_extract(?4,'$.locale')=?5 AND json_extract(?4,'$.path')=?6 AND json_extract(?7,'$.effect_key')=?8",
            params![input.document_id,input.run_id,input.repository_id,input.payload_json,input.locale,input.path,input.work_input_json,input.dedupe_key],
            |row| row.get::<_, Option<bool>>(0),
        )?.unwrap_or(false);
    if !compatible {
        bail!(
            "materialization intent does not match its repository, document, locale, path or effect key"
        );
    }
    supersede_materializations_in_transaction(
        conn,
        input.repository_id,
        input.locale,
        input.path,
        input.dedupe_key,
    )?;
    let work_item_id = match existing {
        Some(existing) => existing.work_item_id,
        None => enqueue_document_work_item_in_transaction(
            conn,
            input.run_id,
            input.document_id,
            input.locale,
            "materialization",
            0,
            input.work_input_json,
        )?,
    };
    let outbox_id = enqueue_materialization_in_transaction(
        conn,
        work_item_id,
        input.dedupe_key,
        input.payload_json,
        now_ms(),
    )?;

    Ok(MaterializationReceipt {
        work_item_id,
        outbox_id,
    })
}

pub(super) fn finish_document_work_in_transaction(
    conn: &Connection,
    work_item_id: i64,
    succeeded: bool,
    result_json: &str,
) -> Result<()> {
    require_json(result_json)?;
    if conn.execute(
            "UPDATE work_items SET status=?2,result_json=?3,updated_at=?4 WHERE id=?1 AND document_id IS NOT NULL",
            params![work_item_id, if succeeded { "succeeded" } else { "failed" }, result_json, now_ms()],
        )? != 1 {
            bail!("document work completion requires a document-scoped work item");
        }
    Ok(())
}

pub(super) fn complete_outbox_in_transaction(
    conn: &Connection,
    kind: OutboxKind,
    id: i64,
    owner: &str,
) -> Result<bool> {
    let table = outbox_table(kind);
    Ok(conn.execute(
            &format!("UPDATE {table} SET state='done',owner=NULL,lease_expires_at=NULL,completed_at=?3 WHERE id=?1 AND state='processing' AND owner=?2"),
            params![id, owner, now_ms()],
        )? == 1)
}

pub(super) fn update_outbox_payload_in_transaction(
    conn: &Connection,
    kind: OutboxKind,
    id: i64,
    owner: &str,
    payload_json: &str,
) -> Result<bool> {
    require_json(payload_json)?;
    let table = outbox_table(kind);
    Ok(conn.execute(
        &format!(
            "UPDATE {table} SET payload_json=?3 WHERE id=?1 AND state='processing' AND owner=?2"
        ),
        params![id, owner, payload_json],
    )? == 1)
}

impl Database {
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
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = preparation::enqueue_work_item_in_transaction(
            &tx, run_id, unit_id, locale, kind, priority, input_json,
        )?;
        tx.commit()?;
        Ok(result)
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
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = enqueue_document_work_item_in_transaction(
            &tx,
            run_id,
            document_id,
            locale,
            kind,
            priority,
            input_json,
        )?;
        tx.commit()?;
        Ok(result)
    }

    pub fn supersede_materializations(
        &self,
        repository_id: i64,
        locale: &str,
        path: &str,
        active_dedupe_key: &str,
    ) -> Result<usize> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = supersede_materializations_in_transaction(
            &tx,
            repository_id,
            locale,
            path,
            active_dedupe_key,
        )?;
        tx.commit()?;
        Ok(result)
    }

    pub fn enqueue_materialization(
        &self,
        work_item_id: i64,
        dedupe_key: &str,
        payload_json: &str,
    ) -> Result<i64> {
        let now = now_ms();
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = enqueue_materialization_in_transaction(
            &tx,
            work_item_id,
            dedupe_key,
            payload_json,
            now,
        )?;
        tx.commit()?;
        Ok(result)
    }

    pub fn update_outbox_payload(
        &self,
        kind: OutboxKind,
        id: i64,
        owner: &str,
        payload_json: &str,
    ) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result =
            effects::update_outbox_payload_in_transaction(&tx, kind, id, owner, payload_json)?;
        tx.commit()?;
        Ok(result)
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
        repository_id: i64,
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
                "SELECT id,owner FROM publication_outbox WHERE locale=?1 AND repository_id=?2 AND state='processing'",
            )?;
            statement
                .query_map(params![locale, repository_id], |row| {
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
            "UPDATE publication_outbox SET state='pending',owner=NULL,lease_expires_at=NULL,last_error=COALESCE(last_error,'worker lease expired') WHERE locale=?1 AND repository_id=?3 AND state='processing' AND lease_expires_at<=?2",
            params![locale, now, repository_id],
        )?;
        let row: Option<(i64, String, String, i64)> = tx
            .query_row(
                "SELECT id,dedupe_key,payload_json,attempt_count FROM publication_outbox WHERE locale=?1 AND repository_id=?3 AND state='pending' AND available_at<=?2 ORDER BY available_at,id LIMIT 1",
                params![locale, now, repository_id],
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
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = effects::complete_outbox_in_transaction(&tx, kind, id, owner)?;
        tx.commit()?;
        Ok(result)
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
}

impl Database {
    pub fn pending_materializations(
        &self,
        repository_id: i64,
        locale: &str,
    ) -> Result<Vec<OutboxEntry>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare(
            "SELECT o.id,o.dedupe_key,o.payload_json,o.attempt_count FROM materialization_outbox o JOIN work_items w ON w.id=o.work_item_id JOIN runs r ON r.id=w.run_id WHERE r.repository_id=?1 AND w.locale=?2 AND o.state<>'done' ORDER BY o.id",
        )?;
        Ok(statement
            .query_map(params![repository_id, locale], |row| {
                Ok(OutboxEntry {
                    id: row.get(0)?,
                    dedupe_key: row.get(1)?,
                    payload_json: row.get(2)?,
                    attempt_count: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub fn cancel_materialization(&self, repository_id: i64, id: i64) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(i64,Option<String>)> = tx.query_row(
            "SELECT o.work_item_id,o.owner FROM materialization_outbox o JOIN work_items w ON w.id=o.work_item_id JOIN runs r ON r.id=w.run_id WHERE o.id=?1 AND r.repository_id=?2 AND o.state<>'done'",
            params![id,repository_id], |row| Ok((row.get(0)?,row.get(1)?)),
        ).optional()?;
        if let Some((work, owner)) = row {
            if owner
                .as_deref()
                .is_some_and(|owner| !dead_process_owner(owner))
            {
                bail!("incompatible materialization is still owned by a live worker");
            }
            tx.execute(
                "UPDATE work_items SET status='cancelled',updated_at=?2 WHERE id=?1",
                params![work, now_ms()],
            )?;
            tx.execute("UPDATE materialization_outbox SET state='done',owner=NULL,lease_expires_at=NULL,last_error='superseded: incompatible or unbound document intent',completed_at=?2 WHERE id=?1",params![id,now_ms()])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn effect_key(&self, kind: OutboxKind, base: &str) -> Result<String> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = effects::effect_key_in_transaction(&tx, kind, base)?;
        tx.commit()?;
        Ok(result)
    }
    pub fn materialization_work(&self, dedupe_key: &str) -> Result<Option<i64>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT work_item_id FROM materialization_outbox WHERE dedupe_key=?1 AND state<>'done'",
                [dedupe_key],
                |row| row.get(0),
            )
            .optional()?)
    }
    pub fn schedule_materialization(
        &self,
        input: MaterializationIntentInput<'_>,
    ) -> Result<MaterializationReceipt> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = effects::schedule_materialization_in_transaction(&tx, input)?;
        tx.commit()?;
        Ok(result)
    }
    pub fn finish_document_work(
        &self,
        work_item_id: i64,
        succeeded: bool,
        result_json: &str,
    ) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        effects::finish_document_work_in_transaction(&tx, work_item_id, succeeded, result_json)?;
        tx.commit()?;
        Ok(())
    }
}
