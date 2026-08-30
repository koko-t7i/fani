use fani::test_support::gitout::{
    ChangeKind, PathChange, create_candidate, create_candidate_from_contents, push_candidate,
};
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use tempfile::tempdir;

use fani::test_support::github::{EnsurePullRequest, GhClient, ReconcileAction, locale_branch};

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn setup_repo() -> tempfile::TempDir {
    let repo = tempdir().unwrap();
    git(repo.path(), &["init", "-q", "-b", "main"]);
    git(repo.path(), &["config", "user.name", "Test"]);
    git(repo.path(), &["config", "user.email", "test@example.com"]);
    fs::write(repo.path().join("modify.txt"), "old\n").unwrap();
    fs::write(repo.path().join("delete.txt"), "delete\n").unwrap();
    fs::write(repo.path().join("keep.txt"), "keep\n").unwrap();
    fs::write(repo.path().join("staged.txt"), "base\n").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-qm", "source"]);
    repo
}

#[test]
fn plumbing_publication_preserves_checkout_index_and_unrelated_files() {
    let repo = setup_repo();
    fs::write(repo.path().join("modify.txt"), "translated\n").unwrap();
    fs::write(repo.path().join("add.txt"), "added\n").unwrap();
    fs::remove_file(repo.path().join("delete.txt")).unwrap();
    fs::write(repo.path().join("staged.txt"), "staged change\n").unwrap();
    git(repo.path(), &["add", "staged.txt"]);
    fs::write(repo.path().join("scratch.txt"), "unrelated\n").unwrap();

    let head = git(repo.path(), &["rev-parse", "HEAD"]);
    let branch = git(repo.path(), &["symbolic-ref", "--short", "HEAD"]);
    let index_path = repo.path().join(".git/index");
    let index_bytes = fs::read(&index_path).unwrap();
    let staged = git(repo.path(), &["diff", "--cached", "--name-status"]);
    let scratch = fs::read(repo.path().join("scratch.txt")).unwrap();

    let changes = vec![
        PathChange {
            path: "add.txt".into(),
            kind: ChangeKind::Add,
        },
        PathChange {
            path: "delete.txt".into(),
            kind: ChangeKind::Delete,
        },
        PathChange {
            path: "modify.txt".into(),
            kind: ChangeKind::Modify,
        },
    ];
    let candidate = create_candidate(
        repo.path(),
        &head,
        "i18n/zh-CN",
        &changes,
        "i18n(zh-CN): update",
    )
    .unwrap();

    assert_eq!(candidate.source.commit, head);
    assert_eq!(
        candidate.source.tree,
        git(repo.path(), &["rev-parse", &format!("{}^{{tree}}", head)])
    );
    assert_eq!(git(repo.path(), &["rev-parse", "HEAD"]), head);
    assert_eq!(
        git(repo.path(), &["symbolic-ref", "--short", "HEAD"]),
        branch
    );
    assert_eq!(fs::read(index_path).unwrap(), index_bytes);
    assert_eq!(
        git(repo.path(), &["diff", "--cached", "--name-status"]),
        staged
    );
    assert_eq!(fs::read(repo.path().join("scratch.txt")).unwrap(), scratch);
    assert_eq!(
        fs::read_to_string(repo.path().join("modify.txt")).unwrap(),
        "translated\n"
    );
    assert!(!repo.path().join("delete.txt").exists());

    assert_eq!(git(repo.path(), &["show", "i18n/zh-CN:add.txt"]), "added");
    assert_eq!(
        git(repo.path(), &["show", "i18n/zh-CN:modify.txt"]),
        "translated"
    );
    let deleted = Command::new("git")
        .args(["cat-file", "-e", "i18n/zh-CN:delete.txt"])
        .current_dir(repo.path())
        .status()
        .unwrap();
    assert!(!deleted.success());
    assert_eq!(git(repo.path(), &["show", "i18n/zh-CN:keep.txt"]), "keep");
    assert_eq!(git(repo.path(), &["show", "i18n/zh-CN:staged.txt"]), "base");
}

