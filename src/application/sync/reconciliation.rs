use crate::application::contracts::{
    CheckManifestFile, CheckRequest, DocumentWorkResult, manifest_hash,
};
use crate::application::ports::{
    AdoptDocumentInput, AdoptUnitInput, BeginVerificationInput, CanonicalFileInput,
    CompleteVerificationInput, DiscardIntentInput, DocumentationChecker, Materialization,
    MaterializationResult, Materializer, PlanningStore, PublicationFile, ReconciliationStore,
    SourceReader, VerificationFinding, VerificationRequest, VerificationStore,
};
use crate::application::settings::RepoConfig;
use crate::application::sync::context::{
    content_hash, document_identity, hash, previous_units, stable_unit_hints, stable_unit_id,
};
use crate::domain::document::{unit_metadata, verify_document};
use crate::domain::matching::{MatchKind, match_units_with_stable_ids};
use crate::domain::model::{
    Freshness, MemoryTier, PublicationState, ReviewState, TranslationProvenance, ValidationState,
};
use crate::domain::prompts;
use anyhow::{Context, Result, anyhow, bail};
use std::path::Path;

pub(crate) struct ReconciliationService<'a> {
    pub repo: &'a RepoConfig,
    pub store: &'a dyn ReconciliationStore,
    pub materializer: &'a dyn Materializer,
    pub git: &'a dyn SourceReader,
}

