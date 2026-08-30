# Compatibility baseline (superseded)

This document described the experimental fani 0.2 external-skill/Python compatibility boundary. fani was not released, so the 0.3 native engine intentionally provides no compatibility migration or rollback protocol.

This document is retained only as historical context and must not be used as an implementation or operations guide. The accepted replacement decision is [`ADR-0001`](adr-0001-native-single-authority.md), and the active architecture and product contract are in [`native-i18n.md`](native-i18n.md). Runtime code must not import old JSON/JSONL state, create Python-compatible locks, invoke an external i18n skill, or require Python/`uv`.
