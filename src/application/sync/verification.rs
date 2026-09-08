use crate::application::contracts::AssemblyRequest;
use crate::application::contracts::{
    CheckFailure, CheckManifestFile, CheckRequest, DocumentWorkResult, manifest_hash,
};
use crate::application::ports::{DocumentationChecker, PublicationFile};
use crate::application::settings::RepoConfig;
use crate::domain::document::{UnitTranslation, assemble_document};
use crate::domain::model::Finding;
use crate::domain::model::{DecisionCode, FindingSeverity};
use anyhow::{Result, anyhow};

use crate::application::ports::{
    BeginVerificationInput, CompleteVerificationInput, VerificationFinding, VerificationRequest,
    VerificationStore,
};
use crate::application::sync::context::{content_hash, finding};
use crate::application::sync::types::PlannedDocument;
pub(crate) struct VerificationService<'a> {
    pub repo: &'a RepoConfig,
    pub database: &'a dyn VerificationStore,
    pub documentation: &'a dyn DocumentationChecker,
}
impl VerificationService<'_> {
    pub(crate) fn verify(
        &self,
        run_id: &str,
        language: &str,
        source_revision: &str,
        documents: &[PlannedDocument],
    ) -> Result<VerificationOutcome> {
        let mut candidates = Vec::new();
        let mut findings = Vec::new();
        for document in documents.iter() {
            let missing: Vec<_> = document
                .units
                .iter()
                .filter(|unit| unit.translation.is_none())
                .collect();
            if !missing.is_empty() {
                let unresolved_scheduled = missing
                    .iter()
                    .copied()
                    .filter(|unit| unit.work_item_id.is_some())
                    .collect::<Vec<_>>();
                if !unresolved_scheduled.is_empty() {
                    findings.push(finding(
                        &document.source_path,
                        unresolved_scheduled
                            .first()
                            .map(|unit| unit.stable_id.clone()),
                        DecisionCode::VerificationFailed.as_str(),
                        format!(
                            "{} scheduled unit(s) have no verified translation",
                            unresolved_scheduled.len()
                        ),
                    ));
                }
                continue;
            }
            let translations: Vec<_> = document
                .units
                .iter()
                .map(|unit| UnitTranslation {
                    id: unit.unit.id.clone(),
                    text: unit.translation.clone().expect("checked translation"),
                })
                .collect();
            let work_item_id = self.database.begin_verification(BeginVerificationInput {
                run_id,
                document_id: document.document_id,
                locale: language,
                request: VerificationRequest::Assembly(AssemblyRequest {
                    source_revision,
                    path: &document.source_path,
                    target_path: &document.target_path,
                    contract: &document.parsed.contract,
                    document_identity: &document.identity,
                }),
            })?;
            match assemble_document(&document.parsed, &translations) {
                Ok(assembled) => {
                    self.database
                        .complete_verification(CompleteVerificationInput {
                            work_item_id,
                            result: DocumentWorkResult::Assembled {
                                content_hash: &content_hash(assembled.as_bytes()),
                            },
                            findings: &[],
                        })?;
                    candidates.push(PublicationFile {
                        path: document.target_path.clone(),
                        content: assembled.into_bytes(),
                    });
                }
                Err(_) => {
                    let message = "document assembly or deterministic verification failed";
                    self.database
                        .complete_verification(CompleteVerificationInput {
                            work_item_id,
                            result: DocumentWorkResult::Rejected {
                                code: "DOCUMENT-VERIFY",
                            },
                            findings: &[VerificationFinding {
                                key: "DOCUMENT-VERIFY",
                                code: "DOCUMENT-VERIFY",
                                message,
                                details: None,
                            }],
                        })?;
                    findings.push(finding(
                        &document.source_path,
                        None,
                        "DOCUMENT-VERIFY",
                        message,
                    ));
                }
            }
        }
        if findings
            .iter()
            .any(|item| item.severity == FindingSeverity::Error)
        {
            return Ok(VerificationOutcome {
                batch: None,
                verified_documents: candidates.len(),
                findings,
            });
        }
        findings.extend(self.check_documentation(
            run_id,
            language,
            source_revision,
            documents,
            &candidates,
        )?);
        if findings
            .iter()
            .any(|item| item.severity == FindingSeverity::Error)
        {
            return Ok(VerificationOutcome {
                batch: None,
                verified_documents: candidates.len(),
                findings,
            });
        }
        let verified_documents = candidates.len();
        let verified = candidates
            .into_iter()
            .map(|file| {
                let document = documents
                    .iter()
                    .find(|document| document.target_path == file.path)
                    .expect("assembled document identity")
                    .clone();
                VerifiedDocument { document, file }
            })
            .collect();
        Ok(VerificationOutcome {
            batch: Some(VerifiedBatch {
                documents: verified,
            }),
            verified_documents,
            findings,
        })
    }

    pub(crate) fn check_documentation(
        &self,
        run_id: &str,
        language: &str,
        source_revision: &str,
        documents: &[PlannedDocument],
        candidates: &[PublicationFile],
    ) -> Result<Vec<Finding>> {
        if self.repo.documentation.commands.is_empty() || candidates.is_empty() {
            return Ok(Vec::new());
        }
        let document = documents
            .iter()
            .find(|document| document.target_path == candidates[0].path)
            .ok_or_else(|| anyhow!("project check candidate has no document identity"))?;
        let manifest = candidates
            .iter()
            .map(|candidate| CheckManifestFile {
                path: &candidate.path,
                content_hash: content_hash(&candidate.content),
            })
            .collect::<Vec<_>>();
        let manifest_hash = manifest_hash(&manifest)?;
        let work_item_id = self.database.begin_verification(BeginVerificationInput {
            run_id,
            document_id: document.document_id,
            locale: language,
            request: VerificationRequest::ProjectCheck(CheckRequest {
                source_revision,
                manifest: &manifest,
                manifest_hash: &manifest_hash,
            }),
        })?;
        let result = self
            .documentation
            .check(self.repo, source_revision, candidates)?;
        let findings = result
            .failures
            .iter()
            .map(|(index, failure)| {
                let code = if failure.timed_out {
                    "DOC-CHECK-TIMEOUT"
                } else {
                    "DOC-CHECK-FAILED"
                };
                finding(
                    ".",
                    None,
                    code,
                    format!("documentation command {index} {}", failure.message),
                )
            })
            .collect::<Vec<_>>();
        let keys = result
            .failures
            .iter()
            .zip(&findings)
            .map(|((index, _), finding)| format!("{}:{index}", finding.code))
            .collect::<Vec<_>>();
        let persisted = result
            .failures
            .iter()
            .zip(&findings)
            .zip(&keys)
            .map(|(((index, failure), finding), key)| VerificationFinding {
                key,
                code: &finding.code,
                message: &finding.message,
                details: Some(CheckFailure {
                    argv: &self.repo.documentation.commands[*index],
                    command_index: *index,
                    timed_out: failure.timed_out,
                }),
            })
            .collect::<Vec<_>>();
        self.database
            .complete_verification(CompleteVerificationInput {
                work_item_id,
                result: DocumentWorkResult::Checked {
                    manifest_hash: &manifest_hash,
                },
                findings: &persisted,
            })?;
        Ok(findings)
    }
}

