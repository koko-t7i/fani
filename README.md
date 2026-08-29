# fani

fani keeps translated documentation synchronized with its source. It watches configured repositories, asks a headless Agent CLI to translate only changed chunks, validates the result through the i18n skill, and publishes verified translations to per-language Git branches.

The shipped `fani` executable is implemented in Rust. fani-owned durable run state, Agent call history, migrations, and repository locks are stored in SQLite. The external i18n skill continues to own its translation-memory JSON protocol.

## Safety model

The model is used as a text function, not as an operator. Each Agent invocation receives one prompt on stdin and returns one text result through stdout or a configured output file. fani—not the Agent—reads tasks, writes result JSON, decides whether output is acceptable, modifies files, and publishes Git commits.

A normal successful run is:

```text
plan → dispatch → apply → verify → publish
```

Repair and review are conditional branches only:

```text
assembly rejected → one targeted assembly retry
verify failed      → bounded repair loop
revision enabled   → blocking bilingual review
proofread enabled  → advisory review
```

Exhausted Agent retries stop before apply, avoiding the previous redundant apply/redispatch failure path.

## Requirements

- Rust 1.85 or newer to build;
- Git;
- [`uv`](https://docs.astral.sh/uv/) and the external i18n skill;
- at least one headless Agent CLI, such as Claude Code, Codex, or Grok.

The installed fani binary has no Python runtime dependency of its own. The current external i18n skill still uses `uv` and Python behind `scripts/run.sh`.

## Install

```bash
cargo install --path .
cp examples/fani.toml fani.toml
fani doctor
```

For development:

```bash
cargo build
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

## Commands

```bash
fani doctor    # validate config, skill, uv, Agent binaries, repositories and SQLite
fani status    # plan only; no Agent calls and no translation cost
fani sync      # translate, validate, record, report and optionally publish
```

Common filters:

```bash
fani status --repo product-docs --lang zh-CN
fani sync --config ./fani.toml --report-dir ./reports --quiet
```

## Exit codes

| Exit | Meaning | Scheduler action |
| ---: | --- | --- |
| 0 | Up to date, or translated and verified | Nothing |
| 1 | A human decision is required | Read `report.md` |
| 2 | Configuration, environment, lock, skill, or infrastructure error | Fix the environment |
| 3 | This bounded batch succeeded; deferred chunks remain | Run again sooner |

For multiple repositories/languages, severity is `error > needs_human > partial > ok`.

## Configuration

`examples/fani.toml` is the annotated reference. It defines:

- repositories, languages, paths and safety limits;
- Agent argv, stages, concurrency, timeout and retries;
- stage-to-Agent routing;
- Git branch, commit and push behavior.

Secrets never belong in TOML. Agent CLIs inherit credentials from the execution environment or the systemd `EnvironmentFile`.

## Persistent state

Each configured repository stores fani's database at:

```text
<repo>/<state_dir>/fani.db
```

SQLite contains:

- repeatable schema migration history;
- CLI run and per-language outcomes;
- state-machine transitions;
- Agent task outcomes and attempt counts;
- repository lock ownership;
- idempotent snapshots imported from old JSON/JSONL records.

SQLite is authoritative for locking. While the previous Python release remains a possible rollout or rollback peer, Rust also holds a transient `<state_dir>/fani.lock` marker with the predecessor's PID/time format. This prevents old and new executables from concurrently mutating the external skill's `state.json`; the marker is removed on release and is not durable history.

Connection policy is `foreign_keys=ON`, rollback journal, `synchronous=FULL`, and a bounded busy timeout. Long Agent, skill, and Git calls never hold a database transaction.

JSON remains only at intentional boundaries:

- `state.json`, task/result JSON and review JSON owned by the external i18n skill;
- `report.json` as a latest-run machine snapshot;
- `report.md` as the human report.

On first and later runs, legacy external state and old Python dispatch/verify records are imported idempotently by path and SHA-256 digest. They are not deleted. This allows rollback to the old executable while making SQLite the only write source for fani-owned durable history.

## Reports

`fani sync` writes:

```text
<report-dir>/report.json
<report-dir>/report.md
```

The report includes status, exit code, database paths, files written, conflicts, findings, task outcomes, retries, repair rounds, deferred tasks, and Git publication details. SQLite is authoritative for historical runs; reports are replaceable latest-run views.

Keep reports outside translated repositories, or explicitly exclude the report directory.

## Git publication

Verified translations are committed to a stable branch per language, `i18n/{lang}` by default. fani uses a temporary Git index and plumbing commands:

```text
read-tree → update-index → write-tree → commit-tree → update-ref
```

This guarantees that publication does not switch the checked-out branch, move HEAD, modify the real index, or include unrelated worktree changes. Only apply-reported translated files and the external skill's `state.json`, `glossary.json`, and `style.json` are eligible.

`fani.db`, SQLite journals, work directories, and locks are intentionally local and are never committed to translation branches.

## Scheduling

Install the user service and timer:

```bash
mkdir -p ~/.config/systemd/user
cp systemd/fani.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now fani.timer
```

The service treats exits 0, 1 and 3 as completed scheduler outcomes. Exit 2 is an infrastructure failure. `KillMode=control-group` ensures that stopping the service also terminates descendant Agent processes.

## Architecture and research

- [Rust + SQLite rewrite design](docs/architecture/rust-sqlite-rewrite.md)
- [Compatibility baseline](docs/architecture/compatibility-baseline.md)
- [2026 Rust CLI technology research](docs/research/2026-rust-cli-stack.md)
