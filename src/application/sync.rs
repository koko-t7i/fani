use crate::application::command::OutputReporter;
use crate::application::ports::{
    AgentExecutor, AttemptCandidateInput, AttemptInput, CanonicalFileInput,
    CanonicalTranslationInput, CodeHost, DocumentationChecker, EnsurePullRequest, FindingInput,
    GitPublisher, Materialization, MaterializationResult, Materializer, OutboxKind,
    PublicationFile, PublicationManifestFile, PublicationManifestInput, PullRequestStateInput,
    StateStore, TrustTranslationInput,
};
use crate::application::settings::RepoConfig;
use crate::domain::document::{
    DocumentFormat, ParsedDocument, TranslatableUnit, UnitTranslation, assemble_document,
    repair_leading_strong_separator, unit_metadata, validate_provenance, validate_unit,
    verify_document,
};
use crate::domain::matching::{MatchKind, PreviousUnit, match_units_with_stable_ids};
use crate::domain::model::{
    AgentResult, AgentStage, AgentTask, CanonicalTransition, DecisionCode, DocumentStatistics,
    Finding, FindingSeverity, Freshness, LanguageOutcome, MemoryTier, PlanSummary,
    PublicationState, ReviewState, Status, TranslationProvenance, ValidationState,
};
use crate::domain::prompts;
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

const REPAIR_CONTEXT_VERSION: &str = "v6";
const LEADING_STRONG_SEPARATOR_VERSION: &str = "fani-leading-strong-separator-v1";

fn hash(parts: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    format!("{:x}", digest.finalize())
}

fn content_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn reusable_candidate(
    path: &str,
    unit: &TranslatableUnit,
    candidate: &crate::application::ports::TranslationCandidate,
    allow_candidate: bool,
) -> bool {
    !candidate.text.is_empty()
        && (candidate.trusted
            || (allow_candidate
                && candidate.provenance.policy_fingerprint == prompts::policy_fingerprint()
                && candidate
                    .deterministic_model
                    .as_deref()
                    .is_none_or(|model| model == LEADING_STRONG_SEPARATOR_VERSION)))
        && validate_provenance(path, unit, &candidate.provenance, &candidate.text).is_ok()
}

#[derive(Deserialize)]
struct StoredReviewRequest {
    schema: String,
    task: AgentTask,
}

fn compatible_review_request(
    receipt: &crate::application::ports::RecoveredAttempt,
    stage: &str,
    path: &str,
    unit: &TranslatableUnit,
    translated: &str,
) -> bool {
    let Ok(request) = serde_json::from_str::<StoredReviewRequest>(&receipt.request_json) else {
        return false;
    };
    let task = request.task;
    request.schema == crate::application::ports::AGENT_REQUEST_SCHEMA
        && task.source_format == unit.context.format
        && task.unit_context == unit.context
        && task.context_key == unit.memory_context_key(path)
        && task.message_syntax == crate::domain::document::message_syntax(unit)
        && task.token_permissions == crate::domain::model::TokenPermissions::for_unit(unit)
        && task.stage.as_str() == stage
        && task.source == unit.protected_source
        && (task.previous_translation.as_deref() == Some(translated)
            || (stage == "revision"
                && !receipt.output.trim().eq_ignore_ascii_case("OK")
                && receipt.output == translated))
}

fn stable_unit_id(path: &str, unit: &TranslatableUnit, ordinal: usize) -> String {
    if unit.context.format == DocumentFormat::Json {
        return format!(
            "unit-{}",
            &hash(&[b"json-pointer-v1", path.as_bytes(), unit.id.as_bytes()])[..24]
        );
    }
    format!(
        "unit-{}",
        &hash(&[
            path.as_bytes(),
            unit.id.as_bytes(),
            ordinal.to_string().as_bytes(),
        ])[..24]
    )
}

