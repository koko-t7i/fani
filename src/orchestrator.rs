use crate::agent::{AgentExecutor, CommandAgent};
use crate::config::{Config, RepoConfig};
use crate::db::{
    AttemptInput, CanonicalFileInput, Database, FindingInput, OutboxKind, PullRequestStateInput,
    TrustTranslationInput,
};
use crate::github::{EnsurePullRequest, GhClient, locale_branch};
use crate::markdown::{
    MarkdownUnit, UnitTranslation, apply_translations, extract_units, validate_translation,
};
use crate::matching::{MatchKind, PreviousUnit, match_units};
use crate::materialize::{
    Materialization, MaterializationResult, apply as materialize, content_hash, restore, safe_path,
};
use crate::model::{
    AgentResult, AgentStage, AgentTask, DecisionCode, Finding, FindingSeverity, LanguageOutcome,
    PlanSummary, Status,
};
use crate::source;
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Instant;

fn hash(parts: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    format!("{:x}", digest.finalize())
}

fn kind_name(unit: &MarkdownUnit) -> String {
    format!("{:?}", unit.kind)
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

fn previous_units(rows: &[crate::db::UnitHistory]) -> Vec<PreviousUnit> {
    rows.iter()
        .filter_map(|row| {
            let kind = serde_json::from_str::<serde_json::Value>(&row.context_json)
                .ok()?
                .get("kind")?
                .as_str()?
                .to_owned();
            let kind = match kind.as_str() {
                "Paragraph" => crate::markdown::UnitKind::Paragraph,
                "Heading" => crate::markdown::UnitKind::Heading,
                "TableCell" => crate::markdown::UnitKind::TableCell,
                "DefinitionTerm" => crate::markdown::UnitKind::DefinitionTerm,
                "Definition" => crate::markdown::UnitKind::Definition,
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

pub struct Orchestrator<'a> {
    config: &'a Config,
    repo: &'a RepoConfig,
    database: &'a Database,
    config_path: &'a Path,
    quiet: bool,
}

impl<'a> Orchestrator<'a> {
    pub fn new(
        config: &'a Config,
        repo: &'a RepoConfig,
        database: &'a Database,
        config_path: &'a Path,
        quiet: bool,
    ) -> Self {
        Self {
            config,
            repo,
            database,
            config_path,
            quiet,
        }
    }

    fn log(&self, message: &str) {
        if !self.quiet {
            eprintln!("{message}");
        }
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
        let source_revision = source::resolve_source_revision(self.repo)?;
        let repository_id = self.repository_id()?;
        let documents = source::discover(self.repo, &source_revision)?;
        let mut pending = 0;
        let mut reused = 0;
        let mut conflicts = 0;
        for document in &documents {
            let text = std::str::from_utf8(&document.bytes)
                .with_context(|| format!("{} is not UTF-8", document.path))?;
            let document_id = self.database.upsert_document(
                repository_id,
                &document.path,
                Some(&source_revision),
                &document.content_hash,
                "{}",
            )?;
            let units = extract_units(text);
            let history = self.database.unit_history(document_id, language)?;
            let matched = match_units(&previous_units(&history), &units);
            for (unit, matched) in units.iter().zip(matched) {
                match matched.kind {
                    MatchKind::Ambiguous => conflicts += 1,
                    MatchKind::Exact | MatchKind::Moved
                        if matched
                            .previous_translation
                            .as_ref()
                            .is_some_and(|text| !text.is_empty()) =>
                    {
                        reused += 1
                    }
                    _ => {
                        let context = kind_name(unit);
                        let source_hash = hash(&[unit.source.as_bytes()]);
                        if self
                            .database
                            .trusted_translation(repository_id, language, &source_hash, &context)?
                            .is_some()
                        {
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
        let documents = source::discover(self.repo, source_revision)?;
        for document in documents {
            let source_text = String::from_utf8(document.bytes)
                .with_context(|| format!("{} is not UTF-8", document.path))?;
            let document_id = self.database.upsert_document(
                repository_id,
                &document.path,
                Some(source_revision),
                &document.content_hash,
                "{}",
            )?;
            let markdown_units = extract_units(&source_text);
            let history = self.database.unit_history(document_id, language)?;
            let matched = match_units(&previous_units(&history), &markdown_units);
            let history_by_key: HashMap<_, _> = history
                .iter()
                .map(|row| (row.unit_key.as_str(), row))
                .collect();
            let mut units = Vec::new();
            for (ordinal, (markdown, matched)) in
                markdown_units.into_iter().zip(matched).enumerate()
            {
                if matched.kind == MatchKind::Ambiguous {
                    conflicts.push(finding(
                        &document.path,
                        Some(markdown.id.clone()),
                        DecisionCode::AmbiguousMatch.as_str(),
                        "multiple previous units match this Markdown unit",
                    ));
                    continue;
                }
                let stable_id = matched.stable_id.clone().unwrap_or_else(|| {
                    format!(
                        "unit-{}",
                        &hash(&[
                            document.path.as_bytes(),
                            markdown.id.as_bytes(),
                            ordinal.to_string().as_bytes()
                        ])[..24]
                    )
                });
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
                let candidate = history_by_key.get(stable_id.as_str()).and_then(|row| {
                    (row.source_hash == source_hash)
                        .then(|| row.translation.clone())
                        .flatten()
                });
                let trusted = self.database.trusted_translation(
                    repository_id,
                    language,
                    &source_hash,
                    &context,
                )?;
                let reusable = candidate.or(trusted).or_else(|| {
                    matches!(matched.kind, MatchKind::Exact | MatchKind::Moved)
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
                        "translate",
                        0,
                        &input,
                    )?);
                    scheduled += 1;
                }
                units.push(planned);
            }
            let target = source::target_path(self.repo, language, &document.path)?;
            let target_path = target.to_string_lossy().into_owned();
            let canonical = self
                .database
                .canonical_file(repository_id, language, &target_path)?;
            if let Some(canonical) = &canonical {
                let disk = safe_path(&self.repo.path, &target)?;
                let actual = fs::read(&disk).ok().map(|bytes| content_hash(&bytes));
                if actual.as_deref() == Some(canonical.content_hash.as_str()) {
                    if canonical.materialized_hash.as_deref()
                        != Some(canonical.content_hash.as_str())
                    {
                        self.database.set_canonical_file_state(
                            canonical.id,
                            "materialized",
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
        let config = self.config.agent_for("translate")?;
        let executor = CommandAgent::new(config);
        let mut tasks = Vec::new();
        for document in documents.iter() {
            for unit in &document.units {
                if unit.translation.is_none() && unit.work_item_id.is_some() {
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
        }
        self.log(&format!(
            "    dispatching {} native Markdown unit(s) to {}",
            tasks.len(),
            config.name
        ));
        let results = executor.execute(&tasks)?;
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
                let status = if result.ok {
                    "succeeded"
                } else if result.code.as_deref() == Some(DecisionCode::AgentTimeout.as_str()) {
                    "timed_out"
                } else {
                    "failed"
                };
                let request_json = serde_json::to_string(
                    &json!({"task_id": unit.stable_id, "stage": "translate"}),
                )?;
                let response_json = result
                    .ok
                    .then(|| serde_json::to_string(&json!({"output": result.output})))
                    .transpose()?;
                let receipt = self.database.record_attempt(AttemptInput {
                    work_item_id,
                    dedupe_key: &format!("{}:translate", unit.stable_id),
                    agent: &config.name,
                    status,
                    request_json: &request_json,
                    response_json: response_json.as_deref(),
                    error: (!result.ok).then_some(result.diagnostic.as_str()),
                })?;
                if result.ok {
                    match validate_translation(&unit.markdown, &result.output) {
                        Ok(_) => {
                            unit.translation = Some(result.output.clone());
                            self.database.select_canonical_candidate(
                                unit.database_id,
                                language,
                                &format!("attempt:{}", receipt.id),
                                &result.output,
                                Some(receipt.id),
                                Some(1.0),
                            )?;
                        }
                        Err(findings) => {
                            for validation in findings {
                                self.database.record_finding(FindingInput {
                                    work_item_id,
                                    attempt_id: Some(receipt.id),
                                    finding_key: &format!("{}:{}", validation.code, unit.stable_id),
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
        let config = self.config.agent_for("repair")?;
        let executor = CommandAgent::new(config);
        let mut rounds = 0;
        for round in 0..self.repo.repair_budget {
            let mut tasks = Vec::new();
            for document in documents.iter() {
                for unit in &document.units {
                    if unit.translation.is_none() && unit.work_item_id.is_some() {
                        tasks.push(AgentTask {
                            id: unit.stable_id.clone(),
                            stage: AgentStage::Repair,
                            source_language: "auto".into(),
                            target_language: language.into(),
                            source: unit.markdown.protected_source.clone(),
                            previous_source: unit.previous_source.clone(),
                            previous_translation: unit.previous_translation.clone(),
                            findings: vec![finding(
                                &document.source_path,
                                Some(unit.stable_id.clone()),
                                DecisionCode::VerificationFailed.as_str(),
                                "previous output failed deterministic Markdown validation",
                            )],
                            protected_tokens: unit
                                .markdown
                                .protected
                                .iter()
                                .map(|span| span.token.clone())
                                .collect(),
                        });
                    }
                }
            }
            if tasks.is_empty() {
                break;
            }
            rounds += 1;
            let results = executor.execute(&tasks)?;
            let by_id: HashMap<_, _> = results
                .iter()
                .map(|result| (result.task_id.as_str(), result))
                .collect();
            for document in documents.iter_mut() {
                for unit in &mut document.units {
                    if let Some(result) = by_id
                        .get(unit.stable_id.as_str())
                        .filter(|result| result.ok)
                    {
                        if validate_translation(&unit.markdown, &result.output).is_ok() {
                            unit.translation = Some(result.output.clone());
                            self.database.select_canonical_candidate(
                                unit.database_id,
                                language,
                                &format!("repair:{}:{round}", unit.stable_id),
                                &result.output,
                                None,
                                Some(1.0),
                            )?;
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
            let config = self.config.agent_for(stage_name)?;
            let executor = CommandAgent::new(config);
            let mut tasks = Vec::new();
            for document in documents.iter() {
                for unit in &document.units {
                    if unit.work_item_id.is_some() && unit.translation.is_some() {
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
            }
            if tasks.is_empty() {
                continue;
            }
            let results = executor.execute(&tasks)?;
            let by_id: HashMap<_, _> = results
                .iter()
                .map(|result| (result.task_id.as_str(), result))
                .collect();
            for document in documents.iter_mut() {
                for unit in &mut document.units {
                    let Some(result) = by_id.get(unit.stable_id.as_str()) else {
                        continue;
                    };
                    if !result.ok {
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
                        continue;
                    }
                    if blocking {
                        match validate_translation(&unit.markdown, &result.output) {
                            Ok(_) => {
                                unit.translation = Some(result.output.clone());
                                self.database.select_canonical_candidate(
                                    unit.database_id,
                                    language,
                                    &format!("revision:{}", unit.stable_id),
                                    &result.output,
                                    None,
                                    Some(1.0),
                                )?;
                            }
                            Err(validation) => {
                                findings.push(finding(
                                    &document.source_path,
                                    Some(unit.stable_id.clone()),
                                    DecisionCode::VerificationFailed.as_str(),
                                    format!("revision output failed deterministic validation: {validation:?}"),
                                ));
                            }
                        }
                    } else {
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
    ) -> Result<(Vec<String>, Vec<Finding>)> {
        let mut written = Vec::new();
        let mut findings = Vec::new();
        for document in documents {
            let missing: Vec<_> = document
                .units
                .iter()
                .filter(|unit| unit.translation.is_none())
                .collect();
            if !missing.is_empty() {
                findings.push(finding(
                    &document.source_path,
                    missing.first().map(|unit| unit.stable_id.clone()),
                    DecisionCode::VerificationFailed.as_str(),
                    format!("{} unit(s) have no verified translation", missing.len()),
                ));
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
            let canonical_id = self.database.upsert_canonical_file(CanonicalFileInput {
                repository_id,
                locale: language,
                path: &document.target_path,
                source_revision,
                content: &desired,
                content_hash: &desired_hash,
                materialized_hash: document.expected_materialized_hash.as_deref(),
                state: "candidate",
            })?;
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
            self.database.enqueue_materialization(
                work_item_id,
                &dedupe,
                &serde_json::to_string(&json!({
                    "repository_id": repository_id,
                    "locale": language,
                    "path": document.target_path,
                }))?,
            )?;
            let owner = format!("materialize:{}:{}", std::process::id(), run_id);
            let claimed = self.database.claim_outbox_key(
                OutboxKind::Materialization,
                &dedupe,
                &owner,
                Utc::now().timestamp_millis(),
                60_000,
            )?;
            if let Some(entry) = claimed {
                let operation = Materialization {
                    path: document.target_path.clone().into(),
                    expected_hash: document.expected_materialized_hash.clone(),
                    desired,
                };
                let result = materialize(&self.repo.path, &operation);
                match result {
                    Ok(MaterializationResult::Written { hash }) => {
                        self.database.set_canonical_file_state(
                            canonical_id,
                            "materialized",
                            Some(&hash),
                        )?;
                        written.push(document.target_path.clone());
                    }
                    Ok(MaterializationResult::AlreadyCurrent { hash }) => {
                        self.database.set_canonical_file_state(
                            canonical_id,
                            "materialized",
                            Some(&hash),
                        )?;
                    }
                    Ok(MaterializationResult::HumanEdit { actual_hash }) => {
                        self.database.set_canonical_file_state(
                            canonical_id,
                            "human_edit",
                            Some(&actual_hash),
                        )?;
                        findings.push(finding(
                            &document.target_path,
                            None,
                            DecisionCode::HumanEdit.as_str(),
                            "target changed outside fani and was not overwritten",
                        ));
                    }
                    Err(error) => {
                        self.database.retry_outbox(
                            OutboxKind::Materialization,
                            entry.id,
                            &owner,
                            &error.to_string(),
                            Utc::now().timestamp_millis() + 1_000,
                        )?;
                        return Err(error);
                    }
                }
                if !self
                    .database
                    .complete_outbox(OutboxKind::Materialization, entry.id, &owner)?
                {
                    bail!("materialization outbox lease was lost before completion");
                }
            }
        }
        Ok((written, findings))
    }

    fn reconcile_pull_request(&self, repository_id: i64, language: &str) -> Result<()> {
        if !self.repo.publish.github.enabled {
            return Ok(());
        }
        let branch = locale_branch(&self.repo.publish.branch, language)?;
        let Some(stored) =
            self.database
                .pull_request_for_branch(repository_id, "github", &branch)?
        else {
            return Ok(());
        };
        let Some(number) = stored.number else {
            return Ok(());
        };
        let pull = GhClient::new(&self.repo.path)
            .pull_request(&self.repo.publish.github.repository, &number.to_string())?;
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
            payload_json: &serde_json::to_string(&pull)?,
        })?;
        if state == "merged" {
            self.database
                .promote_merged_locale(repository_id, language, "github_merged")?;
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
        let owner = format!("publish:{}:{}", std::process::id(), run_id);
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
                files.push(json!({"path": path, "content_hash": canonical.content_hash}));
            }
            let payload = serde_json::to_string(&json!({
                "files": files,
                "language": language,
                "source_revision": outcome.source_revision,
            }))?;
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
            self.database.claim_outbox_key(
                OutboxKind::Publication,
                &dedupe,
                &owner,
                Utc::now().timestamp_millis(),
                180_000,
            )?
        };
        let Some(entry) = entry else {
            outcome.published.skipped =
                "no file changed and no publication recovery is pending".into();
            return Ok(());
        };
        let payload: serde_json::Value = serde_json::from_str(&entry.payload_json)?;
        let payload_language = payload["language"]
            .as_str()
            .ok_or_else(|| anyhow!("publication outbox is missing language"))?;
        if payload_language != language {
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
        let records = payload["files"]
            .as_array()
            .cloned()
            .ok_or_else(|| anyhow!("publication outbox is missing files"))?;
        outcome.published = match crate::gitout::publish(self.repo, language, &records) {
            Ok(published) => published,
            Err(error) => {
                self.database.retry_outbox(
                    OutboxKind::Publication,
                    entry.id,
                    &owner,
                    &error.to_string(),
                    Utc::now().timestamp_millis() + 5_000,
                )?;
                return Err(error);
            }
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
        let reconcile = (|| -> Result<()> {
            if self.repo.publish.github.enabled && outcome.published.pushed {
                let branch = locale_branch(&self.repo.publish.branch, language)?;
                let title = format!("i18n({language}): update translated documentation");
                let body = format!(
                    "Automated verified documentation translation from `{}`.",
                    outcome.source_revision
                );
                let reconciled =
                    GhClient::new(&self.repo.path).ensure_pull_request(EnsurePullRequest {
                        repository: &self.repo.publish.github.repository,
                        head: &branch,
                        base: &self.repo.publish.github.base,
                        title: &title,
                        body: &body,
                        draft: self.repo.publish.github.draft,
                        durable: None,
                    })?;
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
                    payload_json: &serde_json::to_string(&reconciled)?,
                })?;
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
        Ok(())
    }

    pub fn run_language(&self, language: &str) -> LanguageOutcome {
        let started = Instant::now();
        let mut outcome = LanguageOutcome::new(&self.repo.path, language);
        let result = (|| -> Result<()> {
            let source_revision = source::resolve_source_revision(self.repo)?;
            outcome.source_revision = source_revision.clone();
            let repository_id = self.repository_id()?;
            self.reconcile_pull_request(repository_id, language)?;
            let invocation = format!("sync:{repository_id}:{language}:{source_revision}");
            let run_id =
                self.database
                    .begin_run(repository_id, &invocation, self.config_path, "{}")?;
            outcome.run_id = run_id.clone();
            let (mut documents, conflicts, reused, scheduled) =
                self.prepare(repository_id, &run_id, language, &source_revision)?;
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
                outcome.agent_calls = self.dispatch(&run_id, language, &mut documents)?;
                if documents
                    .iter()
                    .flat_map(|document| &document.units)
                    .any(|unit| unit.translation.is_none() && unit.work_item_id.is_some())
                {
                    outcome.transitions.push("repairing".into());
                    outcome.repair_rounds =
                        self.repair(language, &mut documents, &mut outcome.agent_calls)?;
                }
            }
            outcome.transitions.push("reviewing".into());
            let mut review_findings =
                self.review(language, &mut documents, &mut outcome.agent_calls)?;
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
            let (written, mut findings) = self.assemble_and_materialize(
                repository_id,
                &run_id,
                language,
                &source_revision,
                &mut documents,
            )?;
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
            self.publish(
                repository_id,
                &run_id,
                language,
                &outcome.written.clone(),
                &mut outcome,
            )?;
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
        outcome
    }
}

pub fn adopt_human_edit(repo: &RepoConfig, database: &Database, language: &str) -> Result<usize> {
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
    let source_revision = source::resolve_source_revision(repo)?;
    let documents = source::discover(repo, &source_revision)?;
    let mut adopted = 0;
    for document in documents {
        let target = source::target_path(repo, language, &document.path)?
            .to_string_lossy()
            .into_owned();
        let Some(canonical) = database.canonical_file(repository_id, language, &target)? else {
            continue;
        };
        let target_disk_path = safe_path(&repo.path, Path::new(&target))?;
        let bytes = fs::read(&target_disk_path)
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
        let document_id = database.upsert_document(
            repository_id,
            &document.path,
            Some(&source_revision),
            &document.content_hash,
            "{}",
        )?;
        let history = database.unit_history(document_id, language)?;
        let matched = match_units(&previous_units(&history), &source_units);
        for (ordinal, ((source_unit, target_unit), matched)) in source_units
            .iter()
            .zip(&target_units)
            .zip(matched)
            .enumerate()
        {
            if matched.kind == MatchKind::Ambiguous {
                bail!("human target {target} cannot be mapped to stable source units");
            }
            let stable_id = matched.stable_id.unwrap_or_else(|| {
                format!(
                    "unit-{}",
                    &hash(&[
                        document.path.as_bytes(),
                        source_unit.id.as_bytes(),
                        ordinal.to_string().as_bytes()
                    ])[..24]
                )
            });
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
            state: "adopted",
        })?;
        database.set_canonical_file_state(canonical.id, "adopted", Some(&adopted_hash))?;
        adopted += 1;
    }
    Ok(adopted)
}

pub fn discard_human_edit(repo: &RepoConfig, database: &Database, language: &str) -> Result<usize> {
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
    let source_revision = source::resolve_source_revision(repo)?;
    let documents = source::discover(repo, &source_revision)?;
    let mut discarded = 0;
    for document in documents {
        let target = source::target_path(repo, language, &document.path)?
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
        let result = restore(&repo.path, &operation)?;
        let hash = match result {
            MaterializationResult::Written { hash }
            | MaterializationResult::AlreadyCurrent { hash } => hash,
            MaterializationResult::HumanEdit { .. } => {
                return Err(anyhow!("cannot discard {target}"));
            }
        };
        database.set_canonical_file_state(canonical.id, "materialized", Some(&hash))?;
        discarded += 1;
    }
    Ok(discarded)
}
