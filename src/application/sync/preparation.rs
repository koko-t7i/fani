use crate::application::ports::SourceReader;
use crate::application::settings::RepoConfig;
use crate::domain::document::unit_metadata;
use crate::domain::model::{DocumentStatistics, Finding};
use anyhow::Result;
use serde_json::json;

use crate::application::contracts::PipelineRequest;
use crate::application::ports::{PreparationStore, PreparedDocumentInput, PreparedUnitInput};
use crate::application::sync::context::{count_formats, hash};
use crate::application::sync::planning::{self, Planner};
use crate::application::sync::types::{PlannedDocument, PlannedUnit};
pub(crate) struct PreparationService<'a> {
    pub repo: &'a RepoConfig,
    pub database: &'a dyn PreparationStore,
    pub git: &'a dyn SourceReader,
    pub planner: Planner<'a>,
}
impl PreparationService<'_> {
    pub(crate) fn prepare(
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
        let documents = self.git.discover(self.repo, source_revision)?;
        count_formats(statistics, &documents);
        for document in documents {
            let Some(plan) = self.planner.inspect_document(
                Some(repository_id),
                language,
                run_id,
                &document,
                statistics,
                &mut conflicts,
            )?
            else {
                continue;
            };
            let planning::DocumentPlan {
                parsed,
                identity,
                units: decisions,
                canonical,
                target_path,
                ..
            } = plan;
            let inputs = decisions
                .iter()
                .enumerate()
                .map(|(ordinal, decision)| {
                    let enqueue = if decision.candidate.is_some() {
                        reused += 1;
                        self.repo.quality.revision || self.repo.quality.proofread
                    } else if scheduled < self.repo.max_tasks {
                        scheduled += 1;
                        true
                    } else {
                        false
                    };
                    PreparedUnitInput {
                        unit_key: decision.stable_id.clone(),
                        ordinal: ordinal as i64,
                        source_text: decision.unit.source.clone(),
                        source_hash: hash(&[decision.unit.source.as_bytes()]),
                        context_json: unit_metadata(&decision.unit, &document.path),
                        context_key: decision.unit.memory_context_key(&document.path),
                        candidate: decision.candidate.clone(),
                        enqueue: enqueue.then(|| PipelineRequest {
                            source_revision: source_revision.into(),
                            path: document.path.clone(),
                            unit: decision.stable_id.clone(),
                            document_identity: identity.clone(),
                        }),
                    }
                })
                .collect::<Vec<_>>();
            let receipt = self.database.prepare_document(PreparedDocumentInput {
                repository_id,
                run_id,
                locale: language,
                path: &document.path,
                source_revision,
                content_hash: &document.content_hash,
                metadata_json: &serde_json::to_string(
                    &json!({"format": parsed.format, "contract": parsed.contract}),
                )?,
                units: &inputs,
            })?;
            let document_id = receipt.document_id;
            let units = decisions
                .into_iter()
                .zip(receipt.units)
                .map(|(decision, receipt)| PlannedUnit {
                    unit: decision.unit,
                    database_id: receipt.unit_id,
                    stable_id: decision.stable_id,
                    translation: decision.candidate.map(|candidate| candidate.text),
                    previous_source: decision.previous_source,
                    previous_translation: decision.previous_translation,
                    work_item_id: receipt.work_item_id,
                })
                .collect();
            planned_documents.push(PlannedDocument {
                identity,
                document_id,
                source_path: document.path,
                target_path,
                parsed,
                units,
                expected_materialized_hash: canonical.and_then(|value| value.materialized_hash),
            });
        }
        Ok((planned_documents, conflicts, reused, scheduled))
    }
}
