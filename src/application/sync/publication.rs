use crate::application::contracts::{PublicationPayload, PublicationRecord};
use crate::application::ports::{
    CodeHost, DocumentationChecker, EnsurePullRequest, GitPublisher, OutboxKind,
    PublicationCandidateInput, PublicationFile, PublicationIntentInput, PublicationManifestFile,
    PublicationManifestInput, PublicationObservationInput, PublicationSettlementInput,
    PublicationWorkflowStore, PullRequestStateInput,
};
use crate::application::settings::RepoConfig;
use crate::application::sync::context::{
    compatible_document_identity, content_hash, count_formats, document_identity, finding, hash,
};
use crate::domain::document::verify_document;
use crate::domain::model::{LanguageOutcome, PublicationState};
use crate::domain::prompts;
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use std::collections::HashSet;
use std::time::Instant;

mod codec;
mod lifecycle;
use lifecycle::{MergeAssessment, Preparation, PullRequestLifecycle, preparation};

pub(crate) struct PublicationService<'a> {
    pub repo: &'a RepoConfig,
    pub store: &'a dyn PublicationWorkflowStore,
    pub git: &'a dyn GitPublisher,
    pub code_host: &'a dyn CodeHost,
    pub documentation: &'a dyn DocumentationChecker,
    pub owner_identity: &'a str,
    pub failpoint: fn(&str),
}

