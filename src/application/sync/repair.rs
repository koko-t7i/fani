use crate::application::contracts::AttemptStatus;
use crate::application::contracts::{
    DeterministicRepairRequest, DeterministicRepairResponse, encode,
};
use crate::application::ports::{AgentExecutor, AttemptCandidateInput, AttemptInput, FindingInput};
use crate::application::settings::RepoConfig;
use crate::domain::document::{repair_leading_strong_separator, validate_unit};
use crate::domain::model::{
    AgentResult, AgentStage, AgentTask, DecisionCode, TranslationProvenance,
};
use crate::domain::prompts;
use anyhow::{Result, anyhow};
use std::collections::HashMap;

use crate::application::ports::PipelineStore;
use crate::application::sync::context::{
    LEADING_STRONG_SEPARATOR_VERSION, REPAIR_CONTEXT_VERSION, finding, hash,
};
use crate::application::sync::recovery::RecoveryService;
use crate::application::sync::types::PlannedDocument;

pub(crate) struct RepairService<'a> {
    pub repo: &'a RepoConfig,
    pub database: &'a dyn PipelineStore,
    pub agents: &'a dyn AgentExecutor,
    pub failpoint: fn(&str),
}
impl RepairService<'_> {
    pub(crate) fn repair(
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
                if let Some(recovered) = (RecoveryService {
                    database: self.database,
                })
                .recovered_attempt(
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
                let request_json = encode(&DeterministicRepairRequest {
                    schema: "fani.deterministic.repair.request.v1",
                    task_id: &unit.stable_id,
                    operation: LEADING_STRONG_SEPARATOR_VERSION,
                    source_attempt_id: failed.attempt_id,
                    source_output_hash: &rejected_hash,
                })?;
                let response_json = encode(&DeterministicRepairResponse {
                    schema: "fani.agent.response.v1",
                    task_id: &unit.stable_id,
                    output: &repaired,
                })?;
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
                            status: AttemptStatus::Succeeded,
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
                                provenance: TranslationProvenance::RepairedAi,
                            })?;
                        (self.failpoint)("repair_candidate_committed");
                        unit.translation = Some(result.output.clone());
                    } else {
                        let status = if result.code.as_deref()
                            == Some(DecisionCode::AgentTimeout.as_str())
                        {
                            AttemptStatus::TimedOut
                        } else {
                            AttemptStatus::Failed
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
}
