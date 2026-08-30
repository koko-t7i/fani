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
| `fani doctor` | May create/open the configured SQLite database only after strict config validation | Validate config, Git repositories, Agent executables, SQLite, and optional GitHub prerequisites. |
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

## Git and GitHub publication

A candidate tree starts from the fixed source commit and applies a typed allowlist of add/modify/delete target changes using a temporary index. `commit-tree` creates the candidate commit; local refs use compare-and-swap, and remote updates use force-with-lease. The checked-out branch, `HEAD`, real index bytes/staging, unrelated worktree files, database, prompts, responses, reports, and work files are never changed or published by candidate construction.

Push and pull-request operations have durable idempotency keys and reconciliation. Each repository/language has one stable branch and open pull request. Uncertain delivery is queried before retry. Merge reconciliation records the observed GitHub metadata and is the normal path for promoting merged translations to trusted memory.

## Module direction

The project remains one Cargo package and a modular monolith. `main` delegates to `cli`; command handlers compose application operations; pure Markdown/domain functions do not depend on SQLite, GitHub, or subprocesses; adapters implement coarse state, Agent, materialization, Git, and code-host boundaries. A workspace is reconsidered only when a separately versioned/published artifact exists.
