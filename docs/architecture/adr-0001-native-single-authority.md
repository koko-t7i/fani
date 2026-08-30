# ADR-0001: Native Rust engine and single SQLite authority

- **Status:** Accepted
- **Date:** 2026-08-30
- **Decision owners:** fani maintainers
- **Supersedes:** the experimental external-skill/Python compatibility architecture documented in [`compatibility-baseline.md`](compatibility-baseline.md) and [`rust-sqlite-rewrite.md`](rust-sqlite-rewrite.md)

## Context

The experimental pre-release design split fani-owned state between SQLite and files maintained by an external Python i18n skill. Runtime operation depended on Python, `uv`, a user-installed skill checkout, JSON/JSONL protocols, and compatibility locking. Planning could observe mutable worktree content, and file/state publication could diverge after interruption.

fani has not shipped a compatibility contract. Preserving that experimental boundary would retain two authorities, make clean CI execution dependent on user state, and prevent atomic recovery across translation, materialization, and publication.

## Decision

fani is a single Rust package with one library and one binary. The dependency direction is:

```text
main -> cli -> application -> domain
                    ^
                    |
                 adapters
```

The application layer owns the coarse I/O ports `StateStore`, `AgentExecutor`, `Materializer`, `GitPublisher`, and `CodeHost`. Concrete SQLite, HTTPS provider, process, filesystem, Git, and GitHub adapters are wired only at the outer composition root. The domain contains pure Markdown extraction, matching, protected-syntax handling, assembly, and verification.

The runtime authority boundaries are:

1. A resolved Git commit is authoritative for source bytes used by a run.
2. One fani-identified SQLite database is the sole fani-owned authority for document and unit identity, translation memory, work attempts, findings, canonical content, recovery intents, leases, publication, and pull-request metadata.
3. Agent output is untrusted candidate text. Agents receive bounded typed work in an isolated temporary directory and never write repository or database state.
4. The target worktree is a materialized projection. Divergence from canonical bytes is reported as `HUMAN-EDIT` and requires explicit `adopt` or `discard`.
5. Git publication starts from the recorded source commit and uses typed changes plus compare-and-swap updates. Reports and temporary protocol files are replaceable evidence, not authority.

Numbered SQL migrations and versioned prompts are embedded into the binary. Experimental SQLite schemas are reset explicitly; they are not migrated. No runtime path imports old JSON/JSONL state, invokes an external i18n skill, creates a Python-compatible lock, or requires Python or `uv`.

## Consequences

### Positive

- Clean-clone builds and CI need only the declared Rust toolchain, lockfile, Git, SQLite support bundled by `rusqlite`, and configured external provider/GitHub executables.
- Durable state transitions and outboxes support idempotent recovery without repeating completed Agent work.
- Pure native Markdown behavior can be tested independently from SQLite, Git, processes, and GitHub.
- Fixed-source planning and publication prevent mutable-worktree races and preserve unrelated checkout state.

### Costs and constraints

- Existing experimental state must be discarded through the explicit reset path.
- The initial product remains Linux-first, Markdown/CommonMark/GFM-only, GitHub-only, and command-line-provider-only.
- Schema migrations and prompt resources are append-only/versioned release inputs and must be reviewed like code.
- SQLite remains deployment-local authority; hosted persistence requires a separately protected state-ref protocol rather than a cache.

## Rejected alternatives

- **Keep the external skill as a compatibility adapter:** rejected because it preserves two authorities and non-reproducible runtime dependencies.
- **Import old JSON/JSONL or experimental SQLite state:** rejected because fani was unreleased and migration would encode unstable semantics.
- **Let the Agent edit the repository:** rejected because model output cannot be trusted with paths, state, or publication decisions.
- **Create a multi-crate workspace or general plugin runtime now:** rejected because no independently versioned artifact or second production frontend exists.
- **Use mutable worktree content as the source plan:** rejected because source identity and publication would be race-prone.

## Enforcement

The active contract is detailed in [`native-i18n.md`](native-i18n.md). Architecture tests enforce dependency direction and the coarse application-owned ports. Integration tests use real temporary SQLite databases, Git repositories, provider processes, materialization, and GitHub command fixtures. CI builds with `--locked`, checks Rust 1.85 compatibility, and does not install or invoke the superseded Python/skill runtime.
