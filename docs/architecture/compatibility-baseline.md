# fani Rust rewrite compatibility baseline

Date: 2026-08-29

This document freezes the behavior that the Rust executable must preserve from the Python 0.1 implementation. The detailed independent audit was performed by subagent `01a04eca-0846-7282-a5c4-00da8cbc9163`; the shipped regression tests are the executable source of truth.

## Public CLI

- Commands: `doctor`, `status`, `sync`.
- Common options: `--config`, `--repo`, `--lang`.
- Sync-only options: `--report-dir`, `--quiet`.
- Default config: `fani.toml`; default report directory: `.fani`.
- `status` invokes only skill `plan`; it never invokes an Agent, apply, verify, review, or Git.
- `doctor` checks config, skill path, `uv`, enabled Agent binaries, stage routing, repositories, and the SQLite schema.

## Exit status contract

| Result | Exit |
| --- | ---: |
| Up to date or translated and verified | 0 |
| Human decision required | 1 |
| Configuration, environment, lock, skill, or infrastructure error | 2 |
| Successful bounded batch with deferred chunks | 3 |

For multiple outcomes the precedence is `error > needs_human > partial > ok`.

## Configuration

Human configuration remains TOML. The Rust model preserves the existing repository, Agent, stage, routing, concurrency, timeout, retry, branch, commit, push, and safety-limit fields. Agent commands remain argv arrays and are never interpreted by a shell.

## External i18n skill boundary

The external skill remains an explicit compatibility boundary and continues to own `state.json`, `glossary.json`, `style.json`, work task JSON, result JSON, and review JSON. Rust always calls `scripts/run.sh` with explicit `--root`, `--state-dir`, and `--json` arguments.

Required operations:

- `plan`: consumes `run_id`, `task_count`, `conflicts`, `files`, `fuzzy_matched`, and `truncated_tasks`.
- `apply`: consumes `written` and `rejected`.
- `verify`: consumes `status`, `findings`, and `retry_files`.
- `review plan/collect`: consumes review task count/run id and findings.

External protocol JSON is intentionally not moved into SQLite.

## Agent protocol

- One task is one headless Agent process.
- Prompt goes to stdin; final text comes from stdout or `{output_file}`.
- Each Agent process has its own Unix process group.
- Timeout sends TERM to the process group, waits, sends KILL if needed, and reaps the child.
- Stable failure codes are `DSP-TIMEOUT`, `DSP-EXIT`, and `DSP-EMPTY`.
- Each task receives at most `retries + 1` attempts.
- A single outer markdown/md/json fence may be stripped.
- Translation results are written as `{chunk_id, translated_text}` JSON by fani, not by the Agent.
- Task `result_path` is now validated as repository-relative; this intentionally closes the Python path-escape defect.

## Safety invariants

- Conflicts stop before Agent calls or file application.
- A suspicious fresh-task count in a repository with known external skill state stops before Agent calls.
- One SQLite-backed lock covers all selected languages of a repository.
- External `state.json` is backed up before a mutating run.
- Exhausted Agent dispatch stops before apply, removing a redundant failure-path skill call.
- Agent timeout covers both leader lifetime and stdout/stderr drain. A leader that exits while a SIGTERM-ignoring descendant retains the pipes must still return `DSP-TIMEOUT` within the configured deadline and the descendant must be killed and reaped.
- Assembly rejection and verify failure enter bounded repair branches only.
- Revision runs only when configured and blocks only on error findings.
- Proofread is implemented as an optional advisory review; this fixes the documented-but-unused Python setting.
- Git publication happens only after deterministic validation and optional blocking review.

## Git publication

Publication retains the existing plumbing design:

1. Read the language branch or `HEAD` into a temporary index.
2. Add only translated targets reported by apply plus external skill state files.
3. Write a tree and create a commit with `commit-tree`.
4. Update the language branch with old-value compare-and-swap.

The current branch, HEAD, real index, unrelated tracked edits, and untracked files remain untouched. `fani.db`, SQLite journal files, work directories, and locks are explicitly local and excluded from translation commits.

## Persistent data boundary

SQLite is the only write source for fani-owned durable state:

- schema migrations;
- run and per-language history;
- state transitions;
- Agent attempt summaries;
- repository locks;
- idempotent imports of old JSON/JSONL records.

JSON remains only at the external skill protocol and report boundaries. `report.json` and `report.md` remain human/machine snapshots, while durable history is retained in SQLite.

## Black-box regression matrix

The Rust test suite must drive the shipped binary and cover:

- healthy and broken doctor;
- status without Agent calls;
- successful first sync and no-op second sync;
- human edit conflict;
- Agent non-zero exit, empty output, timeout, retry, and leader-exit/descendant-retains-pipes cleanup;
- assembly rejection and finite verify repair;
- lock contention;
- SQLite initialization, restart persistence, and idempotent legacy import;
- report status/exit semantics;
- Git branch isolation and database exclusion.
