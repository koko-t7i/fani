# Best practices

This guide describes a conservative path from the first local translation to unattended GitHub publication. Keep the generated `fani init` defaults until each earlier stage has produced a clean result.

## Recommended rollout

1. **Start locally:** run `fani init`, keep revision and publication disabled, use `max_tasks = 10`, and set provider concurrency to 1.
2. **Validate before spending tokens:** run `fani doctor`, then `fani status`. Review the fixed source revision, discovered documents, pending units, and conflicts.
3. **Run one bounded sync:** run `fani sync`, inspect the materialized targets and both reports, and resolve every `needs_human` result.
4. **Add repository checks:** configure deterministic documentation build or lint commands and confirm candidates pass them.
5. **Increase quality and throughput gradually:** enable revision only after translation quality is acceptable; raise `max_tasks` and concurrency in small steps while observing provider limits and cost.
6. **Enable local publication:** enable candidate branch creation while leaving remote push and GitHub pull requests disabled.
7. **Enable remote publication:** use a dedicated stable branch per language, least-privilege credentials, and required checks.
8. **Schedule only after recovery is familiar:** practise `adopt`, `discard`, and handling exit codes 1, 2, and 3 before enabling the timer.

This order keeps the first run cheap and reversible. It also separates translation quality problems from GitHub permissions, branch protection, and scheduler configuration.

## Providers and credentials

### Prefer a built-in provider for common services

Built-in Anthropic, OpenAI, xAI, and DeepSeek support needs only a provider name, model, and standard environment variable. No adapter script or provider CLI is needed.

| Provider | Configuration value | Credential variable |
| --- | --- | --- |
| Anthropic | `anthropic` | `ANTHROPIC_API_KEY` |
| OpenAI | `openai` | `OPENAI_API_KEY` |
| xAI | `xai` | `XAI_API_KEY` |
| DeepSeek | `deepseek` | `DEEPSEEK_API_KEY` |

Official provider endpoints and credential-variable names are fixed. This prevents repository configuration from redirecting a standard provider key. Built-in clients also reject redirects and do not inherit proxy environment variables.

OpenAI and OpenAI-compatible native agents may set `reasoning_effort` to `none`, `minimal`, `low`, `medium`, `high`, or `xhigh`. Confirm that the selected provider and model support the value. Leave it unset to preserve the provider's default behavior; fani omits the field entirely. The setting is part of the provider fingerprint, so changing it cannot reuse attempts made with a different effort.

Use `openai-compatible` only for a service you trust. Give it an explicit HTTPS endpoint and a dedicated credential variable that is not reused for an official provider:

```toml
[agents.primary]
provider = "openai-compatible"
model = "example-model"
endpoint = "https://models.example.com/v1/chat/completions"
api_key_env = "FANI_EXAMPLE_API_KEY"
concurrency = 1
timeout_s = 300
retries = 2
enabled = true
```

Use `command-json-v1` for a private integration that cannot expose an OpenAI-compatible HTTPS API. The command must implement fani's strict versioned JSON request and response envelopes. Allow only the environment variables it needs:

```toml
[agents.primary]
adapter = "command-json-v1"
provider = "private"
model = "example-model"
cmd = ["fani-provider"]
env_allow = ["PRIVATE_PROVIDER_TOKEN"]
concurrency = 1
timeout_s = 300
retries = 2
enabled = true
```

### Keep secrets out of the repository

- Store keys in the process environment, a local mode-`0600` environment file, or the CI secret store.
- Never put a key in `fani.toml`, command arguments, reports, or committed shell files.
- Use a separate key per environment and provider where possible; rotate it if a job log or repository may have exposed it.
- Give GitHub credentials only the repository permissions required for branch updates and pull requests.
- Run `fani doctor` in the same environment as `fani sync`; a successful interactive check does not prove a systemd or CI environment has the same credentials.

fani diagnostics intentionally omit provider bodies, prompts, translations, credentials, and environment values. Keep this property when wrapping fani with other tooling.

## Repository layout

The starter layout is appropriate for most repositories:

```text
README.md
fani.toml
docs/
i18n/
  zh-CN/
    README.md
    docs/
.fani/
  fani.db
```

Recommended configuration rules:

- Include only source Markdown that should be translated.
- Exclude generated target roots such as `i18n/**`; otherwise translated files can be discovered as new sources.
- Exclude archives, generated API references, vendored content, changelogs, or legal files unless they are intentionally in scope.
- Legacy `target_pattern` requires `{lang}` and `{relpath}`. Explicit source sets may omit `{relpath}` only for one exact non-glob filename; preflight rejects missing inputs and target collisions across all configured languages.
- Keep `data_dir` inside the repository root but outside publication targets. Do not commit `<data_dir>/fani.db`.
- Keep reports outside translated target paths. Reports are replaceable views, not state or publication inputs.
- Use one configuration for related repositories only when they share an operating schedule and credential boundary. Otherwise use separate files and invocations.

fani reads source blobs from `publish.source_ref`, not from mutable worktree source files. Set it to the branch or ref that represents approved source documentation, normally `origin/main` in a continuously fetched checkout.

### Explicit source sets and upgrade safety

For independent directory or filename mappings, replace the repo-level `include`, `exclude`, and `target_pattern` fields with `[[repo.sources]]` tables (see the annotated configuration). Do not leave even empty legacy fields alongside source sets: the two modes are mutually exclusive. Every set declares `format = "markdown"` or `format = "json"`, a nonempty `include`, optional `exclude` and `strip_prefix`, and a target pattern containing `{lang}`. No default set or global exclude is added.

`strip_prefix = "website/docs/"` removes directory components, not a string substring, and every matching input must be below it. `{relpath}` is the remaining full relative filename. A single-file mapping such as `include = ["README.md"]` and `target_pattern = "readme/{lang}.md"` needs no `{relpath}`. More complex filename renaming is expressed with one set per file. Language values are used verbatim: `zh-CN` does **not** become `zh`; locale aliases are not supported.

**Upgrade boundary:** legacy globs that match non-`.md` files now fail preflight with source-set migration guidance instead of parsing arbitrary extensions as Markdown. Narrow or exclude those paths. JSON requires an explicit source set and `message_syntax = "plain"` or `"i18next-interpolation-v1"`. MDX remains unavailable because parser safety gates are blocked. Renaming a file or relying on content sniffing does not enable another format. `fani init` remains Markdown-only.