#[test]
fn durable_content_candidate_ignores_changed_worktree_bytes() {
    let repo = setup_repo();
    let source = git(repo.path(), &["rev-parse", "HEAD"]);
    fs::write(repo.path().join("modify.txt"), "newer worktree bytes\n").unwrap();
    let candidate = create_candidate_from_contents(
        repo.path(),
        &source,
        "i18n/es",
        &[("modify.txt".into(), b"durable translation\n".to_vec())],
        "i18n(es): update",
    )
    .unwrap();
    assert_eq!(
        git(
            repo.path(),
            &["show", &format!("{}:modify.txt", candidate.commit)]
        ),
        "durable translation"
    );
    assert_eq!(
        fs::read_to_string(repo.path().join("modify.txt")).unwrap(),
        "newer worktree bytes\n"
    );
}

#[test]
fn candidate_commit_is_stable_across_delayed_replay() {
    let repo = setup_repo();
    let source = git(repo.path(), &["rev-parse", "HEAD"]);
    fs::write(repo.path().join("modify.txt"), "translated\n").unwrap();
    let changes = [PathChange {
        path: "modify.txt".into(),
        kind: ChangeKind::Modify,
    }];
    let first = create_candidate(
        repo.path(),
        &source,
        "i18n/de",
        &changes,
        "i18n(de): update",
    )
    .unwrap();
    git(
        repo.path(),
        &["update-ref", "refs/heads/i18n/de", &source, &first.commit],
    );
    std::thread::sleep(Duration::from_millis(1_100));
    let replay = create_candidate(
        repo.path(),
        &source,
        "i18n/de",
        &changes,
        "i18n(de): update",
    )
    .unwrap();
    assert_eq!(replay.commit, first.commit);
}

#[test]
fn candidate_ref_and_remote_push_are_compare_and_swap() {
    let repo = setup_repo();
    let source = git(repo.path(), &["rev-parse", "HEAD"]);
    fs::write(repo.path().join("modify.txt"), "first\n").unwrap();
    let first = create_candidate(
        repo.path(),
        &source,
        "i18n/fr",
        &[PathChange {
            path: "modify.txt".into(),
            kind: ChangeKind::Modify,
        }],
        "first",
    )
    .unwrap();

    let remote = tempdir().unwrap();
    git(remote.path(), &["init", "--bare", "-q"]);
    git(
        repo.path(),
        &["remote", "add", "origin", remote.path().to_str().unwrap()],
    );
    push_candidate(repo.path(), "origin", &first, None).unwrap();
    assert_eq!(
        git(remote.path(), &["rev-parse", "refs/heads/i18n/fr"]),
        first.commit
    );

    fs::write(repo.path().join("modify.txt"), "second\n").unwrap();
    let second = create_candidate(
        repo.path(),
        &source,
        "i18n/fr",
        &[PathChange {
            path: "modify.txt".into(),
            kind: ChangeKind::Modify,
        }],
        "second",
    )
    .unwrap();
    git(
        remote.path(),
        &["update-ref", "refs/heads/i18n/fr", &source, &first.commit],
    );
    let error = push_candidate(repo.path(), "origin", &second, Some(&first.commit)).unwrap_err();
    assert!(error.to_string().contains("remote ref"), "{error:#}");
    assert_eq!(
        git(remote.path(), &["rev-parse", "refs/heads/i18n/fr"]),
        source
    );

    git(
        repo.path(),
        &["update-ref", "refs/heads/i18n/fr", &source, &second.commit],
    );
    let error = push_candidate(repo.path(), "origin", &second, Some(&source)).unwrap_err();
    assert!(error.to_string().contains("no longer the tip"), "{error:#}");
}

