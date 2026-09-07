use crate::domain::document::{
    DocumentError, DocumentFormat, FormatContract, ParsedDocument, ProtectedKind, ProtectedSpan,
    TranslatableUnit, UnitContext, UnitKind, UnitTranslation, ValidationFinding,
};
use crate::domain::model::MessageSyntax;
use std::collections::{BTreeMap, BTreeSet};

const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_STRING: usize = 64 * 1024;
const MAX_NODES: usize = 100_000;
const MAX_DEPTH: usize = 64;
const TOKEN_PREFIX: &str = "@@FANI_";

pub fn contract(syntax: MessageSyntax) -> FormatContract {
    FormatContract {
        format: DocumentFormat::Json,
        parser: "fani-json-byte-spans-v1".into(),
        verifier: "fani-json-document-v1".into(),
        tokens: "fani-json-interpolation-tokens-v1".into(),
        units: "fani-json-pointer-skip-whitespace-v1".into(),
        message_syntax: Some(
            match syntax {
                MessageSyntax::Plain => "plain",
                MessageSyntax::I18nextInterpolationV1 => "i18next-interpolation-v1",
            }
            .into(),
        ),
    }
}

pub fn syntax(context: &UnitContext) -> Option<MessageSyntax> {
    if context.format != DocumentFormat::Json
        || context.token_contract != "fani-json-interpolation-tokens-v1"
        || context.structural_path.is_none()
    {
        return None;
    }
    match context.version.as_str() {
        "fani-json-pointer-skip-whitespace-v1:plain" => Some(MessageSyntax::Plain),
        "fani-json-pointer-skip-whitespace-v1:i18next-interpolation-v1" => {
            Some(MessageSyntax::I18nextInterpolationV1)
        }
        _ => None,
    }
}

fn invalid() -> DocumentError {
    DocumentError::Parse {
        format: DocumentFormat::Json,
        message: "invalid JSON resource".into(),
    }
}
fn unsupported() -> DocumentError {
    DocumentError::MessageUnsupported
}

fn placeholders(
    text: &str,
    dialect: MessageSyntax,
) -> Result<Vec<std::ops::Range<usize>>, DocumentError> {
    if text.len() > MAX_STRING {
        return Err(DocumentError::ResourceLimit("JSON string bytes".into()));
    }
    if text.contains(TOKEN_PREFIX) {
        return Err(unsupported());
    }
    let mut spans = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if dialect == MessageSyntax::I18nextInterpolationV1 && text[i..].starts_with("{{") {
            let end = text[i + 2..]
                .find("}}")
                .map(|n| i + 2 + n)
                .ok_or_else(unsupported)?;
            let name = text[i + 2..end].trim_matches(|c: char| c.is_ascii_whitespace());
            if !name.split('.').all(|part| {
                let mut chars = part.bytes();
                chars
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
                    && chars.all(|c| c.is_ascii_alphanumeric() || c == b'_')
            }) {
                return Err(unsupported());
            }
            if spans.len() == 256 {
                return Err(DocumentError::ResourceLimit(
                    "JSON interpolation occurrences".into(),
                ));
            }
            spans.push(i..end + 2);
            i = end + 2;
            continue;
        }
        // Ambiguous template and rich-text candidates are not plain prose.
        if matches!(bytes[i], b'{' | b'}' | b'<' | b'>')
            || text[i..].starts_with("$t(")
            || text[i..].starts_with("%(")
            || text[i..].starts_with("%{")
            || text[i..].starts_with("[[")
            || text[i..].starts_with("[%")
            || text[i..].starts_with("<%")
            || (bytes[i] == b'%'
                && bytes.get(i + 1).is_some_and(|c| {
                    c.is_ascii_alphabetic()
                        || c.is_ascii_digit()
                        || c.is_ascii_whitespace()
                        || matches!(c, b'+' | b'-' | b'.' | b'%' | b'#' | b'*' | b'\'')
                }))
        {
            return Err(unsupported());
        }
        i += text[i..]
            .chars()
            .next()
            .expect("remaining character")
            .len_utf8();
    }
    Ok(spans)
}

fn plural_key(key: &str) -> bool {
    let tail = key.rsplit('_').next().unwrap_or(key);
    matches!(
        tail,
        "zero" | "one" | "two" | "few" | "many" | "other" | "plural" | "ordinal"
    ) && key.contains('_')
        || key.contains("_ordinal_")
        || (key.contains('_') && !tail.is_empty() && tail.bytes().all(|c| c.is_ascii_digit()))
}