Preflight rejects generated paths that match any effective source rule, even if the targets do not exist yet. Always exclude generated target roots when using broad includes. It also checks all configured languages, source overlaps, target collisions and input/state/report/Git path protection before database opening, leases, PR reconciliation, recovery, or translation. Selected source files and directories must not alias another filesystem path, even if their Git blobs are valid. The validated commit remains pinned across all languages in the invocation. Custom command providers must upgrade to [request v2](architecture/native-i18n.md#request-v2-upgrade); response v1 and the `command-json-v1` adapter name remain unchanged.

### JSON message resources

Use `plain` only for ordinary prose without template candidates. Braces, printf-style parameters, `$t(...)`, rich tags, and common custom delimiters are rejected rather than silently protected. `i18next-interpolation-v1` supports only `{{name}}` and `{{user.name}}`, with each name segment matching `[A-Za-z_][A-Za-z0-9_]*` and optional leading/trailing ASCII whitespace inside the delimiters. Parameter names, occurrences, and literal delimiter whitespace must remain unchanged; independent arguments may reorder within their own message.

ICU plural/select/selectordinal, i18next plural/ordinal/legacy-number key suffixes, unescaped or formatted parameters, nesting, Trans/rich text, and custom delimiter configurations are unsupported. `MESSAGE-UNSUPPORTED` requires a person to choose a supported resource dialect or exclude the file. Not every application's custom syntax can be inferred: callers must use the fixed declared dialect, not treat it as full i18next support.

JSON Pointer identity is document-scoped and arrays are index-based. A source change at the same pointer requires translation; the previous target is context only. Adopt reparses the complete target and checks immutable structure and parameters before trust. For arrays consisting only of translatable strings, automatic checks cannot prove a human did not exchange their meanings; semantic review remains necessary. Empty/whitespace-only values and documents with no selected strings are exact pass-through with no model calls. Node and the browser consumer fixture are test-only, not binary runtime dependencies.

## Translation quality and cost

Start with bounded work:

```toml
max_tasks = 10
repair_budget = 2

[repo.quality]
revision = false
proofread = false

[agents.primary]
concurrency = 1
```

Then tune one dimension at a time:

- Raise `max_tasks` to change the maximum work completed by one run. Exit code 3 means the bounded batch succeeded and later work remains.
- Raise concurrency only after confirming provider rate limits, latency, and account spend.
- Enable revision after the base translation prompt and terminology are producing acceptable output.
- Enable proofread only when its advisory findings have a defined human workflow.
- Keep retry counts small. Deterministic configuration, authentication, and response-shape failures need correction, not repeated calls.

Configure repository-specific checks before publication. Commands are argv arrays rather than shell strings:

```toml
[repo.documentation]
commands = [
  ["mdbook", "build"],
  ["markdownlint", "docs/zh-CN"],
]
timeout_s = 120
```

Each command runs once per ordered complete candidate set in independent fixed-source staging with the exact candidate targets overlaid and without provider credentials. Incomplete documents are excluded; an empty set runs no commands. A failed or timed-out check returns `needs_human` without replacing canonical content or files, or publishing that set. Make checks deterministic, non-interactive, and independent of mutable local build products.

Zero-unit Markdown passes through byte-identically without model calls or fabricated translation memory. Report schema 4 separates discovered files, parse failures, verified documents, and pass-through from actual writes, so an unchanged rerun can report verified documents and zero writes. Before upgrading, stop all processes and retain the schema-4 upgrader's pre-upgrade backup. Historical pending intents lacking original mapping evidence are superseded and replanned under current rules; do not infer their original mappings from today's configuration.

## Daily operation

A normal manual run is:

```bash
git fetch --prune origin
fani doctor
fani status
fani sync --report-dir ./reports
```

Operational rules:

- Fetch the configured source ref before planning; fani intentionally resolves the local Git view of that ref.
- Read `report.md` after every nonzero exit and retain `report.json` when automation needs structured results.
- Re-run `fani sync` after exit 3. Completed attempts and canonical state are durable, so the next run resumes instead of starting over.
- Treat exit 1 as a decision queue, not an infrastructure incident.
- Treat exit 2 as an operational failure that should alert and stop publication automation.
- Avoid deleting `.fani/fani.db` to clear an error. It contains translation memory, canonical bytes, recovery intents, and publication state.
- Back up the database together with the repository identity and configuration when moving a long-lived installation.

### Human edits: adopt or discard

If a materialized target differs from its recorded canonical hash, fani reports `HUMAN-EDIT` and does not silently overwrite it.

Review the file and choose explicitly:

```bash
fani adopt --repo product-docs --lang zh-CN
# or
fani discard --repo product-docs --lang zh-CN
```

Use `adopt` when the human edit is correct and should become canonical trusted translation memory. Adoption still performs deterministic validation. Use `discard` when the edit is accidental or obsolete and the last canonical verified target should be restored.

Resolve human edits before unattended publication. Repeatedly discarding intentional reviewer changes wastes work and undermines the trust model.

## Git and GitHub publication

Enable publication in stages:

1. Set `[repo.publish].enabled = true` with `push = false` and inspect the candidate branch/commit behavior locally.
2. Set a stable branch pattern such as `i18n/{lang}`. Do not generate a new branch name for every run.
3. Enable `push` only after the remote and credentials are correct.
4. Enable `[repo.publish.github]` only after `gh auth status` succeeds in the same runtime environment.
5. Configure required checks and branch protection on the base branch before relying on unattended pull requests.

fani builds candidate commits from the fixed source commit with a typed target-file allowlist. Remote updates use compare-and-swap/force-with-lease semantics, and one stable open pull request is maintained per repository and language. It does not automatically merge pull requests.

Use a dedicated automation identity where practical. Grant the minimum repository permissions needed to push locale branches and create or update pull requests; do not expose release, administration, or unrelated organization credentials to translation jobs.

## Scheduling with systemd

The templates in [`../systemd/`](../systemd/) run one hourly user service with randomized delay. Their packaged paths match the release installation: `~/.local/bin/fani`, `~/.config/fani/fani.toml`, and `~/.local/state/fani/reports`. Before installing them:

- adjust `ExecStart` if fani or its configuration is installed elsewhere;
- add every configured repository parent and the report/state parent to `ReadWritePaths`;
- put provider credentials in `~/.config/fani/env` with mode `0600`;
- run the exact `ExecStart` command manually;
- confirm exit codes 1 and 3 are accepted outcomes and exit 2 remains a service failure;
- verify the configured source ref is fetched by a separate trusted process when the checkout is not updated elsewhere.

Install and inspect the user units:

```bash
install -Dm644 systemd/fani.service "$HOME/.config/systemd/user/fani.service"
install -Dm644 systemd/fani.timer "$HOME/.config/systemd/user/fani.timer"
systemctl --user daemon-reload
systemctl --user enable --now fani.timer
systemctl --user list-timers fani.timer
journalctl --user -u fani.service -n 50
```

A timer is not a substitute for alerting. Monitor exit 2 and repeated exit 1 results, and periodically confirm that exit 3 runs eventually drain the pending queue.

## CI and trusted automation

Use `fani check` in ordinary pull-request CI when you need deterministic planning without model calls. Configuration validation still requires the configured provider credential variable to be present, so supply a non-secret placeholder only for this read-only command:

```bash
ANTHROPIC_API_KEY=check-only-placeholder fani check --config ./fani.toml
```

Use the variable matching the configured official provider. This placeholder is safe only because `check` never contacts the provider; never reuse this pattern with `sync`.

Keep model-backed `fani sync` out of workflows triggered by untrusted pull requests. Such workflows can expose secrets, spend provider quota, publish attacker-controlled branches, or run repository-defined documentation commands with trusted credentials.

A safer split is:

- **Pull-request CI:** build and test fani/configuration changes without provider or GitHub write credentials; use a dedicated CI configuration with `publish.source_ref = "HEAD"` so the checked-out pull-request merge commit is planned, and run `fani check` only when the required local repository and state are available.
- **Trusted synchronization:** run `fani sync` from a scheduled or manually dispatched workflow on the protected default branch with environment approval and least-privilege secrets.
- **Publication checks:** validate locale pull requests with normal repository documentation tests, independent of provider access.
- **Release CI:** keep fani's own formatting, Clippy, tests, minimum-Rust, dependency audit, policy, and reproducibility gates separate from translation synchronization.

SQLite is durable application state, not a disposable dependency cache. A hosted runner must restore and persist the exact database safely, or use a long-lived protected runner. Never let concurrent jobs write the same database or locale publication state.

### Repository workflow template

[`.github/workflows/provider-sync.yml`](../.github/workflows/provider-sync.yml) is a trusted default-branch state-persistence template, not a universal drop-in publication workflow. It runs only for default-branch pushes, schedules, or manual dispatches; serializes runs per repository; restores an authoritative SQLite file from `refs/heads/fani-state`; and publishes that file with compare-and-swap after synchronization.

Before enabling it:

- set repository variable `FANI_CONFIG_PATH` to the CI-specific configuration;
- ensure `FANI_STATE_DB` exactly matches the selected repository's `<data_dir>/fani.db`;
- keep the configuration to one persisted repository/database, or invoke fani with a repository selection and create a separate protected state design for every additional database;
- replace local absolute repository paths produced by `fani init` with paths valid in the runner checkout;
- keep `publish.push = false` and GitHub pull-request publication disabled unless you separately add narrowly scoped Git/`gh` authentication to the provider step;
- add only the credential required by the configured provider.

The checked-in template currently injects only `ANTHROPIC_API_KEY`. OpenAI, xAI, DeepSeek, OpenAI-compatible services, and custom Agents require explicit secret and environment wiring. Avoid injecting every provider secret into one job; select the provider deliberately and expose only its credential.

Treat `fani-state` as protected application state rather than a cache. Restrict who can update or delete it, back it up, and never persist SQLite journal, WAL, or shared-memory side files. The workflow's `contents: write` permission is used by the state restore/publish steps; checkout credentials are not persisted, and the provider step does not receive Git or `gh` publication credentials.

## Troubleshooting order

When a run fails, diagnose in this order:

1. `fani doctor` for configuration, credentials, Git, SQLite, custom Agent, and GitHub prerequisites.
2. `fani status` for the resolved source revision and deterministic planning conflicts.
3. `report.md` and `report.json` for the stage and decision code.
4. `FANI_LOG=info` for redacted operational diagnostics.
5. Git remote and `gh auth status` for publication-only failures.

Use `FANI_LOG_FORMAT=json` when a log collector needs newline-delimited JSON. Do not add wrappers that print environment variables, provider payloads, or translated content merely to make a failure easier to inspect.
