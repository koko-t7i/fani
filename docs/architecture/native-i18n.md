# Native i18n architecture

Date: 2026-08-30  
Status: implementation contract for fani 0.3  
Decision record: [`ADR-0001`](adr-0001-native-single-authority.md)

## Product boundary

fani 0.3 is a Linux-first continuous documentation translation CLI for GitHub repositories. It supports Markdown/CommonMark/GFM, built-in HTTPS model providers, a strict custom command Agent protocol, SQLite state, Git branch publication, and GitHub pull requests. MDX, resource files, other forges, automatic merge, a Web UI, and compatibility with the experimental Python/i18n-skill implementation are outside this version.

## Authority

- A fixed Git commit is authoritative for each source plan.
- One SQLite database at `<repo>/<data_dir>/fani.db` is authoritative for every fani-owned document, unit, translation-memory, work, review, recovery, and publication record.
- Agent output is untrusted candidate text.
- A target translation becomes trusted memory only after merge reconciliation or explicit verified adoption.
- Reports are replaceable views. Prompts and migrations are embedded resources. Neither is a second state authority.

No runtime path invokes an external i18n skill, Python, or `uv`. No old JSON, JSONL, lock, or SQLite format is imported.

## Command contract

| Command | Side effects | Meaning |
| --- | --- | --- |
| `fani init` | Atomically creates a validated configuration; refuses overwrite unless `--force` is given | Create a conservative local-only starter configuration for a built-in provider. |
| `fani doctor` | May create/open the configured SQLite database only after strict config validation | Validate config, Git repositories, built-in provider credentials or custom Agent commands, SQLite, and optional GitHub prerequisites. |
| `fani status` | Read-only with respect to repositories and Agents | Plan from the immutable source revision and report pending/reused/conflicting work. |
| `fani check` | Read-only with respect to repositories and Agents | Alias of the deterministic planning/check path for CI readability. |
| `fani sync` | Durable SQLite work, bounded Agent calls, verified materialization/publication | Resume or perform translation work. |
| `fani adopt` | Durable reconciliation after deterministic validation | Adopt a verified human-edited target as canonical content and trusted translation memory. |
| `fani discard` | Durable reconciliation | Discard a divergent human edit and restore the canonical verified target. |

Stable process outcomes are `0 ok`, `1 needs_human`, `2 error`, and `3 partial`. Configuration uses `serde(deny_unknown_fields)` at every table and is fully parsed and validated before database, repository, Agent, Git, or GitHub side effects.

## Decision codes

| Code | Meaning |
| --- | --- |
| `CFG-INVALID` | Invalid or unknown configuration. |
| `LOCK-BUSY` | Another live lease owns the repository/language. |
| `SRC-CHANGED` | The requested source revision no longer satisfies a guarded operation. |
| `HUMAN-EDIT` | Materialized target differs from the recorded canonical bytes. |
| `MATCH-AMBIGUOUS` | Unit continuity cannot be selected deterministically. |
| `AGENT-EXIT` | Provider failed to execute successfully. |
| `AGENT-TIMEOUT` | Absolute provider deadline expired. |
| `AGENT-INVALID` | Provider output is empty, oversized, malformed, or violates its envelope. |
| `VERIFY-FAILED` | Candidate violates protected tokens or Markdown structure. |
| `PUBLISH-CONFLICT` | Git compare-and-swap or remote lease failed. |
| `PUBLISH-RETRY` | A durable publication intent remains retryable. |

## Native document processing

1. Resolve `publish.source_ref` to one commit SHA.
2. Discover configured Markdown blobs with `git ls-tree`; read each blob from that SHA, never from the mutable worktree.
3. Parse with `pulldown-cmark` offset events and derive stable document/unit identity.
4. Protect code, link destinations, HTML, placeholders, and non-translatable frontmatter ranges.
5. Reuse only exact trusted translation memory before an Agent call. Fuzzy results provide context or a deterministic conflict; they are never silently published.
6. Assemble with descending byte-range replacement. Empty replacement is byte-identical; bytes outside selected ranges remain byte-identical.
7. Verify token identity, range validity, Markdown structure, and configured project checks.
8. Persist canonical target bytes before materialization or publication.

## SQLite contract

The schema is intentionally destructive for 0.3. Opening a file with an unsupported application ID or schema version fails with reset guidance; no compatibility migration runs.

The model covers repositories and languages, source revisions, documents and unit occurrences, translation versions and trusted memory, runs and work items, completed attempts, findings and transitions, canonical candidate blobs, materialization/publication outboxes, pull-request lifecycle, and renewable leases.

