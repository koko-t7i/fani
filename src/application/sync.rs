use crate::application::command::OutputReporter;
use crate::application::ports::{
    AgentExecutor, AttemptCandidateInput, AttemptInput, CanonicalFileInput,
    CanonicalTranslationInput, CodeHost, DocumentationChecker, EnsurePullRequest, FindingInput,
    GitPublisher, Materialization, MaterializationResult, Materializer, OutboxKind,
    PublicationFile, PublicationManifestFile, PublicationManifestInput, PullRequestStateInput,
    StateStore, TrustTranslationInput,
};
use crate::application::settings::RepoConfig;
use crate::domain::markdown::{
    MarkdownUnit, UnitTranslation, apply_translations, extract_units,
    repair_leading_strong_separator, validate_translation,
};
use crate::domain::matching::{MatchKind, PreviousUnit, match_units_with_stable_ids};
use crate::domain::model::{
    AgentResult, AgentStage, AgentTask, CanonicalTransition, DecisionCode, Finding,
    FindingSeverity, Freshness, LanguageOutcome, MemoryTier, PlanSummary, PublicationState,
    ReviewState, Status, TranslationProvenance, ValidationState,
};
use crate::domain::prompts;
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

const REPAIR_CONTEXT_VERSION: &str = "v3";
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

fn target_path(repo: &RepoConfig, language: &str, source_path: &str) -> Result<PathBuf> {
    let value = repo
        .target_pattern
        .replace("{lang}", language)
        .replace("{relpath}", source_path);
    let path = PathBuf::from(value);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("target path escapes repository: {}", path.display());
    }
    Ok(path)
}

fn kind_name(unit: &MarkdownUnit) -> String {
    format!("{:?}", unit.kind)
}

fn stable_unit_id(path: &str, unit: &MarkdownUnit, ordinal: usize) -> String {
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
    kind: String,
}

fn previous_units(rows: &[crate::application::ports::UnitHistory]) -> Vec<PreviousUnit> {
    rows.iter()
        .filter_map(|row| {
            let kind = serde_json::from_str::<UnitContext>(&row.context_json)
                .ok()?
                .kind;
            let kind = match kind.as_str() {
                "Paragraph" => crate::domain::markdown::UnitKind::Paragraph,
                "Heading" => crate::domain::markdown::UnitKind::Heading,
                "ListItem" => crate::domain::markdown::UnitKind::ListItem,
                "TableCell" => crate::domain::markdown::UnitKind::TableCell,
                "DefinitionTerm" => crate::domain::markdown::UnitKind::DefinitionTerm,
                "Definition" => crate::domain::markdown::UnitKind::Definition,
                _ => return None,
            };
            Some(PreviousUnit {
                stable_id: row.unit_key.clone(),
                kind,
                ordinal: row.ordinal,
                source: row.source_text.clone(),
                translation: row.translation.clone().unwrap_or_default(),
                trusted: row.trusted,
            })
        })
        .collect()
}

#[derive(Clone)]
struct PlannedUnit {
    markdown: MarkdownUnit,
    database_id: i64,
    stable_id: String,
    translation: Option<String>,
    previous_source: Option<String>,
    previous_translation: Option<String>,
    work_item_id: Option<i64>,
}

