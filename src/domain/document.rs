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
    pub context: UnitContext,
}

impl TranslatableUnit {
    pub fn memory_context_key(&self, document_identity: &str) -> String {
        serde_json::to_string(&serde_json::json!({
            "document": document_identity,
            "kind": self.kind.as_str(),
            "context": self.context,
        }))
        .expect("unit contexts are serializable")
    }
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
            verifier: "fani-markdown-document-verifier-v1".into(),
            tokens: "fani-markdown-tokens-v1".into(),
            units: "fani-markdown-unit-v1".into(),
            message_syntax: None,
        }),
        DocumentFormat::Mdx | DocumentFormat::Json => Err(DocumentError::UnsupportedFormat(format)),
    }
}

#[derive(Clone, Debug)]
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
            units: markdown::extract_units(source),
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
