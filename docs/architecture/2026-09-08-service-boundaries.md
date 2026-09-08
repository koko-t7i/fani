# Complete application and persistence boundaries

This change completes the service and transaction boundaries left after PR #17. One native Rust package and one SQLite authority remain the deployment model.

## Acceptance contract

| Boundary | Required behavior | Evidence |
| --- | --- | --- |
| Preview | Planning receives only source reads, target reads, and planning queries; no writer or model executor | Capability checks, standalone planner tests, CLI preview parity |
| Coordination | The coordinator sequences independently constructed services; child modules do not implement the coordinator | Architecture checks and service-level tests |
| Preparation | Document identity, unit registration, reuse, and scheduled work commit together | Transaction rollback injection |
| Verification | Findings and work outcomes commit together; failed checks produce no materializable batch | Service snapshot tests and transaction rollback injection |
| Canonical acceptance | Content, immutable links, document identity, and materialization intent commit together | Crash recovery and rollback injection |
| Materialization | File I/O occurs outside SQLite; content state, work outcome, and receipt completion commit together | Success-before-ack recovery and failed-ack rollback |
| Publication | Candidate authorization and durable payload commit together; terminal outcomes and receipts commit together | Publication crash and authorization tests |
| Human reconciliation | A checked document's trusted translations, canonical content, identity, and completion commit together | Adoption rollback and discard recovery |
| Compatibility | Existing schemas, provider contracts, source identities, budgets, and recovery receipts remain readable | Existing integration suite and release checks |

## Capability boundaries

`StateStore` only selects capabilities at composition and coordination boundaries. Services receive `PlanningStore`, `PreparationStore`, `PipelineStore`, `VerificationStore`, `MaterializationStore`, `PublicationWorkflowStore`, or `ReconciliationStore` directly. The former broad CRUD traits are removed from application ports. Low-level database entrypoints used by integration fixtures remain adapter implementation details.

`VerifiedBatch` has a private constructor and owns a snapshot of its checked documents and bytes. Materialization consumes that snapshot, preventing a caller from replacing the plan after checks pass. Verification findings and work results commit together. Human adoption shares the verification capability and reads sources through `SourceReader`.

Publication recovery decodes missing, explicitly absent, and present remote-tip receipts without changing their wire representation. Candidate authorization checks the supplied payload against its immutable content manifest. Materialization settlement validates the receipt against canonical bytes before changing state.

## Transaction policy

Database transactions contain only local database operations. Filesystem writes, Git, provider calls, and GitHub operations cannot participate in a SQLite transaction. Their durable intents commit first; completion is recorded after external success. Recovery validates current source/mapping/policy bindings and preserves original compatible receipts.

Human adoption validates the complete candidate set before persisting documents. Each document is an atomic adoption unit; this is not a claim of an all-or-nothing transaction over an entire repository or several repositories.

Old database layouts and payloads remain supported. A changed failpoint inside a newly unified transaction must test rollback; existing post-commit failpoints continue to test recovery after durable intent exists.

New adoption persists exact immutable translation links. A compatibility regression explicitly recreates a historical linkless adoption receipt and verifies recovery under current document checks. Legacy null candidate/remote-tip fields remain valid unprepared receipts; a concrete remote tip without a candidate is rejected.

The `materialization_state_transitioned` crash window is now inside settlement. A killed process leaves the external file write intact while rolling back canonical state and work completion together; recovery completes the original receipt without another provider call.

## Validation record

The complete local suite passes: 249 tests across 22 suites with `cargo test --locked --all-targets`. This includes standalone read-only planning, immutable verified snapshots, 10 business-transaction rollback/binding tests, verification rollback, human-discard replay protection, historical linkless adoption recovery, provider-call deduplication, CLI contracts, and clean-clone candidate reproducibility.

`cargo check --locked --all-targets`, `cargo clippy --locked --all-targets --all-features -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check` pass. `cargo build --locked --release` succeeds, and the release entry point reports `fani 0.3.0`. The repository-pinned Rust toolchain is 1.85.0. Remote CI additionally verifies the two-environment reproducible release; its result is recorded on the pull request.