fn stable_unit_hints(
    database: &dyn StateStore,
    repository_id: i64,
    path: &str,
    content_hash: &str,
    stable_ids: &[String],
) -> Result<Vec<String>> {
    let current = database
        .unchanged_document_unit_keys(repository_id, path, content_hash)?
        .into_iter()
        .collect::<HashSet<_>>();
    Ok(stable_ids
        .iter()
        .map(|stable_id| {
            current
                .contains(stable_id)
                .then(|| stable_id.clone())
                .unwrap_or_default()
        })
        .collect())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct DocumentIdentity {
    source_path: String,
    source_revision: String,
    source_hash: String,
    source_set_id: String,
    mapping_identity: String,
    locale: String,
    target_path: String,
    contract: crate::domain::document::FormatContract,
    request_schema: String,
    policy_fingerprint: String,
    request_identity: String,
}

impl DocumentIdentity {
    fn request_hash(&self) -> String {
        let mut request = self.clone();
        request.request_identity.clear();
        content_hash(&serde_json::to_vec(&request).expect("document identity serialization"))
    }

    fn mapping_hash(&self) -> String {
        let mut mapping = self.clone();
        mapping.source_revision.clear();
        mapping.request_hash()
    }
}

fn document_identity(
    document: &crate::domain::model::SourceDocument,
    language: &str,
    parsed: &ParsedDocument,
) -> DocumentIdentity {
    let mut identity = DocumentIdentity {
        source_path: document.path.clone(),
        source_revision: document.source_revision.clone(),
        source_hash: document.content_hash.clone(),
        source_set_id: document.source_set_id.clone(),
        mapping_identity: document.mapping_identity.clone(),
        locale: language.into(),
        target_path: document
            .target_path(language)
            .to_string_lossy()
            .into_owned(),
        contract: parsed.contract.clone(),
        request_schema: crate::application::ports::AGENT_REQUEST_SCHEMA.into(),
        policy_fingerprint: prompts::policy_fingerprint(),
        request_identity: String::new(),
    };
    identity.request_identity = identity.request_hash();
    identity
}

fn compatible_document_identity(stored: &DocumentIdentity, current: &DocumentIdentity) -> bool {
    stored.request_identity == stored.request_hash()
        && stored.mapping_hash() == current.mapping_hash()
}

fn count_formats(
    statistics: &mut DocumentStatistics,
    documents: &[crate::domain::model::SourceDocument],
) {
    statistics.markdown_files = documents
        .iter()
        .filter(|d| d.source_format == DocumentFormat::Markdown)
        .count();
    statistics.json_files = documents
        .iter()
        .filter(|d| d.source_format == DocumentFormat::Json)
        .count();
    statistics.mdx_files = documents
        .iter()
        .filter(|d| d.source_format == DocumentFormat::Mdx)
        .count();
}

fn finding(path: &str, unit_id: Option<String>, code: &str, message: impl Into<String>) -> Finding {
    Finding {
        severity: FindingSeverity::Error,
        code: code.into(),
        path: path.into(),
        unit_id,
        message: message.into(),
    }
}

#[derive(Deserialize)]
struct UnitContext {
    kind: crate::domain::document::UnitKind,
    context: Option<crate::domain::document::UnitContext>,
    contract: Option<crate::domain::document::FormatContract>,
}

fn previous_units(
    rows: &[crate::application::ports::UnitHistory],
    candidates: &[crate::application::ports::TranslationCandidate],
    path: &str,
) -> Vec<PreviousUnit> {
    let mut by_unit = HashMap::<_, Vec<_>>::new();
    for candidate in candidates {
        by_unit
            .entry(candidate.unit_id)
            .or_default()
            .push(candidate);
    }
    rows.iter()
        .filter_map(|row| {
            let stored = serde_json::from_str::<UnitContext>(&row.context_json).ok()?;
            let context = stored
                .context
                .unwrap_or_else(crate::domain::document::UnitContext::markdown);
            let compatible = stored.contract.as_ref().is_none_or(|contract| {
                crate::domain::document::context_contract(&context)
                    .as_ref()
                    .ok()
                    == Some(contract)
            });
            let kind = stored.kind;
            let mut translation = row.translation.clone();
            if translation.is_none() && context.format == DocumentFormat::Json {
                if let Some(candidates) = by_unit.get(&row.id) {
                    translation = candidates.iter().find_map(|candidate| {
                        let bound = crate::domain::document::UnitProvenance {
                            document_path: path.into(),
                            source: row.source_text.clone(),
                            source_revision: candidate.provenance.source_revision.clone(),
                            context_json: row.context_json.clone(),
                            policy_fingerprint: candidate.provenance.policy_fingerprint.clone(),
                        };
                        let previous = crate::domain::document::stored_unit(&bound)?;
                        validate_provenance(path, &previous, &candidate.provenance, &candidate.text)
                            .is_ok()
                            .then(|| candidate.text.clone())
                    });
                }
            }
            Some(PreviousUnit {
                stable_id: row.unit_key.clone(),
                kind,
                context,
                ordinal: row.ordinal,
                source: row.source_text.clone(),
                translation: translation.unwrap_or_default(),
                trusted: row.trusted && compatible,
            })
        })
        .collect()
}

#[derive(Clone)]
struct PlannedUnit {
    unit: TranslatableUnit,
    database_id: i64,
    stable_id: String,
    translation: Option<String>,
    previous_source: Option<String>,
    previous_translation: Option<String>,
    work_item_id: Option<i64>,
}

struct PlannedDocument {
    identity: DocumentIdentity,
    document_id: i64,
    source_path: String,
    target_path: String,
    parsed: ParsedDocument,
    units: Vec<PlannedUnit>,
    canonical_id: Option<i64>,
    expected_materialized_hash: Option<String>,
}

#[derive(Deserialize)]
struct MaterializationPayload {
    repository_id: i64,
    locale: String,
    path: String,
    document_identity: DocumentIdentity,
    content_hash: String,
    canonical_content_version_id: i64,
    canonical_file_id: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PublicationRecord {
    #[serde(default)]
    document_identity: Option<DocumentIdentity>,
    canonical_content_version_id: i64,
    canonical_file_id: i64,
    content: String,
    content_hash: String,
    path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PublicationPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expected_remote_tip: Option<Option<String>>,
    files: Vec<PublicationRecord>,
    language: String,
    policy_fingerprint: String,
    run_id: String,
    source_revision: String,
}

pub struct Orchestrator<'a> {
    repo: &'a RepoConfig,
    database: &'a dyn StateStore,
    materializer: &'a dyn Materializer,
    agents: &'a dyn AgentExecutor,
    documentation: &'a dyn DocumentationChecker,
    git: &'a dyn GitPublisher,
    code_host: &'a dyn CodeHost,
    config_path: &'a Path,
    owner_identity: &'a str,
    failpoint: fn(&str),
    output: &'a dyn OutputReporter,
    quiet: bool,
}

impl<'a> Orchestrator<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo: &'a RepoConfig,
        database: &'a dyn StateStore,
        materializer: &'a dyn Materializer,
        agents: &'a dyn AgentExecutor,
        documentation: &'a dyn DocumentationChecker,
        git: &'a dyn GitPublisher,
        code_host: &'a dyn CodeHost,
        config_path: &'a Path,
        owner_identity: &'a str,
        failpoint: fn(&str),
        output: &'a dyn OutputReporter,
        quiet: bool,
    ) -> Self {
        Self {
            repo,
            database,
            materializer,
            agents,
            documentation,
            git,
            code_host,
            config_path,
            owner_identity,
            failpoint,
            output,
            quiet,
        }
    }

    fn log(&self, message: &str) {
        if !self.quiet {
            self.output.stderr(message);
        }
    }

    fn traced_stage<T>(
        &self,
        language: &str,
        run_id: &str,
        stage: &str,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let started = Instant::now();
        let repository_id =
            crate::diagnostics::safe_id(self.repo.path.to_string_lossy().as_bytes());
        let run_id = crate::diagnostics::safe_id(run_id);
        tracing::info!(
            event = "stage.started",
            repository_id,
            run_id,
            locale = language,
            stage,
        );
        let result = operation();
        tracing::info!(
            event = "stage.completed",
            repository_id,
            run_id,
            locale = language,
            stage,
            status = if result.is_ok() {
                "succeeded"
            } else {
                "failed"
            },
            duration_ms = started.elapsed().as_millis() as u64,
        );
        result
    }

    fn repository_id(&self) -> Result<i64> {
        let key = self
            .repo
            .path
            .canonicalize()
            .unwrap_or_else(|_| self.repo.path.clone());
        self.database.upsert_repository(
            &hash(&[key.to_string_lossy().as_bytes()]),
            &self.repo.path,
            Some(&self.repo.publish.github.base),
            None,
        )
    }

    fn invocation_key(&self, repository_id: i64, language: &str, revision: &str) -> Result<String> {
        let configuration = json!({
            "sources": self.repo.sources,
            "include": self.repo.include,
            "exclude": self.repo.exclude,
            "target_pattern": self.repo.target_pattern,
            "checks": self.repo.documentation.commands,
            "check_timeout": self.repo.documentation.timeout_s,
            "revision": self.repo.quality.revision,
            "proofread": self.repo.quality.proofread,
            "agent": self.agents.configuration_fingerprint()?,
        });
        Ok(format!(
            "sync:{repository_id}:{language}:{revision}:{}:{}",
            prompts::policy_fingerprint(),
            content_hash(configuration.to_string().as_bytes())
        ))
    }

    pub fn plan_language(&self, language: &str) -> Result<PlanSummary> {
        let source_revision = self.git.resolve_source_revision(self.repo)?;
        let repository_id = self.repository_id()?;
        let invocation = self.invocation_key(repository_id, language, &source_revision)?;
        let documents = self.git.discover(self.repo, &source_revision)?;
        let mut pending = 0;
        let mut reused = 0;
        let mut conflicts = 0;
        let mut document_statistics = DocumentStatistics::default();
        count_formats(&mut document_statistics, &documents);
        for document in &documents {
            let parsed = match document.parse() {
                Ok(parsed) => parsed,
                Err(_) => {
                    conflicts += 1;
                    document_statistics.parse_failures += 1;
                    continue;
                }
            };
            let units = &parsed.units;
            let mut translations = Vec::new();
            if units.is_empty() {
                document_statistics.pass_through_documents += 1;
            }
            let stable_ids = units
                .iter()
                .enumerate()
                .map(|(ordinal, unit)| stable_unit_id(&document.path, unit, ordinal))
                .collect::<Vec<_>>();
            let stable_hints = stable_unit_hints(
                self.database,
                repository_id,
                &document.path,
                &document.content_hash,
                &stable_ids,
            )?;
            let (history, candidates) =
                match self.database.document_id(repository_id, &document.path)? {
                    Some(document_id) => (
                        self.database.unit_history(document_id, language)?,
                        self.database
                            .translation_candidates(document_id, language)?,
                    ),
                    None => (Vec::new(), Vec::new()),
                };
            let matched = match_units_with_stable_ids(
                &previous_units(&history, &candidates, &document.path),
                units,
                &stable_hints,
            );
            for (ordinal, (unit, matched)) in units.iter().zip(matched).enumerate() {
                match matched.kind {
                    MatchKind::Ambiguous => conflicts += 1,
                    _ => {
                        let stable_id = matched.stable_id.as_deref().or_else(|| {
                            (!stable_hints[ordinal].is_empty())
                                .then_some(stable_hints[ordinal].as_str())
                        });
                        let database_id = stable_id.and_then(|stable_id| {
                            history
                                .iter()
                                .find(|row| row.unit_key == stable_id)
                                .map(|row| row.id)
                        });
                        let candidate = candidates.iter().find(|candidate| {
                            reusable_candidate(
                                &document.path,
                                unit,
                                candidate,
                                Some(candidate.unit_id) == database_id
                                    && (candidate.invocation_key.as_deref()
                                        == Some(invocation.as_str())
                                        || !stable_hints[ordinal].is_empty()),
                            )
                        });
                        let compatible = match candidate {
                            Some(candidate) => self.reviews_compatible(
                                &document.path,
                                unit,
                                candidate,
                                language,
                                &invocation,
                            )?,
                            None => false,
                        };
                        if compatible {
                            reused += 1;
                            translations.push(UnitTranslation {
                                id: unit.id.clone(),
                                text: candidate.expect("compatible candidate").text.clone(),
                            });
                        } else {
                            pending += 1;
                        }
                    }
                }
            }
            if translations.len() == units.len()
                && assemble_document(&parsed, &translations).is_ok()
            {
                document_statistics.verified_documents += 1;
            }
        }
        Ok(PlanSummary {
            document_statistics,
            repository: self.repo.path.clone(),
            language: language.into(),
            source_revision,
            documents: documents.len(),
            pending_units: pending.min(self.repo.max_tasks),
            reused_units: reused,
            conflicts,
            deferred_units: pending.saturating_sub(self.repo.max_tasks),
        })
    }

    fn prepare(
        &self,
        repository_id: i64,
        run_id: &str,
        language: &str,
        source_revision: &str,
        statistics: &mut DocumentStatistics,
    ) -> Result<(Vec<PlannedDocument>, Vec<Finding>, usize, usize)> {
        let mut planned_documents = Vec::new();
        let mut conflicts = Vec::new();
        let mut reused = 0;
        let mut scheduled = 0;
        let policy_fingerprint = prompts::policy_fingerprint();
        let documents = self.git.discover(self.repo, source_revision)?;
        count_formats(statistics, &documents);
        for document in documents {
            let parsed = match document.parse() {
                Ok(parsed) => parsed,
                Err(error) => {
                    statistics.parse_failures += 1;
                    conflicts.push(finding(
                        &document.path,
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
                    continue;
                }
            };
            let identity = document_identity(&document, language, &parsed);
            let document_units = &parsed.units;
            if document_units.is_empty() {
                statistics.pass_through_documents += 1;
            }
            let stable_ids = document_units
                .iter()
                .enumerate()
                .map(|(ordinal, unit)| stable_unit_id(&document.path, unit, ordinal))
                .collect::<Vec<_>>();
            let stable_hints = stable_unit_hints(
                self.database,
                repository_id,
                &document.path,
                &document.content_hash,
                &stable_ids,
            )?;
            let (history, candidates) =
                match self.database.document_id(repository_id, &document.path)? {
                    Some(document_id) => (
                        self.database.unit_history(document_id, language)?,
                        self.database
                            .translation_candidates(document_id, language)?,
                    ),
                    None => (Vec::new(), Vec::new()),
                };
            let matched = match_units_with_stable_ids(
                &previous_units(&history, &candidates, &document.path),
                document_units,
                &stable_hints,
            );
            if matched
                .iter()
                .any(|matched| matched.kind == MatchKind::Ambiguous)
            {
                conflicts.extend(
                    document_units
                        .iter()
                        .zip(&matched)
                        .filter(|(_, matched)| matched.kind == MatchKind::Ambiguous)
                        .map(|(unit, _)| {
                            finding(
                                &document.path,
                                Some(unit.id.clone()),
                                DecisionCode::AmbiguousMatch.as_str(),
                                "multiple previous units match this document unit",
                            )
                        }),
                );
                continue;
            }
            let document_id = self.database.upsert_document(
                repository_id,
                &document.path,
                Some(source_revision),
                &document.content_hash,
                &serde_json::to_string(
                    &json!({"format": parsed.format, "contract": parsed.contract}),
                )?,
            )?;
            self.database
                .quarantine_incompatible_translations(document_id, language)?;
            let mut units = Vec::new();
            for (ordinal, (unit, matched)) in
                document_units.iter().cloned().zip(matched).enumerate()
            {
                let stable_id = matched
                    .stable_id
                    .clone()
                    .unwrap_or_else(|| stable_ids[ordinal].clone());
                let source_hash = hash(&[unit.source.as_bytes()]);
                let context = unit.memory_context_key(&document.path);
                let database_id = self.database.upsert_unit(
                    document_id,
                    &stable_id,
                    ordinal as i64,
                    &unit.source,
                    &source_hash,
                    &unit_metadata(&unit, &document.path),
                )?;
                let reusable = candidates.iter().find(|candidate| {
                    reusable_candidate(
                        &document.path,
                        &unit,
                        candidate,
                        candidate.unit_id == database_id
                            && (candidate.run_id.as_deref() == Some(run_id)
                                || !stable_hints[ordinal].is_empty()),
                    )
                });
                let reusable = match reusable {
                    Some(candidate)
                        if self.reviews_compatible(
                            &document.path,
                            &unit,
                            candidate,
                            language,
                            run_id,
                        )? =>
                    {
                        Some(candidate)
                    }
                    _ => None,
                };
                if let Some(candidate) = reusable.filter(|candidate| !candidate.trusted) {
                    if crate::domain::document::compatible_metadata(&candidate.provenance)
                        .is_some_and(|metadata| metadata.needs_markdown_snapshot_upgrade())
                    {
                        self.database
                            .revalidate_candidate(database_id, language, candidate)?;
                    }
                }
                if let Some(candidate) = reusable.filter(|candidate| candidate.trusted) {
                    self.database.trust_translation(TrustTranslationInput {
                        repository_id,
                        unit_id: Some(database_id),
                        locale: language,
                        source_hash: &source_hash,
                        context_key: &context,
                        target_text: &candidate.text,
                        provenance: "compatible_memory",
                        policy_fingerprint: &policy_fingerprint,
                    })?;
                }
                let reusable = reusable.map(|candidate| candidate.text.clone());
                let mut planned = PlannedUnit {
                    unit,
                    database_id,
                    stable_id,
                    translation: reusable,
                    previous_source: matched.previous_source,
                    previous_translation: matched.previous_translation,
                    work_item_id: None,
                };
                if planned.translation.is_some() {
                    reused += 1;
                    if self.repo.quality.revision || self.repo.quality.proofread {
                        let input = serde_json::to_string(&json!({
                            "source_revision": source_revision,
                            "path": document.path,
                            "unit": planned.stable_id,
                            "document_identity": identity,
                        }))?;
                        planned.work_item_id = Some(self.database.enqueue_work_item(
                            run_id,
                            database_id,
                            language,
                            "pipeline",
                            0,
                            &input,
                        )?);
                    }
                } else if scheduled < self.repo.max_tasks {
                    let input = serde_json::to_string(&json!({
                        "source_revision": source_revision,
                        "path": document.path,
                        "unit": planned.stable_id,
                            "document_identity": identity,
                    }))?;
                    planned.work_item_id = Some(self.database.enqueue_work_item(
                        run_id,
                        database_id,
                        language,
                        "pipeline",
                        0,
                        &input,
                    )?);
                    scheduled += 1;
                }
                units.push(planned);
            }
            let target = document.target_path(language);
            let target_path = target.to_string_lossy().into_owned();
            let canonical = self
                .database
                .canonical_file(repository_id, language, &target_path)?;
            if let Some(canonical) = &canonical {
                let actual = self
                    .materializer
                    .read(&self.repo.path, &target)?
                    .map(|bytes| content_hash(&bytes));
                if actual.as_deref() != Some(canonical.content_hash.as_str())
                    && actual.as_deref() != canonical.materialized_hash.as_deref()
                {
                    conflicts.push(finding(
                        &target_path,
                        None,
                        DecisionCode::HumanEdit.as_str(),
                        "materialized target differs from the canonical SQLite content; use adopt or discard",
                    ));
                }
            }
            planned_documents.push(PlannedDocument {
                identity,
                document_id,
                source_path: document.path,
                target_path,
                parsed,
                units,
                canonical_id: canonical.as_ref().map(|value| value.id),
                expected_materialized_hash: canonical.and_then(|value| value.materialized_hash),
            });
        }
        Ok((planned_documents, conflicts, reused, scheduled))
    }

    fn reviews_compatible(
        &self,
        path: &str,
        unit: &TranslatableUnit,
        candidate: &crate::application::ports::TranslationCandidate,
        language: &str,
        run_or_invocation: &str,
    ) -> Result<bool> {
        if !self.repo.quality.revision && !self.repo.quality.proofread {
            return Ok(true);
        }
        for receipt in
            self.database
                .review_attempts(candidate.unit_id, language, run_or_invocation)?
        {
            let stage = receipt.dedupe_key.rsplit(':').next().unwrap_or("");
            if (stage == "revision" && !self.repo.quality.revision)
                || (stage == "proofread" && !self.repo.quality.proofread)
            {
                continue;
            }
            if !compatible_review_request(&receipt, stage, path, unit, &candidate.text)
                || !receipt.provenance.as_ref().is_some_and(|bound| {
                    bound.policy_fingerprint == prompts::policy_fingerprint()
                        && validate_provenance(path, unit, bound, &unit.protected_source).is_ok()
                })
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn recovered_attempt(
        &self,
        work_item_id: i64,
        key: &str,
        path: &str,
        unit: &TranslatableUnit,
        translation: bool,
        reviewed_translation: Option<&str>,
    ) -> Result<Option<crate::application::ports::RecoveredAttempt>> {
        let Some(receipt) = self.database.successful_attempt(work_item_id, key)? else {
            return Ok(None);
        };
        let approval =
            key.ends_with(":revision") && receipt.output.trim().eq_ignore_ascii_case("OK");
        let text = if translation && !approval {
            receipt.output.as_str()
        } else {
            unit.protected_source.as_str()
        };
        let review_matches = reviewed_translation.is_none_or(|text| {
            compatible_review_request(
                &receipt,
                key.rsplit(':').next().unwrap_or(""),
                path,
                unit,
                text,
            )
        });
        if review_matches
            && receipt.provenance.as_ref().is_some_and(|provenance| {
                provenance.policy_fingerprint == prompts::policy_fingerprint()
                    && validate_provenance(path, unit, provenance, text).is_ok()
            })
        {
            return Ok(Some(receipt));
        }
        self.database.retire_attempt(receipt.id)?;
        Ok(None)
    }

    fn dispatch(
        &self,
        run_id: &str,
        language: &str,
        documents: &mut [PlannedDocument],
    ) -> Result<Vec<AgentResult>> {
        let mut tasks = Vec::new();
        for document in documents.iter_mut() {
            for unit in &mut document.units {
                let Some(work_item_id) = unit.work_item_id else {
                    continue;
                };
                if unit.translation.is_some() {
                    continue;
                }
                let dedupe_key = format!("{}:translate", unit.stable_id);
                if let Some(recovered) = self.recovered_attempt(
                    work_item_id,
                    &dedupe_key,
                    &document.source_path,
                    &unit.unit,
                    true,
                    None,
                )? {
                    validate_unit(&unit.unit, &recovered.output).map_err(|findings| {
                        anyhow!(
                            "durable Agent output for {} failed validation: {findings:?}",
                            unit.stable_id
                        )
                    })?;
                    self.database.select_canonical_candidate(
                        unit.database_id,
                        language,
                        &format!("attempt:{}", recovered.id),
                        &recovered.output,
                        Some(recovered.id),
                        Some(1.0),
                    )?;
                    unit.translation = Some(recovered.output);
                    continue;
                }
                if self
                    .database
                    .attempt_status(work_item_id, &dedupe_key)?
                    .is_some()
                {
                    continue;
                }
                tasks.push(AgentTask {
                    source_format: unit.unit.context.format,
                    unit_context: unit.unit.context.clone(),
                    context_key: unit.unit.memory_context_key(&document.source_path),
                    message_syntax: crate::domain::document::message_syntax(&unit.unit),
                    token_permissions: crate::domain::model::TokenPermissions::for_unit(&unit.unit),
                    id: unit.stable_id.clone(),
                    stage: AgentStage::Translate,
                    source_language: "auto".into(),
                    target_language: language.into(),
                    source: unit.unit.protected_source.clone(),
                    previous_source: unit.previous_source.clone(),
                    previous_translation: unit.previous_translation.clone(),
                    findings: Vec::new(),
                    protected_tokens: unit
                        .unit
                        .protected
                        .iter()
                        .map(|span| span.token.clone())
                        .collect(),
                });
            }
        }
        if tasks.is_empty() {
            return Ok(Vec::new());
        }
        let execution = self.agents.execute(&tasks)?;
        let agent_name = execution.agent;
        let provider = execution.provider;
        let model = execution.model;
        let adapter = execution.adapter;
        let provider_fingerprint = execution.provider_fingerprint;
        let results = execution.results;
        self.log(&format!(
            "    dispatched {} native document unit(s) to {}",
            tasks.len(),
            agent_name
        ));
        let by_id: HashMap<_, _> = results
            .iter()
            .map(|result| (result.task_id.as_str(), result))
            .collect();
        for document in documents {
            for unit in &mut document.units {
                let Some(work_item_id) = unit.work_item_id else {
                    continue;
                };
                let Some(result) = by_id.get(unit.stable_id.as_str()) else {
                    continue;
                };
                let dedupe_key = format!("{}:translate", unit.stable_id);
                if result.ok {
                    match validate_unit(&unit.unit, &result.output) {
                        Ok(_) => {
                            let candidate_key = format!("attempt:{dedupe_key}");
                            self.database
                                .record_attempt_candidate(AttemptCandidateInput {
                                    attempt: AttemptInput {
                                        work_item_id,
                                        dedupe_key: &dedupe_key,
                                        agent: &agent_name,
                                        provider: &provider,
                                        model: &model,
                                        adapter: &adapter,
                                        provider_fingerprint: &provider_fingerprint,
                                        prompt_version: &result.prompt_version,
                                        prompt_hash: &result.prompt_hash,
                                        policy_fingerprint: &result.policy_fingerprint,
                                        status: "succeeded",
                                        request_json: result.request_json.as_str(),
                                        response_json: result.response_json.as_deref(),
                                        error: None,
                                    },
                                    unit_id: unit.database_id,
                                    locale: language,
                                    candidate_key: &candidate_key,
                                    target_text: &result.output,
                                    score: Some(1.0),
                                    policy_fingerprint: &prompts::policy_fingerprint(),
                                    provenance: TranslationProvenance::Ai,
                                })?;
                            (self.failpoint)("agent_candidate_committed");
                            unit.translation = Some(result.output.clone());
                        }
                        Err(findings) => {
                            let receipt = self.database.record_attempt(AttemptInput {
                                work_item_id,
                                dedupe_key: &dedupe_key,
                                agent: &agent_name,
                                provider: &provider,
                                model: &model,
                                adapter: &adapter,
                                provider_fingerprint: &provider_fingerprint,
                                prompt_version: &result.prompt_version,
                                prompt_hash: &result.prompt_hash,
                                policy_fingerprint: &result.policy_fingerprint,
                                status: "failed",
                                request_json: result.request_json.as_str(),
                                response_json: result.response_json.as_deref(),
                                error: Some("deterministic document validation failed"),
                            })?;
                            for validation in findings {
                                self.database.record_finding(FindingInput {
                                    work_item_id,
                                    attempt_id: Some(receipt.id),
                                    finding_key: &format!(
                                        "{}:{}:{}:{}",
                                        validation.code,
                                        unit.stable_id,
                                        receipt.id,
                                        &hash(&[validation.message.as_bytes()])[..16]
                                    ),
                                    severity: "error",
                                    code: validation.code,
                                    message: &validation.message,
                                    details_json: "{}",
                                })?;
                            }
                        }
                    }
                } else {
                    let status =
                        if result.code.as_deref() == Some(DecisionCode::AgentTimeout.as_str()) {
                            "timed_out"
                        } else {
                            "failed"
                        };
                    self.database.record_attempt(AttemptInput {
                        work_item_id,
                        dedupe_key: &dedupe_key,
                        agent: &agent_name,
                        provider: &provider,
                        model: &model,
                        adapter: &adapter,
                        provider_fingerprint: &provider_fingerprint,
                        prompt_version: &result.prompt_version,
                        prompt_hash: &result.prompt_hash,
                        policy_fingerprint: &result.policy_fingerprint,
                        status,
                        request_json: result.request_json.as_str(),
                        response_json: None,
                        error: Some(result.diagnostic.as_str()),
                    })?;
                }
            }
        }
        let _ = run_id;
        Ok(results)
    }

    fn repair(
        &self,
        language: &str,
        documents: &mut [PlannedDocument],
        all_results: &mut Vec<AgentResult>,
    ) -> Result<usize> {
        for document in documents.iter_mut() {
            for unit in &mut document.units {
                let Some(work_item_id) = unit.work_item_id.filter(|_| unit.translation.is_none())
                else {
                    continue;
                };
                let Some(failed) = self.database.failed_attempt_context(work_item_id)? else {
                    continue;
                };
                let Some(rejected) = failed.output.as_deref() else {
                    continue;
                };
                let Some(repaired) = repair_leading_strong_separator(&unit.unit, rejected)
                    .filter(|output| validate_unit(&unit.unit, output).is_ok())
                else {
                    continue;
                };
                let deterministic_key = format!(
                    "{}:repair:{REPAIR_CONTEXT_VERSION}:{LEADING_STRONG_SEPARATOR_VERSION}",
                    unit.stable_id
                );
                if let Some(recovered) = self.recovered_attempt(
                    work_item_id,
                    &deterministic_key,
                    &document.source_path,
                    &unit.unit,
                    true,
                    None,
                )? {
                    self.database.select_canonical_candidate(
                        unit.database_id,
                        language,
                        &format!("attempt:{}", recovered.id),
                        &recovered.output,
                        Some(recovered.id),
                        Some(1.0),
                    )?;
                    unit.translation = Some(recovered.output);
                    continue;
                }
                if self
                    .database
                    .attempt_status(work_item_id, &deterministic_key)?
                    .is_some()
                {
                    continue;
                }
                let rejected_hash = hash(&[rejected.as_bytes()]);
                let request_json = serde_json::to_string(&json!({
                    "schema": "fani.deterministic.repair.request.v1",
                    "task_id": unit.stable_id,
                    "operation": LEADING_STRONG_SEPARATOR_VERSION,
                    "source_attempt_id": failed.attempt_id,
                    "source_output_hash": rejected_hash,
                }))?;
                let response_json = serde_json::to_string(&json!({
                    "schema": "fani.agent.response.v1",
                    "task_id": unit.stable_id,
                    "output": repaired,
                }))?;
                let algorithm_hash = hash(&[LEADING_STRONG_SEPARATOR_VERSION.as_bytes()]);
                self.database
                    .record_attempt_candidate(AttemptCandidateInput {
                        attempt: AttemptInput {
                            work_item_id,
                            dedupe_key: &deterministic_key,
                            agent: "fani",
                            provider: "deterministic",
                            model: LEADING_STRONG_SEPARATOR_VERSION,
                            adapter: "native",
                            provider_fingerprint: &algorithm_hash,
                            prompt_version: LEADING_STRONG_SEPARATOR_VERSION,
                            prompt_hash: &algorithm_hash,
                            policy_fingerprint: &prompts::policy_fingerprint(),
                            status: "succeeded",
                            request_json: &request_json,
                            response_json: Some(&response_json),
                            error: None,
                        },
                        unit_id: unit.database_id,
                        locale: language,
                        candidate_key: &format!("attempt:{deterministic_key}"),
                        target_text: &repaired,
                        score: Some(1.0),
                        policy_fingerprint: &prompts::policy_fingerprint(),
                        provenance: TranslationProvenance::RepairedAi,
                    })?;
                (self.failpoint)("repair_candidate_committed");
                unit.translation = Some(repaired);
            }
        }

        let mut rounds = 0;
        for round in 0..self.repo.repair_budget {
            let mut tasks = Vec::new();
            let mut skipped_terminal_attempt = false;
            for document in documents.iter_mut() {
                for unit in &mut document.units {
                    let Some(work_item_id) =
                        unit.work_item_id.filter(|_| unit.translation.is_none())
                    else {
                        continue;
                    };
                    let dedupe_key =
                        format!("{}:repair:{REPAIR_CONTEXT_VERSION}:{round}", unit.stable_id);
                    if let Some(recovered) = self.recovered_attempt(
                        work_item_id,
                        &dedupe_key,
                        &document.source_path,
                        &unit.unit,
                        true,
                        None,
                    )? {
                        validate_unit(&unit.unit, &recovered.output).map_err(|findings| {
                            anyhow!(
                                "durable repair output for {} failed validation: {findings:?}",
                                unit.stable_id
                            )
                        })?;
                        self.database.select_canonical_candidate(
                            unit.database_id,
                            language,
                            &format!("attempt:{}", recovered.id),
                            &recovered.output,
                            Some(recovered.id),
                            Some(1.0),
                        )?;
                        unit.translation = Some(recovered.output);
                        continue;
                    }
                    if self
                        .database
                        .attempt_status(work_item_id, &dedupe_key)?
                        .is_some()
                    {
                        skipped_terminal_attempt = true;
                        continue;
                    }
                    let failed = self.database.failed_attempt_context(work_item_id)?;
                    let previous_translation = failed
                        .as_ref()
                        .and_then(|context| context.output.clone())
                        .or_else(|| unit.previous_translation.clone());
                    let mut repair_findings = failed
                        .as_ref()
                        .and_then(|context| context.output.as_deref())
                        .and_then(|output| validate_unit(&unit.unit, output).err())
                        .map(|validation| {
                            validation
                                .into_iter()
                                .map(|stored| {
                                    finding(
                                        &document.source_path,
                                        Some(unit.stable_id.clone()),
                                        stored.code,
                                        stored.message,
                                    )
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    if repair_findings.is_empty() {
                        repair_findings.push(finding(
                            &document.source_path,
                            Some(unit.stable_id.clone()),
                            DecisionCode::VerificationFailed.as_str(),
                            failed
                                .as_ref()
                                .and_then(|context| context.error.as_deref())
                                .unwrap_or(
                                    "previous output failed deterministic document validation",
                                ),
                        ));
                    }
                    tasks.push(AgentTask {
                        source_format: unit.unit.context.format,
                        unit_context: unit.unit.context.clone(),
                        context_key: unit.unit.memory_context_key(&document.source_path),
                        message_syntax: crate::domain::document::message_syntax(&unit.unit),
                        token_permissions: crate::domain::model::TokenPermissions::for_unit(
                            &unit.unit,
                        ),
                        id: unit.stable_id.clone(),
                        stage: AgentStage::Repair,
                        source_language: "auto".into(),
                        target_language: language.into(),
                        source: unit.unit.protected_source.clone(),
                        previous_source: unit.previous_source.clone(),
                        previous_translation,
                        findings: repair_findings,
                        protected_tokens: unit
                            .unit
                            .protected
                            .iter()
                            .map(|span| span.token.clone())
                            .collect(),
                    });
                }
            }
            if tasks.is_empty() {
                if skipped_terminal_attempt {
                    continue;
                }
                break;
            }
            rounds += 1;
            let execution = self.agents.execute(&tasks)?;
            let agent_name = execution.agent;
            let provider = execution.provider;
            let model = execution.model;
            let adapter = execution.adapter;
            let provider_fingerprint = execution.provider_fingerprint;
            let results = execution.results;
            let by_id: HashMap<_, _> = results
                .iter()
                .map(|result| (result.task_id.as_str(), result))
                .collect();
            for document in documents.iter_mut() {
                for unit in &mut document.units {
                    let Some(work_item_id) = unit.work_item_id else {
                        continue;
                    };
                    let Some(result) = by_id.get(unit.stable_id.as_str()) else {
                        continue;
                    };
                    let dedupe_key =
                        format!("{}:repair:{REPAIR_CONTEXT_VERSION}:{round}", unit.stable_id);
                    if result.ok && validate_unit(&unit.unit, &result.output).is_ok() {
                        self.database
                            .record_attempt_candidate(AttemptCandidateInput {
                                attempt: AttemptInput {
                                    work_item_id,
                                    dedupe_key: &dedupe_key,
                                    agent: &agent_name,
                                    provider: &provider,
                                    model: &model,
                                    adapter: &adapter,
                                    provider_fingerprint: &provider_fingerprint,
                                    prompt_version: &result.prompt_version,
                                    prompt_hash: &result.prompt_hash,
                                    policy_fingerprint: &result.policy_fingerprint,
                                    status: "succeeded",
                                    request_json: result.request_json.as_str(),
                                    response_json: result.response_json.as_deref(),
                                    error: None,
                                },
                                unit_id: unit.database_id,
                                locale: language,
                                candidate_key: &format!("attempt:{dedupe_key}"),
                                target_text: &result.output,
                                score: Some(1.0),
                                policy_fingerprint: &prompts::policy_fingerprint(),
                                provenance: TranslationProvenance::RepairedAi,
                            })?;
                        (self.failpoint)("repair_candidate_committed");
                        unit.translation = Some(result.output.clone());
                    } else {
                        let status = if result.code.as_deref()
                            == Some(DecisionCode::AgentTimeout.as_str())
                        {
                            "timed_out"
                        } else {
                            "failed"
                        };
                        let receipt = self.database.record_attempt(AttemptInput {
                            work_item_id,
                            dedupe_key: &dedupe_key,
                            agent: &agent_name,
                            provider: &provider,
                            model: &model,
                            adapter: &adapter,
                            provider_fingerprint: &provider_fingerprint,
                            prompt_version: &result.prompt_version,
                            prompt_hash: &result.prompt_hash,
                            policy_fingerprint: &result.policy_fingerprint,
                            status,
                            request_json: result.request_json.as_str(),
                            response_json: result.response_json.as_deref(),
                            error: Some(if result.ok {
                                "deterministic document validation failed"
                            } else {
                                result.diagnostic.as_str()
                            }),
                        })?;
                        if result.ok {
                            for validation in validate_unit(&unit.unit, &result.output)
                                .expect_err("invalid repair output was checked above")
                            {
                                self.database.record_finding(FindingInput {
                                    work_item_id,
                                    attempt_id: Some(receipt.id),
                                    finding_key: &format!(
                                        "{}:{}:{}:{}",
                                        validation.code,
                                        unit.stable_id,
                                        receipt.id,
                                        &hash(&[validation.message.as_bytes()])[..16]
                                    ),
                                    severity: "error",
                                    code: validation.code,
                                    message: &validation.message,
                                    details_json: "{}",
                                })?;
                            }
                        }
                    }
                }
            }
            all_results.extend(results);
        }
        Ok(rounds)
    }

    fn review(
        &self,
        language: &str,
        documents: &mut [PlannedDocument],
        all_results: &mut Vec<AgentResult>,
    ) -> Result<Vec<Finding>> {
        let mut findings = Vec::new();
        for (stage_name, stage, blocking) in [
            ("revision", AgentStage::Revision, true),
            ("proofread", AgentStage::Proofread, false),
        ] {
            let enabled = match stage {
                AgentStage::Revision => self.repo.quality.revision,
                AgentStage::Proofread => self.repo.quality.proofread,
                _ => false,
            };
            if !enabled {
                continue;
            }
            let mut tasks = Vec::new();
            for document in documents.iter_mut() {
                for unit in &mut document.units {
                    let Some(work_item_id) =
                        unit.work_item_id.filter(|_| unit.translation.is_some())
                    else {
                        continue;
                    };
                    let dedupe_key = format!("{}:{stage_name}", unit.stable_id);
                    if let Some(recovered) = self.recovered_attempt(
                        work_item_id,
                        &dedupe_key,
                        &document.source_path,
                        &unit.unit,
                        blocking,
                        unit.translation.as_deref(),
                    )? {
                        if recovered.output.trim().eq_ignore_ascii_case("OK") {
                            continue;
                        }
                        if blocking {
                            validate_unit(&unit.unit, &recovered.output).map_err(
                                |validation| {
                                    anyhow!(
                                        "durable revision output for {} failed validation: {validation:?}",
                                        unit.stable_id
                                    )
                                },
                            )?;
                            self.database.select_canonical_candidate(
                                unit.database_id,
                                language,
                                &format!("attempt:{}", recovered.id),
                                &recovered.output,
                                Some(recovered.id),
                                Some(1.0),
                            )?;
                            unit.translation = Some(recovered.output);
                        } else {
                            findings.push(Finding {
                                severity: FindingSeverity::Warning,
                                code: "PROOFREAD-ADVISORY".into(),
                                path: document.source_path.clone(),
                                unit_id: Some(unit.stable_id.clone()),
                                message: recovered.output,
                            });
                        }
                        continue;
                    }
                    if let Some(status) = self.database.attempt_status(work_item_id, &dedupe_key)? {
                        if blocking {
                            findings.push(finding(
                                &document.source_path,
                                Some(unit.stable_id.clone()),
                                DecisionCode::AgentExit.as_str(),
                                format!("durable blocking revision attempt ended as {status}"),
                            ));
                        }
                        continue;
                    }
                    tasks.push(AgentTask {
                        source_format: unit.unit.context.format,
                        unit_context: unit.unit.context.clone(),
                        context_key: unit.unit.memory_context_key(&document.source_path),
                        message_syntax: crate::domain::document::message_syntax(&unit.unit),
                        token_permissions: crate::domain::model::TokenPermissions::for_unit(
                            &unit.unit,
                        ),
                        id: unit.stable_id.clone(),
                        stage: stage.clone(),
                        source_language: "auto".into(),
                        target_language: language.into(),
                        source: unit.unit.protected_source.clone(),
                        previous_source: Some(unit.unit.protected_source.clone()),
                        previous_translation: unit.translation.clone(),
                        findings: Vec::new(),
                        protected_tokens: unit
                            .unit
                            .protected
                            .iter()
                            .map(|span| span.token.clone())
                            .collect(),
                    });
                }
            }
            if tasks.is_empty() {
                continue;
            }
            let execution = self.agents.execute(&tasks)?;
            let agent_name = execution.agent;
            let provider = execution.provider;
            let model = execution.model;
            let adapter = execution.adapter;
            let provider_fingerprint = execution.provider_fingerprint;
            let results = execution.results;
            let by_id: HashMap<_, _> = results
                .iter()
                .map(|result| (result.task_id.as_str(), result))
                .collect();
            for document in documents.iter_mut() {
                for unit in &mut document.units {
                    let Some(work_item_id) = unit.work_item_id else {
                        continue;
                    };
                    let Some(result) = by_id.get(unit.stable_id.as_str()) else {
                        continue;
                    };
                    let dedupe_key = format!("{}:{stage_name}", unit.stable_id);
                    if !result.ok {
                        let status = if result.code.as_deref()
                            == Some(DecisionCode::AgentTimeout.as_str())
                        {
                            "timed_out"
                        } else {
                            "failed"
                        };
                        self.database.record_attempt(AttemptInput {
                            work_item_id,
                            dedupe_key: &dedupe_key,
                            agent: &agent_name,
                            provider: &provider,
                            model: &model,
                            adapter: &adapter,
                            provider_fingerprint: &provider_fingerprint,
                            prompt_version: &result.prompt_version,
                            prompt_hash: &result.prompt_hash,
                            policy_fingerprint: &result.policy_fingerprint,
                            status,
                            request_json: result.request_json.as_str(),
                            response_json: None,
                            error: Some(result.diagnostic.as_str()),
                        })?;
                        if blocking {
                            findings.push(finding(
                                &document.source_path,
                                Some(unit.stable_id.clone()),
                                result
                                    .code
                                    .as_deref()
                                    .unwrap_or(DecisionCode::AgentExit.as_str()),
                                format!("blocking revision failed: {}", result.diagnostic),
                            ));
                        }
                        continue;
                    }
                    if result.output.trim().eq_ignore_ascii_case("OK") {
                        self.database.record_attempt(AttemptInput {
                            work_item_id,
                            dedupe_key: &dedupe_key,
                            agent: &agent_name,
                            provider: &provider,
                            model: &model,
                            adapter: &adapter,
                            provider_fingerprint: &provider_fingerprint,
                            prompt_version: &result.prompt_version,
                            prompt_hash: &result.prompt_hash,
                            policy_fingerprint: &result.policy_fingerprint,
                            status: "succeeded",
                            request_json: result.request_json.as_str(),
                            response_json: result.response_json.as_deref(),
                            error: None,
                        })?;
                        continue;
                    }
                    if blocking {
                        match validate_unit(&unit.unit, &result.output) {
                            Ok(_) => {
                                self.database
                                    .record_attempt_candidate(AttemptCandidateInput {
                                        attempt: AttemptInput {
                                            work_item_id,
                                            dedupe_key: &dedupe_key,
                                            agent: &agent_name,
                                            provider: &provider,
                                            model: &model,
                                            adapter: &adapter,
                                            provider_fingerprint: &provider_fingerprint,
                                            prompt_version: &result.prompt_version,
                                            prompt_hash: &result.prompt_hash,
                                            policy_fingerprint: &result.policy_fingerprint,
                                            status: "succeeded",
                                            request_json: result.request_json.as_str(),
                                            response_json: result.response_json.as_deref(),
                                            error: None,
                                        },
                                        unit_id: unit.database_id,
                                        locale: language,
                                        candidate_key: &format!("attempt:{dedupe_key}"),
                                        target_text: &result.output,
                                        score: Some(1.0),
                                        policy_fingerprint: &prompts::policy_fingerprint(),
                                        provenance: TranslationProvenance::Ai,
                                    })?;
                                (self.failpoint)("revision_candidate_committed");
                                unit.translation = Some(result.output.clone());
                            }
                            Err(validation) => {
                                self.database.record_attempt(AttemptInput {
                                    work_item_id,
                                    dedupe_key: &dedupe_key,
                                    agent: &agent_name,
                                    provider: &provider,
                                    model: &model,
                                    adapter: &adapter,
                                    provider_fingerprint: &provider_fingerprint,
                                    prompt_version: &result.prompt_version,
                                    prompt_hash: &result.prompt_hash,
                                    policy_fingerprint: &result.policy_fingerprint,
                                    status: "failed",
                                    request_json: result.request_json.as_str(),
                                    response_json: result.response_json.as_deref(),
                                    error: Some("deterministic document validation failed"),
                                })?;
                                findings.push(finding(
                                    &document.source_path,
                                    Some(unit.stable_id.clone()),
                                    DecisionCode::VerificationFailed.as_str(),
                                    format!("revision output failed deterministic validation: {validation:?}"),
                                ));
                            }
                        }
                    } else {
                        self.database.record_attempt(AttemptInput {
                            work_item_id,
                            dedupe_key: &dedupe_key,
                            agent: &agent_name,
                            provider: &provider,
                            model: &model,
                            adapter: &adapter,
                            provider_fingerprint: &provider_fingerprint,
                            prompt_version: &result.prompt_version,
                            prompt_hash: &result.prompt_hash,
                            policy_fingerprint: &result.policy_fingerprint,
                            status: "succeeded",
                            request_json: result.request_json.as_str(),
                            response_json: result.response_json.as_deref(),
                            error: None,
                        })?;
                        findings.push(Finding {
                            severity: FindingSeverity::Warning,
                            code: "PROOFREAD-ADVISORY".into(),
                            path: document.source_path.clone(),
                            unit_id: Some(unit.stable_id.clone()),
                            message: result.output.clone(),
                        });
                    }
                }
            }
            all_results.extend(results);
        }
        Ok(findings)
    }

    fn assemble_and_materialize(
        &self,
        repository_id: i64,
        run_id: &str,
        language: &str,
        source_revision: &str,
        documents: &mut [PlannedDocument],
    ) -> Result<(Vec<String>, Vec<PublicationFile>, Vec<Finding>)> {
        let mut written = Vec::new();
        let mut candidates = Vec::new();
        let mut findings = Vec::new();
        for document in documents.iter() {
            let missing: Vec<_> = document
                .units
                .iter()
                .filter(|unit| unit.translation.is_none())
                .collect();
            if !missing.is_empty() {
                let unresolved_scheduled = missing
                    .iter()
                    .copied()
                    .filter(|unit| unit.work_item_id.is_some())
                    .collect::<Vec<_>>();
                if !unresolved_scheduled.is_empty() {
                    findings.push(finding(
                        &document.source_path,
                        unresolved_scheduled
                            .first()
                            .map(|unit| unit.stable_id.clone()),
                        DecisionCode::VerificationFailed.as_str(),
                        format!(
                            "{} scheduled unit(s) have no verified translation",
                            unresolved_scheduled.len()
                        ),
                    ));
                }
                continue;
            }
            let translations: Vec<_> = document
                .units
                .iter()
                .map(|unit| UnitTranslation {
                    id: unit.unit.id.clone(),
                    text: unit.translation.clone().expect("checked translation"),
                })
                .collect();
            let work_item_id = self.database.enqueue_document_work_item(
                run_id,
                document.document_id,
                language,
                "assembly",
                0,
                &serde_json::to_string(&json!({
                    "source_revision": source_revision,
                    "path": document.source_path,
                    "target_path": document.target_path,
                    "contract": document.parsed.contract,
                    "document_identity": document.identity,
                }))?,
            )?;
            match assemble_document(&document.parsed, &translations) {
                Ok(assembled) => {
                    self.database.finish_document_work(
                        work_item_id,
                        true,
                        &json!({"content_hash": content_hash(assembled.as_bytes())}).to_string(),
                    )?;
                    candidates.push(PublicationFile {
                        path: document.target_path.clone(),
                        content: assembled.into_bytes(),
                    });
                }
                Err(_) => {
                    let message = "document assembly or deterministic verification failed";
                    self.database.record_finding(FindingInput {
                        work_item_id,
                        attempt_id: None,
                        finding_key: "DOCUMENT-VERIFY",
                        severity: "error",
                        code: "DOCUMENT-VERIFY",
                        message,
                        details_json: "{}",
                    })?;
                    self.database.finish_document_work(
                        work_item_id,
                        false,
                        "{\"code\":\"DOCUMENT-VERIFY\"}",
                    )?;
                    findings.push(finding(
                        &document.source_path,
                        None,
                        "DOCUMENT-VERIFY",
                        message,
                    ));
                }
            }
        }
        if findings
            .iter()
            .any(|item| item.severity == FindingSeverity::Error)
        {
            return Ok((written, candidates, findings));
        }
        findings.extend(self.check_documentation(
            run_id,
            language,
            source_revision,
            documents,
            &candidates,
        )?);
        if findings
            .iter()
            .any(|item| item.severity == FindingSeverity::Error)
        {
            return Ok((written, candidates, findings));
        }
        if !candidates.is_empty() {
            (self.failpoint)("project_checks_completed");
        }
        for document in documents {
            let Some(candidate) = candidates
                .iter()
                .find(|file| file.path == document.target_path)
            else {
                continue;
            };
            let desired = candidate.content.clone();
            let desired_hash = content_hash(&desired);
            let exact_translations = document
                .units
                .iter()
                .map(|unit| CanonicalTranslationInput {
                    unit_id: unit.database_id,
                    target_text: unit.translation.as_deref().expect("checked translation"),
                })
                .collect::<Vec<_>>();
            let canonical = self.database.persist_canonical_file(
                CanonicalFileInput {
                    repository_id,
                    locale: language,
                    path: &document.target_path,
                    source_revision,
                    content: &desired,
                    content_hash: &desired_hash,
                    materialized_hash: document.expected_materialized_hash.as_deref(),
                    freshness: Freshness::Exact,
                    provenance: if document.units.is_empty() {
                        TranslationProvenance::Imported
                    } else {
                        TranslationProvenance::Ai
                    },
                    validation: ValidationState::Passed,
                    review: ReviewState::Unreviewed,
                    publication: PublicationState::Candidate,
                    trust_tier: MemoryTier::Candidate,
                    policy_fingerprint: &prompts::policy_fingerprint(),
                },
                &exact_translations,
            )?;
            self.database.bind_canonical_document_intent(
                canonical.content_version_id,
                &serde_json::to_string(&document.identity)?,
            )?;
            let canonical_id = canonical.id;
            document.canonical_id = Some(canonical_id);
            (self.failpoint)("canonical_persisted_before_outbox");
            let dedupe = format!(
                "materialize:{repository_id}:{language}:{}:{desired_hash}:{}",
                document.target_path,
                document.identity.mapping_hash(),
            );
            let base_dedupe = dedupe;
            let dedupe = self
                .database
                .effect_key(OutboxKind::Materialization, &base_dedupe)?;
            let pending_work = self.database.materialization_work(&dedupe)?;
            // Terminal receipts describe history, not the current target bytes.
            if pending_work.is_none()
                && dedupe != base_dedupe
                && self
                    .materializer
                    .read(&self.repo.path, Path::new(&document.target_path))?
                    .is_some_and(|bytes| bytes == desired)
            {
                self.database.transition_canonical_file(
                    canonical_id,
                    CanonicalTransition::Materialized,
                    Some(&desired_hash),
                )?;
                continue;
            }
            self.database.supersede_materializations(
                repository_id,
                language,
                &document.target_path,
                &dedupe,
            )?;
            let work_item_id = match pending_work {
                Some(id) => id,
                None => self.database.enqueue_document_work_item(
                    run_id,
                    document.document_id,
                    language,
                    "materialization",
                    0,
                    &json!({"document_identity": document.identity, "content_hash": desired_hash, "effect_key": dedupe})
                        .to_string(),
                )?,
            };
            self.database.enqueue_materialization(
                work_item_id,
                &dedupe,
                &serde_json::to_string(&json!({
                    "repository_id": repository_id,
                    "locale": language,
                    "path": document.target_path,
                    "document_identity": document.identity,
                    "content_hash": desired_hash,
                    "canonical_content_version_id": canonical.content_version_id,
                    "canonical_file_id": canonical.id,
                }))?,
            )?;
            let owner = format!("materialize:{}:{run_id}", self.owner_identity);
            let claimed = self.database.claim_outbox_key(
                OutboxKind::Materialization,
                &dedupe,
                &owner,
                Utc::now().timestamp_millis(),
                60_000,
            )?;
            if let Some(entry) = claimed {
                let payload =
                    serde_json::from_str::<MaterializationPayload>(&entry.payload_json).ok();
                let compatible = if let Some(payload) = payload {
                    payload.repository_id == repository_id
                        && payload.locale == language
                        && payload.path == document.target_path
                        && payload.content_hash == desired_hash
                        && compatible_document_identity(
                            &payload.document_identity,
                            &document.identity,
                        )
                        && self.database.canonical_content_matches(
                            repository_id,
                            language,
                            &payload.document_identity.source_revision,
                            &PublicationManifestFile {
                                canonical_content_version_id: payload.canonical_content_version_id,
                                canonical_file_id: payload.canonical_file_id,
                                content_hash: payload.content_hash.clone(),
                            },
                            candidate,
                        )?
                        && self
                            .database
                            .canonical_compatible(payload.canonical_content_version_id)?
                } else {
                    false
                };
                if !compatible {
                    self.database.finish_document_work(
                        work_item_id,
                        false,
                        "{\"code\":\"DOCUMENT-INTENT\"}",
                    )?;
                    self.database
                        .complete_outbox(OutboxKind::Materialization, entry.id, &owner)?;
                    findings.push(finding(
                        &document.target_path,
                        None,
                        "DOCUMENT-INTENT",
                        "stored materialization identity failed current validation",
                    ));
                    continue;
                }
                let outbox_started = Instant::now();
                tracing::info!(
                    event = "outbox.claimed",
                    kind = "materialization",
                    outbox_id = entry.id,
                    run_id = %crate::diagnostics::safe_id(run_id),
                    locale = language,
                    item_id = %crate::diagnostics::safe_id(&dedupe),
                );
                let operation = Materialization {
                    path: document.target_path.clone().into(),
                    expected_hash: document.expected_materialized_hash.clone(),
                    desired,
                };
                (self.failpoint)("materialization_before_file_write");
                let result = self.materializer.apply(&self.repo.path, &operation);
                let operation_status = match result {
                    Ok(MaterializationResult::Written { hash }) => {
                        (self.failpoint)("materialized_file_written");
                        self.database.transition_canonical_file(
                            canonical_id,
                            CanonicalTransition::Materialized,
                            Some(&hash),
                        )?;
                        written.push(document.target_path.clone());
                        "written"
                    }
                    Ok(MaterializationResult::AlreadyCurrent { hash }) => {
                        self.database.transition_canonical_file(
                            canonical_id,
                            CanonicalTransition::Materialized,
                            Some(&hash),
                        )?;
                        "already_current"
                    }
                    Ok(MaterializationResult::HumanEdit { actual_hash }) => {
                        self.database.transition_canonical_file(
                            canonical_id,
                            CanonicalTransition::HumanEdit,
                            Some(&actual_hash),
                        )?;
                        findings.push(finding(
                            &document.target_path,
                            None,
                            DecisionCode::HumanEdit.as_str(),
                            "target changed outside fani and was not overwritten",
                        ));
                        "human_edit"
                    }
                    Err(error) => {
                        tracing::warn!(
                            event = "outbox.completed",
                            kind = "materialization",
                            outbox_id = entry.id,
                            run_id = %crate::diagnostics::safe_id(run_id),
                            locale = language,
                            status = "retry",
                            duration_ms = outbox_started.elapsed().as_millis() as u64,
                        );
                        self.database.retry_outbox(
                            OutboxKind::Materialization,
                            entry.id,
                            &owner,
                            &error.to_string(),
                            Utc::now().timestamp_millis() + 1_000,
                        )?;
                        return Err(error);
                    }
                };
                self.database.finish_document_work(
                    work_item_id,
                    operation_status != "human_edit",
                    &json!({"status": operation_status, "content_hash": desired_hash}).to_string(),
                )?;
                (self.failpoint)("materialization_state_transitioned");
                if !self
                    .database
                    .complete_outbox(OutboxKind::Materialization, entry.id, &owner)?
                {
                    bail!("materialization outbox lease was lost before completion");
                }
                tracing::info!(
                    event = "outbox.completed",
                    kind = "materialization",
                    outbox_id = entry.id,
                    run_id = %crate::diagnostics::safe_id(run_id),
                    locale = language,
                    status = operation_status,
                    duration_ms = outbox_started.elapsed().as_millis() as u64,
                );
            }
        }
        Ok((written, candidates, findings))
    }

    fn check_documentation(
        &self,
        run_id: &str,
        language: &str,
        source_revision: &str,
        documents: &[PlannedDocument],
        candidates: &[PublicationFile],
    ) -> Result<Vec<Finding>> {
        if self.repo.documentation.commands.is_empty() || candidates.is_empty() {
            return Ok(Vec::new());
        }
        let document = documents
            .iter()
            .find(|document| document.target_path == candidates[0].path)
            .ok_or_else(|| anyhow!("project check candidate has no document identity"))?;
        let manifest = candidates
            .iter()
            .map(|candidate| {
                json!({
                    "path": candidate.path,
                    "content_hash": content_hash(&candidate.content),
                })
            })
            .collect::<Vec<_>>();
        let manifest_hash = content_hash(&serde_json::to_vec(&manifest)?);
        let work_item_id = self.database.enqueue_document_work_item(
            run_id,
            document.document_id,
            language,
            "project_check",
            0,
            &serde_json::to_string(&json!({
                "source_revision": source_revision,
                "manifest": manifest,
                "manifest_hash": manifest_hash,
            }))?,
        )?;
        let result = self
            .documentation
            .check(self.repo, source_revision, candidates)?;
        let mut findings = Vec::new();
        for (index, failure) in result.failures {
            let code = if failure.timed_out {
                "DOC-CHECK-TIMEOUT"
            } else {
                "DOC-CHECK-FAILED"
            };
            let message = format!("documentation command {index} {}", failure.message);
            self.database.record_finding(FindingInput {
                work_item_id,
                attempt_id: None,
                finding_key: &format!("{code}:{index}"),
                severity: "error",
                code,
                message: &message,
                details_json: &serde_json::to_string(&json!({
                    "argv": self.repo.documentation.commands[index],
                    "command_index": index,
                    "timed_out": failure.timed_out,
                }))?,
            })?;
            findings.push(finding(".", None, code, message));
        }
        self.database.finish_document_work(
            work_item_id,
            findings.is_empty(),
            &json!({"manifest_hash": manifest_hash}).to_string(),
        )?;
        Ok(findings)
    }

    fn retire_incompatible_materializations(
        &self,
        repository_id: i64,
        language: &str,
        revision: &str,
    ) -> Result<()> {
        let sources = self.git.discover(self.repo, revision)?;
        for entry in self
            .database
            .pending_materializations(repository_id, language)?
        {
            let payload = serde_json::from_str::<MaterializationPayload>(&entry.payload_json).ok();
            let compatible = payload.as_ref().is_some_and(|payload| {
                payload.repository_id == repository_id
                    && payload.locale == language
                    && sources.iter().any(|source| {
                        source.target_path(language).to_string_lossy() == payload.path
                            && source.parse().is_ok_and(|parsed| {
                                compatible_document_identity(
                                    &payload.document_identity,
                                    &document_identity(source, language, &parsed),
                                )
                            })
                    })
            });
            if !compatible {
                self.database
                    .cancel_materialization(repository_id, entry.id)?;
            }
        }
        Ok(())
    }

    fn reconcile_pull_request(&self, repository_id: i64, language: &str) -> Result<()> {
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

    fn publish(
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

    pub fn run_language(&self, language: &str) -> LanguageOutcome {
        let started = Instant::now();
        let repository_id =
            crate::diagnostics::safe_id(self.repo.path.to_string_lossy().as_bytes());
        tracing::info!(
            event = "locale.run.started",
            repository_id,
            locale = language,
        );
        let mut outcome = LanguageOutcome::new(&self.repo.path, language);
        let result = (|| -> Result<()> {
            let source_revision = self.git.resolve_source_revision(self.repo)?;
            outcome.source_revision = source_revision.clone();
            let repository_id = self.repository_id()?;
            self.retire_incompatible_materializations(repository_id, language, &source_revision)?;
            self.reconcile_pull_request(repository_id, language)?;
            let policy_fingerprint = prompts::policy_fingerprint();
            let invocation = self.invocation_key(repository_id, language, &source_revision)?;
            let run_id = self.database.begin_run(
                repository_id,
                &invocation,
                self.config_path,
                "{}",
                &policy_fingerprint,
            )?;
            outcome.run_id = run_id.clone();
            tracing::info!(
                event = "run.started",
                repository_id = %crate::diagnostics::safe_id(self.repo.path.to_string_lossy().as_bytes()),
                run_id = %crate::diagnostics::safe_id(&run_id),
                locale = language,
                source_revision_id = %crate::diagnostics::safe_id(&source_revision),
                policy_id = %crate::diagnostics::safe_id(&policy_fingerprint),
            );
            if self.repo.publish.enabled {
                outcome.transitions.push("recovering:publication".into());
                self.traced_stage(language, &run_id, "publication_recovery", || {
                    self.publish(repository_id, &run_id, language, &[], &mut outcome)
                })?;
                if !outcome.published.commit.is_empty() {
                    if outcome.documents.parse_failures > 0 {
                        outcome.status = Status::NeedsHuman;
                        outcome.message =
                            "source document parse failures require human attention".into();
                        outcome.transitions.push("needs_human:conflict".into());
                    } else {
                        outcome.message = "recovered pending publication".into();
                        outcome.transitions.push("complete:ok".into());
                    }
                    return Ok(());
                }
            }
            let (mut documents, conflicts, reused, scheduled) =
                self.traced_stage(language, &run_id, "planning", || {
                    self.prepare(
                        repository_id,
                        &run_id,
                        language,
                        &source_revision,
                        &mut outcome.documents,
                    )
                })?;
            outcome.reused_units = reused;
            outcome.conflicts = conflicts;
            if !outcome.conflicts.is_empty() {
                outcome.status = Status::NeedsHuman;
                outcome.message = format!(
                    "{} conflict(s) require adopt/discard or unit disambiguation",
                    outcome.conflicts.len()
                );
                outcome.transitions.push("needs_human:conflict".into());
                return Ok(());
            }
            let pending_total = documents
                .iter()
                .flat_map(|document| &document.units)
                .filter(|unit| unit.translation.is_none())
                .count();
            outcome.remaining_tasks = pending_total.saturating_sub(scheduled);
            if scheduled > 0 {
                outcome.transitions.push("dispatching".into());
                outcome.agent_calls = self.traced_stage(language, &run_id, "translate", || {
                    self.dispatch(&run_id, language, &mut documents)
                })?;
                if documents
                    .iter()
                    .flat_map(|document| &document.units)
                    .any(|unit| unit.translation.is_none() && unit.work_item_id.is_some())
                {
                    outcome.transitions.push("repairing".into());
                    outcome.repair_rounds =
                        self.traced_stage(language, &run_id, "repair", || {
                            self.repair(language, &mut documents, &mut outcome.agent_calls)
                        })?;
                }
            }
            outcome.transitions.push("reviewing".into());
            let mut review_findings = self.traced_stage(language, &run_id, "review", || {
                self.review(language, &mut documents, &mut outcome.agent_calls)
            })?;
            let review_blocked = review_findings
                .iter()
                .any(|finding| finding.severity == FindingSeverity::Error);
            outcome.findings.append(&mut review_findings);
            if review_blocked {
                outcome.status = Status::NeedsHuman;
                outcome.message = "blocking bilingual revision did not pass".into();
                outcome.transitions.push("needs_human:revision".into());
                return Ok(());
            }
            outcome.transitions.push("verifying".into());
            let (written, candidates, mut findings) =
                self.traced_stage(language, &run_id, "materialization", || {
                    self.assemble_and_materialize(
                        repository_id,
                        &run_id,
                        language,
                        &source_revision,
                        &mut documents,
                    )
                })?;
            outcome.written = written;
            outcome.documents.verified_documents = candidates.len();
            outcome.findings.append(&mut findings);
            if outcome
                .findings
                .iter()
                .any(|finding| finding.severity == FindingSeverity::Error)
            {
                outcome.status = Status::NeedsHuman;
                outcome.message = "deterministic findings block publication".into();
                outcome.transitions.push("needs_human:verification".into());
                return Ok(());
            }
            if outcome.remaining_tasks > 0 {
                outcome.status = Status::Partial;
                outcome.message = format!(
                    "verified this bounded batch; {} unit(s) remain",
                    outcome.remaining_tasks
                );
            } else if scheduled == 0 && outcome.written.is_empty() {
                outcome.message = "every translation is up to date".into();
            } else {
                outcome.message =
                    format!("verified {} translated document(s)", outcome.written.len());
            }
            outcome.transitions.push("publishing".into());
            let eligible = candidates
                .iter()
                .map(|file| file.path.clone())
                .collect::<Vec<_>>();
            self.traced_stage(language, &run_id, "publication", || {
                self.publish(repository_id, &run_id, language, &eligible, &mut outcome)
            })?;
            outcome
                .transitions
                .push(format!("complete:{}", outcome.status.as_str()));
            Ok(())
        })();
        if let Err(error) = result {
            outcome.status = Status::Error;
            outcome.message = error.to_string();
            outcome.transitions.push("error".into());
        }
        if !outcome.run_id.is_empty() {
            if let Err(error) = self
                .database
                .finish_run(&outcome.run_id, outcome.status.as_str())
            {
                outcome.status = Status::Error;
                outcome.message = format!("cannot finalize durable run: {error}");
                outcome.transitions.push("error:run_finalize".into());
            }
        }
        outcome.duration_s = started.elapsed().as_secs_f64();
        tracing::info!(
            event = "run.completed",
            repository_id = %crate::diagnostics::safe_id(self.repo.path.to_string_lossy().as_bytes()),
            run_id = %crate::diagnostics::safe_id(&outcome.run_id),
            locale = language,
            status = outcome.status.as_str(),
            duration_ms = (outcome.duration_s * 1000.0) as u64,
            agent_calls = outcome.agent_calls.len(),
            files_written = outcome.written.len(),
            findings = outcome.findings.len(),
            conflicts = outcome.conflicts.len(),
            remaining_tasks = outcome.remaining_tasks,
            publication_id = %crate::diagnostics::safe_id(&outcome.published.commit),
            publication_pushed = outcome.published.pushed,
        );
        outcome
    }
}

pub fn adopt_human_edit(
    repo: &RepoConfig,
    database: &dyn StateStore,
    materializer: &dyn Materializer,
    git: &dyn GitPublisher,
    documentation: &dyn DocumentationChecker,
    language: &str,
) -> Result<usize> {
    let repository_key = repo
        .path
        .canonicalize()
        .unwrap_or_else(|_| repo.path.clone());
    let repository_id = database.upsert_repository(
        &hash(&[repository_key.to_string_lossy().as_bytes()]),
        &repo.path,
        Some(&repo.publish.github.base),
        None,
    )?;
    let source_revision = git.resolve_source_revision(repo)?;
    let documents = git.discover(repo, &source_revision)?;
    let mut adoption_files = Vec::new();
    for document in &documents {
        let target = document
            .target_path(language)
            .to_string_lossy()
            .into_owned();
        if database
            .canonical_file(repository_id, language, &target)?
            .is_none()
        {
            continue;
        }
        let bytes = materializer
            .read(&repo.path, Path::new(&target))?
            .with_context(|| format!("cannot read human target {target}"))?;
        let source = document.parse()?;
        let translated = document.parse_bytes(&bytes)?;
        verify_document(&source, &translated.source)
            .with_context(|| format!("human target {target} failed validation"))?;
        if source.units.len() != translated.units.len()
            || source
                .units
                .iter()
                .zip(&translated.units)
                .any(|(source, target)| {
                    crate::domain::document::translated_unit_text(source, target).is_none()
                })
        {
            bail!("human target {target} failed validation");
        }
        adoption_files.push(PublicationFile {
            path: target,
            content: bytes,
        });
    }
    if adoption_files.is_empty() {
        return Ok(0);
    }
    let manifest = adoption_files
        .iter()
        .map(|file| json!({"path": file.path, "content_hash": content_hash(&file.content)}))
        .collect::<Vec<_>>();
    let manifest_hash = content_hash(&serde_json::to_vec(&manifest)?);
    let run_id = database.begin_run(
        repository_id,
        &format!("adopt:{repository_id}:{language}:{source_revision}:{manifest_hash}"),
        Path::new("adopt"),
        "{}",
        &prompts::policy_fingerprint(),
    )?;
    let anchor = documents
        .iter()
        .find(|document| document.target_path(language).to_string_lossy() == adoption_files[0].path)
        .ok_or_else(|| anyhow!("adoption candidate has no document identity"))?;
    let anchor_id = database
        .document_id(repository_id, &anchor.path)?
        .ok_or_else(|| anyhow!("adoption document is missing"))?;
    let check_work = if repo.documentation.commands.is_empty() {
        None
    } else {
        Some(database.enqueue_document_work_item(&run_id, anchor_id, language, "project_check", 0, &json!({"source_revision":source_revision,"manifest":manifest,"manifest_hash":manifest_hash}).to_string())?)
    };
    let checked = documentation
        .check(repo, &source_revision, &adoption_files)?
        .failures
        .is_empty();
    if let Some(work) = check_work {
        database.finish_document_work(
            work,
            checked,
            &json!({"manifest_hash":manifest_hash}).to_string(),
        )?;
    }
    if !checked {
        database.finish_run(&run_id, "needs_human")?;
        bail!("human targets failed configured documentation checks; no translations were trusted");
    }
    let mut adopted = 0;
    for document in documents {
        let target = document
            .target_path(language)
            .to_string_lossy()
            .into_owned();
        let Some(canonical) = database.canonical_file(repository_id, language, &target)? else {
            continue;
        };
        let bytes = adoption_files
            .iter()
            .find(|file| file.path == target)
            .ok_or_else(|| anyhow!("human target {target} was not checked"))?
            .content
            .clone();
        let target_text = std::str::from_utf8(&bytes).context("human target is not UTF-8")?;
        let source_document = document.parse()?;
        let target_document = document.parse_bytes(target_text.as_bytes())?;
        verify_document(&source_document, target_text)
            .with_context(|| format!("human target {target} failed validation"))?;
        let source_units = &source_document.units;
        let target_units = &target_document.units;
        if source_units.len() != target_units.len() {
            bail!("human target {target} does not preserve the source document unit structure");
        }
        let translations = source_units
            .iter()
            .zip(target_units)
            .map(|(source, translated)| {
                crate::domain::document::translated_unit_text(source, translated)
                    .ok_or_else(|| anyhow!("human target {target} failed validation"))
            })
            .collect::<Result<Vec<_>>>()?;
        let stable_ids = source_units
            .iter()
            .enumerate()
            .map(|(ordinal, unit)| stable_unit_id(&document.path, unit, ordinal))
            .collect::<Vec<_>>();
        let stable_hints = stable_unit_hints(
            database,
            repository_id,
            &document.path,
            &document.content_hash,
            &stable_ids,
        )?;
        let history = match database.document_id(repository_id, &document.path)? {
            Some(document_id) => database.unit_history(document_id, language)?,
            None => Vec::new(),
        };
        let matched = match_units_with_stable_ids(
            &previous_units(&history, &[], &document.path),
            source_units,
            &stable_hints,
        );
        if matched
            .iter()
            .any(|matched| matched.kind == MatchKind::Ambiguous)
        {
            bail!("human target {target} cannot be mapped to stable source units");
        }
        let document_id = database.upsert_document(
            repository_id,
            &document.path,
            Some(&source_revision),
            &document.content_hash,
            &serde_json::to_string(
                &json!({"format": source_document.format, "contract": source_document.contract}),
            )?,
        )?;
        let identity = document_identity(&document, language, &source_document);
        let assembly_work = database.enqueue_document_work_item(
            &run_id,
            document_id,
            language,
            "assembly",
            0,
            &serde_json::to_string(&identity)?,
        )?;
        database.finish_document_work(
            assembly_work,
            true,
            &json!({"content_hash": content_hash(&bytes)}).to_string(),
        )?;
        for (ordinal, ((source_unit, translated), matched)) in source_units
            .iter()
            .zip(&translations)
            .zip(matched)
            .enumerate()
        {
            let stable_id = matched
                .stable_id
                .unwrap_or_else(|| stable_ids[ordinal].clone());
            let source_hash = hash(&[source_unit.source.as_bytes()]);
            let context = source_unit.memory_context_key(&document.path);
            let unit_id = database.upsert_unit(
                document_id,
                &stable_id,
                ordinal as i64,
                &source_unit.source,
                &source_hash,
                &unit_metadata(source_unit, &document.path),
            )?;
            database.trust_translation(TrustTranslationInput {
                repository_id,
                unit_id: Some(unit_id),
                locale: language,
                source_hash: &source_hash,
                context_key: &context,
                target_text: translated,
                provenance: "human_adopted",
                policy_fingerprint: &prompts::policy_fingerprint(),
            })?;
        }
        let adopted_hash = content_hash(&bytes);
        database.upsert_canonical_file(CanonicalFileInput {
            repository_id,
            locale: language,
            path: &target,
            source_revision: &source_revision,
            content: &bytes,
            content_hash: &adopted_hash,
            materialized_hash: Some(&adopted_hash),
            freshness: Freshness::Exact,
            provenance: if source_units.is_empty() {
                TranslationProvenance::Imported
            } else {
                TranslationProvenance::Human
            },
            validation: ValidationState::Passed,
            review: ReviewState::Approved,
            publication: PublicationState::Candidate,
            trust_tier: MemoryTier::Trusted,
            policy_fingerprint: &prompts::policy_fingerprint(),
        })?;
        let current = database
            .canonical_file(repository_id, language, &target)?
            .ok_or_else(|| anyhow!("adopted canonical content is missing"))?;
        database.bind_canonical_document_intent(
            current.content_version_id,
            &serde_json::to_string(&document_identity(&document, language, &source_document))?,
        )?;
        database.transition_canonical_file(
            canonical.id,
            CanonicalTransition::Adopted,
            Some(&adopted_hash),
        )?;
        let materialization_work = database.enqueue_document_work_item(
            &run_id,
            document_id,
            language,
            "materialization",
            0,
            &serde_json::to_string(&identity)?,
        )?;
        database.finish_document_work(
            materialization_work,
            true,
            &json!({"status":"adopted", "content_hash":adopted_hash}).to_string(),
        )?;
        adopted += 1;
    }
    database.finish_run(&run_id, "ok")?;
    Ok(adopted)
}

pub fn discard_human_edit(
    repo: &RepoConfig,
    database: &dyn StateStore,
    materializer: &dyn Materializer,
    git: &dyn GitPublisher,
    language: &str,
) -> Result<usize> {
    let repository_key = repo
        .path
        .canonicalize()
        .unwrap_or_else(|_| repo.path.clone());
    let repository_id = database.upsert_repository(
        &hash(&[repository_key.to_string_lossy().as_bytes()]),
        &repo.path,
        Some(&repo.publish.github.base),
        None,
    )?;
    let source_revision = git.resolve_source_revision(repo)?;
    let documents = git.discover(repo, &source_revision)?;
    let mut discarded = 0;
    for document in documents {
        let target = document
            .target_path(language)
            .to_string_lossy()
            .into_owned();
        let Some(canonical) = database.canonical_file(repository_id, language, &target)? else {
            continue;
        };
        let operation = Materialization {
            path: target.clone().into(),
            expected_hash: None,
            desired: canonical.content.clone(),
        };
        let result = materializer.restore(&repo.path, &operation)?;
        let hash = match result {
            MaterializationResult::Written { hash }
            | MaterializationResult::AlreadyCurrent { hash } => hash,
            MaterializationResult::HumanEdit { .. } => {
                return Err(anyhow!("cannot discard {target}"));
            }
        };
        database.transition_canonical_file(
            canonical.id,
            CanonicalTransition::Materialized,
            Some(&hash),
        )?;
        discarded += 1;
    }
    Ok(discarded)
}
