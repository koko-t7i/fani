use super::*;

pub(super) fn record_publication_authorization_in_transaction(
    conn: &Connection,
    input: PublicationManifestInput<'_>,
    authorization_key: &str,
) -> Result<i64> {
    if authorization_key.is_empty() {
        bail!("publication authorization key cannot be empty");
    }
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
    let run: Option<(i64, String)> = conn
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
    let inserted = conn.execute(
        r#"INSERT OR IGNORE INTO publication_manifests(
                   repository_id,run_id,locale,source_revision,candidate_commit,
                   policy_fingerprint,state,created_at,authorization_key)
               VALUES (?1,?2,?3,?4,?5,?6,'commit_created',?7,?8)"#,
        params![
            repository_id,
            run_id,
            locale,
            source_revision,
            candidate_commit,
            policy_fingerprint,
            now,
            authorization_key
        ],
    )? == 1;
    let manifest: (i64, String, String, String, String) = conn.query_row(
        r#"SELECT id,run_id,source_revision,policy_fingerprint,candidate_commit
               FROM publication_manifests
               WHERE repository_id=?1 AND locale=?2 AND authorization_key=?3"#,
        params![repository_id, locale, authorization_key],
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
    if manifest.1 != run_id
        || manifest.2 != source_revision
        || manifest.3 != policy_fingerprint
        || manifest.4 != candidate_commit
    {
        bail!("publication candidate commit conflicts with its durable manifest");
    }
    for file in files {
        let valid = conn.query_row(
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
        let mut statement = conn.prepare(
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
            conn.execute(
                    "INSERT INTO publication_manifest_files(manifest_id,canonical_content_version_id,canonical_file_id,content_hash) VALUES (?1,?2,?3,?4)",
                    params![manifest.0, file.canonical_content_version_id, file.canonical_file_id, file.content_hash],
                )?;
        }
    } else if stored != requested {
        bail!("publication manifest file set conflicts with durable state");
    }
    let conflicting: bool = conn.query_row(
            r#"SELECT EXISTS(SELECT 1 FROM publication_manifests other
               WHERE other.repository_id=?1 AND other.locale=?2 AND other.candidate_commit=?3 AND other.id<>?4
                 AND (other.source_revision<>?5
                   OR EXISTS(SELECT canonical_content_version_id,canonical_file_id,content_hash FROM publication_manifest_files WHERE manifest_id=other.id
                             EXCEPT SELECT canonical_content_version_id,canonical_file_id,content_hash FROM publication_manifest_files WHERE manifest_id=?4)
                   OR EXISTS(SELECT canonical_content_version_id,canonical_file_id,content_hash FROM publication_manifest_files WHERE manifest_id=?4
                             EXCEPT SELECT canonical_content_version_id,canonical_file_id,content_hash FROM publication_manifest_files WHERE manifest_id=other.id)))"#,
            params![repository_id,locale,candidate_commit,manifest.0,source_revision], |row| row.get(0),
        )?;
    if conflicting {
        bail!("publication commit snapshot conflicts with another authorization");
    }
    refresh_publication_states(conn, manifest.0, now)?;

    Ok(manifest.0)
}

