use super::*;

impl Orchestrator<'_> {
    pub(super) fn reconcile_pull_request(&self, repository_id: i64, language: &str) -> Result<()> {
        if !self.repo.publish.github.enabled {
            return Ok(());
        }
        let branch = self.git.branch(self.repo, language)?;
        let Some(stored) =
            self.database
                .pull_request_for_branch(repository_id, "github", &branch)?
        else {
            return Ok(());
        };
        let Some(number) = stored.number else {
            return Ok(());
        };
        let pull = self.code_host.pull_request(
            self.repo,
            &self.repo.publish.github.repository,
            &number.to_string(),
        )?;
        let state = if pull.state.eq_ignore_ascii_case("merged") {
            "merged"
        } else if pull.state.eq_ignore_ascii_case("open") && pull.draft {
            "draft"
        } else if pull.state.eq_ignore_ascii_case("open") {
            "open"
        } else {
            "closed"
        };
        if state == "merged" {
            let expected = stored
                .head_revision
                .as_deref()
                .ok_or_else(|| anyhow!("stored pull request has no candidate head revision"))?;
            let observed = pull.head_revision.as_deref().ok_or_else(|| {
                anyhow!("GitHub did not return the merged pull request head revision")
            })?;
            if observed != expected {
                bail!(
                    "merged pull request head {observed} does not match published candidate {expected}"
                );
            }
        }
        self.database.record_pr_state(PullRequestStateInput {
            repository_id,
            provider: "github",
            external_id: &stored.external_id,
            number: stored.number,
            branch: &branch,
            url: Some(&pull.url),
            state,
            head_revision: stored.head_revision.as_deref(),
            event_key: &format!(
                "observe:{state}:{}",
                pull.head_revision.as_deref().unwrap_or("unknown")
            ),
            payload_json: &pull.payload_json,
        })?;
        if state == "merged" {
            let candidate_commit = stored
                .head_revision
                .as_deref()
                .ok_or_else(|| anyhow!("stored pull request has no candidate head revision"))?;
            self.promote_verified_publication(repository_id, language, candidate_commit)?;
        }
        Ok(())
    }

    fn promote_verified_publication(
        &self,
        repository_id: i64,
        language: &str,
        candidate_commit: &str,
    ) -> Result<()> {
        let snapshot =
            self.database
                .publication_snapshot(repository_id, language, candidate_commit)?;
        let files = snapshot
            .iter()
            .map(|file| PublicationFile {
                path: file.path.clone(),
                content: file.content.clone(),
            })
            .collect::<Vec<_>>();
        let mut compatible = !snapshot.is_empty();
        if let Some(first) = snapshot.first() {
            compatible &= snapshot
                .iter()
                .all(|file| file.source_revision == first.source_revision);
            compatible &=
                self.verify_publication_files(&first.source_revision, language, &files)?;
            if compatible {
                compatible &= self
                    .documentation
                    .check(self.repo, &first.source_revision, &files)?
                    .failures
                    .is_empty();
            }
        }
        let revision = self.git.resolve_source_revision(self.repo)?;
        let current_sources = self.git.discover(self.repo, &revision)?;
        let mut verified_zero_unit_contents = Vec::new();
        for file in &snapshot {
            let stored = self
                .database
                .canonical_document_intent(file.content_version_id)?
                .map(|identity| serde_json::from_str::<DocumentIdentity>(&identity))
                .transpose()?;
            let current = current_sources
                .iter()
                .find(|document| document.target_path(language).to_string_lossy() == file.path);
            compatible &= match (stored.as_ref(), current) {
                (Some(stored), Some(current)) => current.parse().is_ok_and(|parsed| {
                    if parsed.units.is_empty() && current.bytes == file.content {
                        verified_zero_unit_contents.push(file.content_version_id);
                    }
                    stored.source_revision == file.source_revision
                        && compatible_document_identity(
                            stored,
                            &document_identity(current, language, &parsed),
                        )
                }),
                _ => false,
            };
            compatible &= self
                .database
                .canonical_compatible(file.content_version_id)?;
        }
        if compatible {
            self.database.promote_merged_publication(
                repository_id,
                language,
                candidate_commit,
                "github_merged",
                &verified_zero_unit_contents,
            )?;
        } else if !snapshot.is_empty() {
            self.database.transition_publication_manifest(
                repository_id,
                language,
                candidate_commit,
                PublicationState::Superseded,
            )?;
        }
        Ok(())
    }

    fn verify_publication_files(
        &self,
        revision: &str,
        language: &str,
        files: &[PublicationFile],
    ) -> Result<bool> {
        let sources = self.git.discover(self.repo, revision)?;
        let mut seen = HashSet::new();
        for file in files {
            if !seen.insert(&file.path) {
                return Ok(false);
            }
            let mut source = None;
            for document in &sources {
                if document.target_path(language).to_string_lossy() == file.path {
                    source = Some(document);
                    break;
                }
            }
            let Some(source) = source else {
                return Ok(false);
            };
            let Ok(parsed) = source.parse() else {
                return Ok(false);
            };
            let Ok(text) = std::str::from_utf8(&file.content) else {
                return Ok(false);
            };
            if verify_document(&parsed, text).is_err() {
                return Ok(false);
            }
            let Ok(target) = source.parse_bytes(&file.content) else {
                return Ok(false);
            };
            if parsed.units.len() != target.units.len()
                || parsed
                    .units
                    .iter()
                    .zip(&target.units)
                    .any(|(source, target)| {
                        crate::domain::document::translated_unit_text(source, target).is_none()
                    })
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(super) fn publish(
        &self,
        repository_id: i64,
        run_id: &str,
        language: &str,
        written: &[String],
        outcome: &mut LanguageOutcome,
    ) -> Result<()> {
        if !self.repo.publish.enabled {
            outcome.published.skipped = "publication is disabled".into();
            return Ok(());
        }
        let owner = format!("publish:{}:{run_id}", self.owner_identity);
        let entry = if written.is_empty() {
            self.database.claim_publication_locale(
                repository_id,
                language,
                &owner,
                Utc::now().timestamp_millis(),
                180_000,
            )?
        } else {
            let mut files = Vec::with_capacity(written.len());
            for path in written {
                let canonical = self
                    .database
                    .canonical_file(repository_id, language, path)?
                    .ok_or_else(|| anyhow!("missing canonical publication content for {path}"))?;
                let content = String::from_utf8(canonical.content).with_context(|| {
                    format!("canonical publication content is not UTF-8: {path}")
                })?;
                files.push(PublicationRecord {
                    document_identity: self
                        .database
                        .canonical_document_intent(canonical.content_version_id)?
                        .map(|identity| serde_json::from_str(&identity))
                        .transpose()?,
                    canonical_content_version_id: canonical.content_version_id,
                    canonical_file_id: canonical.id,
                    content,
                    content_hash: canonical.content_hash,
                    path: path.clone(),
                });
            }
            let payload = serde_json::to_string(&PublicationPayload {
                commit: None,
                expected_remote_tip: None,
                files,
                language: language.to_owned(),
                policy_fingerprint: prompts::policy_fingerprint(),
                run_id: run_id.to_owned(),
                source_revision: outcome.source_revision.clone(),
            })?;
            let dedupe = format!(
                "publish:{repository_id}:{language}:{}",
                hash(&[payload.as_bytes()])
            );
            let dedupe = self.database.effect_key(OutboxKind::Publication, &dedupe)?;
            self.database.enqueue_publication(
                repository_id,
                Some(run_id),
                language,
                &dedupe,
                &payload,
            )?;
            self.database.claim_publication_locale(
                repository_id,
                language,
                &owner,
                Utc::now().timestamp_millis(),
                180_000,
            )?
        };
        let Some(entry) = entry else {
            if outcome.published.commit.is_empty() {
                outcome.published.skipped =
                    "no file changed and no publication recovery is pending".into();
            }
            return Ok(());
        };
        let outbox_started = Instant::now();
        tracing::info!(
            event = "outbox.claimed",
            kind = "publication",
            outbox_id = entry.id,
            run_id = %crate::diagnostics::safe_id(run_id),
            locale = language,
        );
        let mut payload: PublicationPayload = serde_json::from_str(&entry.payload_json)?;
        if payload.language != language {
            self.database.retry_outbox(
                OutboxKind::Publication,
                entry.id,
                &owner,
                "publication belongs to another locale",
                Utc::now().timestamp_millis(),
            )?;
            outcome.published.skipped = "another locale has pending publication recovery".into();
            return Ok(());
        }
        let payload_source_revision = payload.source_revision.clone();
        let records = payload.files.clone();
        let mut durable_files = Vec::with_capacity(records.len());
        let manifest_files = records
            .iter()
            .map(|record| PublicationManifestFile {
                canonical_content_version_id: record.canonical_content_version_id,
                canonical_file_id: record.canonical_file_id,
                content_hash: record.content_hash.clone(),
            })
            .collect::<Vec<_>>();
        for record in &records {
            if content_hash(record.content.as_bytes()) != record.content_hash {
                bail!(
                    "durable publication content hash changed for {}",
                    record.path
                );
            }
            durable_files.push(PublicationFile {
                path: record.path.clone(),
                content: record.content.as_bytes().to_vec(),
            });
        }
        let mut compatible = payload.policy_fingerprint == prompts::policy_fingerprint();
        let current_sources = self.git.discover(self.repo, &outcome.source_revision)?;
        for record in &records {
            let current = current_sources
                .iter()
                .find(|document| document.target_path(language).to_string_lossy() == record.path);
            compatible &= match (record.document_identity.as_ref(), current) {
                (Some(stored), Some(current)) => current.parse().is_ok_and(|parsed| {
                    stored.source_revision == payload.source_revision
                        && compatible_document_identity(
                            stored,
                            &document_identity(current, language, &parsed),
                        )
                }),
                _ => false,
            };
        }
        for ((record, binding), file) in records.iter().zip(&manifest_files).zip(&durable_files) {
            compatible &= self.database.canonical_content_matches(
                repository_id,
                language,
                &payload_source_revision,
                binding,
                file,
            )?;
            compatible &= self
                .database
                .canonical_compatible(record.canonical_content_version_id)?;
        }
        compatible &=
            self.verify_publication_files(&payload_source_revision, language, &durable_files)?;
        if compatible && (written.is_empty() || entry.attempt_count > 1 || payload.run_id != run_id)
        {
            compatible &= self
                .documentation
                .check(self.repo, &payload_source_revision, &durable_files)?
                .failures
                .is_empty();
        }
        if !compatible {
            if let Some(commit) = payload.commit.as_deref() {
                self.database.transition_publication_authorization(
                    repository_id,
                    language,
                    commit,
                    &entry.dedupe_key,
                    PublicationState::Superseded,
                )?;
            }
            let mut rejected = serde_json::to_value(&payload)?;
            rejected["superseded_reason"] =
                "document compatibility or current project checks failed".into();
            self.database.update_outbox_payload(
                OutboxKind::Publication,
                entry.id,
                &owner,
                &rejected.to_string(),
            )?;
            self.database
                .complete_outbox(OutboxKind::Publication, entry.id, &owner)?;
            outcome.published.skipped =
                "incompatible publication superseded; replan required".into();
            return Ok(());
        }
        if written.is_empty()
            && outcome.documents.markdown_files
                + outcome.documents.json_files
                + outcome.documents.mdx_files
                == 0
        {
            count_formats(&mut outcome.documents, &current_sources);
            outcome.documents.verified_documents = records.len();
            for source in &current_sources {
                match source.parse() {
                    Ok(parsed) if parsed.units.is_empty() => {
                        outcome.documents.pass_through_documents += 1
                    }
                    Err(error) => {
                        outcome.documents.parse_failures += 1;
                        outcome.conflicts.push(finding(
                            &source.path,
                            None,
                            if matches!(
                                error,
                                crate::domain::document::DocumentError::MessageUnsupported
                            ) {
                                "MESSAGE-UNSUPPORTED"
                            } else {
                                "DOCUMENT-PARSE"
                            },
                            "source document could not be parsed under the configured contract",
                        ));
                    }
                    _ => {}
                }
            }
        }
        outcome.published = if let Some(commit) = payload.commit.as_deref() {
            self.database.record_publication_authorization(
                PublicationManifestInput {
                    repository_id,
                    run_id: &payload.run_id,
                    locale: language,
                    source_revision: &payload_source_revision,
                    candidate_commit: commit,
                    policy_fingerprint: &payload.policy_fingerprint,
                    files: &manifest_files,
                },
                &entry.dedupe_key,
            )?;
            if self.repo.publish.push {
                self.database.transition_publication_authorization(
                    repository_id,
                    language,
                    commit,
                    &entry.dedupe_key,
                    PublicationState::PushPending,
                )?;
                let mut published = self.git.publish_pending(
                    self.repo,
                    language,
                    commit,
                    payload
                        .expected_remote_tip
                        .as_ref()
                        .and_then(|tip| tip.as_deref()),
                )?;
                published.paths = records.iter().map(|record| record.path.clone()).collect();
                published
            } else {
                crate::domain::model::Published {
                    branch: self.git.branch(self.repo, language)?,
                    commit: commit.to_owned(),
                    paths: records.iter().map(|record| record.path.clone()).collect(),
                    ..Default::default()
                }
            }
        } else {
            let prepared_publication = self.git.prepare(
                self.repo,
                language,
                &payload_source_revision,
                &durable_files,
            )?;
            let mut prepared = prepared_publication.published;
            let expected_remote_tip = prepared_publication.expected_remote_tip;
            payload.commit = Some(prepared.commit.clone());
            payload.expected_remote_tip = Some(expected_remote_tip.clone());
            let durable_payload = serde_json::to_string(&payload)?;
            if !self.database.update_outbox_payload(
                OutboxKind::Publication,
                entry.id,
                &owner,
                &durable_payload,
            )? {
                bail!("publication outbox ownership changed before commit persistence");
            }
            self.database.record_publication_authorization(
                PublicationManifestInput {
                    repository_id,
                    run_id: &payload.run_id,
                    locale: language,
                    source_revision: &payload_source_revision,
                    candidate_commit: &prepared.commit,
                    policy_fingerprint: &payload.policy_fingerprint,
                    files: &manifest_files,
                },
                &entry.dedupe_key,
            )?;
            (self.failpoint)("publication_candidate_persisted");
            if self.repo.publish.push {
                self.database.transition_publication_authorization(
                    repository_id,
                    language,
                    &prepared.commit,
                    &entry.dedupe_key,
                    PublicationState::PushPending,
                )?;
                let pushed = self.git.publish_pending(
                    self.repo,
                    language,
                    &prepared.commit,
                    expected_remote_tip.as_deref(),
                )?;
                prepared.pushed = pushed.pushed;
                prepared.error = pushed.error;
            }
            prepared
        };
        if !outcome.published.error.is_empty() {
            self.database.retry_outbox(
                OutboxKind::Publication,
                entry.id,
                &owner,
                &outcome.published.error,
                Utc::now().timestamp_millis() + 5_000,
            )?;
            bail!("{}", outcome.published.error);
        }
        (self.failpoint)("publication_side_effect_completed");
        let reconcile = (|| -> Result<()> {
            if self.repo.publish.github.enabled && outcome.published.pushed {
                let branch = self.git.branch(self.repo, language)?;
                let title = format!("i18n({language}): update translated documentation");
                let body = format!(
                    "Automated verified documentation translation from `{}`.",
                    payload_source_revision
                );
                let reconciled = self.code_host.ensure_pull_request(
                    self.repo,
                    EnsurePullRequest {
                        repository: &self.repo.publish.github.repository,
                        head: &branch,
                        base: &self.repo.publish.github.base,
                        title: &title,
                        body: &body,
                        draft: self.repo.publish.github.draft,
                    },
                )?;
                outcome.published.pr_number = Some(reconciled.pull_request.number);
                outcome.published.pr_url = Some(reconciled.pull_request.url.clone());
                let pr_state = if reconciled.pull_request.state.eq_ignore_ascii_case("merged") {
                    "merged"
                } else if reconciled.pull_request.state.eq_ignore_ascii_case("open")
                    && reconciled.pull_request.draft
                {
                    "draft"
                } else if reconciled.pull_request.state.eq_ignore_ascii_case("open") {
                    "open"
                } else {
                    "closed"
                };
                self.database.record_pr_state(PullRequestStateInput {
                    repository_id,
                    provider: "github",
                    external_id: &reconciled.pull_request.number.to_string(),
                    number: Some(reconciled.pull_request.number as i64),
                    branch: &branch,
                    url: Some(&reconciled.pull_request.url),
                    state: pr_state,
                    head_revision: Some(&outcome.published.commit),
                    event_key: &format!("ensure:{}", outcome.published.commit),
                    payload_json: &reconciled.payload_json,
                })?;
                match pr_state {
                    "merged" => {
                        if reconciled.pull_request.head_revision.as_deref()
                            != Some(outcome.published.commit.as_str())
                        {
                            bail!("merged pull request head does not match published candidate");
                        }
                        self.promote_verified_publication(
                            repository_id,
                            language,
                            &outcome.published.commit,
                        )?;
                    }
                    "open" | "draft" => {
                        self.database.transition_publication_authorization(
                            repository_id,
                            language,
                            &outcome.published.commit,
                            &entry.dedupe_key,
                            PublicationState::PrOpen,
                        )?;
                    }
                    _ => {
                        self.database.transition_publication_authorization(
                            repository_id,
                            language,
                            &outcome.published.commit,
                            &entry.dedupe_key,
                            PublicationState::Superseded,
                        )?;
                    }
                }
            }
            Ok(())
        })();
        if let Err(error) = reconcile {
            self.database.retry_outbox(
                OutboxKind::Publication,
                entry.id,
                &owner,
                &error.to_string(),
                Utc::now().timestamp_millis() + 5_000,
            )?;
            return Err(error);
        }
        if !self
            .database
            .complete_outbox(OutboxKind::Publication, entry.id, &owner)?
        {
            bail!("publication outbox lease was lost before completion");
        }
        tracing::info!(
            event = "outbox.completed",
            kind = "publication",
            outbox_id = entry.id,
            run_id = %crate::diagnostics::safe_id(run_id),
            locale = language,
            status = "completed",
            publication_id = %crate::diagnostics::safe_id(&outcome.published.commit),
            pushed = outcome.published.pushed,
            duration_ms = outbox_started.elapsed().as_millis() as u64,
        );
        self.publish(repository_id, run_id, language, &[], outcome)
    }
}
