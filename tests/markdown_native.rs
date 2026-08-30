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
fn prompts_make_the_native_engine_contract_explicit() {
    let rendered = prompts::render(&model::AgentTask {
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
    assert!(rendered.starts_with("fani-native-markdown-prompt-v1"));
    assert!(rendered.contains("Protected tokens:"));

    let prompt = prompts::translation_prompt(
        "zh-CN",
        "md-123",
        "Hello @@FANI_INLINE_CODE_0000_deadbeefdeadbeef@@",
    );
    assert!(prompt.contains("copy each token exactly once"));
    assert!(prompt.contains("Return only the translated Markdown unit"));
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
