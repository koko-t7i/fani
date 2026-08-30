# Native Rust CLI stack decision

**Status:** implemented and supersedes the experimental pre-release architecture as of 2026-08-30.

## Decision

fani is a Linux-first Rust 2024 command-line application with Rust 1.85 as its minimum supported toolchain. It uses:

- `clap` for the command interface.
- `serde`, `serde_json`, and `toml` for strict typed configuration, Agent envelopes, durable payloads, and reports.
- `pulldown-cmark`, `regex`, `globset`, and `strsim` for native Markdown extraction, protected syntax, source discovery, and deterministic unit matching.
- bundled `rusqlite` as the single fani-owned authority for runs, work, attempts, findings, canonical content, trusted translation memory, leases, outboxes, and pull-request state.
- synchronous bounded subprocess adapters for Agent, Git, and GitHub operations. Each adapter has explicit deadlines, bounded input/output, controlled environment variables, and Linux process-tree cleanup where Agent execution requires it.
- Git plumbing with a temporary index for fixed-source candidate commits without changing the checked-out branch, `HEAD`, real index, staging, or unrelated worktree files.
- `gh` for bounded GitHub pull-request creation and reconciliation.

## Rejected runtime architecture

The shipped application does not invoke an external localization skill, require a scripting-language runtime or package runner, split authority across JSON files and SQLite, import legacy state, or migrate experimental schemas. Those pre-release approaches are intentionally unsupported.

The application also does not use an asynchronous runtime. Its bounded thread-based subprocess and I/O model is sufficient for the configured local concurrency while keeping SQLite transactions short and side-effect boundaries explicit.

## Rationale

The native design keeps Markdown planning, matching, assembly, and deterministic verification independently testable. SQLite provides transactional identity and recovery for local state, while durable outboxes cover filesystem, Git, and GitHub effects that cannot participate in a database transaction. Stable source revisions and compare-and-swap publication prevent concurrent source or branch movement from being silently accepted.

## Current references

- [`../architecture/native-i18n.md`](../architecture/native-i18n.md) defines the active product and architecture contract.
- [`../../README.md`](../../README.md) documents installation, configuration, commands, and exit semantics.
- [`../../examples/fani.toml`](../../examples/fani.toml) is the complete configuration example.
- [`../../.github/workflows/ci.yml`](../../.github/workflows/ci.yml) and [`../../.github/workflows/security.yml`](../../.github/workflows/security.yml) define locked build, test, minimum-Rust, release, audit, and policy gates.
