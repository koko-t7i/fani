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
7. Deterministically verify every complete candidate. Incomplete documents stay outside the candidate set; an assembly/verification failure becomes a stable blocking `DOCUMENT-VERIFY` finding.
8. Run each configured project command once over the ordered complete candidate set in fixed-source staging. The first candidate document owns the `project_check` work, whose input and result bind the full manifest hash. No candidates means no project checks.
9. Only after all blocking checks pass, persist canonical bytes and exact immutable translation links, then materialize and publish. Failed or timed-out checks leave canonical content, targets, and publication unchanged.

Zero-unit Markdown is byte-identical pass-through, including CRLF and opaque content. It creates real document work but no fake unit, Agent attempt, translation version, or translation memory; normal canonical provenance is `imported`, not AI translation. Historical `passed` flags are not authorization: a zero-unit canonical without an outbox is revalidated and checked before new effects. Adoption checks the exact proposed full target set before trust; discard restores canonical bytes without approving publication. Already-current verified targets remain eligible for a missing publication after a crash.

## SQLite contract

Native schemas 1–4 upgrade transactionally to schema 5; pre-native databases and unknown/newer migration ledgers are rejected. Existing migrations 0001–0004 and SHA-256 checksums are immutable.

### Schema 5 upgrade and restore

Stop **all** fani processes (including idle older binaries) before upgrading, and keep them stopped until the upgrade completes. Migration takes a SQLite `BEGIN EXCLUSIVE` lock, refuses application leases except repository owners proven dead by PID/start-time identity, and holds the lock through backup, rebuild, validation, and commit. Processing outbox owners must also be proven dead, even without a repository lease. Lease expiry or an unreadable process identity alone is not proof that an application stopped: migration requires an observed start-time mismatch or kernel-confirmed absence (`ESRCH` from a signal-zero existence probe). Permission and other observation errors fail closed. The lock prevents database access during migration, not an already-running old binary from resuming afterward; mixed-version operation is unsupported.

Before modifying an existing schema, the upgrader validates its own version's ledger, marker, integrity, and foreign keys, then writes `<database>.pre-schema-<version>.bak`. This is a synced, validated, no-clobber copy made under the exclusive lock in DELETE journal mode before any migration writes. The destination directory is synced too. An existing backup blocks another upgrade attempt: retain it under another name before retrying rather than overwriting recovery evidence. Fresh databases need no backup.

Migration 0003 checks the complete recursive reverse-FK graph rooted at `work_items` and rejects unexpected dependencies or custom work-item indexes/triggers. It temporarily disables FK enforcement outside the transaction so dropping the old parent cannot cascade-delete children or null historical attempt links. All old work IDs and fields remain unit-scoped, regardless of kind; attempts, findings, both outboxes, candidate/translation histories, memory links, and canonical-file links remain unchanged. The rebuilt table has nullable `unit_id` and `document_id` with an XOR check and a document FK. Separate partial unique indexes support correctly predicated unit/document upserts. The complete expected schema contract (application ID, user version, marker, full migration ledger/checksums, integrity, and FKs) is validated before commit; failure rolls back the schema, data, ledger, and header, and FK enforcement is restored. Fault-injection regressions install triggers that restore the old marker after the schema-3 update or corrupt an old ledger checksum after migration 3 is inserted. Both failures must leave the old table definitions and all rows intact, including the old marker/header/ledger, with a valid schema-2 backup.

For rollback to an old binary, stop all processes, retain the failed/upgraded database separately, and copy the validated pre-upgrade backup back to the original database path with its original permissions. Do not combine a restored database with `-wal`, `-shm`, or `-journal` files from another database: retain those alongside the displaced database while all processes are stopped. Sync the restored file and directory, then run the **matching old binary's** doctor/integrity checks. Do not use the new binary's `Database::restore` for a downgrade: it expects the current schema and opening an old database performs an upgrade. Older binaries reject the new ledger's unknown migration; no down migration or destructive reset is part of this procedure.

Migration 0004 resolves the old same-content/new-source-revision conflict by rebuilding `canonical_content_versions` with uniqueness on `(canonical_file_id, source_revision, content_hash)`. Repeated bytes at a new revision receive a new content-version ID and exact revision-bound translation links; old revisions, bytes, links, and publication manifests are not rewritten. The migration checks incoming FKs and rejects custom indexes/triggers before the rebuild, preserves every existing ID, and uses the same exclusive backup/rollback protocol. Schema-contract validation runs after each migration and before the aggregate commit. `canonical_document_intents` stores append-only source/mapping/request bindings separately from immutable content.