struct PlannedDocument {
    source_path: String,
    target_path: String,
    source: String,
    units: Vec<PlannedUnit>,
    canonical_id: Option<i64>,
    expected_materialized_hash: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PublicationRecord {
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

    pub fn plan_language(&self, language: &str) -> Result<PlanSummary> {
        let source_revision = self.git.resolve_source_revision(self.repo)?;
        let repository_id = self.repository_id()?;
        let documents = self.git.discover(self.repo, &source_revision)?;
        let mut pending = 0;
        let mut reused = 0;
        let mut conflicts = 0;
        for document in &documents {
            let text = std::str::from_utf8(&document.bytes)
                .with_context(|| format!("{} is not UTF-8", document.path))?;
            let units = extract_units(text);
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
            let history = match self.database.document_id(repository_id, &document.path)? {
                Some(document_id) => self.database.unit_history(document_id, language)?,
                None => Vec::new(),
            };
            let matched =
                match_units_with_stable_ids(&previous_units(&history), &units, &stable_hints);
            for (unit, matched) in units.iter().zip(matched) {
                match matched.kind {
                    MatchKind::Ambiguous => conflicts += 1,
                    MatchKind::Exact | MatchKind::Moved
                        if matched.trusted_reuse
                            && matched
                                .previous_translation
                                .as_ref()
                                .is_some_and(|text| !text.is_empty()) =>
                    {
                        reused += 1
                    }
                    _ => {
                        let context = kind_name(unit);
                        let source_hash = hash(&[unit.source.as_bytes()]);
                        let trusted = self.database.trusted_translation(
                            repository_id,
                            language,
                            &source_hash,
                            &context,
                        )?;
                        if trusted.is_some() {
                            reused += 1;
                        } else {
                            pending += 1;
                        }
                    }
                }
            }
        }
        Ok(PlanSummary {
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
    ) -> Result<(Vec<PlannedDocument>, Vec<Finding>, usize, usize)> {
        let mut planned_documents = Vec::new();
        let mut conflicts = Vec::new();
        let mut reused = 0;
        let mut scheduled = 0;
        let policy_fingerprint = prompts::policy_fingerprint();
        let documents = self.git.discover(self.repo, source_revision)?;
        for document in documents {
            let source_text = String::from_utf8(document.bytes)
                .with_context(|| format!("{} is not UTF-8", document.path))?;
            let markdown_units = extract_units(&source_text);
            let stable_ids = markdown_units
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
            let history = match self.database.document_id(repository_id, &document.path)? {
                Some(document_id) => self.database.unit_history(document_id, language)?,
                None => Vec::new(),
            };
            let matched = match_units_with_stable_ids(
                &previous_units(&history),
                &markdown_units,
                &stable_hints,
            );
            if matched
                .iter()
                .any(|matched| matched.kind == MatchKind::Ambiguous)
            {
                conflicts.extend(
                    markdown_units
                        .iter()
                        .zip(&matched)
                        .filter(|(_, matched)| matched.kind == MatchKind::Ambiguous)
                        .map(|(markdown, _)| {
                            finding(
                                &document.path,
                                Some(markdown.id.clone()),
                                DecisionCode::AmbiguousMatch.as_str(),
                                "multiple previous units match this Markdown unit",
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
                "{}",
            )?;
            let mut units = Vec::new();
            for (ordinal, (markdown, matched)) in
                markdown_units.into_iter().zip(matched).enumerate()
            {
                let stable_id = matched
                    .stable_id
                    .clone()
                    .unwrap_or_else(|| stable_ids[ordinal].clone());
                let source_hash = hash(&[markdown.source.as_bytes()]);
                let context = kind_name(&markdown);
                let database_id = self.database.upsert_unit(
                    document_id,
                    &stable_id,
                    ordinal as i64,
                    &markdown.source,
                    &source_hash,
                    &serde_json::to_string(&json!({"kind": context}))?,
                )?;
                let candidate =
                    self.database
                        .recoverable_candidate(run_id, database_id, language)?;
                let prior_candidate = if stable_hints[ordinal].is_empty() {
                    None
                } else {
                    self.database.recoverable_unit_candidate(
                        database_id,
                        language,
                        &policy_fingerprint,
                    )?
                };
                let trusted = self.database.trusted_translation(
                    repository_id,
                    language,
                    &source_hash,
                    &context,
                )?;
                let reusable = trusted.or(candidate).or(prior_candidate).or_else(|| {
                    (matched.trusted_reuse
                        && matches!(matched.kind, MatchKind::Exact | MatchKind::Moved))
                    .then_some(matched.previous_translation.clone())
                    .flatten()
                    .filter(|text| !text.is_empty())
                });
                let mut planned = PlannedUnit {
                    markdown,
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
            let target = target_path(self.repo, language, &document.path)?;
            let target_path = target.to_string_lossy().into_owned();
            let canonical = self
                .database
                .canonical_file(repository_id, language, &target_path)?;
            if let Some(canonical) = &canonical {
                let actual = self
                    .materializer
                    .read(&self.repo.path, &target)?
                    .map(|bytes| content_hash(&bytes));
                if actual.as_deref() == Some(canonical.content_hash.as_str()) {
                    if canonical.materialized_hash.as_deref()
                        != Some(canonical.content_hash.as_str())
                    {
                        self.database.transition_canonical_file(
                            canonical.id,
                            CanonicalTransition::Materialized,
                            Some(&canonical.content_hash),
                        )?;
                    }
                } else if actual.as_deref() != canonical.materialized_hash.as_deref() {
                    conflicts.push(finding(
                        &target_path,
                        None,
                        DecisionCode::HumanEdit.as_str(),
                        "materialized target differs from the canonical SQLite content; use adopt or discard",
                    ));
                }
            }
            planned_documents.push(PlannedDocument {
                source_path: document.path,
                target_path,
                source: source_text,
                units,
                canonical_id: canonical.as_ref().map(|value| value.id),
                expected_materialized_hash: canonical.and_then(|value| value.materialized_hash),
            });
        }
        Ok((planned_documents, conflicts, reused, scheduled))
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
                if let Some(recovered) = self
                    .database
                    .successful_attempt(work_item_id, &dedupe_key)?
                {
                    validate_translation(&unit.markdown, &recovered.output).map_err(
                        |findings| {
                            anyhow!(
                                "durable Agent output for {} failed validation: {findings:?}",
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
                    id: unit.stable_id.clone(),
                    stage: AgentStage::Translate,
                    source_language: "auto".into(),
                    target_language: language.into(),
                    source: unit.markdown.protected_source.clone(),
                    previous_source: unit.previous_source.clone(),
                    previous_translation: unit.previous_translation.clone(),
                    findings: Vec::new(),
                    protected_tokens: unit
                        .markdown
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
            "    dispatched {} native Markdown unit(s) to {}",
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
                    match validate_translation(&unit.markdown, &result.output) {
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
                                error: Some("deterministic Markdown validation failed"),
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
                let Some(repaired) = repair_leading_strong_separator(&unit.markdown, rejected)
                    .filter(|output| validate_translation(&unit.markdown, output).is_ok())
                else {
                    continue;
                };
                let deterministic_key = format!(
                    "{}:repair:{REPAIR_CONTEXT_VERSION}:leading-strong-separator",
                    unit.stable_id
                );
                if let Some(recovered) = self
                    .database
                    .successful_attempt(work_item_id, &deterministic_key)?
                {
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
                    if let Some(recovered) = self
                        .database
                        .successful_attempt(work_item_id, &dedupe_key)?
                    {
                        validate_translation(&unit.markdown, &recovered.output).map_err(
                            |findings| {
                                anyhow!(
                                    "durable repair output for {} failed validation: {findings:?}",
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
                        .and_then(|output| validate_translation(&unit.markdown, output).err())
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
                                    "previous output failed deterministic Markdown validation",
                                ),
                        ));
                    }
                    tasks.push(AgentTask {
                        id: unit.stable_id.clone(),
                        stage: AgentStage::Repair,
                        source_language: "auto".into(),
                        target_language: language.into(),
                        source: unit.markdown.protected_source.clone(),
                        previous_source: unit.previous_source.clone(),
                        previous_translation,
                        findings: repair_findings,
                        protected_tokens: unit
                            .markdown
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
                    if result.ok && validate_translation(&unit.markdown, &result.output).is_ok() {
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
                                "deterministic Markdown validation failed"
                            } else {
                                result.diagnostic.as_str()
                            }),
                        })?;
                        if result.ok {
                            for validation in validate_translation(&unit.markdown, &result.output)
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
                    if let Some(recovered) = self
                        .database
                        .successful_attempt(work_item_id, &dedupe_key)?
                    {
                        if recovered.output.trim().eq_ignore_ascii_case("OK") {
                            continue;
                        }
                        if blocking {
                            validate_translation(&unit.markdown, &recovered.output).map_err(
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
                        id: unit.stable_id.clone(),
                        stage: stage.clone(),
                        source_language: "auto".into(),
                        target_language: language.into(),
                        source: unit.markdown.protected_source.clone(),
                        previous_source: Some(unit.markdown.protected_source.clone()),
                        previous_translation: unit.translation.clone(),
                        findings: Vec::new(),
                        protected_tokens: unit
                            .markdown
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
                        match validate_translation(&unit.markdown, &result.output) {
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
                                    error: Some("deterministic Markdown validation failed"),
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
        for document in documents {
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
                    id: unit.markdown.id.clone(),
                    text: unit.translation.clone().expect("checked translation"),
                })
                .collect();
            let assembled = apply_translations(
                &document.source,
                &document
                    .units
                    .iter()
                    .map(|unit| unit.markdown.clone())
                    .collect::<Vec<_>>(),
                &translations,
            )
            .with_context(|| format!("cannot assemble {}", document.source_path))?;
            let desired = assembled.into_bytes();
            let desired_hash = content_hash(&desired);
            candidates.push(PublicationFile {
                path: document.target_path.clone(),
                content: desired.clone(),
            });
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
                    provenance: TranslationProvenance::Ai,
                    validation: ValidationState::Passed,
                    review: ReviewState::Unreviewed,
                    publication: PublicationState::Candidate,
                    trust_tier: MemoryTier::Candidate,
                    policy_fingerprint: &prompts::policy_fingerprint(),
                },
                &exact_translations,
            )?;
            let canonical_id = canonical.id;
            document.canonical_id = Some(canonical_id);
            let unit = document.units.first().ok_or_else(|| {
                anyhow!(
                    "{} has no translatable Markdown units",
                    document.source_path
                )
            })?;
            let work_item_id = if let Some(id) = unit.work_item_id {
                id
            } else {
                self.database.enqueue_work_item(
                    run_id,
                    unit.database_id,
                    language,
                    "materialize",
                    0,
                    "{}",
                )?
            };
            let dedupe = format!(
                "materialize:{repository_id}:{language}:{}:{desired_hash}",
                document.target_path
            );
            self.database
                .supersede_materializations(language, &document.target_path, &dedupe)?;
            self.database.enqueue_materialization(
                work_item_id,
                &dedupe,
                &serde_json::to_string(&json!({
                    "repository_id": repository_id,
                    "locale": language,
                    "path": document.target_path,
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
        let result = self
            .documentation
            .check(self.repo, source_revision, candidates)?;
        let unit = documents
            .iter()
            .flat_map(|document| &document.units)
            .next()
            .ok_or_else(|| anyhow!("documentation checks require a translated Markdown unit"))?;
        let work_item_id = self.database.enqueue_work_item(
            run_id,
            unit.database_id,
            language,
            "documentation_check",
            0,
            &serde_json::to_string(&json!({"source_revision": source_revision}))?,
        )?;
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
        Ok(findings)
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
            self.database.promote_merged_publication(
                repository_id,
                language,
                candidate_commit,
                "github_merged",
            )?;
        }
        Ok(())
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
            self.database.enqueue_publication(
                repository_id,
                Some(run_id),
                language,
                &dedupe,
                &payload,
            )?;
            self.database.claim_publication_locale(
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
        outcome.published = if let Some(commit) = payload.commit.as_deref() {
            self.database
                .record_publication_manifest(PublicationManifestInput {
                    repository_id,
                    run_id: &payload.run_id,
                    locale: language,
                    source_revision: &payload_source_revision,
                    candidate_commit: commit,
                    policy_fingerprint: &payload.policy_fingerprint,
                    files: &manifest_files,
                })?;
            if self.repo.publish.push {
                self.database.transition_publication_manifest(
                    repository_id,
                    language,
                    commit,
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
            self.database
                .record_publication_manifest(PublicationManifestInput {
                    repository_id,
                    run_id: &payload.run_id,
                    locale: language,
                    source_revision: &payload_source_revision,
                    candidate_commit: &prepared.commit,
                    policy_fingerprint: &payload.policy_fingerprint,
                    files: &manifest_files,
                })?;
            (self.failpoint)("publication_candidate_persisted");
            if self.repo.publish.push {
                self.database.transition_publication_manifest(
                    repository_id,
                    language,
                    &prepared.commit,
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
                        self.database.promote_merged_publication(
                            repository_id,
                            language,
                            &outcome.published.commit,
                            "github_merged",
                        )?;
                    }
                    "open" | "draft" => {
                        self.database.transition_publication_manifest(
                            repository_id,
                            language,
                            &outcome.published.commit,
                            PublicationState::PrOpen,
                        )?;
                    }
                    _ => {
                        self.database.transition_publication_manifest(
                            repository_id,
                            language,
                            &outcome.published.commit,
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
            self.reconcile_pull_request(repository_id, language)?;
            let policy_fingerprint = prompts::policy_fingerprint();
            let invocation =
                format!("sync:{repository_id}:{language}:{source_revision}:{policy_fingerprint}");
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
                    outcome.message = "recovered pending publication".into();
                    outcome.transitions.push("complete:ok".into());
                    return Ok(());
                }
            }
            let (mut documents, conflicts, reused, scheduled) =
                self.traced_stage(language, &run_id, "planning", || {
                    self.prepare(repository_id, &run_id, language, &source_revision)
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
            outcome.transitions.push("checking:documentation".into());
            let mut documentation_findings =
                self.traced_stage(language, &run_id, "documentation_check", || {
                    self.check_documentation(
                        &run_id,
                        language,
                        &source_revision,
                        &documents,
                        &candidates,
                    )
                })?;
            outcome.findings.append(&mut documentation_findings);
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
            let written = outcome.written.clone();
            self.traced_stage(language, &run_id, "publication", || {
                self.publish(repository_id, &run_id, language, &written, &mut outcome)
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
    let mut adopted = 0;
    for document in documents {
        let target = target_path(repo, language, &document.path)?
            .to_string_lossy()
            .into_owned();
        let Some(canonical) = database.canonical_file(repository_id, language, &target)? else {
            continue;
        };
        let bytes = materializer
            .read(&repo.path, Path::new(&target))?
            .with_context(|| format!("cannot read human target {target}"))?;
        let target_text = std::str::from_utf8(&bytes).context("human target is not UTF-8")?;
        let source_text =
            std::str::from_utf8(&document.bytes).context("source Markdown is not UTF-8")?;
        let source_units = extract_units(source_text);
        let target_units = extract_units(target_text);
        if source_units.len() != target_units.len() || source_units.is_empty() {
            bail!("human target {target} does not preserve the source Markdown unit structure");
        }
        for (source_unit, target_unit) in source_units.iter().zip(&target_units) {
            validate_translation(source_unit, &target_unit.protected_source).map_err(
                |findings| anyhow!("human target {target} failed validation: {findings:?}"),
            )?;
        }
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
        let matched =
            match_units_with_stable_ids(&previous_units(&history), &source_units, &stable_hints);
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
            "{}",
        )?;
        for (ordinal, ((source_unit, target_unit), matched)) in source_units
            .iter()
            .zip(&target_units)
            .zip(matched)
            .enumerate()
        {
            let stable_id = matched
                .stable_id
                .unwrap_or_else(|| stable_ids[ordinal].clone());
            let source_hash = hash(&[source_unit.source.as_bytes()]);
            let context = kind_name(source_unit);
            let unit_id = database.upsert_unit(
                document_id,
                &stable_id,
                ordinal as i64,
                &source_unit.source,
                &source_hash,
                &serde_json::to_string(&json!({"kind": context}))?,
            )?;
            database.trust_translation(TrustTranslationInput {
                repository_id,
                unit_id: Some(unit_id),
                locale: language,
                source_hash: &source_hash,
                context_key: &context,
                target_text: &target_unit.protected_source,
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
            provenance: TranslationProvenance::Human,
            validation: ValidationState::Passed,
            review: ReviewState::Approved,
            publication: PublicationState::Candidate,
            trust_tier: MemoryTier::Trusted,
            policy_fingerprint: &prompts::policy_fingerprint(),
        })?;
        database.transition_canonical_file(
            canonical.id,
            CanonicalTransition::Adopted,
            Some(&adopted_hash),
        )?;
        adopted += 1;
    }
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
        let target = target_path(repo, language, &document.path)?
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
