use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use regex::Regex;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::ops::Range;
use std::sync::OnceLock;
use thiserror::Error;

use crate::domain::document::UnitContext;
pub use crate::domain::document::{
    ProtectedKind, ProtectedSpan, TranslatableUnit as MarkdownUnit, UnitKind, UnitTranslation,
    ValidationFinding,
};

#[derive(Debug, Error, Eq, PartialEq)]
pub enum MarkdownError {
    #[error("source collides with protected token {0}")]
    TokenCollision(String),
    #[error("unknown Markdown unit {0:?}")]
    UnknownUnit(String),
    #[error("duplicate translation for Markdown unit {0:?}")]
    DuplicateTranslation(String),
    #[error("Markdown unit {id:?} has invalid byte range {range:?}")]
    InvalidRange { id: String, range: Range<usize> },
    #[error("Markdown source bytes changed for unit {0:?}")]
    SourceChanged(String),
    #[error("overlapping Markdown unit ranges {first:?} and {second:?}")]
    OverlappingRanges {
        first: Range<usize>,
        second: Range<usize>,
    },
    #[error("translation for {id:?} failed validation: {findings:?}")]
    InvalidTranslation {
        id: String,
        findings: Vec<ValidationFinding>,
    },
}

#[derive(Clone, Debug)]
struct Candidate {
    kind: UnitKind,
    range: Range<usize>,
}

#[derive(Clone, Debug)]
struct LocalProtection {
    range: Range<usize>,
    kind: ProtectedKind,
}

pub fn extract_units(markdown: &str) -> Vec<MarkdownUnit> {
    extract_units_checked(markdown).unwrap_or_default()
}

pub fn extract_units_checked(markdown: &str) -> Result<Vec<MarkdownUnit>, MarkdownError> {
    let options = Options::all();
    let events: Vec<_> = Parser::new_ext(markdown, options)
        .into_offset_iter()
        .collect();
    let mut candidates = Vec::new();

    for (event, range) in &events {
        let kind = match event {
            Event::Start(Tag::Paragraph) => Some(UnitKind::Paragraph),
            Event::Start(Tag::Heading { .. }) => Some(UnitKind::Heading),
            Event::Start(Tag::TableCell) => Some(UnitKind::TableCell),
            Event::Start(Tag::DefinitionListTitle) => Some(UnitKind::DefinitionTerm),
            Event::Start(Tag::DefinitionListDefinition) => Some(UnitKind::Definition),
            _ => None,
        };
        if let Some(kind) = kind {
            let mut range = range.clone();
            while range.end > range.start
                && matches!(markdown.as_bytes()[range.end - 1], b'\n' | b'\r')
            {
                range.end -= 1;
            }
            candidates.push(Candidate { kind, range });
        }
    }

    candidates.extend(tight_list_candidates(&events, &candidates));
    candidates.sort_by_key(|candidate| (candidate.range.start, candidate.range.end));
    candidates.dedup_by(|left, right| left.range == right.range);

    let mut occurrences = BTreeMap::<String, usize>::new();
    let units = candidates
        .into_iter()
        .map(|candidate| build_unit(markdown, &events, candidate, &mut occurrences))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(units.into_iter().flatten().collect())
}

