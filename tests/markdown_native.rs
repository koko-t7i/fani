use fani::domain::markdown::{
    MarkdownError, ProtectedKind, UnitTranslation, apply_translations, extract_units,
    validate_translation,
};
use fani::domain::{model, prompts};

const DOCUMENT: &str = r#"# Hello `{name}`

Welcome, {{user}}. See [the guide](https://example.com/guide "Guide") and <kbd>Enter</kbd>.

```rust
let untranslated = "hello";
```

Final paragraph with %1$s.
"#;

#[test]
fn offset_events_extract_units_and_protect_non_translatable_content() {
    let units = extract_units(DOCUMENT);
    assert_eq!(units.len(), 3, "{units:#?}");

    let heading = &units[0];
    assert_eq!(&DOCUMENT[heading.range.clone()], heading.source);
    assert!(heading.protected_source.contains("@@FANI_INLINE_CODE_"));
    assert_eq!(heading.protected[0].value, "`{name}`");

    let paragraph = &units[1];
    assert_eq!(&DOCUMENT[paragraph.range.clone()], paragraph.source);
    assert!(
        paragraph
            .protected
            .iter()
            .any(|span| span.kind == ProtectedKind::Placeholder && span.value == "{{user}}")
    );
    assert!(paragraph.protected.iter().any(|span| {
        span.kind == ProtectedKind::LinkTarget && span.value == "https://example.com/guide"
    }));
    assert!(
        paragraph
            .protected
            .iter()
            .any(|span| span.kind == ProtectedKind::Html && span.value == "<kbd>")
    );

    assert!(
        units
            .iter()
            .all(|unit| !unit.source.contains("let untranslated"))
    );
}

#[test]
fn stable_ids_do_not_depend_on_absolute_byte_offsets() {
    let first = extract_units(DOCUMENT);
    let shifted = extract_units(&format!("Unrelated preface.\n\n{DOCUMENT}"));

    assert_eq!(
        first.iter().map(|unit| &unit.id).collect::<Vec<_>>(),
        shifted
            .iter()
            .skip(1)
            .map(|unit| &unit.id)
            .collect::<Vec<_>>()
    );
    assert_eq!(extract_units(DOCUMENT), first);
}

#[test]
fn byte_range_replacement_preserves_every_byte_outside_selected_unit() {
    let units = extract_units(DOCUMENT);
    let unit = &units[1];
    let translated = unit
        .protected_source
        .replace("Welcome", "欢迎")
        .replace("See", "请参阅")
        .replace("the guide", "指南")
        .replace("and", "并按");

    let output = apply_translations(
        DOCUMENT,
        &units,
        &[UnitTranslation {
            id: unit.id.clone(),
            text: translated,
        }],
    )
    .unwrap();

    assert_eq!(&output[..unit.range.start], &DOCUMENT[..unit.range.start]);
    let translated_end = output.len() - (DOCUMENT.len() - unit.range.end);
    assert_eq!(&output[translated_end..], &DOCUMENT[unit.range.end..]);
    assert!(output.contains("https://example.com/guide"));
    assert!(output.contains("<kbd>Enter</kbd>"));
    assert!(output.contains("let untranslated = \"hello\";"));
    assert!(output.contains("{{user}}"));
}

#[test]
fn deterministic_validation_rejects_missing_tokens_and_structure_changes() {
    let units = extract_units(DOCUMENT);
    let unit = &units[0];
    let missing = unit
        .protected_source
        .replace(&unit.protected[0].token, "translated code");
    let findings = validate_translation(unit, &missing).unwrap_err();
    assert_eq!(findings[0].code, "MD-PROTECTED");

    let emphasized_source = "A **bold** statement.\n";
    let emphasized = extract_units(emphasized_source);
    let error = validate_translation(&emphasized[0], "一个 bold 句子。\n").unwrap_err();
    assert!(error.iter().any(|finding| finding.code == "MD-STRUCTURE"));

    let duplicate = format!("{} {}", unit.protected_source, unit.protected[0].token);
    let findings = validate_translation(unit, &duplicate).unwrap_err();
    assert_eq!(findings[0].code, "MD-PROTECTED");
}

#[test]
fn validation_allows_protected_inline_code_reordering() {
    let source = "Run `fani doctor` in the same environment as `fani sync`.\n";
    let units = extract_units(source);
    let unit = &units[0];
    let translated = format!(
        "在与 {} 相同的环境中运行 {}。",
        unit.protected[1].token, unit.protected[0].token
    );

    assert_eq!(
        validate_translation(unit, &translated).unwrap(),
        "在与 `fani sync` 相同的环境中运行 `fani doctor`。"
    );
}

#[test]
fn literal_fani_wildcard_namespace_is_protected() {
    let source = "Every @@FANI_*@@ token is immutable.\n";
    let units = extract_units(source);
    let unit = &units[0];
    assert_eq!(unit.protected.len(), 1);
    assert_eq!(unit.protected[0].value, "@@FANI_*@@");

    let translated = format!("每个 {} 标记均不可更改。", unit.protected[0].token);
    assert_eq!(
        validate_translation(unit, &translated).unwrap(),
        "每个 @@FANI_*@@ 标记均不可更改。"
    );
}

#[test]
fn validation_rejects_delimiters_that_change_protected_inline_code() {
    let source = "Use `a` and `b`.\n";
    let units = extract_units(source);
    let unit = &units[0];
    let translated = format!(
        "使用 `` {} `` 和 {}。",
        unit.protected[0].token, unit.protected[1].token
    );

    let findings = validate_translation(unit, &translated).unwrap_err();
    assert!(
        findings
            .iter()
            .any(|finding| finding.code == "MD-PROTECTED-CODE"),
        "{findings:?}"
    );
}

#[test]
fn validation_ignores_adjacent_plain_text_event_segmentation() {
    let source = "Use `command-json-v1` for a private integration. The command must implement fani's strict envelopes.\n";
    let units = extract_units(source);
    let unit = &units[0];
    let translated = format!(
        "对于私有集成，请使用 {}。该命令必须实现 fani 严格的封装。",
        unit.protected[0].token
    );

    assert_eq!(
        validate_translation(unit, &translated).unwrap(),
        "对于私有集成，请使用 `command-json-v1`。该命令必须实现 fani 严格的封装。"
    );
}

#[test]
fn apply_is_order_independent_and_rejects_unknown_or_duplicate_ids() {
    let source = "First paragraph.\n\nSecond paragraph.\n";
    let units = extract_units(source);
    let translations = vec![
        UnitTranslation {
            id: units[1].id.clone(),
            text: "第二段。".into(),
        },
        UnitTranslation {
            id: units[0].id.clone(),
            text: "第一段。".into(),
        },
    ];
    assert_eq!(
        apply_translations(source, &units, &translations).unwrap(),
        "第一段。\n\n第二段。\n"
    );

    let unknown = apply_translations(
        source,
        &units,
        &[UnitTranslation {
            id: "md-unknown".into(),
            text: "x".into(),
        }],
    );
    assert!(matches!(unknown, Err(MarkdownError::UnknownUnit(_))));

    let duplicate = apply_translations(
        source,
        &units,
        &[
            UnitTranslation {
                id: units[0].id.clone(),
                text: "一。".into(),
            },
            UnitTranslation {
                id: units[0].id.clone(),
                text: "二。".into(),
            },
        ],
    );
    assert!(matches!(
        duplicate,
        Err(MarkdownError::DuplicateTranslation(_))
    ));

    let changed_source = source.replacen("First", "Other", 1);
    let stale = apply_translations(
        &changed_source,
        &units,
        &[UnitTranslation {
            id: units[0].id.clone(),
            text: "第一段。".into(),
        }],
    );
    assert!(matches!(stale, Err(MarkdownError::SourceChanged(_))));
}

#[test]
fn representative_commonmark_gfm_corpus_is_byte_stable_and_protected() {
    let corpus = [
        include_str!("fixtures/markdown-corpus/rich-gfm.md"),
        include_str!("fixtures/markdown-corpus/edge-commonmark.md"),
    ];

    for source in corpus {
        let units = extract_units(source);
        assert!(!units.is_empty());
        assert_eq!(apply_translations(source, &units, &[]).unwrap(), source);

        let identity = units
            .iter()
            .map(|unit| UnitTranslation {
                id: unit.id.clone(),
                text: unit.protected_source.clone(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            apply_translations(source, &units, &identity).unwrap(),
            source
        );
        assert!(
            units
                .iter()
                .all(|unit| source[unit.range.clone()] == unit.source)
        );
        assert!(
            units
                .iter()
                .all(|unit| !unit.source.contains("title: Keep"))
        );
        assert!(
            units
                .iter()
                .all(|unit| !unit.source.contains("slug: native"))
        );
        assert!(
            units
                .iter()
                .all(|unit| !unit.source.contains("let untranslated"))
        );
        assert!(
            units
                .iter()
                .all(|unit| !unit.source.contains("indented_code"))
        );
    }

    let rich = extract_units(corpus[0]);
    assert!(
        rich.iter()
            .any(|unit| unit.kind == fani::domain::markdown::UnitKind::Heading)
    );
    assert!(
        rich.iter()
            .any(|unit| unit.kind == fani::domain::markdown::UnitKind::TableCell)
    );
    assert!(
        rich.iter()
            .any(|unit| unit.kind == fani::domain::markdown::UnitKind::Definition)
    );
    assert!(
        rich.iter()
            .any(|unit| unit.source.contains("Preserve task markers"))
    );
    assert!(
        rich.iter()
            .any(|unit| unit.source.contains("A quoted paragraph"))
    );
    assert!(rich.iter().flat_map(|unit| &unit.protected).any(|span| {
        span.kind == ProtectedKind::LinkTarget && span.value == "https://example.com/diagram.png"
    }));
    assert!(
        rich.iter().flat_map(|unit| &unit.protected).any(|span| {
            span.kind == ProtectedKind::Placeholder && span.value == "${ACCOUNT_ID}"
        })
    );

    let edge = extract_units(corpus[1]);
    assert!(edge.iter().flat_map(|unit| &unit.protected).any(|span| {
        span.kind == ProtectedKind::LinkTarget && span.value == "https://example.com/other"
    }));
    assert!(corpus[1].contains("\r\n"));
}

#[test]
fn tight_task_and_nested_lists_translate_without_touching_markers_or_indentation() {
    let source = "- [x] First **bold**\n- Parent\n  - Child `code`\n";
    let units = extract_units(source);
    let list_units = units
        .iter()
        .filter(|unit| unit.kind == fani::domain::markdown::UnitKind::ListItem)
        .collect::<Vec<_>>();
    assert_eq!(list_units.len(), 3, "{units:#?}");
    let translations = list_units
        .iter()
        .map(|unit| UnitTranslation {
            id: unit.id.clone(),
            text: unit
                .protected_source
                .replace("First", "第一")
                .replace("bold", "粗体")
                .replace("Parent", "父项")
                .replace("Child", "子项"),
        })
        .collect::<Vec<_>>();

    assert_eq!(
        apply_translations(source, &units, &translations).unwrap(),
        "- [x] 第一 **粗体**\n- 父项\n  - 子项 `code`\n"
    );
}

#[test]
fn corpus_rejects_malicious_tokens_links_and_structure_changes() {
    let source = include_str!("fixtures/markdown-corpus/edge-commonmark.md");
    let units = extract_units(source);
    let heading = units
        .iter()
        .find(|unit| unit.kind == fani::domain::markdown::UnitKind::Heading)
        .unwrap();
    let changed_level = heading.protected_source.replacen("# ", "## ", 1);
    assert!(
        validate_translation(heading, &changed_level)
            .unwrap_err()
            .iter()
            .any(|finding| finding.code == "MD-STRUCTURE")
    );

    let linked = units
        .iter()
        .find(|unit| {
            unit.protected.iter().any(|span| {
                span.kind == ProtectedKind::LinkTarget && span.value == "https://example.com/other"
            })
        })
        .unwrap();
    let target = linked
        .protected
        .iter()
        .find(|span| span.value == "https://example.com/other")
        .unwrap();
    let replaced_target = linked
        .protected_source
        .replace(&target.token, "https://evil.example/steal");
    assert!(
        validate_translation(linked, &replaced_target)
            .unwrap_err()
            .iter()
            .any(|finding| finding.code == "MD-PROTECTED")
    );

    for unknown in [
        "@@FANI_PLACEHOLDER_9999_deadbeefdeadbeef@@",
        "@@FANI_PLACEHOLDER_9999_DEADBEEFDEADBEEF@@",
        "@@FANI_EVIL@@",
    ] {
        let injected = format!("{} {unknown}", linked.protected_source);
        assert!(
            validate_translation(linked, &injected)
                .unwrap_err()
                .iter()
                .any(|finding| finding.code == "MD-UNKNOWN-TOKEN"),
            "accepted reserved token {unknown}"
        );
    }
}

#[test]
fn prompts_make_the_native_engine_contract_explicit() {
    let rendered = prompts::render(&model::AgentTask {
        source_format: fani::domain::document::DocumentFormat::Markdown,
        unit_context: fani::domain::document::UnitContext::markdown(),
        context_key: "fixture-context".into(),
        message_syntax: None,
        token_permissions: fani::domain::model::TokenPermissions {
            contract: "fani-markdown-tokens-v1".into(),
            reorderable_tokens: Vec::new(),
        },
        id: "md-123".into(),
        stage: model::AgentStage::Translate,
        source_language: "en".into(),
        target_language: "zh-CN".into(),
        source: "Hello".into(),
        previous_source: None,
        previous_translation: None,
        findings: Vec::new(),
        protected_tokens: vec!["@@FANI_PLACEHOLDER_0000_deadbeefdeadbeef@@".into()],
    });
    assert!(rendered.starts_with(prompts::PROMPT_VERSION));
    assert!(rendered.contains("Protected tokens:"));

    let prompt = prompts::translation_prompt(
        "zh-CN",
        "md-123",
        "Hello @@FANI_INLINE_CODE_0000_deadbeefdeadbeef@@",
    );
    assert!(prompt.contains("copy each token exactly once"));
    assert!(prompt.contains("Return only the translated source-format unit"));
    assert!(prompt.contains("Target language: zh-CN"));

    let repair = prompts::repair_prompt(
        "zh-CN",
        "md-123",
        "Hello",
        "你好",
        &["Markdown structure changed".into()],
    );
    assert!(repair.contains("smallest possible edit"));
    assert!(repair.contains("- Markdown structure changed"));
}
