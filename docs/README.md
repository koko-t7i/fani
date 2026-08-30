# Documentation map

Use this page to choose the right fani document. The root [`README.md`](../README.md) is the installation and first-run path; it intentionally does not duplicate every configuration, operations, architecture, or release detail.

## User guides

- [`README.md`](../README.md): product overview, installation, five-minute start, core commands, exit codes, and safety summary.
- [`best-practices.md`](best-practices.md): staged rollout, provider and credential handling, repository layout, daily operation, human edits, publication, scheduling, and CI.
- [`examples/fani.toml`](../examples/fani.toml): complete annotated configuration reference. Copy and adapt it after the starter configuration is working.
- [`systemd/`](../systemd/): user service and timer templates for unattended synchronization.

## Maintainer guides

- [`release.md`](release.md): release archive contract, verification and installation, reproducibility scope, and pre-tag checks.
- [`research/2026-rust-cli-stack.md`](research/2026-rust-cli-stack.md): recorded technology research supporting the native implementation.

## Active architecture

- [`architecture/adr-0001-native-single-authority.md`](architecture/adr-0001-native-single-authority.md): accepted decision for the native Rust engine and one SQLite authority.
- [`architecture/native-i18n.md`](architecture/native-i18n.md): current fani 0.3 product and implementation contract.

When a user guide and an architecture document differ in level of detail, the architecture contract defines system behavior while the user guide defines the recommended operating path.

## Historical records

The following documents describe superseded pre-release designs. They are retained for decision history and must not be used as setup or compatibility instructions:

- [`architecture/compatibility-baseline.md`](architecture/compatibility-baseline.md)
- [`architecture/rust-sqlite-rewrite.md`](architecture/rust-sqlite-rewrite.md)
