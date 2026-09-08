use crate::application::contracts::AttemptStatus;
use crate::application::ports::{AgentExecutor, AttemptCandidateInput, AttemptInput};
use crate::application::settings::RepoConfig;
use crate::domain::document::validate_unit;
use crate::domain::model::{
    AgentResult, AgentStage, AgentTask, DecisionCode, Finding, FindingSeverity,
    TranslationProvenance,
};
use crate::domain::prompts;
use anyhow::{Result, anyhow};
use std::collections::HashMap;

use crate::application::ports::PipelineStore;
use crate::application::sync::context::finding;
use crate::application::sync::recovery::RecoveryService;
use crate::application::sync::types::PlannedDocument;

pub(crate) struct ReviewService<'a> {
    pub repo: &'a RepoConfig,
    pub database: &'a dyn PipelineStore,
    pub agents: &'a dyn AgentExecutor,
    pub failpoint: fn(&str),
}
impl ReviewService<'_> {
    pub(crate) fn review(
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
                    if let Some(recovered) = (RecoveryService {
                        database: self.database,
                    })
                    .recovered_attempt(
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
                                format!(
                                    "durable blocking revision attempt ended as {}",
                                    status.as_str()
                                ),
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
                            status: AttemptStatus::Succeeded,
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
                                            status: AttemptStatus::Succeeded,
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
                                    status: AttemptStatus::Failed,
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
                            status: AttemptStatus::Succeeded,
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
}
