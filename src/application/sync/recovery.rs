use crate::domain::document::{TranslatableUnit, validate_provenance};
use crate::domain::prompts;
use anyhow::Result;

use crate::application::ports::PipelineStore;
use crate::application::sync::context::compatible_review_request;

pub(crate) struct RecoveryService<'a> {
    pub database: &'a dyn PipelineStore,
}
impl RecoveryService<'_> {
    pub(crate) fn recovered_attempt(
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
}
