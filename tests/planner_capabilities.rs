use anyhow::Result;
use fani::application::ports::{
    CanonicalFile, PlanningStore, RecoveredAttempt, SourceReader, TargetReader,
    TranslationCandidate, UnitHistory,
};
use fani::application::settings::RepoConfig;
use fani::application::sync::planning::Planner;
use fani::domain::{document::DocumentFormat, model::SourceDocument};
use std::path::Path;

// This store deliberately implements no writer, executor, or aggregate StateStore.
struct ReadOnlyStore;
impl PlanningStore for ReadOnlyStore {
    fn repository_id(&self, _: &str) -> Result<Option<i64>> {
        Ok(None)
    }
    fn document_id(&self, _: i64, _: &str) -> Result<Option<i64>> {
        Ok(None)
    }
    fn unit_history(&self, _: i64, _: &str) -> Result<Vec<UnitHistory>> {
        Ok(Vec::new())
    }
    fn unchanged_document_unit_keys(&self, _: i64, _: &str, _: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
    fn translation_candidates(&self, _: i64, _: &str) -> Result<Vec<TranslationCandidate>> {
        Ok(Vec::new())
    }
    fn review_attempts(&self, _: i64, _: &str, _: &str) -> Result<Vec<RecoveredAttempt>> {
        Ok(Vec::new())
    }
    fn canonical_file(&self, _: i64, _: &str, _: &str) -> Result<Option<CanonicalFile>> {
        Ok(None)
    }
}
struct FixedSource;
impl SourceReader for FixedSource {
    fn resolve_source_revision(&self, _: &RepoConfig) -> Result<String> {
        Ok("fixed-revision".into())
    }
    fn discover(&self, _: &RepoConfig, revision: &str) -> Result<Vec<SourceDocument>> {
        assert_eq!(revision, "fixed-revision");
        Ok(vec![SourceDocument {
            source_format: DocumentFormat::Markdown,
            source_set_id: "markdown".into(),
            mapping_identity: "mapping".into(),
            mapped_relpath: "guide.md".into(),
            target_pattern: "i18n/{lang}/{relpath}".into(),
            message_syntax: None,
            repository: "fixture".into(),
            source_revision: revision.into(),
            path: "guide.md".into(),
            bytes: b"# Heading\n\nFirst paragraph.\n\nSecond paragraph.\n".to_vec(),
            content_hash: "fixed-source-hash".into(),
        }])
    }
}
struct ExistingTarget(Option<Vec<u8>>);
impl TargetReader for ExistingTarget {
    fn read(&self, _: &Path, relative: &Path) -> Result<Option<Vec<u8>>> {
        assert_eq!(relative, Path::new("i18n/fr/guide.md"));
        Ok(self.0.clone())
    }
}
fn config() -> RepoConfig {
    toml::from_str("path = '.'\nlanguages = ['fr']\nmax_tasks = 1\n[quality]\nrevision = false\nproofread = false\n").unwrap()
}

#[test]
fn standalone_preview_enforces_budget_without_any_write_capability() {
    let repo = config();
    let planner = Planner {
        repo: &repo,
        database: &ReadOnlyStore,
        materializer: &ExistingTarget(None),
        git: &FixedSource,
        agent_fingerprint: None,
    };
    let plan = planner.plan_language("fr").unwrap();
    assert_eq!(plan.source_revision, "fixed-revision");
    assert_eq!(plan.documents, 1);
    assert_eq!(plan.pending_units, 1);
    assert_eq!(plan.deferred_units, 2);
    assert_eq!(plan.conflicts, 0);
}

#[test]
fn standalone_preview_detects_human_target_with_read_only_adapters() {
    let repo = config();
    let target = ExistingTarget(Some(b"human translation".to_vec()));
    let planner = Planner {
        repo: &repo,
        database: &ReadOnlyStore,
        materializer: &target,
        git: &FixedSource,
        agent_fingerprint: Some("provider".into()),
    };
    assert_eq!(planner.plan_language("fr").unwrap().conflicts, 1);
}
