//! Read-only source, reuse, and target decisions shared by preview and execution.
use super::*;
use crate::application::ports::{CanonicalFile, TranslationCandidate};
use crate::domain::model::SourceDocument;

pub(super) struct UnitPlan {
    pub unit: TranslatableUnit,
    pub stable_id: String,
    pub candidate: Option<TranslationCandidate>,
    pub previous_source: Option<String>,
    pub previous_translation: Option<String>,
}

pub(super) struct DocumentPlan {
    pub parsed: ParsedDocument,
    pub identity: DocumentIdentity,
    pub units: Vec<UnitPlan>,
    pub canonical: Option<CanonicalFile>,
    pub target_path: String,
    pub assembled: Option<String>,
}

impl Orchestrator<'_> {
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

    pub(super) fn inspect_document(
        &self,
        repository_id: Option<i64>,
        language: &str,
        run_or_invocation: &str,
        document: &SourceDocument,
        statistics: &mut DocumentStatistics,
        conflicts: &mut Vec<Finding>,
    ) -> Result<Option<DocumentPlan>> {
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
                return Ok(None);
            }
        };
        if parsed.units.is_empty() {
            statistics.pass_through_documents += 1;
        }
        let stable_ids = parsed
            .units
            .iter()
            .enumerate()
            .map(|(ordinal, unit)| stable_unit_id(&document.path, unit, ordinal))
            .collect::<Vec<_>>();
        let (stable_hints, history, candidates) = if let Some(repository_id) = repository_id {
            let hints = stable_unit_hints(
                self.database,
                repository_id,
                &document.path,
                &document.content_hash,
                &stable_ids,
            )?;
            let (history, candidates) =
                match self.database.document_id(repository_id, &document.path)? {
                    Some(id) => (
                        self.database.unit_history(id, language)?,
                        self.database.translation_candidates(id, language)?,
                    ),
                    None => (Vec::new(), Vec::new()),
                };
            (hints, history, candidates)
        } else {
            (
                vec![String::new(); stable_ids.len()],
                Vec::new(),
                Vec::new(),
            )
        };
        let matched = match_units_with_stable_ids(
            &previous_units(&history, &candidates, &document.path),
            &parsed.units,
            &stable_hints,
        );
        if matched
            .iter()
            .any(|matched| matched.kind == MatchKind::Ambiguous)
        {
            conflicts.extend(
                parsed
                    .units
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
            return Ok(None);
        }
        let mut units = Vec::new();
        for (ordinal, (unit, matched)) in parsed.units.iter().zip(matched).enumerate() {
            let stable_id = matched
                .stable_id
                .clone()
                .unwrap_or_else(|| stable_ids[ordinal].clone());
            let database_id = history
                .iter()
                .find(|row| row.unit_key == stable_id)
                .map(|row| row.id);
            let candidate = candidates.iter().find(|candidate| {
                reusable_candidate(
                    &document.path,
                    unit,
                    candidate,
                    Some(candidate.unit_id) == database_id
                        && (candidate.run_id.as_deref() == Some(run_or_invocation)
                            || candidate.invocation_key.as_deref() == Some(run_or_invocation)
                            || !stable_hints[ordinal].is_empty()),
                )
            });
            let candidate = match candidate {
                Some(candidate)
                    if self.reviews_compatible(
                        &document.path,
                        unit,
                        candidate,
                        language,
                        run_or_invocation,
                    )? =>
                {
                    Some(candidate.clone())
                }
                _ => None,
            };
            units.push(UnitPlan {
                unit: unit.clone(),
                stable_id,
                candidate,
                previous_source: matched.previous_source,
                previous_translation: matched.previous_translation,
            });
        }
        let translations = units
            .iter()
            .map(|unit| {
                unit.candidate.as_ref().map(|candidate| UnitTranslation {
                    id: unit.unit.id.clone(),
                    text: candidate.text.clone(),
                })
            })
            .collect::<Option<Vec<_>>>();
        let assembled =
            translations.and_then(|translations| assemble_document(&parsed, &translations).ok());
        let target = document.target_path(language);
        let target_path = target.to_string_lossy().into_owned();
        let canonical = match repository_id {
            Some(id) => self.database.canonical_file(id, language, &target_path)?,
            None => None,
        };
        let actual = self.materializer.read(&self.repo.path, &target)?;
        let actual_hash = actual.as_ref().map(|bytes| content_hash(bytes));
        let human_edit = match &canonical {
            Some(canonical) => {
                actual_hash.as_deref() != Some(canonical.content_hash.as_str())
                    && actual_hash.as_deref() != canonical.materialized_hash.as_deref()
            }
            // An existing untracked target is safe only when it already equals the
            // fully verified desired bytes, as enforced again by the materializer.
            None => actual.as_ref().is_some_and(|bytes| {
                assembled
                    .as_ref()
                    .is_none_or(|desired| bytes.as_slice() != desired.as_bytes())
            }),
        };
        if human_edit {
            conflicts.push(finding(&target_path, None, DecisionCode::HumanEdit.as_str(),
                "target has human content or a materialized target was removed; use adopt or discard for canonical targets, or move the untracked target"));
        }
        Ok(Some(DocumentPlan {
            identity: document_identity(document, language, &parsed),
            parsed,
            units,
            canonical,
            target_path,
            assembled,
        }))
    }
}
