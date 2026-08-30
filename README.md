# fani

fani is a Linux-first continuous documentation translation CLI. It reads Markdown from an immutable Git revision, reuses trusted translation memory from SQLite, sends only unresolved units to a built-in model provider or an isolated custom Agent command, verifies and assembles candidates natively, and can publish one stable branch and GitHub pull request per language.

The shipped binary is Rust. It has no Python, `uv`, external i18n skill, legacy JSON state, or compatibility migration dependency.

## Safety model

- **Git source is fixed:** planning reads blobs from one resolved commit SHA, not the mutable worktree.
- **SQLite is the sole state authority:** translation memory, attempts, findings, canonical target bytes, recovery, outboxes, leases, and pull-request metadata live in `<repo>/<data_dir>/fani.db`.
- **The Agent is untrusted:** it runs in an empty temporary directory with a temporary `HOME`, an environment allowlist, bounded I/O, an absolute timeout, and process-tree cleanup. It never receives a repository path or writes repository files.
- **Markdown is assembled by byte range:** fenced code, inline code, links, HTML, and placeholders are protected by the native parser. Bytes outside translated ranges are preserved.
- **Invalid output is blocked:** protected-token and Markdown-structure verification runs before canonical content is materialized or published.
- **Project checks are deterministic:** configured documentation build/check argv arrays run after assembly and before publication in independent fixed-source staging containing the exact candidate target bytes. They have bounded time/output, process-tree cleanup, and no Agent/provider environment access.
- **Publication is isolated:** a temporary Git index builds a candidate from the fixed source commit and a typed add/modify/delete allowlist. The checked-out branch, `HEAD`, real index, staged state, and unrelated worktree files remain unchanged.
- **Trust is explicit:** only merged or explicitly adopted verified content is promoted to trusted translation memory.

## Requirements

- Rust 1.85 or newer to build;
- Linux;
- Git;
- an API key for a built-in provider: Anthropic, OpenAI, xAI, or DeepSeek;
- `gh` only when GitHub pull-request publication is enabled.

A custom command implementing fani's strict JSON protocol remains available for private or self-hosted providers.

## Install

Source install:

```bash
cargo install --path . --locked
```

## Quick start

No provider script or provider CLI is required. Set one API key, generate a safe local-only configuration, then synchronize:

```bash
export ANTHROPIC_API_KEY='...'
fani init --lang zh-CN --provider anthropic --model claude-sonnet-4-5
fani doctor
fani sync
```

`fani init` uses the current directory, scans Markdown, writes translations under `i18n/<language>/`, disables revision and publication for the first run, and refuses to overwrite an existing `fani.toml` unless `--force` is given.

Other built-in providers use the same flow:

| Provider | `--provider` | Credential environment variable | Default API |
| --- | --- | --- | --- |
| Anthropic | `anthropic` | `ANTHROPIC_API_KEY` | Messages API |
| OpenAI | `openai` | `OPENAI_API_KEY` | Chat Completions API |
| xAI | `xai` | `XAI_API_KEY` | Chat Completions API |
| DeepSeek | `deepseek` | `DEEPSEEK_API_KEY` | Chat Completions API |

For an OpenAI-compatible endpoint, set `provider = "openai-compatible"`, `endpoint`, and `api_key_env` in `fani.toml`. Official provider endpoints and credential-variable names are fixed so a repository configuration cannot redirect a standard API key. Built-in requests do not follow redirects or inherit proxy environment variables.

Tagged releases publish one `x86_64-unknown-linux-gnu` `.tar.xz` archive. The archive contains the `fani` binary, Bash/Zsh/Fish completions, the `fani(1)` man page, systemd user units, the annotated example configuration, and this README. Each archive has a SHA-256 sidecar; the release also contains a CycloneDX XML SBOM and GitHub artifact attestations for the final asset set.

```bash
sha256sum --check fani-x86_64-unknown-linux-gnu.tar.xz.sha256
gh attestation verify fani-x86_64-unknown-linux-gnu.tar.xz --repo koko/fani
```

See [the release process and asset contract](docs/release.md) for verification, installation, and exact reproducibility scope. For the locked Rust 1.85.0, cargo-dist 0.32.0, cargo-cyclonedx 0.5.9, `x86_64-unknown-linux-gnu` contract, two isolated clean clones must produce byte-identical archives, checksums, and SBOMs, with matching binary hashes/build IDs, packaged asset bytes, and extracted metadata. Other targets, host/tool versions, source revisions, and lockfiles are outside this claim.

Development checks:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --test reproducible_candidate
cargo test --locked --all-targets
cargo build --locked --release
scripts/release/verify-reproducible-release.sh
```

The `reproducible_candidate` end-to-end test creates a fixed local source repository and two independent clean clones with absent `.fani` databases and target caches. It invokes the built debug `fani` entry point in each environment with the same config and strict recorded JSON provider fixture, while inheriting the runner's `HOME`, Cargo/Rustup homes, XDG cache/config, and package state rather than redirecting them into test scratch space. The provider receives fani's isolated temporary `HOME` and an empty credential allowlist; sentinels fail the test if Python, `uv`, an external skill, or common network clients are invoked. The test verifies that SQLite is the only generated state authority, then requires equal candidate tree OIDs, raw tree bytes, target blob bytes, commit OIDs, and raw commit bytes.

## Commands

```bash
fani init --lang zh-CN --provider anthropic --model MODEL  # create starter config
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

## Diagnostics

Structured diagnostics are written only to stderr and are opt-in with `FANI_LOG`. The default format is compact human-readable output; set `FANI_LOG_FORMAT=json` for newline-delimited JSON:

```bash
FANI_LOG=info fani sync --quiet
FANI_LOG=info FANI_LOG_FORMAT=json fani sync --quiet
```

Diagnostics contain safe hashes/IDs, locale and stage names, durations, statuses, outbox IDs, and publication/provider metadata hashes. They never emit environment values, credentials, provider stderr, prompts, source Markdown, translations, protected tokens, or Agent request/response content. `--quiet` continues to suppress progress and report-location output; command errors and enabled diagnostics still use stderr. OpenTelemetry export is not included.

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
- zero or more project-specific documentation build/check commands;
- stable locale branch, remote push, and GitHub pull-request settings.

Documentation checks are configured under `[repo.documentation]` as one or more argv arrays, for example `commands = [["mdbook", "build"], ["markdownlint", "docs/zh-CN"]]`. Shell strings are not accepted. Every command receives a fresh repository-independent staging tree checked out from the fixed source revision with the exact assembled candidate files overlaid. A nonzero exit or timeout is persisted and reported as a blocking finding, so publication does not start.

A built-in Agent config only needs `provider` and `model`; `adapter` defaults to `native-http-v1`, the standard credential variable is selected automatically, and secrets remain in the process environment rather than TOML. Optional fields include `endpoint`, `api_key_env`, `max_output_tokens`, concurrency, timeout, retries, and enablement.

Advanced integrations can select `adapter = "command-json-v1"`, provide `cmd`, and explicitly list `env_allow`. fani writes a strict `fani.agent.request.v1` JSON envelope to stdin and accepts only a matching `fani.agent.response.v1` JSON envelope from stdout or `{output_file}`. This custom command path preserves the same bounded I/O, timeout, process-tree cleanup, and diagnostic redaction guarantees.

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