impl PublicationService<'_> {
    pub(super) fn reconcile_pull_request(&self, repository_id: i64, language: &str) -> Result<()> {
        if !self.repo.publish.github.enabled {
            return Ok(());
        }
        let branch = self.git.branch(self.repo, language)?;
        let Some(stored) = self
            .store
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
        let state = PullRequestLifecycle::observed(&pull.state, pull.draft);
        if state == PullRequestLifecycle::Merged {
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
        let assessment = if state == PullRequestLifecycle::Merged {
            self.assess_merged_publication(
                repository_id,
                language,
                stored
                    .head_revision
                    .as_deref()
                    .expect("merged head checked"),
            )?
        } else {
            MergeAssessment::Unchanged
        };
        self.store
            .observe_publication(PublicationObservationInput {
                pull_request: PullRequestStateInput {
                    repository_id,
                    provider: "github",
                    external_id: &stored.external_id,
                    number: stored.number,
                    branch: &branch,
                    url: Some(&pull.url),
                    state: state.as_str(),
                    head_revision: stored.head_revision.as_deref(),
                    event_key: &format!(
                        "observe:{}:{}",
                        state.as_str(),
                        pull.head_revision.as_deref().unwrap_or("unknown")
                    ),
                    payload_json: &pull.payload_json,
                },
                locale: language,
                promotion: assessment.promotion(stored.head_revision.as_deref().unwrap_or("")),
                state: assessment.state(),
            })?;
        Ok(())
    }

    fn assess_merged_publication(
        &self,
        repository_id: i64,
        language: &str,
        candidate_commit: &str,
    ) -> Result<MergeAssessment> {
        let snapshot =
            self.store
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
                .store
                .canonical_document_intent(file.content_version_id)?
                .map(|identity| codec::decode_identity(&identity))
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
            compatible &= self.store.canonical_compatible(file.content_version_id)?;
        }
        Ok(if compatible {
            MergeAssessment::Verified(verified_zero_unit_contents)
        } else if !snapshot.is_empty() {
            MergeAssessment::Superseded
        } else {
            MergeAssessment::Unchanged
        })
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
        if self.publish_next(repository_id, run_id, language, written, outcome)? {
            while self.publish_next(repository_id, run_id, language, &[], outcome)? {}
        }
        Ok(())
    }

    fn publish_next(
        &self,
        repository_id: i64,
        run_id: &str,
        language: &str,
        written: &[String],
        outcome: &mut LanguageOutcome,
    ) -> Result<bool> {
        if !self.repo.publish.enabled {
            outcome.published.skipped = "publication is disabled".into();
            return Ok(false);
        }
        let owner = format!("publish:{}:{run_id}", self.owner_identity);
        let entry = if written.is_empty() {
            self.store.claim_publication_locale(
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
                    .store
                    .canonical_file(repository_id, language, path)?
                    .ok_or_else(|| anyhow!("missing canonical publication content for {path}"))?;
                let content = String::from_utf8(canonical.content).with_context(|| {
                    format!("canonical publication content is not UTF-8: {path}")
                })?;
                files.push(PublicationRecord {
                    document_identity: self
                        .store
                        .canonical_document_intent(canonical.content_version_id)?
                        .map(|identity| codec::decode_identity(&identity))
                        .transpose()?,
                    canonical_content_version_id: canonical.content_version_id,
                    canonical_file_id: canonical.id,
                    content,
                    content_hash: canonical.content_hash,
                    path: path.clone(),
                });
            }
            let payload = PublicationPayload {
                commit: None,
                expected_remote_tip: None,
                files,
                language: language.to_owned(),
                policy_fingerprint: prompts::policy_fingerprint(),
                run_id: run_id.to_owned(),
                source_revision: outcome.source_revision.clone(),
                superseded_reason: None,
            };
            let dedupe = format!(
                "publish:{repository_id}:{language}:{}",
                hash(&[codec::encode(&payload)?.as_bytes()])
            );
            self.store.schedule_publication(PublicationIntentInput {
                repository_id,
                run_id,
                locale: language,
                base_dedupe_key: &dedupe,
                payload: &payload,
            })?;
            self.store.claim_publication_locale(
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
            return Ok(false);
        };
        let outbox_started = Instant::now();
        tracing::info!(
            event = "outbox.claimed",
            kind = "publication",
            outbox_id = entry.id,
            run_id = %crate::diagnostics::safe_id(run_id),
            locale = language,
        );
        let mut payload: PublicationPayload = codec::decode(&entry.payload_json)?;
        if payload.language != language {
            self.store.retry_outbox(
                OutboxKind::Publication,
                entry.id,
                &owner,
                "publication belongs to another locale",
                Utc::now().timestamp_millis(),
            )?;
            outcome.published.skipped = "another locale has pending publication recovery".into();
            return Ok(false);
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
        let mut compatible = payload.superseded_reason.is_none()
            && payload.policy_fingerprint == prompts::policy_fingerprint();
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
            compatible &= self.store.canonical_content_matches(
                repository_id,
                language,
                &payload_source_revision,
                binding,
                file,
            )?;
            compatible &= self
                .store
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
            let rejected = codec::rejected(&payload);
            self.store.settle_publication(PublicationSettlementInput {
                repository_id,
                locale: language,
                candidate_commit: payload.commit.as_deref(),
                authorization_key: &entry.dedupe_key,
                outbox_id: entry.id,
                owner: &owner,
                payload: Some(&rejected),
                state: Some(PublicationState::Superseded),
                pull_request: None,
                promotion: None,
            })?;
            outcome.published.skipped =
                "incompatible publication superseded; replan required".into();
            return Ok(false);
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
        outcome.published = match preparation(&payload)? {
            Preparation::Prepared {
                commit,
                expected_remote_tip,
            } => {
                self.store
                    .persist_publication_candidate(PublicationCandidateInput {
                        manifest: PublicationManifestInput {
                            repository_id,
                            run_id: &payload.run_id,
                            locale: language,
                            source_revision: &payload_source_revision,
                            candidate_commit: commit,
                            policy_fingerprint: &payload.policy_fingerprint,
                            files: &manifest_files,
                        },
                        authorization_key: &entry.dedupe_key,
                        outbox_id: entry.id,
                        owner: &owner,
                        payload: &payload,
                        state: if self.repo.publish.push {
                            PublicationState::PushPending
                        } else {
                            PublicationState::CommitCreated
                        },
                    })?;
                if self.repo.publish.push {
                    let mut published = self.git.publish_pending(
                        self.repo,
                        language,
                        commit,
                        expected_remote_tip,
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
            }
            Preparation::Unprepared => {
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
                self.store
                    .persist_publication_candidate(PublicationCandidateInput {
                        manifest: PublicationManifestInput {
                            repository_id,
                            run_id: &payload.run_id,
                            locale: language,
                            source_revision: &payload_source_revision,
                            candidate_commit: &prepared.commit,
                            policy_fingerprint: &payload.policy_fingerprint,
                            files: &manifest_files,
                        },
                        authorization_key: &entry.dedupe_key,
                        outbox_id: entry.id,
                        owner: &owner,
                        payload: &payload,
                        state: if self.repo.publish.push {
                            PublicationState::PushPending
                        } else {
                            PublicationState::CommitCreated
                        },
                    })?;
                (self.failpoint)("publication_candidate_persisted");
                if self.repo.publish.push {
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
            }
        };
        if !outcome.published.error.is_empty() {
            self.store.retry_outbox(
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
                let pr_state = PullRequestLifecycle::observed(
                    &reconciled.pull_request.state,
                    reconciled.pull_request.draft,
                );
                let assessment = if pr_state == PullRequestLifecycle::Merged {
                    if reconciled.pull_request.head_revision.as_deref()
                        != Some(outcome.published.commit.as_str())
                    {
                        bail!("merged pull request head does not match published candidate");
                    }
                    self.assess_merged_publication(
                        repository_id,
                        language,
                        &outcome.published.commit,
                    )?
                } else {
                    MergeAssessment::Unchanged
                };
                self.store.settle_publication(PublicationSettlementInput {
                    repository_id,
                    locale: language,
                    candidate_commit: Some(&outcome.published.commit),
                    authorization_key: &entry.dedupe_key,
                    outbox_id: entry.id,
                    owner: &owner,
                    payload: None,
                    state: match pr_state {
                        PullRequestLifecycle::Merged => assessment.state(),
                        PullRequestLifecycle::Open | PullRequestLifecycle::Draft => {
                            Some(PublicationState::PrOpen)
                        }
                        _ => Some(PublicationState::Superseded),
                    },
                    pull_request: Some(PullRequestStateInput {
                        repository_id,
                        provider: "github",
                        external_id: &reconciled.pull_request.number.to_string(),
                        number: Some(reconciled.pull_request.number as i64),
                        branch: &branch,
                        url: Some(&reconciled.pull_request.url),
                        state: pr_state.as_str(),
                        head_revision: Some(&outcome.published.commit),
                        event_key: &format!("ensure:{}", outcome.published.commit),
                        payload_json: &reconciled.payload_json,
                    }),
                    promotion: assessment.promotion(&outcome.published.commit),
                })?;
            } else {
                self.store.settle_publication(PublicationSettlementInput {
                    repository_id,
                    locale: language,
                    candidate_commit: Some(&outcome.published.commit),
                    authorization_key: &entry.dedupe_key,
                    outbox_id: entry.id,
                    owner: &owner,
                    payload: None,
                    state: None,
                    pull_request: None,
                    promotion: None,
                })?;
            }
            Ok(())
        })();
        if let Err(error) = reconcile {
            self.store.retry_outbox(
                OutboxKind::Publication,
                entry.id,
                &owner,
                &error.to_string(),
                Utc::now().timestamp_millis() + 5_000,
            )?;
            return Err(error);
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
        Ok(true)
    }
}
