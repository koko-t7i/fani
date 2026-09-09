# ADR-0002: Explicit JSON resources with byte-span verification

**Status:** Accepted and implemented.

## Decision

Keep the existing pure Markdown backend and its semantic compatibility fingerprint unchanged. Dispatch JSON through the shared document, reuse, verification, adoption, and check-before-effects pipeline. JSON requires an explicit source set and versioned `plain` or `i18next-interpolation-v1` message dialect. MDX remains unavailable because its parser safety gates are blocked; an enum variant is not enablement.

The JSON backend is a bounded strict UTF-8 byte-span parser with duplicate-key rejection, including equivalent escaped keys. It translates decoded nonempty, non-whitespace string values only. Assembly replaces only changed value literals and preserves original escapes when decoded output is unchanged. No container serialization, JavaScript execution, or Node runtime is introduced.

RFC 6901 pointers identify units within their source document. Arrays use indices, not text matching. A changed source at an existing pointer must be translated again; historical target text is reference context only. Per-unit context includes the document, pointer, dialect and semantic contracts. Every reuse entry validates source and contracts; current whole-document verification and configured project checks remain mandatory before canonical effects. No schema migration or history rewrite is needed.

The interpolation dialect supports only named/dotted default-delimiter arguments with ASCII delimiter whitespace retained. Each occurrence has one immutable token; independent tokens may reorder within their message. Unsupported plural/ordinal/legacy-suffix resources, ICU, formatting, unescaped parameters, nesting, custom delimiter candidates and rich text fail closed rather than being protected as allegedly supported constructs. Applications must honor the fixed dialect; arbitrary custom syntax cannot be inferred from text alone.

Full target reparse compares pointer/topology signatures, arrays, raw keys, nonstring literals, unselected strings and placeholder schemas. Object member order is not identity; adoption aligns sorted pointers. All-string arrays cannot reveal semantic swaps from final translated text alone and require semantic review.

## Consequences

Malformed or unsupported source resources produce stable redacted findings and `needs_human`. Zero-unit documents use existing exact pass-through document work without fake translation history. Existing fixed-revision preflight, mappings, request v2/response v1, authorization, migrations, recovery and publication boundaries remain authoritative.

The isolated frontend fixture consumes externally generated output with independently recorded hashes. Deterministic local provider/browser acceptance proves resource compatibility and interaction behavior, not hosted-model translation quality. Native release archives remain independent of test-only Node and Chromium dependencies.