Migration 0005 adds a transactional current-intent selection, initialized from the previously selected latest identity, without changing historical intent rows or content/translation links. Rebinding A after B selects the original A record. The document-work unique index includes an optional effect key so a newly necessary materialization can have separate work even when its run/document identity was seen before. Compatible pending effects retain their original work/outbox IDs; terminal effects remain history. A current target-byte check avoids redundant writes, while missing or changed targets require a new hash-fenced effect. Superseded publication keys likewise permit a new validated effect without reopening the rejected outbox.

`StateStore::enqueue_document_work_item` connects `assembly`, `materialization`, and `project_check` to the pipeline and adoption; `finish_document_work` records success/failure and exact result hashes. Unit enqueue signatures remain unchanged. Materialization recovery locates the original outbox work ID before enqueueing equivalent document work. Repository IDs scope both `supersede_materializations` and `claim_publication_locale`; cancellation retains the original work/outbox rows using supported `cancelled`/`done` states.

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

Native status/check, sync, adopt, and discard run this complete preflight before opening or snapshotting a database, acquiring a lease, recording a run, reconciling a PR, or recovering an outbox. The validated commit replaces the runtime source ref for every language in that repository invocation; a moving branch cannot introduce unvalidated sources later in the invocation. Selected input paths that resolve through filesystem aliases are rejected, including directory aliases whose final input file does not yet exist. Git blob identity remains fixed-revision and does not follow the worktree alias.

Discovery carries format, message syntax, a content-derived source-set identity, a per-file mapping identity, mapped relative path, and target template. Document work, canonical intent bindings, and outbox payloads retain source revision/hash/path, source-set and mapping identities, locale, target path, format contract, request schema, policy, and a deterministic request identity. Stored Agent receipts additionally snapshot document identity; recovery compares it with current work instead of trusting a mutated work input. Planning, publication recovery, adopt, and discard use the discovered mapping rather than the legacy template.

Compatible pending materializations are reassembled and checked under current rules, then complete their original work/outbox IDs. An unchanged source hash and mapping may remain compatible across an unrelated Git revision, without changing the original request binding. Removed or incompatible mappings cancel old materialization work before replanning. Unbound historical intents, including schema-2 payloads with no original mapping evidence, are conservatively superseded rather than reconstructing an alleged old mapping with today's configuration. Historical content remains available for source-bound translation reuse and current full-document validation. Publication recovery and both merged-PR promotion paths retain deterministic full-document and current project-check gates, including linkless content.

## Report schema 4

JSON reports expose `markdown_files`, `mdx_files`, `json_files`, `parse_failures`, `verified_documents`, and `pass_through_documents` per language and in totals. File counts count discovered files; verification counts deterministically assembled complete documents even when checks block later effects or targets are already current. Pass-through counts parsed zero-unit sources independently of writes. `files_written` still measures actual filesystem writes. `status`/`check` expose the same counters for currently reusable candidates without running project commands or models. Parse/verify diagnostics contain stable codes and no source/protected-code excerpts. JSON and MDX counts remain zero because those formats are explicitly disabled.

## Git and GitHub publication

A candidate tree starts from the fixed source commit and applies a typed allowlist of add/modify/delete target changes using a temporary index. `commit-tree` creates the candidate commit; local refs use compare-and-swap, and remote updates use force-with-lease. The checked-out branch, `HEAD`, real index bytes/staging, unrelated worktree files, database, prompts, responses, reports, and work files are never changed or published by candidate construction.

Push and pull-request operations have durable idempotency keys and reconciliation. Each repository/language has one stable branch and open pull request. Uncertain delivery is queried before retry. Merge reconciliation records the observed GitHub metadata and is the normal path for promoting merged translations to trusted memory.

## Module direction

The project remains one Cargo package and a modular monolith. `main` delegates to `cli`; command handlers compose application operations; pure Markdown/domain functions do not depend on SQLite, GitHub, or subprocesses; adapters implement coarse state, Agent, materialization, Git, and code-host boundaries. A workspace is reconsidered only when a separately versioned/published artifact exists.