pub fn unit(
    text: &str,
    pointer: &str,
    dialect: MessageSyntax,
) -> Result<TranslatableUnit, DocumentError> {
    let spans = placeholders(text, dialect)?;
    let protected: Vec<_> = spans
        .into_iter()
        .enumerate()
        .map(|(index, range)| ProtectedSpan {
            token: format!("{TOKEN_PREFIX}JSON_{index}@@"),
            value: text[range.clone()].into(),
            kind: ProtectedKind::Placeholder,
            range,
        })
        .collect();
    let mut protected_source = text.to_owned();
    for span in protected.iter().rev() {
        protected_source.replace_range(span.range.clone(), &span.token);
    }
    let contract = contract(dialect);
    Ok(TranslatableUnit {
        id: pointer.into(),
        kind: UnitKind::StringValue,
        range: 0..0,
        source: text.into(),
        protected_source,
        protected,
        parser_source: None,
        context: UnitContext {
            format: DocumentFormat::Json,
            version: format!(
                "{}:{}",
                contract.units,
                contract.message_syntax.as_deref().expect("JSON dialect")
            ),
            structural_path: Some(pointer.into()),
            token_contract: contract.tokens,
        },
    })
}

pub fn validate(
    unit: &TranslatableUnit,
    translated: &str,
) -> Result<String, Vec<ValidationFinding>> {
    let check = || -> Result<String, DocumentError> {
        let dialect = syntax(&unit.context).ok_or(DocumentError::IncompatibleContract)?;
        if translated.len() > MAX_STRING + 256 * 32 {
            return Err(DocumentError::ResourceLimit("JSON string bytes".into()));
        }
        let mut restored = translated.to_owned();
        for span in &unit.protected {
            if restored.matches(&span.token).count() != 1 {
                return Err(unsupported());
            }
            restored = restored.replace(&span.token, &span.value);
        }
        let candidate = self::unit(
            &restored,
            unit.context
                .structural_path
                .as_deref()
                .ok_or_else(invalid)?,
            dialect,
        )?;
        if schema(unit) != schema(&candidate) || restored.trim().is_empty() {
            return Err(unsupported());
        }
        Ok(restored)
    };
    check().map_err(|_| {
        vec![ValidationFinding {
            code: "JSON-MESSAGE",
            message: "JSON message or placeholder contract changed".into(),
        }]
    })
}

fn schema(unit: &TranslatableUnit) -> BTreeMap<String, usize> {
    let mut result = BTreeMap::new();
    for span in &unit.protected {
        *result.entry(span.value.clone()).or_default() += 1;
    }
    result
}

