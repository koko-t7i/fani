use crate::domain::markdown;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ops::Range;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentFormat {
    Markdown,
    Mdx,
    Json,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum UnitKind {
    Paragraph,
    Heading,
    ListItem,
    TableCell,
    DefinitionTerm,
    Definition,
    StringValue,
    JsxText,
}

impl UnitKind {
    // These names are part of the existing Markdown memory and ID contract.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Paragraph => "Paragraph",
            Self::Heading => "Heading",
            Self::ListItem => "ListItem",
            Self::TableCell => "TableCell",
            Self::DefinitionTerm => "DefinitionTerm",
            Self::Definition => "Definition",
            Self::StringValue => "StringValue",
            Self::JsxText => "JsxText",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtectedKind {
    InlineCode,
    LinkTarget,
    Html,
    Placeholder,
    MdxEsm,
    MdxExpression,
    MdxJsxSyntax,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProtectedSpan {
    pub token: String,
    pub value: String,
    pub kind: ProtectedKind,
    pub range: Range<usize>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UnitContext {
    pub format: DocumentFormat,
    pub version: String,
    pub structural_path: Option<String>,
    pub token_contract: String,
}

impl UnitContext {
    pub fn markdown() -> Self {
        Self {
            format: DocumentFormat::Markdown,
            version: "fani-markdown-unit-v1".into(),
            structural_path: None,
            token_contract: "fani-markdown-tokens-v1".into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TranslatableUnit {
    pub id: String,
    pub kind: UnitKind,
    pub range: Range<usize>,
    // Logical text may differ from the source range (for example, decoded JSON).
    pub source: String,
    pub protected_source: String,
    pub protected: Vec<ProtectedSpan>,
    #[serde(default)]
    pub parser_source: Option<String>,
    pub context: UnitContext,
}

impl TranslatableUnit {
    pub fn memory_context_key(&self, document_identity: &str) -> String {
        serde_json::to_string(&serde_json::json!({
            "document": document_identity,
            "kind": self.kind.as_str(),
            "context": self.context,
            "contract": format_contract(self.context.format).ok(),
        }))
        .expect("unit contexts are serializable")
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UnitProvenance {
    pub document_path: String,
    pub source: String,
    pub source_revision: String,
    pub context_json: String,
    pub policy_fingerprint: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StoredUnitContext {
    pub kind: UnitKind,
    pub context: Option<UnitContext>,
    pub contract: Option<FormatContract>,
    pub memory_key: Option<String>,
    #[serde(default)]
    pub parser_source: Option<String>,
}

pub fn unit_metadata(unit: &TranslatableUnit, document_path: &str) -> String {
    serde_json::to_string(&StoredUnitContext {
        kind: unit.kind.clone(),
        context: Some(unit.context.clone()),
        contract: format_contract(unit.context.format).ok(),
        memory_key: Some(unit.memory_context_key(document_path)),
        parser_source: unit.parser_source.clone(),
    })
    .expect("unit metadata is serializable")
}

pub fn compatible_metadata(provenance: &UnitProvenance) -> Option<StoredUnitContext> {
    let metadata: StoredUnitContext = serde_json::from_str(&provenance.context_json).ok()?;
    let current = format_contract(DocumentFormat::Markdown).ok()?;
    match (&metadata.context, &metadata.contract) {
        (Some(context), Some(contract)) if context == &UnitContext::markdown() => {
            let mut previous = current.clone();
            previous.verifier = "fani-markdown-document-verifier-v1".into();
            if contract != &current && contract != &previous {
                return None;
            }
        }
        (None, None)
            if matches!(
                metadata.kind,
                UnitKind::Paragraph
                    | UnitKind::Heading
                    | UnitKind::ListItem
                    | UnitKind::TableCell
                    | UnitKind::DefinitionTerm
                    | UnitKind::Definition
            ) && (provenance.policy_fingerprint
                == crate::domain::prompts::policy_fingerprint()
                || provenance.policy_fingerprint == "0".repeat(64)) => {}
        _ => return None,
    }
    Some(metadata)
}

pub fn validate_provenance(
    document_path: &str,
    unit: &TranslatableUnit,
    provenance: &UnitProvenance,
    translated: &str,
) -> Result<String, Vec<ValidationFinding>> {
    let metadata = compatible_metadata(provenance);
    if provenance.document_path != document_path || metadata.is_none() {
        return Err(vec![ValidationFinding {
            code: "DOCUMENT-REUSE",
            message: "document identity or stored contract changed".into(),
        }]);
    }
    let metadata = metadata.expect("checked metadata");
    validate_reuse(
        unit,
        &provenance.source,
        &metadata.kind,
        &UnitContext::markdown(),
        &format_contract(DocumentFormat::Markdown).expect("enabled Markdown contract"),
        translated,
    )
}

pub fn stored_unit(provenance: &UnitProvenance) -> Option<TranslatableUnit> {
    let metadata = compatible_metadata(provenance)?;
    let source = metadata
        .parser_source
        .as_deref()
        .unwrap_or(&provenance.source);
    let document = parse_document(DocumentFormat::Markdown, source.as_bytes()).ok()?;
    let mut unit = document.units.into_iter().find(|unit| {
        unit.source == provenance.source
            && (metadata.parser_source.is_none() || unit.kind == metadata.kind)
    })?;
    if metadata.parser_source.is_none() {
        unit.kind = metadata.kind;
    }
    Some(unit)
}

pub fn validate_stored_translation(provenance: &UnitProvenance, translated: &str) -> bool {
    stored_unit(provenance).is_some_and(|unit| {
        validate_provenance(&provenance.document_path, &unit, provenance, translated).is_ok()
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnitTranslation {
    pub id: String,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationFinding {
    pub code: &'static str,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FormatContract {
    pub format: DocumentFormat,
    pub parser: String,
    pub verifier: String,
    pub tokens: String,
    pub units: String,
    pub message_syntax: Option<String>,
}

impl FormatContract {
    pub fn fingerprint(&self) -> String {
        let bytes = serde_json::to_vec(self).expect("format contracts are serializable");
        format!("{:x}", Sha256::digest(bytes))
    }
}

pub fn format_contract(format: DocumentFormat) -> Result<FormatContract, DocumentError> {
    match format {
        DocumentFormat::Markdown => Ok(FormatContract {
            format,
            parser: "fani-pulldown-cmark-all-v1".into(),
            verifier: "fani-markdown-document-verifier-v2".into(),
            tokens: "fani-markdown-tokens-v1".into(),
            units: "fani-markdown-unit-v1".into(),
            message_syntax: None,
        }),
        DocumentFormat::Mdx | DocumentFormat::Json => Err(DocumentError::UnsupportedFormat(format)),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ParsedDocument {
    pub format: DocumentFormat,
    pub source: String,
    pub units: Vec<TranslatableUnit>,
    pub structure_signature: Vec<String>,
    pub contract: FormatContract,
}

#[derive(Debug, Error)]
pub enum DocumentError {
    #[error("document format {0:?} is not enabled")]
    UnsupportedFormat(DocumentFormat),
    #[error("document is not UTF-8: {0}")]
    Utf8(#[from] std::str::Utf8Error),
    #[error(transparent)]
    Markdown(#[from] markdown::MarkdownError),
    #[error("cannot parse {format:?}: {message}")]
    Parse {
        format: DocumentFormat,
        message: String,
    },
    #[error("invalid document byte range {0:?}")]
    InvalidRange(Range<usize>),
    #[error("overlapping document ranges {first:?} and {second:?}")]
    OverlappingRanges {
        first: Range<usize>,
        second: Range<usize>,
    },
    #[error("protected token validation failed: {0:?}")]
    Token(Vec<ValidationFinding>),
    #[error("document resource limit exceeded: {0}")]
    ResourceLimit(String),
    #[error("document structure changed")]
    Structure,
    #[error("document contract is incompatible")]
    IncompatibleContract,
}

pub fn parse_document(
    format: DocumentFormat,
    source: &[u8],
) -> Result<ParsedDocument, DocumentError> {
    let contract = format_contract(format)?;
    let source = std::str::from_utf8(source)?;
    match format {
        DocumentFormat::Markdown => Ok(ParsedDocument {
            format,
            source: source.into(),
            units: markdown::extract_units_checked(source)?,
            structure_signature: markdown::document_signature(source),
            contract,
        }),
        DocumentFormat::Mdx | DocumentFormat::Json => Err(DocumentError::UnsupportedFormat(format)),
    }
}

pub fn validate_unit(
    unit: &TranslatableUnit,
    translated: &str,
) -> Result<String, Vec<ValidationFinding>> {
    match unit.context.format {
        DocumentFormat::Markdown if unit.context == UnitContext::markdown() => {
            markdown::validate_translation(unit, translated)
        }
        _ => Err(vec![ValidationFinding {
            code: "DOCUMENT-CONTRACT",
            message: "unsupported unit format or context contract".into(),
        }]),
    }
}

pub fn translated_unit_text(
    source: &TranslatableUnit,
    target: &TranslatableUnit,
) -> Option<String> {
    if source.kind != target.kind || source.protected.len() != target.protected.len() {
        return None;
    }
    let mut used = vec![false; source.protected.len()];
    let mut text = target.source.clone();
    for span in target.protected.iter().rev() {
        let index = source
            .protected
            .iter()
            .enumerate()
            .position(|(index, original)| {
                !used[index] && original.kind == span.kind && original.value == span.value
            })?;
        used[index] = true;
        if text.get(span.range.clone())? != span.value {
            return None;
        }
        text.replace_range(span.range.clone(), &source.protected[index].token);
    }
    validate_unit(source, &text).ok()?;
    Some(text)
}

pub fn repair_leading_strong_separator(
    unit: &TranslatableUnit,
    translated: &str,
) -> Option<String> {
    match unit.context.format {
        DocumentFormat::Markdown => markdown::repair_leading_strong_separator(unit, translated),
        _ => None,
    }
}

pub fn assemble_document(
    document: &ParsedDocument,
    translations: &[UnitTranslation],
) -> Result<String, DocumentError> {
    let output = match document.format {
        DocumentFormat::Markdown => {
            markdown::apply_translations(&document.source, &document.units, translations)?
        }
        format => return Err(DocumentError::UnsupportedFormat(format)),
    };
    verify_document(document, &output)?;
    Ok(output)
}

pub fn verify_document(document: &ParsedDocument, translated: &str) -> Result<(), DocumentError> {
    if document.contract != format_contract(document.format)? {
        return Err(DocumentError::IncompatibleContract);
    }
    let candidate = parse_document(document.format, translated.as_bytes())?;
    if candidate.structure_signature != document.structure_signature {
        return Err(DocumentError::Structure);
    }
    Ok(())
}

// Callers must first scope stored candidates by repository/document, locale and request identity.
pub fn validate_reuse(
    unit: &TranslatableUnit,
    previous_source: &str,
    previous_kind: &UnitKind,
    previous_context: &UnitContext,
    previous_contract: &FormatContract,
    translated: &str,
) -> Result<String, Vec<ValidationFinding>> {
    if previous_source != unit.source
        || previous_kind != &unit.kind
        || previous_context != &unit.context
        || format_contract(unit.context.format).as_ref().ok() != Some(previous_contract)
    {
        return Err(vec![ValidationFinding {
            code: "DOCUMENT-REUSE",
            message: "source, context or format contract changed".into(),
        }]);
    }
    validate_unit(unit, translated)
}
