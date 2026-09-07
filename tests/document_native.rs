use fani::domain::document::{
    DocumentError, DocumentFormat, UnitTranslation, assemble_document, format_contract,
    parse_document, validate_reuse, validate_unit, verify_document,
};
use fani::domain::markdown::extract_units;

#[test]
fn markdown_identity_and_unit_ids_remain_compatible() {
    for source in [
        include_str!("fixtures/markdown-corpus/rich-gfm.md"),
        include_str!("fixtures/markdown-corpus/edge-commonmark.md"),
        "# 标题\r\n\r\nUse `code` and {{name}}.\r\n",
        "```rust\nlet value = 1;\n```\n",
        "",
    ] {
        let document = parse_document(DocumentFormat::Markdown, source.as_bytes()).unwrap();
        assert_eq!(document.units, extract_units(source));
        let translations = document
            .units
            .iter()
            .map(|unit| UnitTranslation {
                id: unit.id.clone(),
                text: unit.protected_source.clone(),
            })
            .collect::<Vec<_>>();
        assert_eq!(assemble_document(&document, &translations).unwrap(), source);
        assert_eq!(assemble_document(&document, &[]).unwrap(), source);
    }
}

#[test]
fn formats_are_serialized_but_unimplemented_backends_are_not_enabled() {
    for (format, name) in [
        (DocumentFormat::Markdown, "markdown"),
        (DocumentFormat::Mdx, "mdx"),
        (DocumentFormat::Json, "json"),
    ] {
        assert_eq!(serde_json::to_value(format).unwrap(), name);
        if format != DocumentFormat::Markdown {
            assert!(matches!(
                parse_document(format, b"{}"),
                Err(DocumentError::UnsupportedFormat(_))
            ));
            assert!(format_contract(format).is_err());
        }
    }
    assert!(matches!(
        parse_document(DocumentFormat::Markdown, &[0xff]),
        Err(DocumentError::Utf8(_))
    ));
}

#[test]
fn whole_document_verification_preserves_safe_code_reordering_and_prose_movement() {
    let source = "Run `fani doctor` in the same environment as `fani sync`.\n";
    let document = parse_document(DocumentFormat::Markdown, source.as_bytes()).unwrap();
    let unit = &document.units[0];
    let text = format!(
        "在与 {} 相同的环境中运行 {}。",
        unit.protected[1].token, unit.protected[0].token
    );
    let output = assemble_document(
        &document,
        &[UnitTranslation {
            id: unit.id.clone(),
            text,
        }],
    )
    .unwrap();
    assert_eq!(
        output,
        "在与 `fani sync` 相同的环境中运行 `fani doctor`。\n"
    );
    verify_document(&document, "`fani doctor` and `fani sync` are commands.\n").unwrap();
}

#[test]
fn whole_document_verification_checks_nontranslatable_structure_and_code() {
    let source = "# Heading\n\nText.\n\n```rust\nlet value = 1;\n```\n";
    let document = parse_document(DocumentFormat::Markdown, source.as_bytes()).unwrap();
    for changed in [
        source.replace("# Heading", "## Heading"),
        source.replace("value = 1", "value = 2"),
        source.replace("Text.", "<script>bad()</script>"),
    ] {
        assert!(verify_document(&document, &changed).is_err(), "{changed}");
    }
}

#[test]
fn format_reuse_contract_checks_source_context_and_current_validation() {
    let document = parse_document(DocumentFormat::Markdown, b"Use `code`.\n").unwrap();
    let unit = &document.units[0];
    let check = |source, context, contract, text| {
        validate_reuse(unit, source, &unit.kind, context, contract, text)
    };
    assert!(
        check(
            &unit.source,
            &unit.context,
            &document.contract,
            &unit.protected_source
        )
        .is_ok()
    );
    assert!(
        check(
            "Changed",
            &unit.context,
            &document.contract,
            &unit.protected_source
        )
        .is_err()
    );
    assert!(
        check(
            &unit.source,
            &unit.context,
            &document.contract,
            "Missing token"
        )
        .is_err()
    );
    let mut contract = document.contract.clone();
    contract.message_syntax = Some("new-dialect".into());
    assert_ne!(contract.fingerprint(), document.contract.fingerprint());
    assert!(
        check(
            &unit.source,
            &unit.context,
            &contract,
            &unit.protected_source
        )
        .is_err()
    );
    assert_ne!(
        unit.memory_context_key("a.md"),
        unit.memory_context_key("b.md")
    );
    let serialized = serde_json::to_string(unit).unwrap();
    assert_eq!(
        serde_json::from_str::<fani::domain::document::TranslatableUnit>(&serialized).unwrap(),
        *unit
    );
    let mut changed = unit.clone();
    changed.context.format = DocumentFormat::Json;
    assert_ne!(
        unit.memory_context_key("a.md"),
        changed.memory_context_key("a.md")
    );
    assert!(validate_unit(&changed, &changed.protected_source).is_err());
    assert!(
        check(
            &unit.source,
            &changed.context,
            &document.contract,
            &unit.protected_source
        )
        .is_err()
    );
    assert_eq!(
        document.contract.fingerprint(),
        format_contract(DocumentFormat::Markdown)
            .unwrap()
            .fingerprint()
    );
}
