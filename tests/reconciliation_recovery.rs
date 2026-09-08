use anyhow::{Result, bail};
use fani::application::ports::{
    CanonicalFileInput, DocumentationCheck, DocumentationChecker, Materialization,
    MaterializationResult, Materializer, PublicationFile, SourceReader,
};
use fani::application::settings::RepoConfig;
use fani::application::sync::{adopt_human_edit, discard_human_edit};
use fani::domain::document::DocumentFormat;
use fani::domain::model::{
    Freshness, MemoryTier, PublicationState, ReviewState, SourceDocument, TranslationProvenance,
    ValidationState,
};
use fani::domain::prompts;
use fani::test_support::db::Database;
use sha2::{Digest, Sha256};
use std::cell::{Cell, RefCell};
use std::path::Path;
use tempfile::TempDir;

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

struct FixedGit;
impl SourceReader for FixedGit {
    fn resolve_source_revision(&self, _: &RepoConfig) -> Result<String> {
        Ok("revision".into())
    }
    fn discover(&self, repo: &RepoConfig, revision: &str) -> Result<Vec<SourceDocument>> {
        let bytes = b"# Heading\n\nParagraph.\n".to_vec();
        Ok(vec![SourceDocument {
            source_format: DocumentFormat::Markdown,
            source_set_id: "markdown".into(),
            mapping_identity: "mapping".into(),
            mapped_relpath: "guide.md".into(),
            target_pattern: "i18n/{lang}/{relpath}".into(),
            message_syntax: None,
            repository: repo.path.to_string_lossy().into_owned(),
            source_revision: revision.into(),
            path: "guide.md".into(),
            content_hash: digest(&bytes),
            bytes,
        }])
    }
}

struct Target {
    bytes: RefCell<Option<Vec<u8>>>,
    fail_after_write: Cell<bool>,
    writes: Cell<usize>,
}
impl Target {
    fn new() -> Self {
        Self {
            bytes: RefCell::new(Some(b"# Titre\n\nParagraphe.\n".to_vec())),
            fail_after_write: Cell::new(false),
            writes: Cell::new(0),
        }
    }
}
impl Materializer for Target {
    fn read(&self, _: &Path, _: &Path) -> Result<Option<Vec<u8>>> {
        Ok(self.bytes.borrow().clone())
    }
    fn apply(&self, _: &Path, operation: &Materialization) -> Result<MaterializationResult> {
        let actual = self.bytes.borrow().clone();
        let desired_hash = digest(&operation.desired);
        if actual.as_deref() == Some(operation.desired.as_slice()) {
            return Ok(MaterializationResult::AlreadyCurrent { hash: desired_hash });
        }
        let actual_hash = actual.as_deref().map(digest);
        if actual_hash != operation.expected_hash {
            return Ok(MaterializationResult::HumanEdit {
                actual_hash: actual_hash.unwrap_or_else(|| "missing".into()),
            });
        }
        *self.bytes.borrow_mut() = Some(operation.desired.clone());
        self.writes.set(self.writes.get() + 1);
        if self.fail_after_write.replace(false) {
            bail!("simulated interruption after external write");
        }
        Ok(MaterializationResult::Written { hash: desired_hash })
    }
    fn restore(&self, _: &Path, _: &Materialization) -> Result<MaterializationResult> {
        panic!("discard must use the durable compare-and-swap precondition")
    }
}
struct Check(bool);
impl DocumentationChecker for Check {
    fn check(&self, _: &RepoConfig, _: &str, _: &[PublicationFile]) -> Result<DocumentationCheck> {
        if !self.0 {
            bail!("documentation checker unavailable");
        }
        Ok(DocumentationCheck {
            failures: Vec::new(),
        })
    }
}

struct Fixture {
    _tmp: TempDir,
    repo: RepoConfig,
    db: Database,
    canonical: Vec<u8>,
}
fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let mut repo: RepoConfig = toml::from_str("path = '.'\nlanguages = ['fr']\n").unwrap();
    repo.path = tmp.path().to_path_buf();
    let db = Database::open(tmp.path().join("fani.db")).unwrap();
    let key = repo
        .path
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let mut hasher = Sha256::new();
    hasher.update((key.len() as u64).to_be_bytes());
    hasher.update(key.as_bytes());
    let repository = db
        .upsert_repository(&format!("{:x}", hasher.finalize()), &repo.path, None, None)
        .unwrap();
    let source = FixedGit.discover(&repo, "revision").unwrap().remove(0);
    db.upsert_document(
        repository,
        &source.path,
        Some("revision"),
        &source.content_hash,
        "{}",
    )
    .unwrap();
    let canonical = b"# Ancien\n\nTexte.\n".to_vec();
    db.upsert_canonical_file(CanonicalFileInput {
        repository_id: repository,
        locale: "fr",
        path: "i18n/fr/guide.md",
        source_revision: "revision",
        content: &canonical,
        content_hash: &digest(&canonical),
        materialized_hash: Some(&digest(&canonical)),
        freshness: Freshness::Exact,
        provenance: TranslationProvenance::Imported,
        validation: ValidationState::Passed,
        review: ReviewState::Unreviewed,
        publication: PublicationState::Candidate,
        trust_tier: MemoryTier::Candidate,
        policy_fingerprint: &prompts::policy_fingerprint(),
    })
    .unwrap();
    Fixture {
        _tmp: tmp,
        repo,
        db,
        canonical,
    }
}
fn count(db: &Database, sql: &str) -> i64 {
    db.connect()
        .unwrap()
        .query_row(sql, [], |row| row.get(0))
        .unwrap()
}