pub(super) fn transition_publication_authorization_in_transaction(
    conn: &Connection,
    repository_id: i64,
    locale: &str,
    candidate_commit: &str,
    authorization_key: Option<&str>,
    state: PublicationState,
) -> Result<()> {
    let state = state.as_str();
    if !matches!(
        state,
        "commit_created" | "push_pending" | "pr_open" | "superseded"
    ) {
        bail!("publication manifest cannot transition directly to {state}");
    }
    let manifest: Option<(i64, String)> = conn
            .query_row(
                "SELECT id,state FROM publication_manifests WHERE repository_id=?1 AND locale=?2 AND candidate_commit=?3 AND (?4 IS NULL OR authorization_key=?4) ORDER BY state='superseded',state='merged' DESC,id DESC LIMIT 1",
                params![repository_id, locale, candidate_commit, authorization_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
    let Some(manifest) = manifest else {
        if authorization_key.is_some() && state == "superseded" {
            return Ok(());
        }
        bail!("publication manifest for commit {candidate_commit} does not exist");
    };
    let effective = if manifest.1 == "merged"
        || manifest.1 == "superseded"
        || publication_rank(&manifest.1) >= publication_rank(state)
    {
        manifest.1.as_str()
    } else {
        state
    };
    let now = now_ms();
    conn.execute(
        "UPDATE publication_manifests SET state=?2 WHERE id=?1",
        params![manifest.0, effective],
    )?;
    refresh_publication_states(conn, manifest.0, now)?;

    Ok(())
}

pub(super) fn record_pr_state_in_transaction(
    conn: &Connection,
    input: PullRequestStateInput<'_>,
) -> Result<i64> {
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
    let previous: Option<String> = conn
            .query_row(
                "SELECT state FROM pull_requests WHERE repository_id=?1 AND provider=?2 AND external_id=?3",
                params![repository_id, provider, external_id],
                |row| row.get(0),
            )
            .optional()?;
    let now = now_ms();
    conn.execute(
            r#"INSERT INTO pull_requests(repository_id,provider,external_id,number,branch,url,state,head_revision,opened_at,updated_at,closed_at)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?9,CASE WHEN ?7 IN ('merged','closed') THEN ?9 ELSE NULL END)
               ON CONFLICT(repository_id,provider,external_id) DO UPDATE SET
                 number=excluded.number,branch=excluded.branch,url=excluded.url,state=excluded.state,
                 head_revision=excluded.head_revision,updated_at=excluded.updated_at,closed_at=excluded.closed_at"#,
            params![repository_id, provider, external_id, number, branch, url, state, head_revision, now],
        )?;
    let pr_id: i64 = conn.query_row(
        "SELECT id FROM pull_requests WHERE repository_id=?1 AND provider=?2 AND external_id=?3",
        params![repository_id, provider, external_id],
        |row| row.get(0),
    )?;
    conn.execute(
            "INSERT OR IGNORE INTO pr_events(pull_request_id,event_key,from_state,to_state,payload_json,occurred_at) VALUES (?1,?2,?3,?4,?5,?6)",
            params![pr_id, event_key, previous, state, payload_json, now],
        )?;

    Ok(pr_id)
}

pub(super) fn promote_publication_in_transaction(
    conn: &Connection,
    repository_id: i64,
    locale: &str,
    candidate_commit: &str,
    provenance: &str,
    verified_zero_unit_contents: &[i64],
) -> Result<usize> {
    let manifest_id: i64 = conn
            .query_row(
                "SELECT id FROM publication_manifests WHERE repository_id=?1 AND locale=?2 AND candidate_commit=?3 AND state<>'superseded' ORDER BY state='merged' DESC,id DESC LIMIT 1",
                params![repository_id, locale, candidate_commit],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| anyhow!("verified merged commit has no publication manifest"))?;
    let mut translations = {
        let mut statement = conn.prepare(
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
        let mut statement = conn.prepare("SELECT pmf.canonical_content_version_id,COUNT(cft.translation_version_id) FROM publication_manifest_files pmf LEFT JOIN canonical_file_translations cft ON cft.canonical_content_version_id=pmf.canonical_content_version_id WHERE pmf.manifest_id=?1 GROUP BY pmf.canonical_content_version_id ORDER BY pmf.canonical_content_version_id")?;
        let contents = statement
            .query_map([manifest_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut verified = verified_zero_unit_contents.to_vec();
        verified.sort_unstable();
        if contents.is_empty()
            || contents.iter().any(|(_, links)| *links != 0)
            || contents.iter().map(|(id, _)| *id).collect::<Vec<_>>() != verified
        {
            bail!(
                "publication manifest contains no exact translation versions or verified zero-unit documents"
            );
        }
    }
    for translation in &mut translations {
        let bound = version_provenance(conn, translation.0);
        if !bound
            .as_ref()
            .is_ok_and(|bound| validate_stored_translation(bound, &translation.5))
        {
            conn.execute(
                "UPDATE publication_manifests SET state='superseded' WHERE id=?1",
                [manifest_id],
            )?;
            refresh_publication_states(conn, manifest_id, now_ms())?;

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
        let provenance = serde_json::json!({"origin": provenance, "_fani_unit": version_provenance(conn, *version_id)?}).to_string();
        conn.execute("UPDATE translation_memory_entries SET tier='history',superseded_at=?7 WHERE repository_id=?1 AND locale=?2 AND source_hash=?3 AND context_key=?4 AND tier='trusted' AND superseded_at IS NULL AND (translation_version_id IS NOT ?5 OR target_text<>?6)", params![repository_id,locale,source_hash,context_key,version_id,target_text,now])?;
        let updated = conn.execute(
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
            conn.execute(
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
    conn.execute(
            "UPDATE translation_versions SET publication_state='merged',review_state='approved' WHERE superseded_at IS NULL AND validation_state='passed' AND id IN (SELECT cft.translation_version_id FROM publication_manifest_files pmf JOIN canonical_file_translations cft ON cft.canonical_content_version_id=pmf.canonical_content_version_id WHERE pmf.manifest_id=?1)",
            [manifest_id],
        )?;
    conn.execute(
            "UPDATE canonical_content_versions SET publication_state='merged' WHERE id IN (SELECT canonical_content_version_id FROM publication_manifest_files WHERE manifest_id=?1)",
            [manifest_id],
        )?;
    conn.execute(
            r#"UPDATE canonical_files SET state='merged',publication_state='merged',
                   review_state='approved',trust_tier='trusted',updated_at=?2
               WHERE current_content_version_id IN (
                   SELECT canonical_content_version_id FROM publication_manifest_files WHERE manifest_id=?1)"#,
            params![manifest_id, now],
        )?;
    conn.execute(
        "UPDATE publication_manifests SET state='merged',merged_at=?2 WHERE id=?1",
        params![manifest_id, now],
    )?;

    Ok(translations.len())
}

pub(super) fn enqueue_publication_in_transaction(
    conn: &Connection,
    repository_id: i64,
    run_id: Option<&str>,
    locale: &str,
    dedupe_key: &str,
    payload_json: &str,
) -> Result<i64> {
    require_json(payload_json)?;
    let now = now_ms();
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

impl Database {
    pub fn record_pr_state(&self, input: PullRequestStateInput<'_>) -> Result<i64> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = publication::record_pr_state_in_transaction(&tx, input)?;
        tx.commit()?;
        Ok(result)
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

    pub fn record_publication_manifest(&self, input: PublicationManifestInput<'_>) -> Result<i64> {
        let key = format!("run:{}:{}", input.run_id, input.candidate_commit);
        self.record_publication_authorization(input, &key)
    }

    pub fn record_publication_authorization(
        &self,
        input: PublicationManifestInput<'_>,
        authorization_key: &str,
    ) -> Result<i64> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = publication::record_publication_authorization_in_transaction(
            &tx,
            input,
            authorization_key,
        )?;
        tx.commit()?;
        Ok(result)
    }

    pub fn transition_publication_manifest(
        &self,
        repository_id: i64,
        locale: &str,
        candidate_commit: &str,
        state: PublicationState,
    ) -> Result<()> {
        self.transition_publication_authorization(
            repository_id,
            locale,
            candidate_commit,
            None,
            state,
        )
    }

    pub fn transition_publication_authorization(
        &self,
        repository_id: i64,
        locale: &str,
        candidate_commit: &str,
        authorization_key: Option<&str>,
        state: PublicationState,
    ) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        publication::transition_publication_authorization_in_transaction(
            &tx,
            repository_id,
            locale,
            candidate_commit,
            authorization_key,
            state,
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn publication_snapshot(
        &self,
        repository_id: i64,
        locale: &str,
        commit: &str,
    ) -> Result<Vec<crate::application::ports::CanonicalSnapshot>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare("SELECT cv.id,cv.source_revision,cf.path,cv.content FROM publication_manifests pm JOIN publication_manifest_files pmf ON pmf.manifest_id=pm.id JOIN canonical_content_versions cv ON cv.id=pmf.canonical_content_version_id JOIN canonical_files cf ON cf.id=cv.canonical_file_id WHERE pm.id=(SELECT id FROM publication_manifests WHERE repository_id=?1 AND locale=?2 AND candidate_commit=?3 AND state<>'superseded' ORDER BY state='merged' DESC,id DESC LIMIT 1) ORDER BY cf.path")?;
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
        self.promote_publication(repository_id, locale, candidate_commit, provenance, &[])
    }

    pub(super) fn promote_publication(
        &self,
        repository_id: i64,
        locale: &str,
        candidate_commit: &str,
        provenance: &str,
        verified_zero_unit_contents: &[i64],
    ) -> Result<usize> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = publication::promote_publication_in_transaction(
            &tx,
            repository_id,
            locale,
            candidate_commit,
            provenance,
            verified_zero_unit_contents,
        )?;
        tx.commit()?;
        Ok(result)
    }

    pub fn enqueue_publication(
        &self,
        repository_id: i64,
        run_id: Option<&str>,
        locale: &str,
        dedupe_key: &str,
        payload_json: &str,
    ) -> Result<i64> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = publication::enqueue_publication_in_transaction(
            &tx,
            repository_id,
            run_id,
            locale,
            dedupe_key,
            payload_json,
        )?;
        tx.commit()?;
        Ok(result)
    }
}
