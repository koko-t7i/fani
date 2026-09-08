use crate::application::command::OutputReporter;
use crate::application::contracts::AttemptStatus;
use crate::application::ports::{AgentExecutor, AttemptCandidateInput, AttemptInput, FindingInput};
use crate::domain::document::validate_unit;
use crate::domain::model::{
    AgentResult, AgentStage, AgentTask, DecisionCode, TranslationProvenance,
};
use crate::domain::prompts;
use anyhow::{Result, anyhow};
use std::collections::HashMap;

use crate::application::ports::PipelineStore;
use crate::application::sync::context::hash;
use crate::application::sync::recovery::RecoveryService;
use crate::application::sync::types::PlannedDocument;

pub(crate) struct TranslationService<'a> {
    pub database: &'a dyn PipelineStore,
    pub agents: &'a dyn AgentExecutor,
    pub failpoint: fn(&str),
    pub output: &'a dyn OutputReporter,
    pub quiet: bool,
}
impl TranslationService<'_> {
    fn log(&self, message: &str) {
        if !self.quiet {
            self.output.stderr(message);
        }
    }
    pub(crate) fn dispatch(
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
                if let Some(recovered) = (RecoveryService {
                    database: self.database,
                })
                .recovered_attempt(
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
                                        status: AttemptStatus::Succeeded,
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
                                status: AttemptStatus::Failed,
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
                            AttemptStatus::TimedOut
                        } else {
                            AttemptStatus::Failed
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
}

#[cfg(test)]
mod tests {
    use crate::application::command::OutputReporter;
    use crate::application::contracts::AttemptStatus;
    use crate::application::ports::{
        AgentExecution, AgentExecutor, AttemptCandidateInput, AttemptInput, AttemptReceipt,
        FailedAttemptContext, FindingInput, PipelineStore, RecoveredAttempt,
    };
    use crate::application::sync::context::document_identity;
    use crate::application::sync::translation::TranslationService;
    use crate::application::sync::types::{PlannedDocument, PlannedUnit};
    use crate::domain::{
        document::{DocumentFormat, UnitProvenance, unit_metadata},
        model::{AgentTask, SourceDocument},
        prompts,
    };
    use anyhow::Result;
    use std::cell::Cell;

    struct Receipts {
        recovered: Option<RecoveredAttempt>,
        selected: Cell<usize>,
    }
    impl PipelineStore for Receipts {
        fn attempt_status(&self, _: i64, _: &str) -> Result<Option<AttemptStatus>> {
            Ok(Some(AttemptStatus::Failed))
        }
        fn failed_attempt_context(&self, _: i64) -> Result<Option<FailedAttemptContext>> {
            panic!("translation cannot repair")
        }
        fn record_attempt(&self, _: AttemptInput<'_>) -> Result<AttemptReceipt> {
            panic!("no new attempt")
        }
        fn record_attempt_candidate(&self, _: AttemptCandidateInput<'_>) -> Result<AttemptReceipt> {
            panic!("no new attempt")
        }
        fn record_finding(&self, _: FindingInput<'_>) -> Result<i64> {
            panic!("no new finding")
        }
        fn retire_attempt(&self, _: i64) -> Result<()> {
            panic!("receipt must remain compatible")
        }
        fn select_canonical_candidate(
            &self,
            _: i64,
            _: &str,
            _: &str,
            text: &str,
            attempt: Option<i64>,
            _: Option<f64>,
        ) -> Result<i64> {
            assert_eq!(text, "Bonjour world.");
            assert_eq!(attempt, Some(7));
            self.selected.set(self.selected.get() + 1);
            Ok(1)
        }
        fn successful_attempt(&self, _: i64, _: &str) -> Result<Option<RecoveredAttempt>> {
            Ok(self.recovered.clone())
        }
    }
    struct NoAgent;
    impl AgentExecutor for NoAgent {
        fn execute(&self, _: &[AgentTask]) -> Result<AgentExecution> {
            panic!("durable attempts must not be redispatched")
        }
    }
    struct Quiet;
    impl OutputReporter for Quiet {
        fn stdout(&self, _: &str) {}
        fn stderr(&self, _: &str) {}
    }

    #[test]
    fn recovered_translation_and_failed_attempt_do_not_redispatch_agents() {
        let source = SourceDocument {
            source_format: DocumentFormat::Markdown,
            source_set_id: "markdown".into(),
            mapping_identity: "mapping".into(),
            mapped_relpath: "guide.md".into(),
            target_pattern: "i18n/{lang}/{relpath}".into(),
            message_syntax: None,
            repository: "fixture".into(),
            source_revision: "revision".into(),
            path: "guide.md".into(),
            bytes: b"Hello world.\n".to_vec(),
            content_hash: "hash".into(),
        };
        let parsed = source.parse().unwrap();
        let unit = parsed.units[0].clone();
        let receipt = RecoveredAttempt {
            id: 7,
            dedupe_key: "unit:translate".into(),
            request_json: "{}".into(),
            output: "Bonjour world.".into(),
            provenance: Some(UnitProvenance {
                document_path: source.path.clone(),
                source: unit.source.clone(),
                source_revision: "revision".into(),
                context_json: unit_metadata(&unit, &source.path),
                policy_fingerprint: prompts::policy_fingerprint(),
            }),
        };
        for recovered in [None, Some(receipt)] {
            let expected_reuse = recovered.is_some();
            let store = Receipts {
                recovered,
                selected: Cell::new(0),
            };
            let mut documents = vec![PlannedDocument {
                identity: document_identity(&source, "fr", &parsed),
                document_id: 1,
                source_path: source.path.clone(),
                target_path: "i18n/fr/guide.md".into(),
                parsed: parsed.clone(),
                expected_materialized_hash: None,
                units: vec![PlannedUnit {
                    unit: unit.clone(),
                    database_id: 1,
                    stable_id: "unit".into(),
                    translation: None,
                    previous_source: None,
                    previous_translation: None,
                    work_item_id: Some(1),
                }],
            }];
            let service = TranslationService {
                database: &store,
                agents: &NoAgent,
                failpoint: |_| {},
                output: &Quiet,
                quiet: true,
            };
            assert!(
                service
                    .dispatch("run", "fr", &mut documents)
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(documents[0].units[0].translation.is_some(), expected_reuse);
            assert_eq!(store.selected.get(), usize::from(expected_reuse));
        }
    }
}