#[test]
fn discard_recovers_after_external_write_without_repeating_it() {
    let f = fixture();
    let target = Target::new();
    target.fail_after_write.set(true);
    assert!(discard_human_edit(&f.repo, &f.db, &target, &FixedGit, "fr").is_err());
    assert_eq!(
        count(&f.db, "SELECT COUNT(*) FROM runs WHERE status='error'"),
        1
    );
    assert_eq!(
        count(
            &f.db,
            "SELECT COUNT(*) FROM materialization_outbox WHERE state='pending'"
        ),
        1
    );
    assert_eq!(
        discard_human_edit(&f.repo, &f.db, &target, &FixedGit, "fr").unwrap(),
        1
    );
    assert_eq!(target.writes.get(), 1);
    assert_eq!(target.bytes.borrow().as_ref(), Some(&f.canonical));
    assert_eq!(
        count(
            &f.db,
            "SELECT COUNT(*) FROM materialization_outbox WHERE state='done'"
        ),
        1
    );
    assert_eq!(
        count(&f.db, "SELECT COUNT(*) FROM runs WHERE status='running'"),
        0
    );
}

#[test]
fn discard_recovery_preserves_new_human_edit() {
    let f = fixture();
    let target = Target::new();
    target.fail_after_write.set(true);
    assert!(discard_human_edit(&f.repo, &f.db, &target, &FixedGit, "fr").is_err());
    let new_edit = b"new human edit after interruption".to_vec();
    *target.bytes.borrow_mut() = Some(new_edit.clone());
    assert!(discard_human_edit(&f.repo, &f.db, &target, &FixedGit, "fr").is_err());
    assert_eq!(target.bytes.borrow().as_ref(), Some(&new_edit));
    assert_eq!(target.writes.get(), 1);
    assert_eq!(
        count(
            &f.db,
            "SELECT COUNT(*) FROM materialization_outbox WHERE state='pending'"
        ),
        1
    );
}

#[test]
fn adoption_checker_error_finishes_run_without_trusting_content() {
    let f = fixture();
    let target = Target::new();
    assert!(
        adopt_human_edit(
            &f.repo,
            &f.db,
            &f.db,
            &target,
            &FixedGit,
            &Check(false),
            "fr"
        )
        .is_err()
    );
    assert_eq!(
        count(&f.db, "SELECT COUNT(*) FROM runs WHERE status='error'"),
        1
    );
    assert_eq!(
        count(
            &f.db,
            "SELECT COUNT(*) FROM translation_memory_entries WHERE tier='trusted'"
        ),
        0
    );
}

#[test]
fn adoption_identity_failure_rolls_back_trust_and_canonical_together() {
    let f = fixture();
    let target = Target::new();
    f.db.connect().unwrap().execute_batch("CREATE TRIGGER deny_adoption_identity BEFORE INSERT ON canonical_document_intents BEGIN SELECT RAISE(ABORT, 'injected adoption failure'); END;").unwrap();
    let error = adopt_human_edit(
        &f.repo,
        &f.db,
        &f.db,
        &target,
        &FixedGit,
        &Check(true),
        "fr",
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("injected adoption failure"),
        "{error:#}"
    );
    assert_eq!(
        count(
            &f.db,
            "SELECT COUNT(*) FROM translation_memory_entries WHERE tier='trusted'"
        ),
        0
    );
    assert_eq!(
        count(&f.db, "SELECT COUNT(*) FROM runs WHERE status='error'"),
        1
    );
    let bytes: Vec<u8> =
        f.db.connect()
            .unwrap()
            .query_row("SELECT content FROM canonical_files", [], |row| row.get(0))
            .unwrap();
    assert_eq!(bytes, f.canonical);
    assert_eq!(
        count(
            &f.db,
            "SELECT COUNT(*) FROM work_items WHERE kind='assembly' AND status='succeeded'"
        ),
        0
    );
}