/// Only verification can construct snapshots admitted to materialization.
pub(crate) struct VerifiedBatch {
    documents: Vec<VerifiedDocument>,
}
pub(crate) struct VerifiedDocument {
    document: PlannedDocument,
    file: PublicationFile,
}
impl VerifiedDocument {
    pub fn document(&self) -> &PlannedDocument {
        &self.document
    }
    pub fn file(&self) -> &PublicationFile {
        &self.file
    }
}
impl VerifiedBatch {
    pub fn documents(&self) -> &[VerifiedDocument] {
        &self.documents
    }
}
pub(crate) struct VerificationOutcome {
    pub batch: Option<VerifiedBatch>,
    pub verified_documents: usize,
    pub findings: Vec<Finding>,
}

#[cfg(test)]
mod tests {
    use crate::application::ports::{
        BeginVerificationInput, CompleteVerificationInput, DocumentationCheck,
        DocumentationCheckFailure, DocumentationChecker, PublicationFile, VerificationStore,
    };
    use crate::application::settings::RepoConfig;
    use crate::application::sync::context::document_identity;
    use crate::application::sync::types::PlannedDocument;
    use crate::application::sync::verification::VerificationService;
    use crate::domain::{document::DocumentFormat, model::SourceDocument};
    use anyhow::Result;
    use std::cell::RefCell;

