use fani::domain::{
    document::{self, DocumentError, DocumentFormat, UnitTranslation},
    json,
    model::MessageSyntax,
};

#[test]
fn byte_identity_pointer_paths_and_changed_literal_only() {
    let source = "{\r\n  \"a/~\": [\"\\u0041\", \"你好 \\\"x\\\"\\n\", \"\", \" \\t\", 1.00, false, null], \"\": \"B\"\r\n}";
    let parsed = json::parse(source.as_bytes(), MessageSyntax::Plain).unwrap();
    assert_eq!(
        parsed
            .units
            .iter()
            .map(|u| u.id.as_str())
            .collect::<Vec<_>>(),
        ["/", "/a~1~0/0", "/a~1~0/1"]
    );
    let identity: Vec<_> = parsed
        .units
        .iter()
        .map(|u| UnitTranslation {
            id: u.id.clone(),
            text: u.protected_source.clone(),
        })
        .collect();
    assert_eq!(
        document::assemble_document(&parsed, &identity).unwrap(),
        source
    );
    let mut changed = identity;
    changed[0].text = "C\n\"é\"".into();
    assert_eq!(
        document::assemble_document(&parsed, &changed).unwrap(),
        source.replace("\"B\"", "\"C\\n\\\"é\\\"\"")
    );
    assert!(
        document::parse_document_with_syntax(DocumentFormat::Json, source.as_bytes(), None)
            .is_err()
    );
}

#[test]
fn interpolation_token_expansion_preserves_identity_near_string_limit() {
    let text = format!("{}{}", "a".repeat(64_000), "{{x}}".repeat(256));
    let source = serde_json::to_string(&text).unwrap();
    let parsed = json::parse(source.as_bytes(), MessageSyntax::I18nextInterpolationV1).unwrap();
    assert!(parsed.units[0].protected_source.len() > 65_536);
    assert_eq!(
        document::assemble_document(
            &parsed,
            &[UnitTranslation {
                id: parsed.units[0].id.clone(),
                text: parsed.units[0].protected_source.clone()
            }]
        )
        .unwrap(),
        source
    );
    assert!(
        json::parse(
            serde_json::to_string(&format!("{text}{{{{x}}}}"))
                .unwrap()
                .as_bytes(),
            MessageSyntax::I18nextInterpolationV1
        )
        .is_err()
    );
}

#[test]
fn strict_json_and_bounded_resources_reject_before_translation() {
    for source in [
        r#"{"a":"x","\u0061":"y"}"#,
        r#"{"a":1,}"#,
        "[1,]",
        "01",
        "1e",
        "NaN",
        "true false",
        r#""\uD800""#,
        r#""\x20""#,
        "\u{feff}{}",
        "\"raw\nline\"",
        "{unquoted:1}",
    ] {
        assert!(
            json::parse(source.as_bytes(), MessageSyntax::Plain).is_err(),
            "{source}"
        );
    }
    assert!(json::parse(&[b'"', 0xff, b'"'], MessageSyntax::Plain).is_err());
    for source in [
        format!("{}0{}", "[".repeat(66), "]".repeat(66)),
        format!("\"{}\"", "a".repeat(65_537)),
        " ".repeat(4 * 1024 * 1024 + 1),
        format!("[{}0]", "0,".repeat(100_000)),
        format!("{{\"{}\":1}}", "k".repeat(4097)),
    ] {
        assert!(matches!(
            json::parse(source.as_bytes(), MessageSyntax::Plain),
            Err(DocumentError::ResourceLimit(_))
        ));
    }
}

