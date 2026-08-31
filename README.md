# fani

[简体中文](i18n/zh-CN/README.md)

fani is a Linux-first CLI for continuously translating Markdown documentation. It reads source files from a fixed Git commit, reuses trusted translations from SQLite, sends only unresolved units to a built-in model provider or a strict custom Agent, verifies the result, and can publish one stable branch and GitHub pull request per language.

The shipped binary is fully native Rust. Running fani does not require Python, `uv`, an external i18n skill, or a provider adapter script.

## Requirements

- Linux;
- Git;
- either an API key for Anthropic, OpenAI, xAI, or DeepSeek, or a command implementing the custom Agent protocol;
- `gh` only when GitHub pull-request publication is enabled;
- Rust 1.85 or newer only for source builds.

## Install

For Intel/AMD 64-bit Linux with glibc 2.31 or newer:

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/koko-t7i/fani/releases/latest/download/fani-installer.sh | sh
```

No Rust toolchain is required. For verified downloads or source installation, see [Release and installation](docs/release.md).

## Five-minute start

No provider script or provider CLI is required.

Commit the source Markdown you want to translate, then run:

```bash
export ANTHROPIC_API_KEY='...'
fani init --lang zh-CN --provider anthropic --model claude-sonnet-4-5
fani doctor
fani status
fani sync
```

`fani init` creates a safe local-only `fani.toml` for the current repository. The generated globs discover Markdown during later planning, targets go under `i18n/<language>/`, each run is limited to ten tasks, and revision and publication remain disabled. It refuses to overwrite an existing configuration unless `--force` is given.

`fani status` previews the fixed Git revision and planned work without a model call. Uncommitted source edits are not part of that revision. The first `sync` materializes targets, stores authoritative state in `.fani/fani.db`, and writes `.fani-report/report.md` plus `.fani-report/report.json`; it does not push or open a pull request. Exit code 3 means the bounded run succeeded and another `fani sync` should continue the remaining work.

Add `.fani/` and `.fani-report/` to `.gitignore`. Inspect generated translations and `report.md` before enabling more expensive quality stages, remote pushes, GitHub pull requests, or unattended scheduling.

### Built-in providers

| Provider | `--provider` | Credential environment variable |
| --- | --- | --- |
| Anthropic | `anthropic` | `ANTHROPIC_API_KEY` |
| OpenAI | `openai` | `OPENAI_API_KEY` |
| xAI | `xai` | `XAI_API_KEY` |
| DeepSeek | `deepseek` | `DEEPSEEK_API_KEY` |

For a custom OpenAI-compatible service, use `provider = "openai-compatible"` with an explicit HTTPS `endpoint` and a dedicated `api_key_env`. Advanced private integrations can use the strict `command-json-v1` subprocess protocol. Keep every credential in the environment or a secret store, never in `fani.toml`.

See [Best practices](docs/best-practices.md#providers-and-credentials) and the [annotated configuration](examples/fani.toml) for both paths.

## Core workflow

```bash
fani doctor
fani status
fani sync
```

| Command | Purpose |
| --- | --- |
| `fani init` | Create a conservative starter configuration for one built-in provider. |
| `fani doctor` | Validate configuration, repositories, provider credentials or custom commands, SQLite, and optional GitHub prerequisites. |
| `fani status` | Plan from the fixed source revision without model calls. |
| `fani check` | Run the same read-only planning path with a CI-oriented name. |
| `fani sync` | Resume or perform translation, verification, materialization, and optional publication. |
| `fani adopt` | Validate a human-edited target and make it canonical trusted content. |
| `fani discard` | Replace a divergent human edit with the last canonical verified target. |

Select one configured repository or language when needed:

```bash
fani status --config ./fani.toml --repo product-docs --lang zh-CN
fani sync --config ./fani.toml --report-dir ./reports --quiet
fani adopt --repo PATH_OR_BASENAME --lang zh-CN
```

`fani sync` writes replaceable `report.json` and `report.md` views to the selected report directory. SQLite at `<repo>/<data_dir>/fani.db` remains the sole fani-owned state authority.

### Exit codes

| Exit | Result | Meaning |
| ---: | --- | --- |
| 0 | `ok` | Up to date, or completed and verified. |
| 1 | `needs_human` | A conflict, invalid candidate, review finding, or publication decision needs a person. |
| 2 | `error` | Configuration or infrastructure failed. |
| 3 | `partial` | The bounded batch succeeded and more units remain for a later run. |

For multiple repositories or languages, precedence is `error > needs_human > partial > ok`.

## Safety model

- **Fixed source:** discovery and planning read blobs from one resolved Git commit, never mutable source files in the worktree.
- **Single state authority:** translation memory, findings, canonical target bytes, recovery, and publication state live in SQLite.
- **Untrusted model output:** fani protects Markdown syntax, bounds provider I/O, and verifies candidates before materialization or publication.
- **Explicit human reconciliation:** a changed target is never silently overwritten; choose `adopt` or `discard`.
- **Isolated publication:** candidate commits use a temporary Git index and do not disturb the checked-out branch, `HEAD`, real index, or unrelated files.
- **Secrets stay outside configuration:** official providers use fixed endpoints and fixed credential-variable names; built-in requests do not follow redirects or inherit proxy environment variables.

## Configuration

[`examples/fani.toml`](examples/fani.toml) is the complete annotated reference. Unknown fields are rejected, and semantic validation finishes before fani contacts a provider or GitHub.

Important rules:

- `publish.source_ref` selects the fixed source revision for planning and synchronization even when publication is disabled;
- exclude generated translation directories so they are not translated recursively;
- keep `{lang}` and `{relpath}` in `target_pattern`—for example, `i18n/{lang}/{relpath}` maps `docs/start.md` to `i18n/zh-CN/docs/start.md`;
- configure documentation checks as argv arrays, not shell strings;
- begin with low `max_tasks`, `concurrency = 1`, revision disabled, and publication disabled;
- use one stable publication branch per language when publication is enabled.

The staged rollout and operating guidance are in [Best practices](docs/best-practices.md).

## Diagnostics

Diagnostics are opt-in and go only to stderr:

```bash
FANI_LOG=info fani sync --quiet
FANI_LOG=info FANI_LOG_FORMAT=json fani sync --quiet
```

They include safe identifiers, durations, statuses, and provider/publication metadata hashes. They exclude credentials, environment values, prompts, source Markdown, translations, protected tokens, and provider request/response bodies.

## Documentation

- [Documentation map](docs/README.md) — which document to use and which contracts are current.
- [Best practices](docs/best-practices.md) — providers, credentials, repository layout, daily operation, human edits, publication, scheduling, and CI.
- [Annotated configuration](examples/fani.toml) — complete configuration fields and examples.
- [Release process and asset contract](docs/release.md) — installation verification, release assets, reproducibility, and maintainer gates.
- [Native architecture contract](docs/architecture/native-i18n.md) — active behavior and authority boundaries.
- [ADR-0001](docs/architecture/adr-0001-native-single-authority.md) — why fani uses native Rust and one SQLite authority.

Historical design documents are labeled as superseded in the [documentation map](docs/README.md#historical-records) and are not usage guides.
