use crate::application::contracts::{
    AssemblyRequest, CheckFailure, CheckRequest, DocumentWorkResult,
};
use anyhow::Result;

pub enum VerificationRequest<'a> {
    Assembly(AssemblyRequest<'a>),
    ProjectCheck(CheckRequest<'a>),
}

pub struct BeginVerificationInput<'a> {
    pub run_id: &'a str,
    pub document_id: i64,
    pub locale: &'a str,
    pub request: VerificationRequest<'a>,
}

pub struct VerificationFinding<'a> {
    pub key: &'a str,
    pub code: &'a str,
    pub message: &'a str,
    pub details: Option<CheckFailure<'a>>,
}

pub struct CompleteVerificationInput<'a> {
    pub work_item_id: i64,
    pub result: DocumentWorkResult<'a>,
    pub findings: &'a [VerificationFinding<'a>],
}

/// Durable verification receipts; no translation, publication, or file capabilities.
pub trait VerificationStore {
    fn begin_verification(&self, input: BeginVerificationInput<'_>) -> Result<i64>;
    fn complete_verification(&self, input: CompleteVerificationInput<'_>) -> Result<()>;
}
