use super::*;

pub(super) fn persist_canonical_document_inner_in_transaction(
    conn: &Connection,
    input: CanonicalFileInput<'_>,
    translations: &[CanonicalTranslationInput<'_>],
    identity_json: Option<&str>,
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
    conn.execute(
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
    let canonical_file_id: i64 = conn.query_row(
        "SELECT id FROM canonical_files WHERE repository_id=?1 AND locale=?2 AND path=?3",
        params![repository_id, locale, path],
        |row| row.get(0),
    )?;
    let conflicting_bytes: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM canonical_content_versions WHERE canonical_file_id=?1 AND content_hash=?2 AND content<>?3)",
            params![canonical_file_id, content_hash, content], |row| row.get(0),
        )?;
    if conflicting_bytes {
        bail!("canonical content hash conflicts with immutable durable content");
    }
    let existing_content: Option<(i64, String, Vec<u8>)> = conn
            .query_row(
                "SELECT id,source_revision,content FROM canonical_content_versions WHERE canonical_file_id=?1 AND content_hash=?2 AND source_revision=?3",
                params![canonical_file_id, content_hash, source_revision],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
    if let Some((_, stored_revision, stored_content)) = &existing_content {
        if stored_revision != source_revision || stored_content != content {
            bail!("canonical content hash conflicts with immutable durable content");
        }
    }
    conn.execute(
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
    let content_version_id: i64 = conn.query_row(
            "SELECT id FROM canonical_content_versions WHERE canonical_file_id=?1 AND content_hash=?2 AND source_revision=?3",
            params![canonical_file_id, content_hash, source_revision],
            |row| row.get(0),
        )?;
    conn.execute(
        "UPDATE canonical_files SET current_content_version_id=?2 WHERE id=?1",
        params![canonical_file_id, content_version_id],
    )?;
    crate::adapters::failpoint::reach("canonical_content_before_translation_links");
    let mut translation_version_ids = Vec::with_capacity(translations.len());
    for translation in translations {
        let unit_version: (i64, String, String) = conn
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
        let translation_version_id: Option<i64> = conn
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
            let tm_tier: Option<String> = conn
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
            conn.execute(
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
            conn.last_insert_rowid()
        };
        let bound = version_provenance(conn, translation_version_id)?;
        if !validate_stored_translation(&bound, translation.target_text) {
            bail!("canonical translation failed document compatibility validation");
        }
        translation_version_ids.push(translation_version_id);
    }
    if !translation_version_ids.is_empty() {
        translation_version_ids.sort_unstable();
        translation_version_ids.dedup();
        let stored = {
            let mut statement = conn.prepare(
                    "SELECT translation_version_id FROM canonical_file_translations WHERE canonical_content_version_id=?1 ORDER BY translation_version_id",
                )?;
            statement
                .query_map([content_version_id], |row| row.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if stored.is_empty() {
            for translation_version_id in &translation_version_ids {
                conn.execute(
                        "INSERT INTO canonical_file_translations(canonical_content_version_id,translation_version_id) VALUES (?1,?2)",
                        params![content_version_id, translation_version_id],
                    )?;
            }
        } else if stored != translation_version_ids {
            let mut statement = conn.prepare("SELECT cft.translation_version_id FROM canonical_file_translations cft JOIN translation_versions tv ON tv.id=cft.translation_version_id WHERE cft.canonical_content_version_id=?1 AND tv.superseded_at IS NULL ORDER BY cft.translation_version_id")?;
            let active = statement
                .query_map([content_version_id], |row| row.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if active.len() == stored.len()
                || active
                    .iter()
                    .any(|id| !translation_version_ids.contains(id))
            {
                bail!("canonical content translation set conflicts with immutable durable links");
            }
            // Revalidated identical bytes retain quarantined links as history.
            for id in &translation_version_ids {
                conn.execute("INSERT OR IGNORE INTO canonical_file_translations(canonical_content_version_id,translation_version_id) VALUES (?1,?2)", params![content_version_id,id])?;
            }
        }
    }
    let manifest_id: Option<i64> = conn.query_row(
            "SELECT manifest_id FROM publication_manifest_files WHERE canonical_content_version_id=?1 LIMIT 1",
            [content_version_id], |row| row.get(0),
        ).optional()?;
    if let Some(manifest_id) = manifest_id {
        refresh_publication_states(conn, manifest_id, now)?;
    }
    if let Some(identity_json) = identity_json {
        bind_document_intent_in_transaction(conn, content_version_id, identity_json)?;
    }

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

pub(super) fn transition_canonical_file_in_transaction(
    conn: &Connection,
    id: i64,
    transition: CanonicalTransition,
    materialized_hash: Option<&str>,
) -> Result<()> {
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

pub(super) fn trust_translation_in_transaction(
    conn: &Connection,
    input: TrustTranslationInput<'_>,
) -> Result<i64> {
    require_fingerprint(input.policy_fingerprint, "translation policy")?;
    let unit_id = input
        .unit_id
        .ok_or_else(|| anyhow!("trusted memory requires a source unit"))?;
    let (unit_version_id, mut bound): (i64, UnitProvenance) = conn.query_row(
            "SELECT uv.id,d.path,uv.source_text,uv.source_revision,uv.context_json FROM unit_versions uv JOIN units u ON u.id=uv.unit_id JOIN documents d ON d.id=u.document_id WHERE u.id=?1 AND d.repository_id=?2 AND uv.source_hash=?3 ORDER BY uv.id DESC LIMIT 1",
            params![unit_id,input.repository_id,input.source_hash], |row| Ok((row.get(0)?, UnitProvenance {
                document_path: row.get(1)?, source: row.get(2)?, source_revision: row.get(3)?, context_json: row.get(4)?, policy_fingerprint: input.policy_fingerprint.into(),
            })))?;
    let original = crate::domain::document::compatible_metadata(&bound)
        .ok_or_else(|| anyhow!("incompatible immutable unit context"))?;
    if original.needs_markdown_snapshot_upgrade() {
        let current: Option<String> = conn.query_row("SELECT u.context_json FROM units u JOIN documents d ON d.id=u.document_id WHERE u.id=?1 AND u.source_text=?2 AND d.source_revision=?3", params![unit_id,bound.source,bound.source_revision], |row| row.get(0)).optional()?;
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
    if let Some(id) = conn.query_row(
            "SELECT id FROM translation_memory_entries WHERE repository_id=?1 AND locale=?2 AND source_hash=?3 AND context_key=?4 AND target_text=?5 AND tier='trusted' AND superseded_at IS NULL",
            params![input.repository_id,input.locale,input.source_hash,key,input.target_text], |row| row.get(0)).optional()? {
            return Ok(id);
        }
    let now = now_ms();
    conn.execute("UPDATE translation_memory_entries SET tier='history',superseded_at=?5 WHERE repository_id=?1 AND locale=?2 AND source_hash=?3 AND context_key=?4 AND tier='trusted' AND superseded_at IS NULL",
            params![input.repository_id,input.locale,input.source_hash,key,now])?;
    conn.execute("INSERT INTO translation_versions(unit_version_id,locale,target_text,target_hash,freshness,provenance,validation_state,review_state,publication_state,policy_fingerprint,created_at) VALUES (?1,?2,?3,?4,'exact','trusted_tm','passed','approved','candidate',?5,?6)",
            params![unit_version_id,input.locale,input.target_text,migration_checksum(input.target_text),input.policy_fingerprint,now])?;
    let version_id = conn.last_insert_rowid();
    let audit = serde_json::json!({"origin": input.provenance, "_fani_unit": bound}).to_string();
    conn.execute("INSERT INTO translation_memory_entries(repository_id,unit_id,translation_version_id,locale,source_hash,source_revision,context_key,target_text,tier,provenance,policy_fingerprint,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'trusted',?9,?10,?11)",
            params![input.repository_id,unit_id,version_id,input.locale,input.source_hash,bound.source_revision,key,input.target_text,audit,input.policy_fingerprint,now])?;
    let id = conn.last_insert_rowid();

    Ok(id)
}

impl Database {
    pub fn trust_translation(&self, input: TrustTranslationInput<'_>) -> Result<i64> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = canonical::trust_translation_in_transaction(&tx, input)?;
        tx.commit()?;
        Ok(result)
    }

    pub fn persist_canonical_file(
        &self,
        input: CanonicalFileInput<'_>,
        translations: &[CanonicalTranslationInput<'_>],
    ) -> Result<CanonicalFile> {
        self.persist_canonical_document_inner(input, translations, None)
    }

    pub fn persist_canonical_document(
        &self,
        input: CanonicalFileInput<'_>,
        translations: &[CanonicalTranslationInput<'_>],
        identity_json: &str,
    ) -> Result<CanonicalFile> {
        require_json(identity_json)?;
        self.persist_canonical_document_inner(input, translations, Some(identity_json))
    }

    pub(super) fn persist_canonical_document_inner(
        &self,
        input: CanonicalFileInput<'_>,
        translations: &[CanonicalTranslationInput<'_>],
        identity_json: Option<&str>,
    ) -> Result<CanonicalFile> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = canonical::persist_canonical_document_inner_in_transaction(
            &tx,
            input,
            translations,
            identity_json,
        )?;
        tx.commit()?;
        Ok(result)
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

    pub fn quarantine_incompatible_translations(
        &self,
        document_id: i64,
        locale: &str,
    ) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        preparation::quarantine_incompatible_translations_in_transaction(&tx, document_id, locale)?;
        tx.commit()?;
        Ok(())
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
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        canonical::transition_canonical_file_in_transaction(
            &tx,
            id,
            transition,
            materialized_hash,
        )?;
        tx.commit()?;
        Ok(())
    }
}

impl Database {
    pub fn canonical_document_intent(&self, content_version_id: i64) -> Result<Option<String>> {
        Ok(self.connect()?.query_row(
            "SELECT i.identity_json FROM canonical_document_intent_selection s JOIN canonical_document_intents i ON i.id=s.intent_id AND i.canonical_content_version_id=s.canonical_content_version_id WHERE s.canonical_content_version_id=?1",
            [content_version_id], |row| row.get(0),
        ).optional()?)
    }
    pub fn bind_canonical_document_intent(
        &self,
        content_version_id: i64,
        identity_json: &str,
    ) -> Result<()> {
        require_json(identity_json)?;
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        bind_document_intent_in_transaction(&tx, content_version_id, identity_json)?;
        tx.commit()?;
        Ok(())
    }
    pub fn canonical_content_matches(
        &self,
        repository_id: i64,
        locale: &str,
        source_revision: &str,
        binding: &crate::application::ports::PublicationManifestFile,
        file: &crate::application::ports::PublicationFile,
    ) -> Result<bool> {
        if format!("{:x}", Sha256::digest(&file.content)) != binding.content_hash {
            return Ok(false);
        }
        Ok(self.connect()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM canonical_content_versions v JOIN canonical_files f ON f.id=v.canonical_file_id WHERE v.id=?1 AND v.canonical_file_id=?2 AND v.content_hash=?3 AND v.source_revision=?4 AND v.content=?5 AND f.repository_id=?6 AND f.locale=?7 AND f.path=?8)",
            params![binding.canonical_content_version_id,binding.canonical_file_id,binding.content_hash,source_revision,file.content,repository_id,locale,file.path],
            |row| row.get(0),
        )?)
    }
}