#[test]
fn message_dialects_reject_unsupported_constructs_and_allow_argument_reorder() {
    for value in [
        "Hello {{name}}",
        "{count, plural, one {one} other {many}}",
        "${value}",
        "%s",
        "%1$s",
        "%#x",
        "%*.*f",
        "% d",
        "%'d",
        "$t(key)",
        "<b>Hi</b>",
        "[[name]]",
        "[% name %]",
    ] {
        let source = serde_json::to_vec(value).unwrap();
        assert!(
            matches!(
                json::parse(&source, MessageSyntax::Plain),
                Err(DocumentError::MessageUnsupported)
            ),
            "{value}"
        );
    }
    for value in [
        "{{- name}}",
        "{{name, number}}",
        "{{user..name}}",
        "{{1name}}",
        "{{na-me}}",
        "{{\u{a0}name}}",
        "{{nested {{name}}}}",
        "{count, selectordinal, one {one} other {many}}",
        "$t(key)",
        "<0>Hi</0>",
        "[[name]]",
        "{{name}",
        "@@FANI_JSON_0@@",
    ] {
        assert!(
            json::parse(
                &serde_json::to_vec(value).unwrap(),
                MessageSyntax::I18nextInterpolationV1
            )
            .is_err(),
            "{value}"
        );
    }
    for key in [
        "item_zero",
        "item_one",
        "item_two",
        "item_few",
        "item_many",
        "item_other",
        "item_ordinal_one",
        "item_plural",
        "item_plural_2",
        "item_0",
        "item_12",
    ] {
        assert!(
            matches!(
                json::parse(
                    &serde_json::to_vec(&serde_json::json!({key: "value"})).unwrap(),
                    MessageSyntax::I18nextInterpolationV1
                ),
                Err(DocumentError::MessageUnsupported)
            ),
            "{key}"
        );
    }
    let parsed = json::parse(
        br#"{"message":"Hi {{name}} / {{ user.name }} / {{name}}"}"#,
        MessageSyntax::I18nextInterpolationV1,
    )
    .unwrap();
    let unit = &parsed.units[0];
    let tokens: Vec<_> = unit.protected.iter().map(|s| s.token.as_str()).collect();
    let text = format!("{} puis {} et {}", tokens[1], tokens[2], tokens[0]);
    let assembled = document::assemble_document(
        &parsed,
        &[UnitTranslation {
            id: unit.id.clone(),
            text: text.clone(),
        }],
    )
    .unwrap();
    assert_eq!(
        assembled,
        r#"{"message":"{{ user.name }} puis {{name}} et {{name}}"} "#.trim_end()
    );
    for bad in [
        text.replace(tokens[0], ""),
        text.replace(tokens[0], tokens[1]),
        format!("{text} @@FANI_JSON_99@@"),
        format!("{text} {{{{extra}}}}"),
        format!("<b>{text}</b>"),
    ] {
        assert!(document::validate_unit(unit, &bad).is_err());
    }
}

#[test]
fn full_verification_and_adoption_align_by_pointer_and_preserve_unselected_data() {
    let parsed = json::parse(
        br#"{"a":"Hi {{name}}","b":["One",2,"", "\u0020"],"flag":true}"#,
        MessageSyntax::I18nextInterpolationV1,
    )
    .unwrap();
    let reordered = r#"{"flag":true,"b":["Un",2,"", "\u0020"],"a":"Salut {{name}}"}"#;
    document::verify_document(&parsed, reordered).unwrap();
    let target = json::parse(reordered.as_bytes(), MessageSyntax::I18nextInterpolationV1).unwrap();
    for (source, target) in parsed.units.iter().zip(&target.units) {
        assert!(document::translated_unit_text(source, target).is_some());
    }
    for bad in [
        reordered.replace("\"a\":", "\"c\":"),
        reordered.replace(",2,", ",3,"),
        reordered.replace(",2,", ",\"2\","),
        reordered.replace("true", "false"),
        reordered.replace("\"\",", "\"x\","),
        reordered.replace("\\u0020", " "),
        reordered.replace("{{name}}", "{{user}}"),
        reordered.replace("\"Un\",2", "2,\"Un\""),
        reordered.replace("\"Un\",2", "\"Un\",\"Deux\",2"),
    ] {
        assert!(document::verify_document(&parsed, &bad).is_err(), "{bad}");
    }
    let provenance = document::UnitProvenance {
        document_path: "messages/en.json".into(),
        source: parsed.units[0].source.clone(),
        source_revision: "fixed".into(),
        context_json: document::unit_metadata(&parsed.units[0], "messages/en.json"),
        policy_fingerprint: "old-prompt".into(),
    };
    assert!(
        document::validate_provenance(
            "messages/en.json",
            &parsed.units[0],
            &provenance,
            &parsed.units[0].protected_source
        )
        .is_ok()
    );
    assert!(
        document::validate_provenance(
            "messages/other.json",
            &parsed.units[0],
            &provenance,
            &parsed.units[0].protected_source
        )
        .is_err()
    );
    let changed = json::unit(
        "Changed {{name}}",
        "/a",
        MessageSyntax::I18nextInterpolationV1,
    )
    .unwrap();
    assert!(
        document::validate_provenance(
            "messages/en.json",
            &changed,
            &provenance,
            &changed.protected_source
        )
        .is_err()
    );
    let other_dialect = json::unit("One", "/b/0", MessageSyntax::Plain).unwrap();
    let old = document::UnitProvenance {
        source: "One".into(),
        context_json: document::unit_metadata(&parsed.units[1], "messages/en.json"),
        ..provenance
    };
    assert!(document::validate_provenance("messages/en.json", &other_dialect, &old, "Un").is_err());
}