Connection policy is `foreign_keys=ON`, rollback journal, `synchronous=FULL`, and a bounded busy timeout. Transactions contain only local state transitions. Agent, filesystem, Git, and network operations occur after durable intent commits and before short completion transactions.

## Agent boundary

The provider receives one typed task rendered by an embedded versioned prompt. It runs in an empty temporary working directory with a temporary `HOME`, an explicit environment allowlist, bounded stdin/stdout/stderr/result files, an absolute deadline, process-group TERM/KILL escalation, descendant cleanup, and no repository path. fani validates the returned text and performs every file, database, and publication operation itself.

### Request v2 upgrade

The `command-json-v1` adapter name is unchanged, but commands must now accept `fani.agent.request.v2`. The strict envelope contains `schema`, `task`, `prompt` (`version`, `resource`, `hash`, `content`), and `policy` (`fingerprint`). Response remains `fani.agent.response.v1`, with exactly `schema`, `task_id`, and `output`. Unknown response fields, a mismatched task ID, or an unsupported response schema are rejected. Return unit text rather than a serialized document; fani owns escaping, assembly, and verification.

Every task, including translate-with-previous-source, repair, blocking revision, and advisory proofread, carries these additional required fields:

| Field | Meaning |
| --- | --- |
| `source_format` | Stable `markdown`, `json`, or `mdx` discriminator; this build dispatches only Markdown. |
| `unit_context` | PR1 format, context version, structural path, and token contract. |
| `context_key` | Source-document-bound stable memory context, including kind and semantic format contract; not an absolute repository path. |
| `message_syntax` | `null` for Markdown; reserved typed values are `plain` and `i18next-interpolation-v1`, not enabled JSON support. |
| `token_permissions` | Versioned `contract` and explicit `reorderable_tokens` allowlist; every token must still appear exactly once, unchanged. |

Only listed Markdown inline-code tokens may move when meaning and associations remain intact. Structural tokens retain source order and nesting. JSON interpolation permissions must eventually come from its message dialect, never from model output. MDX executable syntax must never be translated or introduced. Prompt resources and rendered context expose the same constraints for all stages. Prompt hashes change independently of PR1's Markdown semantic compatibility fingerprint; a prompt upgrade does not by itself invalidate verified trusted Markdown memory. Old request receipts cannot serve as current v2 review approvals.

### Source sets and fixed-revision preflight

Absent `sources` retains legacy repo-level `include`, `exclude`, and `target_pattern`; absent or empty legacy include keeps Markdown defaults. Explicit `sources` must be nonempty and cannot coexist with any explicitly provided legacy field, including `include = []`, `exclude = []`, or an empty target pattern. There is no implicit Markdown set and no inherited global exclude in explicit mode.

Configuration loading validates formats, globs, path components, placeholders, and mapping shape without Git, SQLite, or Agent calls. JSON and MDX remain unavailable and are explicitly rejected. Fixed-revision discovery then validates component-based `strip_prefix`, exact `.md` extensions, missing single-file inputs, source-set overlap, and targets for **every configured language**, even when the command selects only one language. Targets cannot collide (including file/directory ancestry), overwrite inputs, match any effective source rule, or overlap `.git`, configured state, or report directories. Sync includes its custom `--report-dir` reservation; status/check share discovery and default reservations without a report-dir option.

Discovery carries format, message syntax, a content-derived source-set identity, a per-file mapping identity, mapped relative path, and target template. Planning, publication verification/recovery, adopt, and discard use the discovered mapping rather than the legacy template. Durable mapping identity comparison across configuration changes remains part of PR2's separate pipeline integration, not a claim implied by these fields.

## Git and GitHub publication

A candidate tree starts from the fixed source commit and applies a typed allowlist of add/modify/delete target changes using a temporary index. `commit-tree` creates the candidate commit; local refs use compare-and-swap, and remote updates use force-with-lease. The checked-out branch, `HEAD`, real index bytes/staging, unrelated worktree files, database, prompts, responses, reports, and work files are never changed or published by candidate construction.

Push and pull-request operations have durable idempotency keys and reconciliation. Each repository/language has one stable branch and open pull request. Uncertain delivery is queried before retry. Merge reconciliation records the observed GitHub metadata and is the normal path for promoting merged translations to trusted memory.

## Module direction

The project remains one Cargo package and a modular monolith. `main` delegates to `cli`; command handlers compose application operations; pure Markdown/domain functions do not depend on SQLite, GitHub, or subprocesses; adapters implement coarse state, Agent, materialization, Git, and code-host boundaries. A workspace is reconsidered only when a separately versioned/published artifact exists.