fn tight_list_candidates(
    events: &[(Event<'_>, Range<usize>)],
    existing: &[Candidate],
) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    for (index, (event, _)) in events.iter().enumerate() {
        if !matches!(event, Event::Start(Tag::Item)) {
            continue;
        }
        let mut range: Option<Range<usize>> = None;
        for (event, event_range) in events.iter().skip(index + 1) {
            match event {
                Event::End(TagEnd::Item) => break,
                Event::Start(tag) if is_block_tag(tag) => break,
                Event::TaskListMarker(_) => continue,
                _ if is_inline_event(event) => {
                    range = Some(match range {
                        Some(current) => {
                            current.start.min(event_range.start)..current.end.max(event_range.end)
                        }
                        None => event_range.clone(),
                    });
                }
                _ => {}
            }
        }
        let Some(range) = range.filter(|range| range.start < range.end) else {
            continue;
        };
        if existing.iter().any(|candidate| {
            candidate.range.start <= range.start && candidate.range.end >= range.end
        }) {
            continue;
        }
        candidates.push(Candidate {
            kind: UnitKind::ListItem,
            range,
        });
    }
    candidates
}

fn is_block_tag(tag: &Tag<'_>) -> bool {
    matches!(
        tag,
        Tag::Paragraph
            | Tag::Heading { .. }
            | Tag::BlockQuote(_)
            | Tag::CodeBlock(_)
            | Tag::HtmlBlock
            | Tag::List(_)
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::MetadataBlock(_)
    )
}

fn is_inline_event(event: &Event<'_>) -> bool {
    matches!(
        event,
        Event::Start(
            Tag::Emphasis
                | Tag::Strong
                | Tag::Strikethrough
                | Tag::Superscript
                | Tag::Subscript
                | Tag::Link { .. }
                | Tag::Image { .. }
        ) | Event::End(
            TagEnd::Emphasis
                | TagEnd::Strong
                | TagEnd::Strikethrough
                | TagEnd::Superscript
                | TagEnd::Subscript
                | TagEnd::Link
                | TagEnd::Image
        ) | Event::Text(_)
            | Event::Code(_)
            | Event::Html(_)
            | Event::InlineHtml(_)
            | Event::FootnoteReference(_)
            | Event::SoftBreak
            | Event::HardBreak
            | Event::InlineMath(_)
            | Event::DisplayMath(_)
    )
}

pub fn repair_leading_strong_separator(unit: &MarkdownUnit, translated: &str) -> Option<String> {
    fn closing_strong_end(value: &str) -> Option<usize> {
        value
            .strip_prefix("**")?
            .find("**")
            .map(|offset| offset + 4)
    }

    let source_end = closing_strong_end(&unit.protected_source)?;
    if !unit.protected_source[source_end..]
        .chars()
        .next()
        .is_some_and(char::is_whitespace)
    {
        return None;
    }
    let translated_end = closing_strong_end(translated)?;
    let next = translated[translated_end..].chars().next()?;
    if next.is_whitespace() {
        return None;
    }
    let mut repaired = translated.to_owned();
    repaired.insert(translated_end, ' ');
    Some(repaired)
}

pub fn validate_translation(
    unit: &MarkdownUnit,
    translated: &str,
) -> Result<String, Vec<ValidationFinding>> {
    let mut findings = Vec::new();

    for protected in &unit.protected {
        let count = translated.matches(&protected.token).count();
        if count != 1 {
            findings.push(ValidationFinding {
                code: "MD-PROTECTED",
                message: format!(
                    "protected token {} occurs {count} times; expected exactly once",
                    protected.token
                ),
            });
        }
    }

    let known_tokens: BTreeSet<&str> = unit
        .protected
        .iter()
        .map(|protected| protected.token.as_str())
        .collect();
    for token in protection_token_regex().find_iter(translated) {
        if !known_tokens.contains(token.as_str()) {
            findings.push(ValidationFinding {
                code: "MD-UNKNOWN-TOKEN",
                message: format!("unknown protected token {}", token.as_str()),
            });
        }
    }

    if !findings.is_empty() {
        return Err(findings);
    }

    let restored = restore_protected(unit, translated);
    let expected_code = inline_code_values(&unit.source);
    let actual_code = inline_code_values(&restored);
    if expected_code != actual_code {
        findings.push(ValidationFinding {
            code: "MD-PROTECTED-CODE",
            message: format!(
                "protected inline code changed (expected {expected_code:?}, found {actual_code:?})"
            ),
        });
    }

    let expected = structure_signature(&unit.source);
    let actual = structure_signature(&restored);
    if expected != actual {
        findings.push(ValidationFinding {
            code: "MD-STRUCTURE",
            message: format!(
                "Markdown structure changed (expected {expected:?}, found {actual:?})"
            ),
        });
    }

    if findings.is_empty() {
        Ok(restored)
    } else {
        Err(findings)
    }
}

pub fn apply_translations(
    markdown: &str,
    units: &[MarkdownUnit],
    translations: &[UnitTranslation],
) -> Result<String, MarkdownError> {
    let by_id: BTreeMap<&str, &MarkdownUnit> =
        units.iter().map(|unit| (unit.id.as_str(), unit)).collect();
    let mut seen = BTreeSet::new();
    let mut replacements = Vec::with_capacity(translations.len());

    for translation in translations {
        if !seen.insert(translation.id.as_str()) {
            return Err(MarkdownError::DuplicateTranslation(translation.id.clone()));
        }
        let unit = by_id
            .get(translation.id.as_str())
            .ok_or_else(|| MarkdownError::UnknownUnit(translation.id.clone()))?;
        let current =
            markdown
                .get(unit.range.clone())
                .ok_or_else(|| MarkdownError::InvalidRange {
                    id: unit.id.clone(),
                    range: unit.range.clone(),
                })?;
        if current != unit.source {
            return Err(MarkdownError::SourceChanged(unit.id.clone()));
        }
        let replacement = validate_translation(unit, &translation.text).map_err(|findings| {
            MarkdownError::InvalidTranslation {
                id: translation.id.clone(),
                findings,
            }
        })?;
        replacements.push((unit.range.clone(), replacement));
    }

    replacements.sort_by_key(|(range, _)| (range.start, range.end));
    for pair in replacements.windows(2) {
        if pair[0].0.end > pair[1].0.start {
            return Err(MarkdownError::OverlappingRanges {
                first: pair[0].0.clone(),
                second: pair[1].0.clone(),
            });
        }
    }

    let mut output = markdown.to_string();
    for (range, replacement) in replacements.into_iter().rev() {
        output.replace_range(range, &replacement);
    }
    Ok(output)
}

fn build_unit(
    markdown: &str,
    events: &[(Event<'_>, Range<usize>)],
    candidate: Candidate,
    occurrences: &mut BTreeMap<String, usize>,
) -> Result<Option<MarkdownUnit>, MarkdownError> {
    let source =
        markdown
            .get(candidate.range.clone())
            .ok_or_else(|| MarkdownError::InvalidRange {
                id: "source".into(),
                range: candidate.range.clone(),
            })?;
    let has_text = events.iter().any(|(event, range)| {
        range.start >= candidate.range.start
            && range.end <= candidate.range.end
            && matches!(event, Event::Text(text) if !text.trim().is_empty())
    });
    if !has_text {
        return Ok(None);
    }

    let mut protections = Vec::new();
    for (event, range) in events {
        if range.start < candidate.range.start || range.end > candidate.range.end {
            continue;
        }
        let kind = match event {
            Event::Code(_) => Some(ProtectedKind::InlineCode),
            Event::Html(_) | Event::InlineHtml(_) => Some(ProtectedKind::Html),
            Event::Start(Tag::Link {
                dest_url,
                title,
                id,
                ..
            })
            | Event::Start(Tag::Image {
                dest_url,
                title,
                id,
                ..
            }) => {
                add_link_protections(
                    markdown,
                    range.clone(),
                    dest_url,
                    title,
                    id,
                    &mut protections,
                );
                None
            }
            _ => None,
        };
        if let Some(kind) = kind {
            protections.push(LocalProtection {
                range: range.clone(),
                kind,
            });
        }
    }
    add_placeholder_protections(markdown, candidate.range.clone(), &mut protections);
    normalize_protections(&mut protections)?;

    let protected = tokenize(markdown, candidate.range.clone(), protections);
    if let Some(span) = protected.iter().find(|span| source.contains(&span.token)) {
        return Err(MarkdownError::TokenCollision(span.token.clone()));
    }
    let protected_source = apply_tokens(source, &protected);
    let fingerprint = stable_fingerprint(&candidate.kind, &protected_source);
    let occurrence = occurrences.entry(fingerprint.clone()).or_default();
    let id = format!("md-{fingerprint}-{:02}", *occurrence);
    *occurrence += 1;

    Ok(Some(MarkdownUnit {
        id,
        kind: candidate.kind,
        range: candidate.range,
        source: source.to_string(),
        protected_source,
        protected,
        parser_source: Some(markdown.to_owned()),
        context: UnitContext::markdown(),
    }))
}

fn add_link_protections(
    markdown: &str,
    event_range: Range<usize>,
    destination: &str,
    title: &str,
    id: &str,
    protections: &mut Vec<LocalProtection>,
) {
    let Some(raw) = markdown.get(event_range.clone()) else {
        return;
    };
    for value in [destination, title, id] {
        if let Some(relative) = (!value.is_empty()).then(|| raw.rfind(value)).flatten() {
            let start = event_range.start + relative;
            protections.push(LocalProtection {
                range: start..start + value.len(),
                kind: ProtectedKind::LinkTarget,
            });
        }
    }
}

fn add_placeholder_protections(
    markdown: &str,
    unit_range: Range<usize>,
    protections: &mut Vec<LocalProtection>,
) {
    let Some(source) = markdown.get(unit_range.clone()) else {
        return;
    };
    for matched in placeholder_regex().find_iter(source) {
        protections.push(LocalProtection {
            range: unit_range.start + matched.start()..unit_range.start + matched.end(),
            kind: ProtectedKind::Placeholder,
        });
    }
}

fn normalize_protections(protections: &mut Vec<LocalProtection>) -> Result<(), MarkdownError> {
    protections.sort_by_key(|protected| {
        (
            protected.range.start,
            std::cmp::Reverse(protected.range.end),
        )
    });
    let mut normalized: Vec<LocalProtection> = Vec::new();
    for protection in protections.drain(..) {
        if let Some(previous) = normalized.last() {
            if protection.range.start < previous.range.end {
                if protection.range.end <= previous.range.end {
                    continue;
                }
                return Err(MarkdownError::OverlappingRanges {
                    first: previous.range.clone(),
                    second: protection.range,
                });
            }
        }
        normalized.push(protection);
    }
    *protections = normalized;
    Ok(())
}

fn tokenize(
    markdown: &str,
    unit_range: Range<usize>,
    protections: Vec<LocalProtection>,
) -> Vec<ProtectedSpan> {
    protections
        .into_iter()
        .enumerate()
        .filter_map(|(index, protection)| {
            let value = markdown.get(protection.range.clone())?.to_string();
            let label = match protection.kind {
                ProtectedKind::InlineCode => "INLINE_CODE",
                ProtectedKind::LinkTarget => "LINK_TARGET",
                ProtectedKind::Html => "HTML",
                ProtectedKind::Placeholder => "PLACEHOLDER",
                ProtectedKind::MdxEsm => "MDX_ESM",
                ProtectedKind::MdxExpression => "MDX_EXPRESSION",
                ProtectedKind::MdxJsxSyntax => "MDX_JSX_SYNTAX",
            };
            let digest = short_hash(&value);
            let token = format!("@@FANI_{label}_{index:04}_{digest}@@");
            Some(ProtectedSpan {
                token,
                value,
                kind: protection.kind,
                range: protection.range.start - unit_range.start
                    ..protection.range.end - unit_range.start,
            })
        })
        .collect()
}

fn apply_tokens(source: &str, protected: &[ProtectedSpan]) -> String {
    let mut output = source.to_string();
    for span in protected.iter().rev() {
        let local = span.range.start..span.range.end;
        debug_assert_eq!(span.value, source[local.clone()]);
        output.replace_range(local, &span.token);
    }
    output
}

fn restore_protected(unit: &MarkdownUnit, translated: &str) -> String {
    let mut restored = translated.to_string();
    for protected in &unit.protected {
        restored = restored.replace(&protected.token, &protected.value);
    }
    restored
}

fn stable_fingerprint(kind: &UnitKind, protected_source: &str) -> String {
    let normalized = protected_source
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    short_hash(&format!("{}\0{normalized}", kind.as_str()))
}

fn short_hash(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest[..8]
        .iter()
        .fold(String::with_capacity(16), |mut output, byte| {
            write!(&mut output, "{byte:02x}").unwrap();
            output
        })
}

fn inline_code_values(markdown: &str) -> Vec<String> {
    let mut values = Parser::new_ext(markdown, Options::all())
        .filter_map(|event| match event {
            Event::Code(value) => Some(value.into_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    values.sort();
    values
}

pub(crate) fn document_signature(markdown: &str) -> Vec<String> {
    let mut signature = structure_signature(markdown)
        .into_iter()
        .filter(|part| part != "text")
        .collect::<Vec<_>>();
    let mut code_block = false;
    for event in Parser::new_ext(markdown, Options::all()) {
        match event {
            Event::Start(Tag::CodeBlock(_) | Tag::MetadataBlock(_)) => code_block = true,
            Event::End(TagEnd::CodeBlock | TagEnd::MetadataBlock(_)) => code_block = false,
            Event::Text(value) if code_block => signature.push(format!("code-text:{value}")),
            _ => {}
        }
    }
    let parser = Parser::new_ext(markdown, Options::all());
    let mut definitions = parser
        .reference_definitions()
        .iter()
        .map(|(_, definition)| definition.span.clone())
        .collect::<Vec<_>>();
    definitions.sort_by_key(|range| range.start);
    for range in definitions {
        signature.push(format!("reference:{}", &markdown[range]));
    }
    if let Ok(units) = extract_units_checked(markdown) {
        let mut end = 0;
        for unit in units {
            if unit.range.start >= end {
                signature.push(format!("immutable:{}", &markdown[end..unit.range.start]));
            }
            // Nested definition units can overlap their containing unit.
            end = end.max(unit.range.end);
        }
        signature.push(format!("immutable:{}", &markdown[end..]));
    }
    // Prose position is not structural; inline code may move safely within a unit.
    signature.extend(
        inline_code_values(markdown)
            .into_iter()
            .map(|value| format!("code-value:{value}")),
    );
    signature
}

fn structure_signature(markdown: &str) -> Vec<String> {
    let mut signature = Vec::new();
    for event in Parser::new_ext(markdown, Options::all()) {
        let part = match event {
            Event::Start(tag) => format!("start:{}", tag_name(&tag)),
            Event::End(tag) => format!("end:{}", tag_end_name(tag)),
            Event::Code(_) => "code".into(),
            Event::Html(value) => format!("html:{value}"),
            Event::InlineHtml(value) => format!("inline-html:{value}"),
            Event::FootnoteReference(value) => format!("footnote:{value}"),
            Event::SoftBreak => "soft-break".into(),
            Event::HardBreak => "hard-break".into(),
            Event::Rule => "rule".into(),
            Event::TaskListMarker(checked) => format!("task:{checked}"),
            Event::InlineMath(value) => format!("inline-math:{value}"),
            Event::DisplayMath(value) => format!("display-math:{value}"),
            Event::Text(_) => "text".into(),
        };
        if part == "text" && signature.last().is_some_and(|previous| previous == "text") {
            continue;
        }
        signature.push(part);
    }
    signature
}

fn tag_name(tag: &Tag<'_>) -> String {
    match tag {
        Tag::Paragraph => "paragraph".into(),
        Tag::Heading {
            level,
            id,
            classes,
            attrs,
        } => {
            format!("heading:{level:?}:{id:?}:{classes:?}:{attrs:?}")
        }
        Tag::BlockQuote(kind) => format!("blockquote:{kind:?}"),
        Tag::CodeBlock(kind) => format!("code-block:{kind:?}"),
        Tag::HtmlBlock => "html-block".into(),
        Tag::List(start) => format!("list:{start:?}"),
        Tag::Item => "item".into(),
        Tag::FootnoteDefinition(label) => format!("footnote-definition:{label}"),
        Tag::DefinitionList => "definition-list".into(),
        Tag::DefinitionListTitle => "definition-title".into(),
        Tag::DefinitionListDefinition => "definition".into(),
        Tag::Table(alignment) => format!("table:{alignment:?}"),
        Tag::TableHead => "table-head".into(),
        Tag::TableRow => "table-row".into(),
        Tag::TableCell => "table-cell".into(),
        Tag::Emphasis => "emphasis".into(),
        Tag::Strong => "strong".into(),
        Tag::Strikethrough => "strikethrough".into(),
        Tag::Superscript => "superscript".into(),
        Tag::Subscript => "subscript".into(),
        Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        } => format!("link:{link_type:?}:{dest_url}:{title}:{id}"),
        Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        } => format!("image:{link_type:?}:{dest_url}:{title}:{id}"),
        Tag::MetadataBlock(kind) => format!("metadata:{kind:?}"),
    }
}

fn tag_end_name(tag: TagEnd) -> String {
    format!("{tag:?}")
}

fn placeholder_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r#"(?x)
            \{\{[^{}\r\n]+\}\}
            |\$\{[A-Za-z_][A-Za-z0-9_.-]*\}
            |%\([A-Za-z_][A-Za-z0-9_.-]*\)[\#0 +\-]?[0-9]*(?:\.[0-9]+)?[A-Za-z]
            |%(?:[1-9][0-9]*\$)?[\#0 +\-]?[0-9]*(?:\.[0-9]+)?[A-Za-z%]
            |\{[A-Za-z_][A-Za-z0-9_.-]*(?::[^{}\r\n]+)?\}
            |@@[A-Za-z_][A-Za-z0-9_.*-]*@@
            "#,
        )
        .expect("placeholder regex is valid")
    })
}

fn protection_token_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX
        .get_or_init(|| Regex::new(r"@@FANI_[^@\r\n]*@@").expect("protection token regex is valid"))
}