#[test]
fn gh_fixture_ensures_and_reconciles_one_pull_request() {
    let tmp = tempdir().unwrap();
    let executable = tmp.path().join("gh-fixture.sh");
    let state = tmp.path().join("state.json");
    let log = tmp.path().join("calls.jsonl");
    let script = format!(
        r#"#!/bin/sh
set -eu
STATE='{state}'
LOG='{log}'
json_args=''
for arg in "$@"; do
    escaped=$(printf '%s' "$arg" | sed 's/\\/\\\\/g; s/"/\\"/g')
    [ -z "$json_args" ] || json_args="$json_args,"
    json_args="$json_args\"$escaped\""
done
printf '{{"args":[%s],"lc":"%s","prompt":"%s"}}\n' "$json_args" "${{LC_ALL:-}}" "${{GH_PROMPT_DISABLED:-}}" >> "$LOG"
value() {{
    wanted=$1
    shift
    while [ "$#" -gt 0 ]; do
        if [ "$1" = "$wanted" ]; then printf '%s' "$2"; return 0; fi
        shift
    done
    return 1
}}
has() {{
    wanted=$1
    shift
    for arg in "$@"; do [ "$arg" != "$wanted" ] || return 0; done
    return 1
}}
save() {{
    mkdir -p "$STATE"
    printf '%s' "$head" > "$STATE/head"
    printf '%s' "$base" > "$STATE/base"
    printf '%s' "$title" > "$STATE/title"
    printf '%s' "$body" > "$STATE/body"
    printf '%s' "$draft" > "$STATE/draft"
}}
load() {{
    head=$(cat "$STATE/head")
    base=$(cat "$STATE/base")
    title=$(cat "$STATE/title")
    body=$(cat "$STATE/body")
    draft=$(cat "$STATE/draft")
}}
emit() {{
    printf '{{"number":7,"url":"https://example.test/acme/docs/pull/7","state":"OPEN","headRefName":"%s","baseRefName":"%s","isDraft":%s,"title":"%s","body":"%s"}}\n' "$head" "$base" "$draft" "$title" "$body"
}}
case "$1 $2" in
    'pr list')
        if [ -d "$STATE" ]; then load; printf '['; emit | tr -d '\n'; printf ']\n'; else printf '[]\n'; fi
        ;;
    'pr create')
        head=$(value --head "$@")
        base=$(value --base "$@")
        title=$(value --title "$@")
        body=$(value --body "$@")
        if has --draft "$@"; then draft=true; else draft=false; fi
        save
        printf '%s\n' 'https://example.test/acme/docs/pull/7'
        ;;
    'pr view')
        [ -d "$STATE" ] || exit 1
        load; emit
        ;;
    'pr edit')
        load
        base=$(value --base "$@")
        title=$(value --title "$@")
        body=$(value --body "$@")
        save
        ;;
    'pr ready')
        load
        if has --undo "$@"; then draft=true; else draft=false; fi
        save
        ;;
    *) exit 2 ;;
esac
"#,
        state = state.display(),
        log = log.display(),
    );
    let staging = tmp.path().join("gh-fixture.sh.tmp");
    let mut file = fs::File::create(&staging).unwrap();
    file.write_all(script.as_bytes()).unwrap();
    file.sync_all().unwrap();
    drop(file);
    let mut permissions = fs::metadata(&staging).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&staging, permissions).unwrap();
    fs::rename(staging, &executable).unwrap();

    let branch = locale_branch("i18n/{lang}", "pt-BR").unwrap();
    assert_eq!(branch, "i18n/pt-BR");
    assert!(locale_branch("i18n/{lang}", "../main").is_err());
    let client =
        GhClient::with_program(tmp.path(), &executable).with_timeout(Duration::from_secs(2));

    let created = client
        .ensure_pull_request(EnsurePullRequest {
            repository: "acme/docs",
            head: &branch,
            base: "main",
            title: "Update Portuguese",
            body: "Automated translations",
            draft: true,
            durable: None,
        })
        .unwrap();
    assert_eq!(created.action, ReconcileAction::Created);

    let reused = client
        .ensure_pull_request(EnsurePullRequest {
            repository: "acme/docs",
            head: &branch,
            base: "main",
            title: "Update Portuguese",
            body: "Automated translations",
            draft: true,
            durable: Some(&created.durable),
        })
        .unwrap();
    assert_eq!(reused.action, ReconcileAction::Reused);
    assert_eq!(reused.durable, created.durable);

    let updated = client
        .ensure_pull_request(EnsurePullRequest {
            repository: "acme/docs",
            head: &branch,
            base: "main",
            title: "Update Portuguese translations",
            body: "Reconciled translations",
            draft: false,
            durable: Some(&created.durable),
        })
        .unwrap();
    assert_eq!(updated.action, ReconcileAction::Updated);
    assert_eq!(updated.pull_request.title, "Update Portuguese translations");
    assert!(!updated.pull_request.draft);

    let calls: Vec<Value> = fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call["args"][0] == "pr" && call["args"][1] == "create")
            .count(),
        1
    );
    assert!(calls.iter().all(|call| call["lc"] == "C"));
    assert!(calls.iter().all(|call| call["prompt"] == "1"));
}