impl ReconciliationService<'_> {
    pub fn adopt(
        &self,
        verification: &dyn VerificationStore,
        documentation: &dyn DocumentationChecker,
        language: &str,
    ) -> Result<usize> {
        let Self {
            repo,
            store: database,
            materializer,
            git,
        } = *self;
        let repository_key = repo
            .path
            .canonicalize()
            .unwrap_or_else(|_| repo.path.clone());
        let repository_id = database.upsert_repository(
            &hash(&[repository_key.to_string_lossy().as_bytes()]),
            &repo.path,
            Some(&repo.publish.github.base),
            None,
        )?;
        let source_revision = git.resolve_source_revision(repo)?;
        let documents = git.discover(repo, &source_revision)?;
        let mut adoption_files = Vec::new();
        for document in &documents {
            let target = document
                .target_path(language)
                .to_string_lossy()
                .into_owned();
            if PlanningStore::canonical_file(database, repository_id, language, &target)?.is_none()
            {
                continue;
            }
            let bytes = materializer
                .read(&repo.path, Path::new(&target))?
                .with_context(|| format!("cannot read human target {target}"))?;
            let source = document.parse()?;
            let translated = document.parse_bytes(&bytes)?;
            verify_document(&source, &translated.source)
                .with_context(|| format!("human target {target} failed validation"))?;
            if source.units.len() != translated.units.len()
                || source
                    .units
                    .iter()
                    .zip(&translated.units)
                    .any(|(source, target)| {
                        crate::domain::document::translated_unit_text(source, target).is_none()
                    })
            {
                bail!("human target {target} failed validation");
            }
            adoption_files.push(PublicationFile {
                path: target,
                content: bytes,
            });
        }
        if adoption_files.is_empty() {
            return Ok(0);
        }
        let manifest = adoption_files
            .iter()
            .map(|file| CheckManifestFile {
                path: &file.path,
                content_hash: content_hash(&file.content),
            })
            .collect::<Vec<_>>();
        let manifest_hash = manifest_hash(&manifest)?;
        let run_id = database.begin_run(
            repository_id,
            &format!("adopt:{repository_id}:{language}:{source_revision}:{manifest_hash}"),
            Path::new("adopt"),
            "{}",
            &prompts::policy_fingerprint(),
        )?;
        let mut failure_status = "error";
        let result = (|| -> Result<usize> {
            let anchor = documents
                .iter()
                .find(|document| {
                    document.target_path(language).to_string_lossy() == adoption_files[0].path
                })
                .ok_or_else(|| anyhow!("adoption candidate has no document identity"))?;
            let anchor_id = PlanningStore::document_id(database, repository_id, &anchor.path)?
                .ok_or_else(|| anyhow!("adoption document is missing"))?;
            let check_work = if repo.documentation.commands.is_empty() {
                None
            } else {
                Some(verification.begin_verification(BeginVerificationInput {
                    run_id: &run_id,
                    document_id: anchor_id,
                    locale: language,
                    request: VerificationRequest::ProjectCheck(CheckRequest {
                        source_revision: &source_revision,
                        manifest: &manifest,
                        manifest_hash: &manifest_hash,
                    }),
                })?)
            };
            let checked = documentation.check(repo, &source_revision, &adoption_files);
            if let Some(work_item_id) = check_work {
                let issue = match &checked {
                    Ok(result) if result.failures.is_empty() => None,
                    Ok(_) => {
                        Some("human targets failed configured documentation checks".to_owned())
                    }
                    Err(error) => Some(error.to_string()),
                };
                let findings = issue
                    .as_ref()
                    .map(|message| VerificationFinding {
                        key: "ADOPTION-CHECK",
                        code: "DOC-CHECK-FAILED",
                        message,
                        details: None,
                    })
                    .into_iter()
                    .collect::<Vec<_>>();
                verification.complete_verification(CompleteVerificationInput {
                    work_item_id,
                    result: DocumentWorkResult::Checked {
                        manifest_hash: &manifest_hash,
                    },
                    findings: &findings,
                })?;
            }
            let checked = checked?.failures.is_empty();
            if !checked {
                failure_status = "needs_human";
                bail!(
                    "human targets failed configured documentation checks; no translations were trusted"
                );
            }
            let mut adopted = 0;
            for document in documents {
                let target = document
                    .target_path(language)
                    .to_string_lossy()
                    .into_owned();
                let Some(_) =
                    PlanningStore::canonical_file(database, repository_id, language, &target)?
                else {
                    continue;
                };
                let bytes = adoption_files
                    .iter()
                    .find(|file| file.path == target)
                    .ok_or_else(|| anyhow!("human target {target} was not checked"))?
                    .content
                    .clone();
                let target_text =
                    std::str::from_utf8(&bytes).context("human target is not UTF-8")?;
                let source_document = document.parse()?;
                let target_document = document.parse_bytes(target_text.as_bytes())?;
                verify_document(&source_document, target_text)
                    .with_context(|| format!("human target {target} failed validation"))?;
                let source_units = &source_document.units;
                let target_units = &target_document.units;
                if source_units.len() != target_units.len() {
                    bail!(
                        "human target {target} does not preserve the source document unit structure"
                    );
                }
                let translations = source_units
                    .iter()
                    .zip(target_units)
                    .map(|(source, translated)| {
                        crate::domain::document::translated_unit_text(source, translated)
                            .ok_or_else(|| anyhow!("human target {target} failed validation"))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let stable_ids = source_units
                    .iter()
                    .enumerate()
                    .map(|(ordinal, unit)| stable_unit_id(&document.path, unit, ordinal))
                    .collect::<Vec<_>>();
                let stable_hints = stable_unit_hints(
                    database,
                    repository_id,
                    &document.path,
                    &document.content_hash,
                    &stable_ids,
                )?;
                let history =
                    match PlanningStore::document_id(database, repository_id, &document.path)? {
                        Some(document_id) => {
                            PlanningStore::unit_history(database, document_id, language)?
                        }
                        None => Vec::new(),
                    };
                let matched = match_units_with_stable_ids(
                    &previous_units(&history, &[], &document.path),
                    source_units,
                    &stable_hints,
                );
                if matched
                    .iter()
                    .any(|matched| matched.kind == MatchKind::Ambiguous)
                {
                    bail!("human target {target} cannot be mapped to stable source units");
                }
                let identity = document_identity(&document, language, &source_document);
                let mut adopted_units = Vec::new();
                for (ordinal, ((source_unit, translated), matched)) in source_units
                    .iter()
                    .zip(&translations)
                    .zip(matched)
                    .enumerate()
                {
                    let stable_id = matched
                        .stable_id
                        .unwrap_or_else(|| stable_ids[ordinal].clone());
                    let source_hash = hash(&[source_unit.source.as_bytes()]);
                    let context = source_unit.memory_context_key(&document.path);
                    adopted_units.push(AdoptUnitInput {
                        unit_key: stable_id,
                        ordinal: ordinal as i64,
                        source_text: source_unit.source.clone(),
                        source_hash,
                        context_json: unit_metadata(source_unit, &document.path),
                        context_key: context,
                        target_text: translated.clone(),
                    });
                }
                let adopted_hash = content_hash(&bytes);
                database.adopt_document(AdoptDocumentInput {
                    canonical: CanonicalFileInput {
                        repository_id,
                        locale: language,
                        path: &target,
                        source_revision: &source_revision,
                        content: &bytes,
                        content_hash: &adopted_hash,
                        materialized_hash: Some(&adopted_hash),
                        freshness: Freshness::Exact,
                        provenance: if source_units.is_empty() {
                            TranslationProvenance::Imported
                        } else {
                            TranslationProvenance::Human
                        },
                        validation: ValidationState::Passed,
                        review: ReviewState::Approved,
                        publication: PublicationState::Candidate,
                        trust_tier: MemoryTier::Trusted,
                        policy_fingerprint: &prompts::policy_fingerprint(),
                    },
                    run_id: &run_id,
                    identity: &identity,
                    units: &adopted_units,
                    metadata: &crate::application::ports::SourceDocumentMetadata {
                        format: source_document.format,
                        contract: source_document.contract.clone(),
                    },
                })?;
                adopted += 1;
            }
            Ok(adopted)
        })();
        database.finish_run(&run_id, if result.is_ok() { "ok" } else { failure_status })?;
        result
    }

    pub fn discard(&self, language: &str) -> Result<usize> {
        let Self {
            repo,
            store: database,
            materializer,
            git,
        } = *self;
        let repository_key = repo
            .path
            .canonicalize()
            .unwrap_or_else(|_| repo.path.clone());
        let repository_id = database.upsert_repository(
            &hash(&[repository_key.to_string_lossy().as_bytes()]),
            &repo.path,
            Some(&repo.publish.github.base),
            None,
        )?;
        let source_revision = git.resolve_source_revision(repo)?;
        let documents = git.discover(repo, &source_revision)?;
        let run_id = database.begin_run(
            repository_id,
            &format!("discard:{repository_id}:{language}:{source_revision}"),
            Path::new("discard"),
            "{}",
            &prompts::policy_fingerprint(),
        )?;
        let failure_status = "error";
        let result = (|| -> Result<usize> {
            let mut discarded = 0;
            for document in documents {
                let target = document
                    .target_path(language)
                    .to_string_lossy()
                    .into_owned();
                let Some(canonical) =
                    PlanningStore::canonical_file(database, repository_id, language, &target)?
                else {
                    continue;
                };
                let document_id =
                    PlanningStore::document_id(database, repository_id, &document.path)?
                        .ok_or_else(|| anyhow!("discard source document is missing"))?;
                let observed = materializer
                    .read(&repo.path, Path::new(&target))?
                    .map(|bytes| content_hash(&bytes));
                let receipt = database.begin_discard(DiscardIntentInput {
                    repository_id,
                    run_id: &run_id,
                    document_id,
                    locale: language,
                    path: &target,
                    canonical_file_id: canonical.id,
                    canonical_content_version_id: canonical.content_version_id,
                    content_hash: &canonical.content_hash,
                    observed_hash: observed.as_deref(),
                })?;
                let operation = Materialization {
                    path: target.clone().into(),
                    expected_hash: receipt.expected_hash.clone(),
                    desired: canonical.content.clone(),
                };
                let result = materializer.apply(&repo.path, &operation)?;
                let hash = match result {
                    MaterializationResult::Written { hash }
                    | MaterializationResult::AlreadyCurrent { hash } => hash,
                    MaterializationResult::HumanEdit { .. } => {
                        return Err(anyhow!("cannot discard {target}"));
                    }
                };
                database.complete_discard(&receipt.materialization, canonical.id, &hash)?;
                discarded += 1;
            }
            Ok(discarded)
        })();
        database.finish_run(&run_id, if result.is_ok() { "ok" } else { failure_status })?;
        result
    }
}

pub fn adopt_human_edit(
    repo: &RepoConfig,
    database: &dyn ReconciliationStore,
    verification: &dyn VerificationStore,
    materializer: &dyn Materializer,
    git: &dyn SourceReader,
    documentation: &dyn DocumentationChecker,
    language: &str,
) -> Result<usize> {
    ReconciliationService {
        repo,
        store: database,
        materializer,
        git,
    }
    .adopt(verification, documentation, language)
}

pub fn discard_human_edit(
    repo: &RepoConfig,
    database: &dyn ReconciliationStore,
    materializer: &dyn Materializer,
    git: &dyn SourceReader,
    language: &str,
) -> Result<usize> {
    ReconciliationService {
        repo,
        store: database,
        materializer,
        git,
    }
    .discard(language)
}
