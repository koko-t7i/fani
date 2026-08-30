---
title: Keep this metadata
slug: native-guide
---

# Native guide `{version}`

Welcome, {{user}} — read [the guide][guide] and visit <https://example.com/status>.

- [x] Preserve task markers
- [ ] Translate nested content
  - Nested **strong** and *emphasized* text with `inline()` code.

> A quoted paragraph with ${ACCOUNT_ID} and <kbd>Enter</kbd>.

| Feature | Status | Notes |
| :--- | ---: | --- |
| Tables | Ready | Escaped \| pipe and ~~old wording~~ |
| Links | Ready | ![diagram](https://example.com/diagram.png "Architecture") |

Term
: Definition text for %(count)03d records.

A footnote reference[^native] and entity &copy; remain structured.

[^native]: Footnote **content** with %1$s.

```rust
let untranslated = "{{user}}";
printf!("%1$s");
```

<div data-id="{immutable}">
HTML block content stays structural.
</div>

[guide]: https://example.com/guide "Guide title"
