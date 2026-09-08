use rusqlite::Connection;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::{TempDir, tempdir};

const FIXED_DATE: &str = "2026-01-02T03:04:05Z";
const FIXED_CONFIG: &str = r#"[[repo]]
path = "clone"
languages = ["fr"]
include = ["docs/**/*.md"]
data_dir = ".fani"
target_pattern = "translations/{lang}/{relpath}"
max_tasks = 10
repair_budget = 0
[repo.quality]
revision = false
proofread = false
[repo.publish]
enabled = true
source_ref = "HEAD"
branch = "i18n/{lang}"
push = false
[agents.recorded]
provider = "recorded-offline-fixture"
model = "recorded-v1"
adapter = "command-json-v1"
cmd = ["/bin/sh", "__PROVIDER__", "__FIXTURE__"]
concurrency = 1
timeout_s = 5
retries = 0
env_allow = []
[routing]
translate = "recorded"
"#;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordedFixture {
    schema: String,
    source: String,
    output: String,
}

struct CandidateRun {
    _root: TempDir,
    repo: PathBuf,
    commit: String,
    tree: String,
    tree_bytes: Vec<u8>,
    commit_bytes: Vec<u8>,
    target_bytes: Vec<u8>,
}

fn checked(command: &mut Command) -> Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "command failed: {command:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = checked(Command::new("git").current_dir(cwd).args(args));
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn git_bytes(cwd: &Path, args: &[&str]) -> Vec<u8> {
    checked(Command::new("git").current_dir(cwd).args(args)).stdout
}

fn seed_remote() -> TempDir {
    let seed = tempdir().unwrap();
    let source = seed.path().join("source");
    let remote = seed.path().join("source.git");
    fs::create_dir(&source).unwrap();
    git(&source, &["init", "-q", "-b", "main"]);
    git(&source, &["config", "user.name", "Reproducibility Fixture"]);
    git(
        &source,
        &["config", "user.email", "fixture@example.invalid"],
    );
    fs::create_dir(source.join("docs")).unwrap();
    fs::write(
        source.join("docs/guide.md"),
        b"Deterministic source bytes.\n",
    )
    .unwrap();
    git(&source, &["add", "docs/guide.md"]);
    checked(
        Command::new("git")
            .current_dir(&source)
            .args(["commit", "-qm", "fixed source"])
            .env("GIT_AUTHOR_DATE", FIXED_DATE)
            .env("GIT_COMMITTER_DATE", FIXED_DATE),
    );
    git(
        seed.path(),
        &["init", "--bare", "-q", remote.to_str().unwrap()],
    );
    git(
        &source,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&source, &["push", "-q", "origin", "main"]);
    git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    seed
}

fn install_forbidden_command_guards(root: &Path) -> (PathBuf, PathBuf) {
    let guards = root.join("forbidden-command-guards");
    let marker = root.join("forbidden-command-invoked");
    fs::create_dir(&guards).unwrap();
    for name in [
        "python",
        "python3",
        "uv",
        "skill",
        "i18n-skill",
        "fani-i18n",
        "curl",
        "wget",
        "nc",
        "ncat",
        "socat",
        "ssh",
    ] {
        let path = guards.join(name);
        fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' '{}' > '{}'\nexit 97\n",
                name,
                marker.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }
    (guards, marker)
}

