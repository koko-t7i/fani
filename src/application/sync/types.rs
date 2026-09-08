use crate::application::contracts::DocumentIdentity;
use crate::domain::document::{ParsedDocument, TranslatableUnit};

#[derive(Clone)]
pub(crate) struct PlannedUnit {
    pub(crate) unit: TranslatableUnit,
    pub(crate) database_id: i64,
    pub(crate) stable_id: String,
    pub(crate) translation: Option<String>,
    pub(crate) previous_source: Option<String>,
    pub(crate) previous_translation: Option<String>,
    pub(crate) work_item_id: Option<i64>,
}

#[derive(Clone)]
pub(crate) struct PlannedDocument {
    pub(crate) identity: DocumentIdentity,
    pub(crate) document_id: i64,
    pub(crate) source_path: String,
    pub(crate) target_path: String,
    pub(crate) parsed: ParsedDocument,
    pub(crate) units: Vec<PlannedUnit>,
    pub(crate) expected_materialized_hash: Option<String>,
}
