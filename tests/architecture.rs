use std::fs;
use std::path::{Path, PathBuf};

fn rust_files(root: &Path) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_path_buf()];
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(rust_files(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    files
}

fn assert_absent(root: &str, forbidden: &[&str]) {
    for path in rust_files(Path::new(root)) {
        let source = fs::read_to_string(&path).unwrap();
        for pattern in forbidden {
            assert!(
                !source.contains(pattern),
                "{} must not depend on {pattern:?}",
                path.display()
            );
        }
    }
}

fn assert_contains_all(path: &str, required: &[&str]) {
    let source = rust_files(Path::new(path))
        .iter()
        .map(|file| fs::read_to_string(file).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    for pattern in required {
        assert!(
            source.contains(pattern),
            "{path} must contain architecture anchor {pattern:?}"
        );
    }
}

#[test]
fn domain_is_pure_and_independent() {
    assert_absent(
        "src/domain",
        &[
            "crate::adapters",
            "crate::application",
            "crate::cli",
            "clap::",
            "rusqlite::",
            "std::fs",
            "std::process",
        ],
    );
}

#[test]
fn application_owns_ports_without_concrete_adapters() {
    assert_absent(
        "src/application",
        &[
            "crate::adapters",
            "crate::cli",
            "clap::",
            "rusqlite::",
            "serde_json::Value",
            "std::fs",
            "std::process",
        ],
    );
    assert_contains_all(
        "src/application/command.rs",
        &["pub trait OutputReporter", "fn stdout", "fn stderr"],
    );
    assert_contains_all(
        "src/application/ports.rs",
        &[
            "pub trait AgentExecutor",
            "pub trait DocumentationChecker",
            "pub trait StateStore",
            "pub trait Materializer",
            "pub trait GitPublisher",
            "pub trait CodeHost",
            "fn resolve_source_revision",
            "fn prepare(",
            "fn ensure_pull_request(",
        ],
    );
    assert_contains_all(
        "src/application",
        &[
            "pub struct Orchestrator",
            "materializer: &'a dyn Materializer",
            "agents: &'a dyn AgentExecutor",
            "documentation: &'a dyn DocumentationChecker",
            "git: &'a dyn GitPublisher",
            "code_host: &'a dyn CodeHost",
            "output: &'a dyn OutputReporter",
            "pub fn plan_language",
            "pub fn run_language",
            "claim_publication_locale",
            "publication_candidate_persisted",
            "publication_side_effect_completed",
            "agent_candidate_committed",
            "materialized_file_written",
        ],
    );
}

#[test]
fn adapters_implement_and_native_composes_application_ports() {
    assert!(!Path::new("src/adapters/orchestrator.rs").exists());
    assert_contains_all(
        "src/adapters/agent.rs",
        &["impl AgentExecutor for RoutedAgentExecutor"],
    );
    assert_contains_all("src/adapters", &["impl StateStore for Database"]);
    assert_contains_all(
        "src/adapters/documentation.rs",
        &["impl DocumentationChecker for NativeDocumentationChecker"],
    );
    assert_contains_all(
        "src/adapters/materialize.rs",
        &["impl Materializer for FilesystemMaterializer"],
    );
    assert_contains_all(
        "src/adapters/gitout.rs",
        &["impl GitPublisher for NativeGitPublisher"],
    );
    assert_contains_all(
        "src/adapters/github.rs",
        &["impl CodeHost for GithubCodeHost"],
    );
    assert_contains_all(
        "src/adapters/native.rs",
        &[
            "RoutedAgentExecutor::new",
            "FilesystemMaterializer",
            "NativeDocumentationChecker",
            "NativeGitPublisher",
            "GithubCodeHost",
            "Orchestrator::new",
            "output.stdout",
            "output.stderr",
        ],
    );
}

#[test]
fn cli_only_parses_requests_and_presents_results() {
    assert_absent(
        "src/cli",
        &["crate::adapters", "rusqlite::", "std::fs", "std::process"],
    );
    assert_absent("src/application", &["println!", "eprintln!"]);
    assert_absent("src/adapters/native.rs", &["println!", "eprintln!"]);
    assert_contains_all(
        "src/cli/mod.rs",
        &[
            "impl OutputReporter for TerminalReporter",
            "writeln!(stdout",
            "stdout.flush()",
            "writeln!(stderr",
            "stderr.flush()",
        ],
    );
    assert_contains_all(
        "src/composition.rs",
        &[
            "output: &dyn OutputReporter",
            "crate::diagnostics::init()",
            "NativeOperations::new(output)",
        ],
    );
}

#[test]
fn adapters_do_not_depend_on_cli() {
    assert_absent("src/adapters", &["crate::cli"]);
}

#[test]
fn accepted_decision_replaces_compatibility_documents() {
    assert_contains_all(
        "docs/architecture/adr-0001-native-single-authority.md",
        &[
            "**Status:** Accepted",
            "**Supersedes:** the unreleased external-skill/Python compatibility architecture",
            "One fani-identified SQLite database is the sole fani-owned authority",
            "StateStore",
            "AgentExecutor",
            "Materializer",
            "GitPublisher",
            "CodeHost",
            "No runtime path imports old JSON/JSONL state",
        ],
    );
    for removed in [
        "docs/architecture/compatibility-baseline.md",
        "docs/architecture/rust-sqlite-rewrite.md",
    ] {
        assert!(
            !Path::new(removed).exists(),
            "{removed} must remain removed"
        );
    }
    assert_contains_all(
        "README.md",
        &[
            "ADR-0001",
            "adr-0001-native-single-authority.md",
            "--repo PATH_OR_BASENAME",
        ],
    );
    assert_contains_all(
        "docs/architecture/native-i18n.md",
        &["trusted translation memory", "Decision record: [`ADR-0001`"],
    );
}

#[test]
fn library_surface_keeps_implementation_layers_private() {
    let source = fs::read_to_string("src/lib.rs").unwrap();
    for private_module in ["adapters", "cli", "composition"] {
        assert!(
            source.contains(&format!("mod {private_module};")),
            "{private_module} must remain a private implementation module"
        );
        assert!(
            !source.contains(&format!("pub mod {private_module};")),
            "{private_module} must not be exposed as a public module"
        );
    }
}

#[test]
fn workflow_services_do_not_share_the_coordinator_or_aggregate_store() {
    assert_absent(
        "src/application/sync",
        &["impl Orchestrator", "use super::*", "dyn StateStore"],
    );
}

#[test]
fn preview_io_capabilities_cannot_mutate_targets_or_publish() {
    assert_absent(
        "src/application/ports/read_io.rs",
        &[
            "fn apply",
            "fn restore",
            "fn prepare",
            "fn publish",
            "fn execute",
        ],
    );
    assert_absent(
        "src/application/sync/planning.rs",
        &[
            "dyn StateStore",
            "dyn Materializer",
            "dyn GitPublisher",
            "dyn AgentExecutor",
            ".upsert_",
            ".enqueue_",
            ".record_",
            ".finish_",
            ".execute(",
        ],
    );
}

#[test]
fn obsolete_crud_ports_cannot_reenter_application_services() {
    assert_absent(
        "src/application",
        &[
            "pub trait DocumentStore",
            "pub trait TranslationStore",
            "pub trait CanonicalStore",
            "pub trait EffectStore",
            "pub trait PublicationStore",
        ],
    );
    assert_absent(
        "src/application/sync/materialization.rs",
        &[
            ".persist_canonical_document(",
            ".enqueue_materialization(",
            ".complete_outbox(",
            ".finish_document_work(",
        ],
    );
    assert_absent(
        "src/application/sync/publication.rs",
        &[
            ".record_publication_authorization(",
            ".update_outbox_payload(",
            ".complete_outbox(",
            ".record_pr_state(",
        ],
    );
    assert_absent(
        "src/application/sync/reconciliation.rs",
        &[
            ".upsert_document(",
            ".upsert_unit(",
            ".trust_translation(",
            ".persist_canonical_document(",
            ".finish_document_work(",
        ],
    );
}
