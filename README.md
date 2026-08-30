# fani

fani is a Linux-first continuous documentation translation CLI. It reads Markdown from an immutable Git revision, reuses trusted translation memory from SQLite, sends only unresolved units to an isolated command-line Agent, verifies and assembles candidates natively, and can publish one stable branch and GitHub pull request per language.

The shipped binary is Rust. It has no Python, `uv`, external i18n skill, legacy JSON state, or compatibility migration dependency.

## Safety model

- **Git source is fixed:** planning reads blobs from one resolved commit SHA, not the mutable worktree.
- **SQLite is the sole state authority:** translation memory, attempts, findings, canonical target bytes, recovery, outboxes, leases, and pull-request metadata live in `<repo>/<data_dir>/fani.db`.
- **The Agent is untrusted:** it runs in an empty temporary directory with a temporary `HOME`, an environment allowlist, bounded I/O, an absolute timeout, and process-tree cleanup. It never receives a repository path or writes repository files.
- **Markdown is assembled by byte range:** fenced code, inline code, links, HTML, and placeholders are protected by the native parser. Bytes outside translated ranges are preserved.
- **Invalid output is blocked:** protected-token and Markdown-structure verification runs before canonical content is materialized or published.
- **Publication is isolated:** a temporary Git index builds a candidate from the fixed source commit and a typed add/modify/delete allowlist. The checked-out branch, `HEAD`, real index, staged state, and unrelated worktree files remain unchanged.
- **Trust is explicit:** only merged or explicitly adopted verified content is promoted to trusted translation memory.

## Requirements

- Rust 1.85 or newer to build;
- Linux;
- Git;
- at least one headless command-line Agent provider;
- `gh` only when GitHub pull-request publication is enabled.

## Install

```bash
cargo install --path . --locked
cp examples/fani.toml fani.toml
fani doctor
```

Development checks:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
cargo build --locked --release
```

## Commands

```bash
fani doctor                         # validate configuration and runtime prerequisites
fani status                         # immutable-source plan; no Agent calls
fani check                          # CI-friendly alias for status/check semantics
fani sync                           # resume/translate/verify/materialize/publish
fani adopt --repo PATH_OR_BASENAME --lang LANG  # validate and adopt a divergent human target
fani discard --repo PATH_OR_BASENAME --lang LANG # restore canonical verified target bytes
```

Common options:

```bash
fani status --config ./fani.toml --repo product-docs --lang zh-CN
fani sync --config ./fani.toml --report-dir ./reports --quiet
```

## Exit codes

| Exit | Result | Meaning |
| ---: | --- | --- |
| 0 | `ok` | Up to date, or completed and verified. |
| 1 | `needs_human` | A conflict, invalid candidate, review finding, or publication decision requires a person. |
| 2 | `error` | Configuration, environment, lock, database, process, Git, or GitHub infrastructure failed. |
| 3 | `partial` | The bounded batch succeeded and deferred units remain. |

For multiple repositories/languages, precedence is `error > needs_human > partial > ok`.

## Configuration

[`examples/fani.toml`](examples/fani.toml) is the annotated reference. Configuration tables reject unknown fields. Parsing and semantic validation finish before fani opens a database, runs Git, starts an Agent, or contacts GitHub.

A repository config defines:

- immutable source ref and Markdown include/exclude globs;
- target path pattern, languages, batch and repair bounds;
- SQLite data directory;
- optional bilingual revision and advisory proofread;
- stable locale branch, remote push, and GitHub pull-request settings.

An Agent config defines argv, concurrency, timeout, retries, enablement, and the exact environment variables copied into its isolated process. Secrets belong in the process environment, never in TOML.

## Persistent state and recovery

Each configured repository uses:

```text
<repo>/<data_dir>/fani.db
```

The fresh 0.3 schema is intentionally incompatible with experimental versions. fani does not import old `state.json`, JSONL, task files, review files, or SQLite schemas.

SQLite uses foreign keys, rollback journal mode, `synchronous=FULL`, and a bounded busy timeout. External Agent, filesystem, Git, and GitHub work is bracketed by durable intent/completion transactions. Reopening resumes completed attempts, pending materialization, and pending publication idempotently.

If a materialized target differs from its recorded canonical hash, fani reports `HUMAN-EDIT` and requires `adopt` or `discard`; sync never silently overwrites the edit.

## Reports

`fani sync` writes replaceable latest-run views to the selected report directory:

```text
report.json
report.md
```

Reports include the fixed source revision, status/exit code, reused units, Agent attempts, findings, canonical materialization, and publication results. Reports are not authoritative and are never included in locale commits.

## GitHub publication

When enabled, fani records a durable publication intent, builds a candidate commit from the fixed source revision, updates the stable locale branch with compare-and-swap/force-with-lease, and ensures one open pull request through bounded `gh` commands. An uncertain push or pull-request operation is reconciled before retry, preventing duplicate pull requests.

The database, prompts, Agent input/output, reports, and temporary work are excluded from candidate changes.

## Scheduling

User-level systemd assets are under [`systemd/`](systemd/). Exit codes 0, 1, and 3 are completed scheduler outcomes; exit 2 is an infrastructure failure. `KillMode=control-group` complements fani's process-tree cleanup.

## Architecture

- [ADR-0001: native Rust engine and single SQLite authority](docs/architecture/adr-0001-native-single-authority.md)
- [Native i18n architecture and contracts](docs/architecture/native-i18n.md)
- [2026 Rust CLI technology research](docs/research/2026-rust-cli-stack.md)
- [Superseded compatibility baseline](docs/architecture/compatibility-baseline.md)
- [Superseded Rust + SQLite rewrite design](docs/architecture/rust-sqlite-rewrite.md)
