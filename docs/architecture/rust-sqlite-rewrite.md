# Rust + SQLite rewrite design (superseded)

The fani 0.2 design retained an external Python i18n skill and a compatibility state boundary. The project had not been released, and that architecture was superseded on 2026-08-30 by the destructive native implementation.

This document is retained only as historical context and must not be used as an implementation or operations guide. See [`ADR-0001`](adr-0001-native-single-authority.md) for the accepted replacement decision and [`native-i18n.md`](native-i18n.md) for the current contract: native Markdown processing, one SQLite authority, isolated typed Agent execution, fixed-source Git publication, and GitHub pull-request reconciliation with no Python, `uv`, external skill, legacy JSON, or compatibility migration.