struct Parser<'a> {
    source: &'a str,
    at: usize,
    nodes: usize,
    path_bytes: usize,
    dialect: MessageSyntax,
    units: Vec<TranslatableUnit>,
    signature: Vec<String>,
}
impl Parser<'_> {
    fn whitespace(&mut self) {
        while self
            .source
            .as_bytes()
            .get(self.at)
            .is_some_and(|c| matches!(c, b' ' | b'\t' | b'\r' | b'\n'))
        {
            self.at += 1;
        }
    }
    fn take(&mut self, byte: u8) -> bool {
        self.whitespace();
        if self.source.as_bytes().get(self.at) == Some(&byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn string(&mut self) -> Result<(String, std::ops::Range<usize>), DocumentError> {
        self.whitespace();
        let start = self.at;
        if !self.take(b'"') {
            return Err(invalid());
        }
        let bytes = self.source.as_bytes();
        loop {
            let byte = *bytes.get(self.at).ok_or_else(invalid)?;
            self.at += 1;
            if self.at - start > MAX_STRING {
                return Err(DocumentError::ResourceLimit("JSON string bytes".into()));
            }
            match byte {
                b'"' => break,
                b'\\' => {
                    self.at += 1;
                }
                0..=31 => return Err(invalid()),
                _ => {}
            }
        }
        let range = start..self.at;
        let decoded = serde_json::from_str(&self.source[range.clone()]).map_err(|_| invalid())?;
        Ok((decoded, range))
    }
    fn mark(&mut self, pointer: &str, kind: &str, value: &str) {
        self.signature
            .push(serde_json::to_string(&(pointer, kind, value)).expect("signature"));
    }
    fn value(&mut self, pointer: &str, depth: usize) -> Result<(), DocumentError> {
        self.nodes += 1;
        self.path_bytes += pointer.len();
        if pointer.len() > 4096
            || self.path_bytes > MAX_BYTES * 4
            || depth > MAX_DEPTH
            || self.nodes > MAX_NODES
        {
            return Err(DocumentError::ResourceLimit("JSON depth or nodes".into()));
        }
        self.whitespace();
        match self.source.as_bytes().get(self.at) {
            Some(b'{') => {
                self.at += 1;
                self.mark(pointer, "object", "");
                let mut keys = BTreeSet::new();
                if self.take(b'}') {
                    return Ok(());
                }
                loop {
                    let (key, range) = self.string()?;
                    if !keys.insert(key.clone()) {
                        return Err(invalid());
                    }
                    if self.dialect == MessageSyntax::I18nextInterpolationV1 && plural_key(&key) {
                        return Err(unsupported());
                    }
                    let path = format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"));
                    self.mark(&path, "key", &self.source[range]);
                    if !self.take(b':') {
                        return Err(invalid());
                    }
                    self.value(&path, depth + 1)?;
                    if self.take(b'}') {
                        break;
                    }
                    if !self.take(b',') {
                        return Err(invalid());
                    }
                }
            }
            Some(b'[') => {
                self.at += 1;
                let mut index = 0;
                if !self.take(b']') {
                    loop {
                        self.value(&format!("{pointer}/{index}"), depth + 1)?;
                        index += 1;
                        if self.take(b']') {
                            break;
                        }
                        if !self.take(b',') {
                            return Err(invalid());
                        }
                    }
                }
                self.mark(pointer, "array", &index.to_string());
            }
            Some(b'"') => {
                let (text, range) = self.string()?;
                if text.trim().is_empty() {
                    self.mark(pointer, "unselected", &self.source[range]);
                } else {
                    let mut unit = unit(&text, pointer, self.dialect)?;
                    unit.range = range;
                    self.mark(
                        pointer,
                        "string",
                        &serde_json::to_string(&schema(&unit)).expect("schema"),
                    );
                    self.units.push(unit);
                }
            }
            Some(_) => {
                let start = self.at;
                while self.source.as_bytes().get(self.at).is_some_and(|c| {
                    !matches!(c, b',' | b']' | b'}' | b' ' | b'\t' | b'\r' | b'\n')
                }) {
                    self.at += 1;
                }
                let literal = &self.source[start..self.at];
                // Deserialization is bounded to one scalar; containers use this strict span parser.
                let scalar: serde_json::Value =
                    serde_json::from_str(literal).map_err(|_| invalid())?;
                if !matches!(
                    scalar,
                    serde_json::Value::Null
                        | serde_json::Value::Bool(_)
                        | serde_json::Value::Number(_)
                ) {
                    return Err(invalid());
                }
                self.mark(pointer, "scalar", literal);
            }
            None => return Err(invalid()),
        }
        Ok(())
    }
}

pub fn parse(source: &[u8], dialect: MessageSyntax) -> Result<ParsedDocument, DocumentError> {
    if source.len() > MAX_BYTES {
        return Err(DocumentError::ResourceLimit("JSON document bytes".into()));
    }
    let source = std::str::from_utf8(source)?;
    let mut parser = Parser {
        source,
        at: 0,
        nodes: 0,
        path_bytes: 0,
        dialect,
        units: Vec::new(),
        signature: Vec::new(),
    };
    parser.value("", 0)?;
    parser.whitespace();
    if parser.at != source.len() {
        return Err(invalid());
    }
    parser.units.sort_by(|a, b| a.id.cmp(&b.id));
    parser.signature.sort();
    Ok(ParsedDocument {
        format: DocumentFormat::Json,
        source: source.into(),
        units: parser.units,
        structure_signature: parser.signature,
        contract: contract(dialect),
    })
}

pub fn assemble(
    document: &ParsedDocument,
    translations: &[UnitTranslation],
) -> Result<String, DocumentError> {
    let units: BTreeMap<_, _> = document
        .units
        .iter()
        .map(|unit| (unit.id.as_str(), unit))
        .collect();
    let mut replacements = Vec::new();
    let mut seen = BTreeSet::new();
    for translation in translations {
        if !seen.insert(&translation.id) {
            return Err(DocumentError::Structure);
        }
        let unit = units
            .get(translation.id.as_str())
            .ok_or(DocumentError::Structure)?;
        let decoded = validate(unit, &translation.text).map_err(DocumentError::Token)?;
        if decoded != unit.source {
            replacements.push((
                unit.range.clone(),
                serde_json::to_string(&decoded).map_err(|_| invalid())?,
            ));
        }
    }
    replacements.sort_by_key(|(range, _)| range.start);
    let mut output = document.source.clone();
    for (range, literal) in replacements.into_iter().rev() {
        if output.get(range.clone()).is_none() {
            return Err(DocumentError::InvalidRange(range));
        }
        output.replace_range(range, &literal);
    }
    Ok(output)
}
