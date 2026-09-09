# Documentation map

The root [`README.md`](../README.md) covers installation, first run, commands, exit codes, and the safety model. Use the focused documents below for details instead of treating every file as a setup guide.

## User and operator guides

- [`best-practices.md`](best-practices.md): rollout, credentials, repository layout, daily operation, publication, scheduling, and CI.
- [`examples/fani.toml`](../examples/fani.toml): complete annotated configuration reference.
- [`systemd/`](../systemd/): user service and timer templates for unattended synchronization.

## Maintainer guide

- [`release.md`](release.md): release assets, installation verification, reproducibility, and pre-release checks.

## Architecture

- [`architecture/native-i18n.md`](architecture/native-i18n.md): current product and implementation contract.
- [`architecture/adr-0001-native-single-authority.md`](architecture/adr-0001-native-single-authority.md): native Rust and one SQLite authority.
- [`architecture/adr-0002-json-resources.md`](architecture/adr-0002-json-resources.md): explicit JSON dialects and byte-span verification.

Architecture decision records explain why a boundary exists; `native-i18n.md` defines current behavior. Pull requests and Git history retain completed implementation plans and validation records.
