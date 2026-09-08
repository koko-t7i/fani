use crate::application::contracts::{StoredReviewRequest, decode};
use crate::domain::document::{
    DocumentFormat, ParsedDocument, TranslatableUnit, validate_provenance,
};
use crate::domain::matching::PreviousUnit;
use crate::domain::model::{DocumentStatistics, Finding, FindingSeverity};
use crate::domain::prompts;
use anyhow::Result;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

use crate::application::contracts::DocumentIdentity;
use crate::application::ports::PlanningStore;

pub(crate) const REPAIR_CONTEXT_VERSION: &str = "v6";
pub(crate) const LEADING_STRONG_SEPARATOR_VERSION: &str = "fani-leading-strong-separator-v1";

pub(crate) fn hash(parts: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    format!("{:x}", digest.finalize())
}

pub(crate) fn content_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn reusable_candidate(
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

pub(crate) fn compatible_review_request(
    receipt: &crate::application::ports::RecoveredAttempt,
    stage: &str,
    path: &str,
    unit: &TranslatableUnit,
    translated: &str,
) -> bool {
    let Ok(request) = decode::<StoredReviewRequest>(&receipt.request_json) else {
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

pub(crate) fn stable_unit_id(path: &str, unit: &TranslatableUnit, ordinal: usize) -> String {
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

pub(crate) fn stable_unit_hints<S: PlanningStore + ?Sized>(
    database: &S,
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

pub(crate) fn document_identity(
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

pub(crate) fn compatible_document_identity(
    stored: &DocumentIdentity,
    current: &DocumentIdentity,
) -> bool {
    stored.request_identity == stored.request_hash()
        && stored.mapping_hash() == current.mapping_hash()
}

pub(crate) fn count_formats(
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

pub(crate) fn finding(
    path: &str,
    unit_id: Option<String>,
    code: &str,
    message: impl Into<String>,
) -> Finding {
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

pub(crate) fn previous_units(
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
