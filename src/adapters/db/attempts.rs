use super::*;

impl Database {
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
                input.status.as_str(),
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

    pub fn attempt_status(
        &self,
        work_item_id: i64,
        dedupe_key: &str,
    ) -> Result<Option<crate::application::contracts::AttemptStatus>> {
        let stored: Option<String> = self
            .connect()?
            .query_row(
                "SELECT status FROM attempts WHERE work_item_id=?1 AND dedupe_key=?2",
                params![work_item_id, dedupe_key],
                |row| row.get(0),
            )
            .optional()?;
        stored
            .as_deref()
            .map(crate::application::contracts::AttemptStatus::from_storage)
            .transpose()
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
            let conn = self.connect()?;
            let (request_json, current): (String, Option<String>) = conn.query_row(
                "SELECT a.request_json,json_extract(w.input_json,'$.document_identity') FROM attempts a JOIN work_items w ON w.id=a.work_item_id WHERE a.id=?1",
                [id], |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let request: serde_json::Value = serde_json::from_str(&request_json)?;
            let binding_matches = match current {
                Some(current) => request.get("_fani_document") == Some(&serde_json::from_str::<serde_json::Value>(&current)?),
                None => request.get("_fani_document").is_none(),
            };
            Ok(RecoveredAttempt {
                id,
                dedupe_key: dedupe_key.to_owned(),
                output: output.to_owned(),
                request_json,
                provenance: if binding_matches { attempt_provenance(&conn, id)? } else { None },
            })
        })
        .transpose()
    }

    pub fn record_attempt_candidate(
        &self,
        input: AttemptCandidateInput<'_>,
    ) -> Result<AttemptReceipt> {
        if !matches!(
            input.attempt.status,
            crate::application::contracts::AttemptStatus::Succeeded
        ) {
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
                    .is_some_and(|metadata| metadata.needs_markdown_snapshot_upgrade())
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
}