    #[derive(Default)]
    struct WorkLog {
        completed: RefCell<Vec<bool>>,
    }
    impl VerificationStore for WorkLog {
        fn begin_verification(&self, _: BeginVerificationInput<'_>) -> Result<i64> {
            Ok(1)
        }
        fn complete_verification(&self, input: CompleteVerificationInput<'_>) -> Result<()> {
            self.completed.borrow_mut().push(input.findings.is_empty());
            Ok(())
        }
    }
    struct Check(bool);
    impl DocumentationChecker for Check {
        fn check(
            &self,
            _: &RepoConfig,
            revision: &str,
            files: &[PublicationFile],
        ) -> Result<DocumentationCheck> {
            assert_eq!(revision, "revision");
            assert_eq!(files[0].content, b"<!-- opaque -->\n");
            Ok(DocumentationCheck {
                failures: if self.0 {
                    Vec::new()
                } else {
                    vec![(
                        0,
                        DocumentationCheckFailure {
                            timed_out: false,
                            message: "failed".into(),
                        },
                    )]
                },
            })
        }
    }
    fn document() -> PlannedDocument {
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
            bytes: b"<!-- opaque -->\n".to_vec(),
            content_hash: "hash".into(),
        };
        let parsed = source.parse().unwrap();
        PlannedDocument {
            identity: document_identity(&source, "fr", &parsed),
            document_id: 1,
            source_path: source.path.clone(),
            target_path: "i18n/fr/guide.md".into(),
            parsed,
            units: Vec::new(),
            expected_materialized_hash: None,
        }
    }
    fn repo() -> RepoConfig {
        toml::from_str("path = '.'\nlanguages = ['fr']\n[documentation]\ncommands = [['check']]\n")
            .unwrap()
    }
    #[test]
    fn failed_project_check_cannot_produce_materialization_capability() {
        let repo = repo();
        let store = WorkLog::default();
        let service = VerificationService {
            repo: &repo,
            database: &store,
            documentation: &Check(false),
        };
        let outcome = service
            .verify("run", "fr", "revision", &[document()])
            .unwrap();
        assert!(outcome.batch.is_none());
        assert_eq!(outcome.findings[0].code, "DOC-CHECK-FAILED");
        assert_eq!(*store.completed.borrow(), vec![true, false]);
    }
    #[test]
    fn verified_snapshot_binds_identity_and_bytes_independently_of_mutable_plan() {
        let repo = repo();
        let store = WorkLog::default();
        let service = VerificationService {
            repo: &repo,
            database: &store,
            documentation: &Check(true),
        };
        let mut plan = document();
        let identity = plan.identity.clone();
        let outcome = service
            .verify("run", "fr", "revision", &[plan.clone()])
            .unwrap();
        plan.identity.target_path = "changed".into();
        let batch = outcome.batch.unwrap();
        assert_eq!(batch.documents()[0].document().identity, identity);
        assert_eq!(batch.documents()[0].file().content, b"<!-- opaque -->\n");
        assert!(outcome.findings.is_empty());
    }
}
