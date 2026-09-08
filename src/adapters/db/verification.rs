use crate::adapters::db::{Database, effects, enqueue_document_work_item_in_transaction, now_ms};
use crate::application::contracts::{DocumentWorkResult, encode};
use crate::application::ports::{
    BeginVerificationInput, CompleteVerificationInput, VerificationRequest, VerificationStore,
};
use anyhow::{Result, bail};
use rusqlite::{TransactionBehavior, params};

impl VerificationStore for Database {
    fn begin_verification(&self, input: BeginVerificationInput<'_>) -> Result<i64> {
        let (kind, payload) = match &input.request {
            VerificationRequest::Assembly(request) => ("assembly", encode(request)?),
            VerificationRequest::ProjectCheck(request) => ("project_check", encode(request)?),
        };
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let work = enqueue_document_work_item_in_transaction(
            &tx,
            input.run_id,
            input.document_id,
            input.locale,
            kind,
            0,
            &payload,
        )?;
        tx.commit()?;
        Ok(work)
    }

    fn complete_verification(&self, input: CompleteVerificationInput<'_>) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let kind: String = tx.query_row(
            "SELECT kind FROM work_items WHERE id=?1 AND document_id IS NOT NULL",
            [input.work_item_id],
            |row| row.get(0),
        )?;
        let expected_kind = match input.result {
            DocumentWorkResult::Assembled { .. } | DocumentWorkResult::Rejected { .. } => {
                "assembly"
            }
            DocumentWorkResult::Checked { .. } => "project_check",
        };
        if kind != expected_kind {
            bail!("verification result does not match work kind");
        }
        for finding in input.findings {
            let details = match &finding.details {
                Some(details) => encode(details)?,
                None => "{}".into(),
            };
            tx.execute(
                "INSERT INTO findings(work_item_id,attempt_id,fingerprint,severity,code,message,details_json,created_at) VALUES (?1,NULL,?2,'error',?3,?4,?5,?6) ON CONFLICT(work_item_id,fingerprint) DO UPDATE SET attempt_id=NULL,severity='error',code=excluded.code,message=excluded.message,details_json=excluded.details_json,resolved_at=NULL",
                params![input.work_item_id,finding.key,finding.code,finding.message,details,now_ms()],
            )?;
        }
        let succeeded = input.findings.is_empty()
            && !matches!(input.result, DocumentWorkResult::Rejected { .. });
        effects::finish_document_work_in_transaction(
            &tx,
            input.work_item_id,
            succeeded,
            &encode(&input.result)?,
        )?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::adapters::db::Database;
    use crate::application::contracts::{CheckRequest, DocumentWorkResult};
    use crate::application::ports::{
        BeginVerificationInput, CompleteVerificationInput, VerificationFinding,
        VerificationRequest, VerificationStore,
    };
    use std::path::Path;

    #[test]
    fn rejected_work_update_rolls_back_all_verification_findings() {
        let temp = tempfile::tempdir().unwrap();
        let database = Database::open(temp.path().join("fani.db")).unwrap();
        let repository = database
            .upsert_repository("repo", temp.path(), None, None)
            .unwrap();
        let document = database
            .upsert_document(repository, "guide.md", Some("revision"), "hash", "{}")
            .unwrap();
        let run = database
            .begin_run(
                repository,
                "invocation",
                Path::new("config"),
                "{}",
                &crate::domain::prompts::policy_fingerprint(),
            )
            .unwrap();
        let work = database
            .begin_verification(BeginVerificationInput {
                run_id: &run,
                document_id: document,
                locale: "fr",
                request: VerificationRequest::ProjectCheck(CheckRequest {
                    source_revision: "revision",
                    manifest: &[],
                    manifest_hash: "hash",
                }),
            })
            .unwrap();
        let conn = database.connect().unwrap();
        conn.execute_batch("CREATE TRIGGER reject_verification BEFORE UPDATE ON work_items BEGIN SELECT RAISE(ABORT,'reject verification'); END;").unwrap();
        let findings = [VerificationFinding {
            key: "check",
            code: "DOC-CHECK-FAILED",
            message: "failed",
            details: None,
        }];
        assert!(
            database
                .complete_verification(CompleteVerificationInput {
                    work_item_id: work,
                    result: DocumentWorkResult::Checked {
                        manifest_hash: "hash"
                    },
                    findings: &findings
                })
                .is_err()
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM findings WHERE work_item_id=?1",
                [work],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        conn.execute_batch("DROP TRIGGER reject_verification")
            .unwrap();
        database
            .complete_verification(CompleteVerificationInput {
                work_item_id: work,
                result: DocumentWorkResult::Checked {
                    manifest_hash: "hash",
                },
                findings: &findings,
            })
            .unwrap();
        assert_eq!(
            conn.query_row("SELECT status FROM work_items WHERE id=?1", [work], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
            "failed"
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM findings WHERE work_item_id=?1",
                [work],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }
}