fn generated_files(repo: &Path) -> BTreeSet<String> {
    fn visit(root: &Path, directory: &Path, files: &mut BTreeSet<String>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path == root.join(".git") {
                continue;
            }
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    let mut files = BTreeSet::new();
    visit(repo, repo, &mut files);
    files
}

fn run_candidate(remote: &Path, provider: &Path, fixture_path: &Path) -> CandidateRun {
    let root = tempdir().unwrap();
    let repo = root.path().join("clone");
    git(
        root.path(),
        &["clone", "-q", remote.to_str().unwrap(), "clone"],
    );
    git(&repo, &["config", "user.name", "Reproducibility Fixture"]);
    git(&repo, &["config", "user.email", "fixture@example.invalid"]);
    assert_eq!(git(&repo, &["status", "--porcelain=v1"]), "");
    assert!(!repo.join(".fani").exists(), "fani state was not empty");
    assert!(
        !repo.join("translations").exists(),
        "candidate cache was not empty"
    );

    let config = FIXED_CONFIG
        .replace("__PROVIDER__", provider.to_str().unwrap())
        .replace("__FIXTURE__", fixture_path.to_str().unwrap());
    fs::write(root.path().join("fani.toml"), config).unwrap();
    let reports = root.path().join("reports");
    let (guards, forbidden_marker) = install_forbidden_command_guards(root.path());
    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let guarded_path = format!("{}:{}", guards.display(), inherited_path.to_string_lossy());

    let output = Command::new(env!("CARGO_BIN_EXE_fani"))
        .current_dir(root.path())
        .args([
            "sync",
            "--config",
            "fani.toml",
            "--report-dir",
            reports.to_str().unwrap(),
            "--quiet",
        ])
        .env("PATH", guarded_path)
        .env("ANTHROPIC_API_KEY", "must-not-reach-provider")
        .env("OPENAI_API_KEY", "must-not-reach-provider")
        .env("AWS_ACCESS_KEY_ID", "must-not-reach-provider")
        .env("GITHUB_TOKEN", "must-not-reach-provider")
        .env("GH_TOKEN", "must-not-reach-provider")
        .env("HTTP_PROXY", "http://must-not-reach-provider.invalid")
        .env("HTTPS_PROXY", "http://must-not-reach-provider.invalid")
        .env("ALL_PROXY", "socks5://must-not-reach-provider.invalid")
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={}\nstderr={}\nreport={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        fs::read_to_string(reports.join("report.json")).unwrap_or_default(),
    );
    assert!(output.stdout.is_empty());
    assert!(
        !forbidden_marker.exists(),
        "Python, uv, an external skill, or a network command was invoked: {}",
        fs::read_to_string(&forbidden_marker).unwrap_or_default()
    );

    let database = repo.join(".fani/fani.db");
    assert!(database.is_file());
    let connection = Connection::open(&database).unwrap();
    let (request_json, response_json): (String, String) = connection
        .query_row(
            "SELECT request_json,response_json FROM attempts WHERE status='succeeded'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let request: Value = serde_json::from_str(&request_json).unwrap();
    let response: Value = serde_json::from_str(&response_json).unwrap();
    assert_eq!(request["schema"], "fani.agent.request.v2");
    assert_eq!(request["task"]["source"], "Deterministic source bytes.");
    assert_eq!(request["task"]["source_format"], "markdown");
    assert_eq!(request["task"]["unit_context"]["format"], "markdown");
    assert!(
        request["task"]["context_key"]
            .as_str()
            .unwrap()
            .contains("docs/guide.md")
    );
    assert!(request["task"]["message_syntax"].is_null());
    assert_eq!(
        request["task"]["token_permissions"]["contract"],
        "fani-markdown-tokens-v1"
    );
    assert_eq!(response["schema"], "fani.agent.response.v1");
    assert_eq!(response["task_id"], request["task"]["id"]);
    assert_eq!(response["output"], "Octets source deterministes.");

    assert_eq!(
        generated_files(&repo),
        BTreeSet::from([
            ".fani/fani.db".to_owned(),
            "docs/guide.md".to_owned(),
            "translations/fr/docs/guide.md".to_owned(),
        ]),
        "fani generated authority or package/config/cache state outside SQLite",
    );

    let commit = git(&repo, &["rev-parse", "refs/heads/i18n/fr^{commit}"]);
    let tree = git(&repo, &["rev-parse", "refs/heads/i18n/fr^{tree}"]);
    let target_bytes = git_bytes(
        &repo,
        &["show", &format!("{commit}:translations/fr/docs/guide.md")],
    );
    assert_eq!(target_bytes, b"Octets source deterministes.\n");
    let tree_bytes = git_bytes(&repo, &["cat-file", "tree", &tree]);
    let commit_bytes = git_bytes(&repo, &["cat-file", "commit", &commit]);

    CandidateRun {
        _root: root,
        repo,
        commit,
        tree,
        tree_bytes,
        commit_bytes,
        target_bytes,
    }
}

#[test]
fn clean_clones_with_empty_state_and_recorded_provider_make_identical_candidates() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let provider = manifest.join("tests/fixtures/recorded-provider.sh");
    let fixture_path = manifest.join("tests/fixtures/recorded-provider-fixture.json");
    let fixture: RecordedFixture =
        serde_json::from_str(&fs::read_to_string(&fixture_path).unwrap()).unwrap();
    assert_eq!(fixture.schema, "fani.recorded-provider-fixture.v1");
    assert_eq!(fixture.source, "Deterministic source bytes.");
    assert_eq!(fixture.output, "Octets source deterministes.");

    let seed = seed_remote();
    let remote = seed.path().join("source.git");
    let first = run_candidate(&remote, &provider, &fixture_path);
    let second = run_candidate(&remote, &provider, &fixture_path);

    assert_ne!(first.repo, second.repo);
    assert_eq!(first.tree, second.tree, "candidate tree OIDs differ");
    assert_eq!(
        first.tree_bytes, second.tree_bytes,
        "raw Git tree bytes differ"
    );
    assert_eq!(
        first.target_bytes, second.target_bytes,
        "target blob bytes differ"
    );
    assert_eq!(
        first.commit, second.commit,
        "deterministic commit OIDs differ"
    );
    assert_eq!(
        first.commit_bytes, second.commit_bytes,
        "candidate commit bytes differ"
    );
}
