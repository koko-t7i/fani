# Compatibility baseline (superseded)

This document described the experimental fani 0.2 external-skill/Python compatibility boundary. fani was not released, so the 0.3 native engine intentionally provides no compatibility migration or rollback protocol.

The active architecture and product contract are in [`native-i18n.md`](native-i18n.md). Runtime code must not import old JSON/JSONL state, create Python-compatible locks, invoke an external i18n skill, or require Python/`uv`.
