use crate::{
    ClaimId, ClaimSetCertificateId, ClaimValidationCertificateId, Digest, NeedKey, NeedKeyError,
    NeedParseError, NeedRequest, multi_need::NeedCoordination,
};
use arrayvec::ArrayVec;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

pub const NEED_IR_FORMAT_REVISION: u16 = 1;
pub const MAX_NEED_IR_BYTES: usize = 16 * 1024;
pub const MAX_NEED_SUBJECTS: usize = 8;
pub const MAX_REQUIRED_OBLIGATIONS: usize = 16;
pub const MAX_PREFERRED_OBLIGATIONS: usize = 8;
pub const MAX_SEMANTIC_CONSTRAINTS: usize = 8;
pub const MAX_NEED_INPUTS: usize = 8;
pub const MAX_OBLIGATION_FACETS: usize = 8;
pub const MAX_PROOF_ARTIFACTS: usize = 8;
pub const MAX_PROOF_CANDIDATES: usize = 64;
pub const MAX_DERIVATION_DEPTH: usize = 3;

pub fn need_grammar_definition_digest() -> Digest {
    let mut hasher = CanonicalHasher::new(b"need-grammar");
    hasher.field_u16(NEED_IR_FORMAT_REVISION);
    for header in [
        "@route",
        "@coordination",
        "@subject",
        "@require",
        "@prefer",
        "@constraint",
        "@world",
        "@input",
        "@project",
    ] {
        hasher.field_str(header);
    }
    hasher.field_u16(MAX_NEED_SUBJECTS as u16);
    hasher.field_u16(MAX_REQUIRED_OBLIGATIONS as u16);
    hasher.field_u16(MAX_PREFERRED_OBLIGATIONS as u16);
    hasher.field_u16(MAX_SEMANTIC_CONSTRAINTS as u16);
    hasher.finish()
}

/// Content address for the transport-independent semantic NeedIR contract.
///
/// This deliberately excludes the textual `@@need` grammar. Hook and MCP
/// transports may encode the same semantic request differently while still
/// compiling to the same identity and proof obligations.
pub fn need_ir_definition_digest() -> Digest {
    let mut hasher = CanonicalHasher::new(b"need-ir-definition");
    hasher.field_u16(NEED_IR_FORMAT_REVISION);
    hasher.field_u32(MAX_NEED_IR_BYTES as u32);
    hasher.field_u16(MAX_NEED_SUBJECTS as u16);
    hasher.field_u16(MAX_REQUIRED_OBLIGATIONS as u16);
    hasher.field_u16(MAX_PREFERRED_OBLIGATIONS as u16);
    hasher.field_u16(MAX_SEMANTIC_CONSTRAINTS as u16);
    hasher.field_u16(MAX_NEED_INPUTS as u16);
    hasher.field_u16(MAX_OBLIGATION_FACETS as u16);
    for subject in
        ["symbol", "cli-option", "configuration-key", "test", "file", "module", "behavior"]
    {
        hasher.field_str(subject);
    }
    for predicate in ["implementation-location", "runtime-flow", "focused-tests"] {
        hasher.field_str(predicate);
    }
    for facet in ["completeness", "granularity", "polarity", "scenario", "selection"] {
        hasher.field_str(facet);
    }
    for world_field in ["source", "platform", "features", "configuration", "toolchain"] {
        hasher.field_str(world_field);
    }
    hasher.finish()
}

macro_rules! semantic_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Digest);

        impl $name {
            pub const fn new(digest: Digest) -> Self {
                Self(digest)
            }

            pub const fn digest(self) -> Digest {
                self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.debug_tuple(stringify!($name)).field(&self.0).finish()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

semantic_id!(NeedId);
semantic_id!(NeedFragmentId);
semantic_id!(ObligationId);
semantic_id!(SubjectId);
semantic_id!(ArtifactRequestId);
semantic_id!(ArtifactId);
semantic_id!(ArtifactValidationCertificateId);
semantic_id!(ReuseSufficiencyCertificateId);
semantic_id!(SelectedPlanId);

/// Incremental, allocation-free encoder for semantic identities.
///
/// Every field is length-prefixed. Callers must still canonical-sort repeated
/// fields before adding them.
pub struct CanonicalHasher {
    inner: blake3::Hasher,
}

impl CanonicalHasher {
    pub fn new(domain: &'static [u8]) -> Self {
        let mut inner = blake3::Hasher::new();
        inner.update(b"needle-canonical\0");
        inner.update(&(domain.len() as u64).to_le_bytes());
        inner.update(domain);
        Self { inner }
    }

    pub fn field_bytes(&mut self, value: &[u8]) {
        self.inner.update(&(value.len() as u64).to_le_bytes());
        self.inner.update(value);
    }

    pub fn field_str(&mut self, value: &str) {
        self.field_bytes(value.as_bytes());
    }

    pub fn field_normalized_lines(&mut self, value: &str) {
        let bytes = value.as_bytes();
        let crlf = bytes.windows(2).filter(|window| *window == b"\r\n").count();
        self.inner.update(&((bytes.len() - crlf) as u64).to_le_bytes());
        let mut start = 0;
        while let Some(relative) = bytes[start..].windows(2).position(|window| window == b"\r\n") {
            let end = start + relative;
            self.inner.update(&bytes[start..end]);
            self.inner.update(b"\n");
            start = end + 2;
        }
        self.inner.update(&bytes[start..]);
    }

    pub fn field_u8(&mut self, value: u8) {
        self.field_bytes(&[value]);
    }

    pub fn field_u16(&mut self, value: u16) {
        self.field_bytes(&value.to_le_bytes());
    }

    pub fn field_u32(&mut self, value: u32) {
        self.field_bytes(&value.to_le_bytes());
    }

    pub fn field_digest(&mut self, value: Digest) {
        self.field_bytes(&value.bytes());
    }

    pub fn finish(self) -> Digest {
        Digest(*self.inner.finalize().as_bytes())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BorrowedFacet<'a> {
    pub key: &'a str,
    pub value: &'a str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BorrowedSubject<'a> {
    pub kind: &'a str,
    pub value: &'a str,
    pub quoted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BorrowedObligation<'a> {
    pub predicate: &'a str,
    pub facets: ArrayVec<BorrowedFacet<'a>, MAX_OBLIGATION_FACETS>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BorrowedNeedIr<'a> {
    pub coordination: Option<NeedCoordination>,
    pub route_hint: Option<&'a str>,
    pub subjects: ArrayVec<BorrowedSubject<'a>, MAX_NEED_SUBJECTS>,
    pub required: ArrayVec<BorrowedObligation<'a>, MAX_REQUIRED_OBLIGATIONS>,
    pub preferred: ArrayVec<BorrowedObligation<'a>, MAX_PREFERRED_OBLIGATIONS>,
    pub semantic_constraints: ArrayVec<&'a str, MAX_SEMANTIC_CONSTRAINTS>,
    pub world: ArrayVec<BorrowedFacet<'a>, MAX_OBLIGATION_FACETS>,
    pub input_artifacts: ArrayVec<&'a str, MAX_NEED_INPUTS>,
    pub projection: ArrayVec<BorrowedFacet<'a>, MAX_OBLIGATION_FACETS>,
    pub body: &'a str,
}

#[derive(Debug, thiserror::Error, Clone, Eq, PartialEq)]
pub enum NeedIrParseError {
    #[error("NeedIR exceeds the 16 KiB input bound")]
    InputBounds,
    #[error("opening marker must be an exact `@@need` line")]
    OpeningMarker,
    #[error("NeedIR requires exactly one closing `@@end` line")]
    ClosingMarker,
    #[error("non-whitespace text follows the closing marker")]
    TrailingText,
    #[error("NeedIR contains a nested marker")]
    NestedMarker,
    #[error("NeedIR header `{0}` is unknown or malformed")]
    Header(String),
    #[error("NeedIR contains duplicate singleton header `{0}`")]
    DuplicateHeader(String),
    #[error("NeedIR exceeds the `{0}` collection bound")]
    CollectionBound(&'static str),
    #[error("NeedIR requires a route, at least one subject, one required obligation, and a body")]
    RequiredFields,
    #[error("NeedIR quoted value is malformed")]
    QuotedValue,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum SemanticInterrupt {
    Legacy { request: NeedRequest },
    Typed { need_ir: NeedIr, coordination: NeedCoordination },
}

#[derive(Debug, thiserror::Error, Clone, Eq, PartialEq)]
pub enum SemanticInterruptParseError {
    #[error("typed NeedIR is malformed: {0}")]
    Typed(#[from] NeedIrParseError),
    #[error("legacy need marker is malformed: {0}")]
    Legacy(#[from] NeedParseError),
}

impl SemanticInterrupt {
    pub fn parse(input: &str) -> Result<Option<Self>, SemanticInterruptParseError> {
        if let Some(borrowed) = BorrowedNeedIr::parse(input)? {
            return Ok(Some(Self::Typed {
                coordination: borrowed.coordination.unwrap_or_default(),
                need_ir: borrowed.to_owned()?,
            }));
        }
        Ok(NeedRequest::parse(input)?.map(|request| Self::Legacy { request }))
    }

    pub fn key(&self) -> &NeedKey {
        match self {
            Self::Legacy { request } => &request.key,
            Self::Typed { need_ir, .. } => {
                need_ir.route_hint.as_ref().expect("the strict typed parser requires a route")
            }
        }
    }

    pub fn body(&self) -> &str {
        match self {
            Self::Legacy { request } => &request.body,
            Self::Typed { need_ir, .. } => &need_ir.body,
        }
    }

    pub fn typed(&self) -> Option<&NeedIr> {
        match self {
            Self::Typed { need_ir, .. } => Some(need_ir),
            Self::Legacy { .. } => None,
        }
    }

    pub fn compatibility_request(&self) -> NeedRequest {
        NeedRequest { key: self.key().clone(), body: self.body().to_owned() }
    }

    pub fn coordination(&self) -> NeedCoordination {
        match self {
            Self::Legacy { .. } => NeedCoordination::WaitResponse,
            Self::Typed { coordination, .. } => *coordination,
        }
    }

    pub fn digest(&self) -> Digest {
        match self {
            Self::Legacy { request } => request.digest(),
            Self::Typed { need_ir, .. } => {
                let mut hash = CanonicalHasher::new(b"semantic-interrupt");
                hash.field_u16(need_ir.format_revision);
                hash.field_str(
                    need_ir.route_hint.as_ref().map(NeedKey::as_str).unwrap_or_default(),
                );
                for subject in &need_ir.subjects {
                    hash.field_u8(subject.kind.tag());
                    hash.field_str(&subject.canonical_name);
                }
                for obligation in need_ir.required.iter().chain(&need_ir.preferred) {
                    hash.field_u8(obligation.predicate.tag());
                    for facet in &obligation.facets {
                        hash.field_str(&facet.key);
                        hash.field_str(&facet.value);
                    }
                }
                for facet in &need_ir.world {
                    hash.field_str(&facet.key);
                    hash.field_str(&facet.value);
                }
                hash.finish()
            }
        }
    }
}

impl<'a> BorrowedNeedIr<'a> {
    /// Parse the v0.4 semantic marker without heap allocation on valid input.
    ///
    /// `Ok(None)` means that the input is not a v0.4 marker. Legacy
    /// `@@need:<route>` is intentionally left to `NeedRequest`.
    pub fn parse(input: &'a str) -> Result<Option<Self>, NeedIrParseError> {
        if input.len() > MAX_NEED_IR_BYTES {
            return if input.trim_start_matches(char::is_whitespace).starts_with("@@need") {
                Err(NeedIrParseError::InputBounds)
            } else {
                Ok(None)
            };
        }
        let input = input.trim_start_matches(char::is_whitespace);
        if !input.starts_with("@@need") {
            return Ok(None);
        }
        let Some(open_end) = input.find('\n') else {
            return Err(NeedIrParseError::OpeningMarker);
        };
        let opening = input[..open_end].strip_suffix('\r').unwrap_or(&input[..open_end]);
        if opening != "@@need" {
            return Ok(None);
        }

        let content = &input[open_end + 1..];
        let mut closing_start = None;
        let mut closing_end = None;
        let mut cursor = 0;
        for segment in content.split_inclusive('\n') {
            let line_with_newline = segment;
            let line = line_with_newline
                .strip_suffix('\n')
                .unwrap_or(line_with_newline)
                .strip_suffix('\r')
                .unwrap_or_else(|| {
                    line_with_newline.strip_suffix('\n').unwrap_or(line_with_newline)
                });
            if line == "@@end" {
                if closing_start.is_some() {
                    return Err(NeedIrParseError::ClosingMarker);
                }
                closing_start = Some(cursor);
                closing_end = Some(cursor + line.len());
            }
            cursor += segment.len();
        }
        let (closing_start, closing_end) =
            closing_start.zip(closing_end).ok_or(NeedIrParseError::ClosingMarker)?;
        if !content[closing_end..].trim().is_empty() {
            return Err(NeedIrParseError::TrailingText);
        }
        let semantic = &content[..closing_start];
        if semantic.contains("\n@@need")
            || semantic.starts_with("@@need")
            || semantic.contains("@@end")
        {
            return Err(NeedIrParseError::NestedMarker);
        }

        let mut parsed = BorrowedNeedIr {
            coordination: None,
            route_hint: None,
            subjects: ArrayVec::new(),
            required: ArrayVec::new(),
            preferred: ArrayVec::new(),
            semantic_constraints: ArrayVec::new(),
            world: ArrayVec::new(),
            input_artifacts: ArrayVec::new(),
            projection: ArrayVec::new(),
            body: "",
        };
        let mut offset = 0;
        let mut body_start = None;
        for segment in semantic.split_inclusive('\n') {
            let raw = segment.strip_suffix('\n').unwrap_or(segment);
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            if line.is_empty() {
                body_start = Some(offset + segment.len());
                break;
            }
            parsed.parse_header(line)?;
            offset += segment.len();
        }
        let Some(body_start) = body_start else {
            return Err(NeedIrParseError::RequiredFields);
        };
        parsed.body = semantic[body_start..].trim_end_matches(['\r', '\n']);
        if parsed.route_hint.is_none()
            || parsed.subjects.is_empty()
            || parsed.required.is_empty()
            || parsed.body.trim().is_empty()
        {
            return Err(NeedIrParseError::RequiredFields);
        }
        Ok(Some(parsed))
    }

    fn parse_header(&mut self, line: &'a str) -> Result<(), NeedIrParseError> {
        let Some((header, value)) = line.split_once(' ') else {
            return Err(NeedIrParseError::Header(line.to_owned()));
        };
        let value = value.trim();
        if value.is_empty() {
            return Err(NeedIrParseError::Header(header.to_owned()));
        }
        match header {
            "@route" => {
                if self.route_hint.replace(value).is_some() {
                    return Err(NeedIrParseError::DuplicateHeader(header.to_owned()));
                }
            }
            "@coordination" => {
                if self.coordination.is_some() {
                    return Err(NeedIrParseError::DuplicateHeader(header.to_owned()));
                }
                self.coordination = Some(
                    NeedCoordination::parse(value)
                        .ok_or_else(|| NeedIrParseError::Header(header.to_owned()))?,
                );
            }
            "@subject" => push_bounded(&mut self.subjects, parse_subject(value)?, "subjects")?,
            "@require" => {
                push_bounded(&mut self.required, parse_obligation(value)?, "required obligations")?
            }
            "@prefer" => push_bounded(
                &mut self.preferred,
                parse_obligation(value)?,
                "preferred obligations",
            )?,
            "@constraint" => {
                push_bounded(&mut self.semantic_constraints, value, "semantic constraints")?
            }
            "@world" => parse_facets(value, &mut self.world)?,
            "@input" => push_bounded(&mut self.input_artifacts, value, "input artifacts")?,
            "@project" => parse_facets(value, &mut self.projection)?,
            _ => return Err(NeedIrParseError::Header(header.to_owned())),
        }
        Ok(())
    }

    pub fn to_owned(&self) -> Result<NeedIr, NeedIrParseError> {
        let route = NeedKey::new(self.route_hint.unwrap_or_default().to_owned())
            .map_err(|_| NeedIrParseError::Header("@route".to_owned()))?;
        let subjects = self
            .subjects
            .iter()
            .map(|subject| {
                Ok(SubjectExpression {
                    kind: SubjectKind::parse(subject.kind)
                        .ok_or_else(|| NeedIrParseError::Header("@subject".to_owned()))?,
                    canonical_name: unquote(subject.value, subject.quoted)?,
                })
            })
            .collect::<Result<Vec<_>, NeedIrParseError>>()?;
        let required = self
            .required
            .iter()
            .map(ObligationExpression::from_borrowed)
            .collect::<Result<_, _>>()?;
        let preferred = self
            .preferred
            .iter()
            .map(ObligationExpression::from_borrowed)
            .collect::<Result<_, _>>()?;
        Ok(NeedIr {
            route_hint: Some(route),
            subjects,
            required,
            preferred,
            semantic_constraints: self
                .semantic_constraints
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            world: self.world.iter().map(Facet::from_borrowed).collect(),
            input_artifacts: self.input_artifacts.iter().map(|value| (*value).to_owned()).collect(),
            projection: self.projection.iter().map(Facet::from_borrowed).collect(),
            body: self.body.to_owned(),
            format_revision: NEED_IR_FORMAT_REVISION,
        })
    }
}

fn push_bounded<T, const N: usize>(
    target: &mut ArrayVec<T, N>,
    value: T,
    label: &'static str,
) -> Result<(), NeedIrParseError> {
    target.try_push(value).map_err(|_| NeedIrParseError::CollectionBound(label))
}

fn parse_subject(value: &str) -> Result<BorrowedSubject<'_>, NeedIrParseError> {
    let (kind, value) =
        value.split_once(':').ok_or_else(|| NeedIrParseError::Header("@subject".to_owned()))?;
    if kind.is_empty() || value.is_empty() {
        return Err(NeedIrParseError::Header("@subject".to_owned()));
    }
    let quoted = value.starts_with('"');
    if quoted {
        if !value.ends_with('"') || value.len() < 2 {
            return Err(NeedIrParseError::QuotedValue);
        }
        validate_quoted(&value[1..value.len() - 1])?;
    } else if value.chars().any(|character| {
        character.is_whitespace() || character.is_control() || matches!(character, '"' | '\\')
    }) {
        return Err(NeedIrParseError::QuotedValue);
    }
    Ok(BorrowedSubject { kind, value, quoted })
}

fn parse_obligation(value: &str) -> Result<BorrowedObligation<'_>, NeedIrParseError> {
    let mut parts = value.split_ascii_whitespace();
    let predicate = parts.next().ok_or_else(|| NeedIrParseError::Header("@require".to_owned()))?;
    let mut facets = ArrayVec::new();
    for part in parts {
        let (key, value) =
            part.split_once('=').ok_or_else(|| NeedIrParseError::Header(predicate.to_owned()))?;
        push_bounded(&mut facets, BorrowedFacet { key, value }, "obligation facets")?;
    }
    Ok(BorrowedObligation { predicate, facets })
}

fn parse_facets<'a>(
    value: &'a str,
    target: &mut ArrayVec<BorrowedFacet<'a>, MAX_OBLIGATION_FACETS>,
) -> Result<(), NeedIrParseError> {
    if !target.is_empty() {
        return Err(NeedIrParseError::DuplicateHeader("facet group".to_owned()));
    }
    for part in value.split_ascii_whitespace() {
        let (key, value) =
            part.split_once('=').ok_or_else(|| NeedIrParseError::Header(part.to_owned()))?;
        push_bounded(target, BorrowedFacet { key, value }, "facets")?;
    }
    Ok(())
}

fn unquote(value: &str, quoted: bool) -> Result<String, NeedIrParseError> {
    if !quoted {
        return Ok(value.to_owned());
    }
    let inner = &value[1..value.len() - 1];
    if !inner.contains('\\') {
        return Ok(inner.to_owned());
    }
    let mut output = String::with_capacity(inner.len());
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        match characters.next() {
            Some('\\') => output.push('\\'),
            Some('"') => output.push('"'),
            _ => return Err(NeedIrParseError::QuotedValue),
        }
    }
    Ok(output)
}

fn validate_quoted(inner: &str) -> Result<(), NeedIrParseError> {
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        if character.is_control() || character == '"' {
            return Err(NeedIrParseError::QuotedValue);
        }
        if character == '\\' && !matches!(characters.next(), Some('\\' | '"')) {
            return Err(NeedIrParseError::QuotedValue);
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NeedIr {
    pub route_hint: Option<NeedKey>,
    pub subjects: Vec<SubjectExpression>,
    pub required: Vec<ObligationExpression>,
    pub preferred: Vec<ObligationExpression>,
    pub semantic_constraints: Vec<String>,
    pub world: Vec<Facet>,
    pub input_artifacts: Vec<String>,
    pub projection: Vec<Facet>,
    pub body: String,
    pub format_revision: u16,
}

impl NeedIr {
    pub fn parse(input: &str) -> Result<Option<Self>, NeedIrParseError> {
        BorrowedNeedIr::parse(input)?.map(|borrowed| borrowed.to_owned()).transpose()
    }

    /// Hashes a canonicalized transport payload without serializing it.
    /// Repeated collections must already be deterministically ordered.
    pub fn transport_digest(&self) -> Digest {
        let mut hasher = CanonicalHasher::new(b"need-ir-transport");
        hasher.field_u16(self.format_revision);
        if let Some(route) = self.route_hint.as_ref() {
            hasher.field_u8(1);
            hasher.field_str(route.as_str());
        } else {
            hasher.field_u8(0);
        }
        for subject in &self.subjects {
            hasher.field_str(subject.kind.as_str());
            hasher.field_str(&subject.canonical_name);
        }
        hash_obligation_expressions(&mut hasher, &self.required);
        hash_obligation_expressions(&mut hasher, &self.preferred);
        for constraint in &self.semantic_constraints {
            hasher.field_str(constraint);
        }
        for facet in &self.world {
            hasher.field_str(&facet.key);
            hasher.field_str(&facet.value);
        }
        for input in &self.input_artifacts {
            hasher.field_str(input);
        }
        for facet in &self.projection {
            hasher.field_str(&facet.key);
            hasher.field_str(&facet.value);
        }
        hasher.field_normalized_lines(&self.body);
        hasher.finish()
    }
}

fn hash_obligation_expressions(hasher: &mut CanonicalHasher, values: &[ObligationExpression]) {
    for obligation in values {
        hasher.field_str(obligation.predicate.as_str());
        for facet in &obligation.facets {
            hasher.field_str(&facet.key);
            hasher.field_str(&facet.value);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubjectKind {
    Symbol,
    CliOption,
    ConfigurationKey,
    Test,
    File,
    Module,
    Behavior,
}

impl SubjectKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Symbol => "symbol",
            Self::CliOption => "cli-option",
            Self::ConfigurationKey => "configuration-key",
            Self::Test => "test",
            Self::File => "file",
            Self::Module => "module",
            Self::Behavior => "behavior",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "symbol" => Some(Self::Symbol),
            "cli-option" => Some(Self::CliOption),
            "configuration-key" | "config-key" => Some(Self::ConfigurationKey),
            "test" => Some(Self::Test),
            "file" => Some(Self::File),
            "module" => Some(Self::Module),
            "behavior" => Some(Self::Behavior),
            _ => None,
        }
    }

    fn tag(self) -> u8 {
        match self {
            Self::Symbol => 0,
            Self::CliOption => 1,
            Self::ConfigurationKey => 2,
            Self::Test => 3,
            Self::File => 4,
            Self::Module => 5,
            Self::Behavior => 6,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubjectExpression {
    pub kind: SubjectKind,
    pub canonical_name: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Facet {
    pub key: String,
    pub value: String,
}

impl Facet {
    fn from_borrowed(value: &BorrowedFacet<'_>) -> Self {
        Self { key: value.key.to_owned(), value: value.value.to_owned() }
    }
}

impl Ord for Facet {
    fn cmp(&self, other: &Self) -> Ordering {
        (&self.key, &self.value).cmp(&(&other.key, &other.value))
    }
}

impl PartialOrd for Facet {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PredicateKind {
    ImplementationLocation,
    RuntimeFlow,
    FocusedTests,
}

impl PredicateKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ImplementationLocation => "implementation-location",
            Self::RuntimeFlow => "runtime-flow",
            Self::FocusedTests => "focused-tests",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "implementation-location" => Some(Self::ImplementationLocation),
            "runtime-flow" => Some(Self::RuntimeFlow),
            "focused-tests" => Some(Self::FocusedTests),
            _ => None,
        }
    }

    fn tag(self) -> u8 {
        match self {
            Self::ImplementationLocation => 0,
            Self::RuntimeFlow => 1,
            Self::FocusedTests => 2,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObligationExpression {
    pub predicate: PredicateKind,
    pub facets: Vec<Facet>,
}

impl ObligationExpression {
    fn from_borrowed(value: &BorrowedObligation<'_>) -> Result<Self, NeedIrParseError> {
        let predicate = PredicateKind::parse(value.predicate)
            .ok_or_else(|| NeedIrParseError::Header(value.predicate.to_owned()))?;
        let mut facets = value.facets.iter().map(Facet::from_borrowed).collect::<Vec<_>>();
        facets.sort();
        facets.dedup();
        Ok(Self { predicate, facets })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subject {
    pub id: SubjectId,
    pub kind: SubjectKind,
    pub canonical_name: String,
    pub repository_lineage: Digest,
}

impl Subject {
    pub fn exact(
        repository_lineage: Digest,
        kind: SubjectKind,
        canonical_name: impl Into<String>,
    ) -> Self {
        let canonical_name = canonical_name.into();
        let mut hash = CanonicalHasher::new(b"subject");
        hash.field_digest(repository_lineage);
        hash.field_u8(kind.tag());
        hash.field_str(&canonical_name);
        Self { id: SubjectId(hash.finish()), kind, canonical_name, repository_lineage }
    }

    pub fn is_canonical(&self) -> bool {
        let mut hash = CanonicalHasher::new(b"subject");
        hash.field_digest(self.repository_lineage);
        hash.field_u8(self.kind.tag());
        hash.field_str(&self.canonical_name);
        self.id == SubjectId(hash.finish())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticWorld {
    pub repository_lineage: Digest,
    pub source_selector: String,
    pub platform: String,
    pub features: String,
    pub configuration: Option<Digest>,
    pub toolchain: Option<Digest>,
}

impl SemanticWorld {
    pub fn id(&self) -> Digest {
        let mut hash = CanonicalHasher::new(b"semantic-world");
        hash.field_digest(self.repository_lineage);
        hash.field_str(&self.source_selector);
        hash.field_str(&self.platform);
        hash.field_str(&self.features);
        if let Some(value) = self.configuration {
            hash.field_u8(1);
            hash.field_digest(value);
        } else {
            hash.field_u8(0);
        }
        if let Some(value) = self.toolchain {
            hash.field_u8(1);
            hash.field_digest(value);
        } else {
            hash.field_u8(0);
        }
        hash.finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Obligation {
    pub id: ObligationId,
    pub predicate: PredicateKind,
    pub subject: SubjectId,
    pub facets: Vec<Facet>,
}

impl Obligation {
    pub fn new(predicate: PredicateKind, subject: SubjectId, mut facets: Vec<Facet>) -> Self {
        facets.sort();
        facets.dedup();
        let mut hash = CanonicalHasher::new(b"obligation");
        hash.field_u8(predicate.tag());
        hash.field_digest(subject.digest());
        for facet in &facets {
            hash.field_str(&facet.key);
            hash.field_str(&facet.value);
        }
        Self { id: ObligationId(hash.finish()), predicate, subject, facets }
    }

    /// Returns whether this validator-derived obligation is at least as
    /// specific as the requested obligation.
    pub fn satisfies(&self, requested: &Self) -> bool {
        self.predicate == requested.predicate
            && self.subject == requested.subject
            && requested.facets.iter().all(|required| {
                self.facets
                    .iter()
                    .any(|facet| facet.key == required.key && facet.value == required.value)
            })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResidualReason {
    UndeclaredExactAnchor,
    UnsupportedPredicate,
    UnsupportedFacet,
    ConflictingDeclaration,
    AmbiguousSubject,
    UnparsedConstraint,
    LegacyFreeForm,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResidualIntent {
    pub raw_digest: Digest,
    pub reason: ResidualReason,
    pub mandatory: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Need {
    pub id: NeedId,
    pub subjects: Vec<Subject>,
    pub required: Vec<Obligation>,
    pub preferred: Vec<Obligation>,
    pub semantic_constraints: Vec<String>,
    pub world: SemanticWorld,
    pub input_artifacts: Vec<ArtifactId>,
    pub residual: Option<ResidualIntent>,
    pub body_digest: Digest,
    pub format_revision: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NeedFragment {
    pub id: NeedFragmentId,
    pub subjects: Vec<SubjectId>,
    pub subject_definitions: Vec<Subject>,
    pub obligations: Vec<Obligation>,
    pub world: SemanticWorld,
    pub semantic_inputs: Vec<ArtifactId>,
    pub root_need: NeedId,
}

impl NeedFragment {
    pub fn is_consistent(&self) -> bool {
        !self.subjects.is_empty()
            && self.subjects.len() <= MAX_NEED_SUBJECTS
            && self.obligations.len() <= MAX_REQUIRED_OBLIGATIONS
            && self.semantic_inputs.len() <= MAX_NEED_INPUTS
            && self.subjects.len() == self.subject_definitions.len()
            && self.subjects.windows(2).all(|pair| pair[0] < pair[1])
            && self.subject_definitions.windows(2).all(|pair| pair[0].id < pair[1].id)
            && self.obligations.windows(2).all(|pair| pair[0] < pair[1])
            && self.semantic_inputs.windows(2).all(|pair| pair[0] < pair[1])
            && self.subject_definitions.iter().all(|subject| {
                subject.is_canonical()
                    && subject.repository_lineage == self.world.repository_lineage
                    && self.subjects.binary_search(&subject.id).is_ok()
            })
            && self
                .obligations
                .iter()
                .all(|obligation| self.subjects.binary_search(&obligation.subject).is_ok())
            && self.id
                == compute_fragment_id(
                    &self.subjects,
                    &self.obligations,
                    &self.world,
                    &self.semantic_inputs,
                )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredicateContract {
    pub predicate: PredicateKind,
    pub allowed_subject_kinds: Vec<SubjectKind>,
    pub allowed_facets: Vec<String>,
    pub world_dimensions: Vec<String>,
    pub definition_digest: Digest,
}

impl PredicateContract {
    pub fn new(
        predicate: PredicateKind,
        allowed_subject_kinds: Vec<SubjectKind>,
        allowed_facets: Vec<String>,
        world_dimensions: Vec<String>,
    ) -> Self {
        let mut hash = CanonicalHasher::new(b"predicate-contract");
        hash.field_u8(predicate.tag());
        for kind in &allowed_subject_kinds {
            hash.field_u8(kind.tag());
        }
        for facet in &allowed_facets {
            hash.field_str(facet);
        }
        for dimension in &world_dimensions {
            hash.field_str(dimension);
        }
        Self {
            predicate,
            allowed_subject_kinds,
            allowed_facets,
            world_dimensions,
            definition_digest: hash.finish(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteContract {
    pub route: NeedKey,
    pub required: Vec<ObligationExpression>,
    pub preferred: Vec<ObligationExpression>,
    pub allowed_predicates: Vec<PredicateKind>,
    pub proof_budget: ProofBudget,
    pub definition_digest: Digest,
}

impl RouteContract {
    pub fn new(
        route: NeedKey,
        required: Vec<ObligationExpression>,
        preferred: Vec<ObligationExpression>,
        allowed_predicates: Vec<PredicateKind>,
        proof_budget: ProofBudget,
    ) -> Self {
        let mut hash = CanonicalHasher::new(b"route-contract");
        hash.field_str(route.as_str());
        for obligation in required.iter().chain(preferred.iter()) {
            hash.field_u8(obligation.predicate.tag());
            for facet in &obligation.facets {
                hash.field_str(&facet.key);
                hash.field_str(&facet.value);
            }
        }
        for predicate in &allowed_predicates {
            hash.field_u8(predicate.tag());
        }
        hash.field_u16(proof_budget.max_candidates);
        Self {
            route,
            required,
            preferred,
            allowed_predicates,
            proof_budget,
            definition_digest: hash.finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityMode {
    Disabled,
    Shadow,
    Advisory,
    Authoritative,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReuseUnit {
    #[default]
    Artifact,
    Claim,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityClass {
    pub id: String,
    pub predicate: PredicateKind,
    #[serde(default)]
    pub reuse_unit: ReuseUnit,
    pub exact_subject_only: bool,
    pub positive_only: bool,
    pub single_world_only: bool,
    pub composition: bool,
    pub mode: CapabilityMode,
    pub definition_digest: Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProofBudget {
    pub max_candidates: u16,
    pub max_artifacts: u8,
    pub max_derivation_depth: u8,
    pub max_plan_nodes: u8,
    pub max_validation_millis: u32,
    pub max_projection_tokens: u32,
    pub minimum_expected_net_microusd: i64,
}

impl Default for ProofBudget {
    fn default() -> Self {
        Self {
            max_candidates: MAX_PROOF_CANDIDATES as u16,
            max_artifacts: MAX_PROOF_ARTIFACTS as u8,
            max_derivation_depth: MAX_DERIVATION_DEPTH as u8,
            max_plan_nodes: 16,
            max_validation_millis: 100,
            max_projection_tokens: 1_200,
            minimum_expected_net_microusd: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageEntry {
    pub obligation: Obligation,
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageManifest {
    pub entries: Vec<CoverageEntry>,
    pub world: SemanticWorld,
    pub dependency_manifest_digest: Digest,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TestPlanEvidenceStatus {
    #[default]
    Located,
    Executed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactValidationCertificate {
    pub id: ArtifactValidationCertificateId,
    pub artifact: ArtifactId,
    pub input_artifacts: Vec<ArtifactId>,
    /// Immutable execution evidence used by the trusted validator. These IDs
    /// are validation provenance and intentionally do not affect ArtifactId.
    #[serde(default)]
    pub evidence_ids: Vec<String>,
    /// A focused test may be statically located without being executed. This
    /// distinction is validation provenance and never changes ArtifactId.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_plan_evidence: Option<TestPlanEvidenceStatus>,
    pub coverage: CoverageManifest,
    pub validator_definition: Digest,
    pub dependency_checks_digest: Digest,
    pub issued_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatisfactionStep {
    pub obligation: ObligationId,
    pub artifact: ArtifactId,
    pub validation_certificate: ArtifactValidationCertificateId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReuseSufficiencyCertificate {
    pub id: ReuseSufficiencyCertificateId,
    pub need: NeedId,
    pub obligations: Vec<ObligationId>,
    pub artifacts: Vec<ArtifactId>,
    pub validation_certificates: Vec<ArtifactValidationCertificateId>,
    pub satisfaction_steps: Vec<SatisfactionStep>,
    pub world_digest: Digest,
    pub freshness_digest: Digest,
    pub contradiction_digest: Digest,
    pub residual: Option<ResidualIntent>,
    pub engine_definition: Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanEconomics {
    pub expected_fresh_microusd: Option<u64>,
    pub expected_selected_microusd: Option<u64>,
    pub proof_overhead_micros: u64,
    pub expected_net_microusd: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedPlan {
    pub id: SelectedPlanId,
    pub need: NeedId,
    pub artifact_ids: Vec<ArtifactId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claim_ids: Vec<ClaimId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claim_validation_certificate_ids: Vec<ClaimValidationCertificateId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claim_set_certificate_ids: Vec<ClaimSetCertificateId>,
    pub covered_mask: u16,
    pub missing_mask: u16,
    pub economics: PlanEconomics,
    pub proof_budget: ProofBudget,
    pub decision_reason: String,
}

#[derive(Debug, thiserror::Error)]
pub enum NeedCompileError {
    #[error("NeedIR format revision is unsupported")]
    UnsupportedFormat,
    #[error("NeedIR exceeds a semantic collection bound")]
    Bounds,
    #[error("NeedIR route does not match the selected route contract")]
    RouteMismatch,
    #[error("NeedIR requires at least one exact subject")]
    MissingSubject,
    #[error("NeedIR has more than one subject and cannot be resolved authoritatively")]
    AmbiguousSubject,
    #[error("NeedIR contains a predicate not allowed by the route")]
    PredicateNotAllowed,
    #[error("NeedIR contains an unsupported predicate facet")]
    UnsupportedFacet,
    #[error("NeedIR contains conflicting semantic declarations")]
    ConflictingDeclaration,
    #[error("NeedIR contains an unsupported semantic-world dimension")]
    UnsupportedWorld,
    #[error("NeedIR input artifact handle is invalid")]
    InvalidInput,
    #[error("need key is invalid: {0}")]
    NeedKey(#[from] NeedKeyError),
}

pub fn compile_need(
    ir: &NeedIr,
    repository_lineage: Digest,
    route: &RouteContract,
) -> Result<Need, NeedCompileError> {
    if ir.format_revision != NEED_IR_FORMAT_REVISION {
        return Err(NeedCompileError::UnsupportedFormat);
    }
    if ir.body.len() > MAX_NEED_IR_BYTES
        || ir.subjects.len() > MAX_NEED_SUBJECTS
        || ir.required.len() > MAX_REQUIRED_OBLIGATIONS
        || ir.preferred.len() > MAX_PREFERRED_OBLIGATIONS
        || ir.semantic_constraints.len() > MAX_SEMANTIC_CONSTRAINTS
        || ir.input_artifacts.len() > MAX_NEED_INPUTS
        || ir.world.len() > MAX_OBLIGATION_FACETS
        || ir.projection.len() > MAX_OBLIGATION_FACETS
        || ir
            .required
            .iter()
            .chain(&ir.preferred)
            .any(|obligation| obligation.facets.len() > MAX_OBLIGATION_FACETS)
    {
        return Err(NeedCompileError::Bounds);
    }
    if ir.route_hint.as_ref() != Some(&route.route) {
        return Err(NeedCompileError::RouteMismatch);
    }
    if ir.subjects.iter().any(|subject| subject.canonical_name.is_empty()) {
        return Err(NeedCompileError::MissingSubject);
    }
    let mut subjects = ir
        .subjects
        .iter()
        .map(|subject| {
            Subject::exact(repository_lineage, subject.kind, subject.canonical_name.clone())
        })
        .collect::<Vec<_>>();
    subjects.sort_by_key(|subject| subject.id);
    subjects.dedup_by_key(|subject| subject.id);
    let subject_record = subjects.first().ok_or(NeedCompileError::MissingSubject)?;
    if subjects.len() != 1 {
        return Err(NeedCompileError::AmbiguousSubject);
    }
    let required_expressions = merge_obligation_expressions(&route.required, &ir.required)?;
    let mut preferred_expressions = merge_obligation_expressions(&route.preferred, &ir.preferred)?;
    preferred_expressions.retain(|preferred| {
        !required_expressions.iter().any(|required| required.predicate == preferred.predicate)
    });
    let mut required = compile_obligations(&required_expressions, subject_record, route)?;
    let mut preferred = compile_obligations(&preferred_expressions, subject_record, route)?;
    required.sort();
    required.dedup_by_key(|obligation| obligation.id);
    preferred.sort();
    preferred.dedup_by_key(|obligation| obligation.id);

    if ir.world.iter().any(|facet| {
        !matches!(
            facet.key.as_str(),
            "source" | "platform" | "features" | "configuration" | "toolchain"
        )
    }) || has_duplicate_facet_keys(&ir.world)
    {
        return Err(NeedCompileError::UnsupportedWorld);
    }
    let configuration = facet_value(&ir.world, "configuration")
        .map(Digest::parse)
        .transpose()
        .map_err(|_| NeedCompileError::UnsupportedWorld)?;
    let toolchain = facet_value(&ir.world, "toolchain")
        .map(Digest::parse)
        .transpose()
        .map_err(|_| NeedCompileError::UnsupportedWorld)?;
    let world = SemanticWorld {
        repository_lineage,
        source_selector: facet_value(&ir.world, "source").unwrap_or("current").to_owned(),
        platform: facet_value(&ir.world, "platform").unwrap_or("current").to_owned(),
        features: facet_value(&ir.world, "features").unwrap_or("default").to_owned(),
        configuration,
        toolchain,
    };
    let mut input_artifacts = ir
        .input_artifacts
        .iter()
        .map(|value| {
            Digest::parse(value).map(ArtifactId).map_err(|_| NeedCompileError::InvalidInput)
        })
        .collect::<Result<Vec<_>, _>>()?;
    input_artifacts.sort();
    input_artifacts.dedup();
    let mut semantic_constraints = ir.semantic_constraints.clone();
    semantic_constraints.sort();
    semantic_constraints.dedup();
    let residual = if !semantic_constraints.is_empty() {
        Some(ResidualIntent {
            raw_digest: Digest::blake3(semantic_constraints.join("\n")),
            reason: ResidualReason::UnparsedConstraint,
            mandatory: true,
        })
    } else {
        undeclared_exact_anchor(&ir.body, &subjects).map(|anchor| ResidualIntent {
            raw_digest: Digest::blake3(anchor.as_bytes()),
            reason: ResidualReason::UndeclaredExactAnchor,
            mandatory: true,
        })
    };

    let mut hash = CanonicalHasher::new(b"need");
    for subject in &subjects {
        hash.field_digest(subject.id.digest());
    }
    for obligation in &required {
        hash.field_digest(obligation.id.digest());
    }
    for obligation in &preferred {
        hash.field_digest(obligation.id.digest());
    }
    for constraint in &semantic_constraints {
        hash.field_str(constraint);
    }
    hash.field_digest(world.id());
    for input in &input_artifacts {
        hash.field_digest(input.digest());
    }
    let id = NeedId(hash.finish());
    Ok(Need {
        id,
        subjects,
        required,
        preferred,
        semantic_constraints,
        world,
        input_artifacts,
        residual,
        body_digest: Digest::blake3(ir.body.as_bytes()),
        format_revision: NEED_IR_FORMAT_REVISION,
    })
}

fn compile_obligations(
    expressions: &[ObligationExpression],
    subject: &Subject,
    route: &RouteContract,
) -> Result<Vec<Obligation>, NeedCompileError> {
    expressions
        .iter()
        .map(|expression| {
            if !route.allowed_predicates.contains(&expression.predicate) {
                return Err(NeedCompileError::PredicateNotAllowed);
            }
            let contract = built_in_predicate_contracts()
                .into_iter()
                .find(|contract| contract.predicate == expression.predicate)
                .ok_or(NeedCompileError::PredicateNotAllowed)?;
            if !contract.allowed_subject_kinds.contains(&subject.kind)
                || expression
                    .facets
                    .iter()
                    .any(|facet| !contract.allowed_facets.contains(&facet.key))
            {
                return Err(NeedCompileError::UnsupportedFacet);
            }
            Ok(Obligation::new(expression.predicate, subject.id, expression.facets.clone()))
        })
        .collect()
}

fn merge_obligation_expressions(
    route: &[ObligationExpression],
    declared: &[ObligationExpression],
) -> Result<Vec<ObligationExpression>, NeedCompileError> {
    let mut merged = BTreeMap::<PredicateKind, BTreeMap<String, String>>::new();
    for expression in route.iter().chain(declared) {
        let facets = merged.entry(expression.predicate).or_default();
        for facet in &expression.facets {
            if let Some(existing) = facets.get(&facet.key)
                && existing != &facet.value
            {
                return Err(NeedCompileError::ConflictingDeclaration);
            }
            facets.insert(facet.key.clone(), facet.value.clone());
        }
    }
    Ok(merged
        .into_iter()
        .map(|(predicate, facets)| ObligationExpression {
            predicate,
            facets: facets.into_iter().map(|(key, value)| Facet { key, value }).collect(),
        })
        .collect())
}

fn facet_value<'a>(facets: &'a [Facet], key: &str) -> Option<&'a str> {
    facets.iter().find(|facet| facet.key == key).map(|facet| facet.value.as_str())
}

fn has_duplicate_facet_keys(facets: &[Facet]) -> bool {
    facets
        .iter()
        .enumerate()
        .any(|(index, facet)| facets[..index].iter().any(|prior| prior.key == facet.key))
}

fn undeclared_exact_anchor<'a>(body: &'a str, subjects: &[Subject]) -> Option<&'a str> {
    for token in body.split_ascii_whitespace() {
        let trimmed = token.trim_matches(|character: char| {
            matches!(character, ',' | '.' | ';' | ':' | '(' | ')' | '[' | ']' | '`' | '"')
        });
        let exact = trimmed.starts_with("--")
            || trimmed.contains("::")
            || trimmed.ends_with(".rs")
            || trimmed.ends_with(".toml");
        if exact && !subjects.iter().any(|subject| subject.canonical_name == trimmed) {
            return Some(trimmed);
        }
    }
    None
}

pub fn need_fragment(
    need: &Need,
    obligations: Vec<Obligation>,
    semantic_inputs: Vec<ArtifactId>,
) -> NeedFragment {
    let mut obligations = obligations;
    obligations.sort();
    obligations.dedup_by_key(|obligation| obligation.id);
    let mut subjects = obligations.iter().map(|obligation| obligation.subject).collect::<Vec<_>>();
    subjects.sort();
    subjects.dedup();
    let subject_definitions = need
        .subjects
        .iter()
        .filter(|subject| subjects.contains(&subject.id))
        .cloned()
        .collect::<Vec<_>>();
    let mut semantic_inputs = semantic_inputs;
    semantic_inputs.sort();
    semantic_inputs.dedup();
    let id = compute_fragment_id(&subjects, &obligations, &need.world, &semantic_inputs);
    NeedFragment {
        id,
        subjects,
        subject_definitions,
        obligations,
        world: need.world.clone(),
        semantic_inputs,
        root_need: need.id,
    }
}

fn compute_fragment_id(
    subjects: &[SubjectId],
    obligations: &[Obligation],
    world: &SemanticWorld,
    semantic_inputs: &[ArtifactId],
) -> NeedFragmentId {
    let mut hash = CanonicalHasher::new(b"need-fragment");
    for subject in subjects {
        hash.field_digest(subject.digest());
    }
    for obligation in obligations {
        hash.field_digest(obligation.id.digest());
    }
    hash.field_digest(world.id());
    for input in semantic_inputs {
        hash.field_digest(input.digest());
    }
    NeedFragmentId(hash.finish())
}

pub fn built_in_predicate_contracts() -> Vec<PredicateContract> {
    let subject_kinds = vec![
        SubjectKind::Symbol,
        SubjectKind::CliOption,
        SubjectKind::ConfigurationKey,
        SubjectKind::Test,
        SubjectKind::File,
        SubjectKind::Module,
        SubjectKind::Behavior,
    ];
    vec![
        PredicateContract::new(
            PredicateKind::ImplementationLocation,
            subject_kinds.clone(),
            vec!["granularity".to_owned(), "polarity".to_owned(), "selection".to_owned()],
            vec!["repository".to_owned(), "source".to_owned(), "features".to_owned()],
        ),
        PredicateContract::new(
            PredicateKind::RuntimeFlow,
            subject_kinds.clone(),
            vec!["completeness".to_owned(), "granularity".to_owned(), "scenario".to_owned()],
            vec![
                "repository".to_owned(),
                "source".to_owned(),
                "platform".to_owned(),
                "features".to_owned(),
            ],
        ),
        PredicateContract::new(
            PredicateKind::FocusedTests,
            subject_kinds,
            vec!["completeness".to_owned(), "polarity".to_owned(), "selection".to_owned()],
            vec![
                "repository".to_owned(),
                "source".to_owned(),
                "platform".to_owned(),
                "features".to_owned(),
            ],
        ),
    ]
}

fn expression(predicate: PredicateKind, facets: &[(&str, &str)]) -> ObligationExpression {
    ObligationExpression {
        predicate,
        facets: facets
            .iter()
            .map(|(key, value)| Facet { key: (*key).to_owned(), value: (*value).to_owned() })
            .collect(),
    }
}

pub fn built_in_route_contracts() -> Vec<RouteContract> {
    vec![
        RouteContract::new(
            NeedKey::new("locate.implementation").expect("built-in route is valid"),
            vec![expression(
                PredicateKind::ImplementationLocation,
                &[
                    ("granularity", "exact-location"),
                    ("polarity", "positive"),
                    ("selection", "primary"),
                ],
            )],
            Vec::new(),
            vec![PredicateKind::ImplementationLocation, PredicateKind::FocusedTests],
            ProofBudget::default(),
        ),
        RouteContract::new(
            NeedKey::new("trace.state-flow").expect("built-in route is valid"),
            vec![
                expression(
                    PredicateKind::ImplementationLocation,
                    &[("polarity", "positive"), ("selection", "primary")],
                ),
                expression(
                    PredicateKind::RuntimeFlow,
                    &[
                        ("completeness", "contract-complete"),
                        ("granularity", "stepwise"),
                        ("scenario", "default"),
                    ],
                ),
            ],
            vec![expression(PredicateKind::FocusedTests, &[("selection", "representative")])],
            vec![
                PredicateKind::ImplementationLocation,
                PredicateKind::RuntimeFlow,
                PredicateKind::FocusedTests,
            ],
            ProofBudget::default(),
        ),
        RouteContract::new(
            NeedKey::new("tests.relevant").expect("built-in route is valid"),
            vec![expression(
                PredicateKind::FocusedTests,
                &[
                    ("completeness", "open-world"),
                    ("polarity", "positive"),
                    ("selection", "representative"),
                ],
            )],
            Vec::new(),
            vec![PredicateKind::FocusedTests],
            ProofBudget::default(),
        ),
    ]
}

pub fn built_in_capability_classes() -> Vec<CapabilityClass> {
    [
        (
            "implementation-location.exact-positive-single-world",
            PredicateKind::ImplementationLocation,
        ),
        ("runtime-flow.exact-positive-single-world", PredicateKind::RuntimeFlow),
        ("focused-tests.exact-positive-single-world", PredicateKind::FocusedTests),
    ]
    .into_iter()
    .map(|(id, predicate)| {
        let mut hash = CanonicalHasher::new(b"capability-class");
        hash.field_str(id);
        hash.field_u8(predicate.tag());
        hash.field_u8(1);
        hash.field_u8(1);
        hash.field_u8(1);
        hash.field_u8(1);
        CapabilityClass {
            id: id.to_owned(),
            predicate,
            reuse_unit: ReuseUnit::Artifact,
            exact_subject_only: true,
            positive_only: true,
            single_world_only: true,
            composition: true,
            mode: CapabilityMode::Shadow,
            definition_digest: hash.finish(),
        }
    })
    .collect()
}

pub fn built_in_claim_capability_classes() -> Vec<CapabilityClass> {
    [
        (
            "claim.implementation-location.exact-positive-single-world",
            PredicateKind::ImplementationLocation,
        ),
        ("claim.runtime-flow.exact-positive-single-world", PredicateKind::RuntimeFlow),
        ("claim.focused-tests.exact-positive-single-world", PredicateKind::FocusedTests),
    ]
    .into_iter()
    .map(|(id, predicate)| {
        let mut hash = CanonicalHasher::new(b"claim-capability-class");
        hash.field_str(id);
        hash.field_u8(predicate.tag());
        hash.field_u8(1);
        hash.field_u8(1);
        hash.field_u8(1);
        hash.field_u8(1);
        hash.field_u8(1);
        CapabilityClass {
            id: id.to_owned(),
            predicate,
            reuse_unit: ReuseUnit::Claim,
            exact_subject_only: true,
            positive_only: true,
            single_world_only: true,
            composition: true,
            mode: CapabilityMode::Shadow,
            definition_digest: hash.finish(),
        }
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> &'static str {
        "@@need\n\
@route trace.state-flow\n\
@subject cli-option:\"--glob-case-insensitive\"\n\
@require implementation-location selection=primary granularity=exact-location\n\
@require runtime-flow scenario=default completeness=contract-complete\n\
@prefer focused-tests selection=representative\n\
@world source=current features=default\n\
@project detail=compact\n\
\n\
Trace the implementation path and provide evidence.\n\
@@end"
    }

    fn route() -> RouteContract {
        RouteContract::new(
            NeedKey::new("trace.state-flow").unwrap(),
            vec![
                ObligationExpression {
                    predicate: PredicateKind::ImplementationLocation,
                    facets: vec![Facet {
                        key: "selection".to_owned(),
                        value: "primary".to_owned(),
                    }],
                },
                ObligationExpression {
                    predicate: PredicateKind::RuntimeFlow,
                    facets: vec![Facet { key: "scenario".to_owned(), value: "default".to_owned() }],
                },
            ],
            Vec::new(),
            vec![
                PredicateKind::ImplementationLocation,
                PredicateKind::RuntimeFlow,
                PredicateKind::FocusedTests,
            ],
            ProofBudget::default(),
        )
    }

    #[test]
    fn exact_unversioned_marker_parses_and_legacy_is_left_alone() {
        let borrowed = BorrowedNeedIr::parse(example()).unwrap().unwrap();
        assert_eq!(borrowed.coordination, None);
        assert_eq!(borrowed.route_hint, Some("trace.state-flow"));
        assert_eq!(borrowed.subjects.len(), 1);
        assert_eq!(borrowed.required.len(), 2);
        assert_eq!(borrowed.body, "Trace the implementation path and provide evidence.");
        assert_eq!(BorrowedNeedIr::parse("@@need:trace.state-flow\nx\n@@end"), Ok(None));
        assert_eq!(BorrowedNeedIr::parse("ordinary output"), Ok(None));
    }

    #[test]
    fn coordination_defaults_to_wait_and_is_excluded_from_semantic_identity() {
        let wait = example().replacen(
            "@route trace.state-flow\n",
            "@route trace.state-flow\n@coordination wait-response\n",
            1,
        );
        let concurrent = example().replacen(
            "@route trace.state-flow\n",
            "@route trace.state-flow\n@coordination continue-working\n",
            1,
        );
        let default = SemanticInterrupt::parse(example()).unwrap().unwrap();
        let wait = SemanticInterrupt::parse(&wait).unwrap().unwrap();
        let concurrent = SemanticInterrupt::parse(&concurrent).unwrap().unwrap();

        assert_eq!(default.coordination(), NeedCoordination::WaitResponse);
        assert_eq!(wait.coordination(), NeedCoordination::WaitResponse);
        assert_eq!(concurrent.coordination(), NeedCoordination::ContinueWorking);
        assert_eq!(default.digest(), wait.digest());
        assert_eq!(wait.digest(), concurrent.digest());
    }

    #[test]
    fn coordination_is_a_bounded_singleton() {
        let duplicate = example().replacen(
            "@route trace.state-flow\n",
            "@route trace.state-flow\n@coordination wait-response\n@coordination continue-working\n",
            1,
        );
        let unknown = example().replacen(
            "@route trace.state-flow\n",
            "@route trace.state-flow\n@coordination later\n",
            1,
        );
        assert!(matches!(
            BorrowedNeedIr::parse(&duplicate),
            Err(NeedIrParseError::DuplicateHeader(_))
        ));
        assert!(matches!(BorrowedNeedIr::parse(&unknown), Err(NeedIrParseError::Header(_))));
    }

    #[test]
    fn malformed_unknown_and_nested_markers_fail_closed() {
        assert!(BorrowedNeedIr::parse("@@need\n@wat x\n\nbody\n@@end").is_err());
        assert!(BorrowedNeedIr::parse("@@need\n@route x\n\n@@need\n@@end").is_err());
        assert!(BorrowedNeedIr::parse("@@need\n@route x\n\nbody\n@@end\ntrailing").is_err());
        assert!(BorrowedNeedIr::parse(
            "@@need\n@route x\n@subject symbol:\"bad\"suffix\"\n@require focused-tests\n\nbody\n@@end"
        )
        .is_err());
    }

    #[test]
    fn input_and_collection_bounds_include_leading_bytes() {
        let mut oversized = " ".repeat(MAX_NEED_IR_BYTES);
        oversized.push_str(example());
        assert_eq!(BorrowedNeedIr::parse(&oversized), Err(NeedIrParseError::InputBounds));

        let mut subjects = String::from("@@need\n@route locate.implementation\n");
        for index in 0..=MAX_NEED_SUBJECTS {
            subjects.push_str(&format!("@subject symbol:\"symbol-{index}\"\n"));
        }
        subjects
            .push_str("@require implementation-location selection=primary\n\nLocate it.\n@@end");
        assert_eq!(
            BorrowedNeedIr::parse(&subjects),
            Err(NeedIrParseError::CollectionBound("subjects"))
        );
    }

    #[test]
    fn quoted_values_cannot_inject_headers() {
        let injected = "@@need\n\
@route locate.implementation\n\
@subject symbol:\"answer\n\
@require implementation-location selection=primary\"\n\
@require implementation-location selection=primary\n\
\n\
Locate it.\n\
@@end";
        assert!(matches!(BorrowedNeedIr::parse(injected), Err(NeedIrParseError::QuotedValue)));
    }

    #[test]
    fn escaped_subject_uses_only_the_owned_slow_path() {
        let input = "@@need\n@route tests.relevant\n@subject symbol:\"café\\\\\\\"test\"\n@require focused-tests selection=representative\n\nFind tests.\n@@end";
        let borrowed = BorrowedNeedIr::parse(input).unwrap().unwrap();
        assert_eq!(borrowed.subjects[0].value, "\"café\\\\\\\"test\"");
        let owned = borrowed.to_owned().unwrap();
        assert_eq!(owned.subjects[0].canonical_name, "café\\\"test");
    }

    #[test]
    fn canonical_need_excludes_body_and_route_independent_fragment_excludes_root() {
        let ir = NeedIr::parse(example()).unwrap().unwrap();
        let lineage = Digest::blake3(b"repo");
        let need = compile_need(&ir, lineage, &route()).unwrap();
        let mut wording = ir.clone();
        wording.body = "Use different words without changing semantics.".to_owned();
        let other = compile_need(&wording, lineage, &route()).unwrap();
        assert_eq!(need.id, other.id);
        let fragment = need_fragment(&need, vec![need.required[0].clone()], Vec::new());
        let other_fragment = need_fragment(&other, vec![other.required[0].clone()], Vec::new());
        assert_eq!(fragment.id, other_fragment.id);
        assert!(fragment.is_consistent());
        let mut tampered = fragment;
        tampered.subject_definitions[0].canonical_name = "different".to_owned();
        assert!(!tampered.is_consistent());
    }

    #[test]
    fn canonical_need_id_has_a_golden_encoding() {
        let ir = NeedIr::parse(example()).unwrap().unwrap();
        let need = compile_need(&ir, Digest::blake3(b"repo"), &route()).unwrap();
        assert_eq!(
            need.id.to_string(),
            "b3:8432b7b7e6d9d9c8264db426c955b58f3b5bb7c3414054739a07de45566735c3"
        );
    }

    #[test]
    fn undeclared_exact_anchor_creates_mandatory_residual() {
        let mut ir = NeedIr::parse(example()).unwrap().unwrap();
        ir.body.push_str(" Also inspect src/other.rs.");
        let need = compile_need(&ir, Digest::blake3(b"repo"), &route()).unwrap();
        assert_eq!(
            need.residual.as_ref().map(|residual| residual.reason),
            Some(ResidualReason::UndeclaredExactAnchor)
        );
        assert!(need.residual.unwrap().mandatory);
    }

    #[test]
    fn canonical_hasher_is_stable_without_intermediate_encoding() {
        let mut one = CanonicalHasher::new(b"test");
        one.field_str("a");
        one.field_u16(7);
        let mut two = CanonicalHasher::new(b"test");
        two.field_bytes(b"a");
        two.field_bytes(&7_u16.to_le_bytes());
        assert_eq!(one.finish(), two.finish());
    }

    #[test]
    fn conflicting_polarity_and_ambiguous_subjects_fail_closed() {
        let mut polarity = NeedIr::parse(example()).unwrap().unwrap();
        polarity.required[0]
            .facets
            .push(Facet { key: "selection".to_owned(), value: "supporting".to_owned() });
        assert!(matches!(
            compile_need(&polarity, Digest::blake3(b"repo"), &route()),
            Err(NeedCompileError::ConflictingDeclaration)
        ));

        let mut ambiguous = NeedIr::parse(example()).unwrap().unwrap();
        ambiguous.subjects.push(SubjectExpression {
            kind: SubjectKind::Symbol,
            canonical_name: "other".to_owned(),
        });
        assert!(matches!(
            compile_need(&ambiguous, Digest::blake3(b"repo"), &route()),
            Err(NeedCompileError::AmbiguousSubject)
        ));
    }

    #[test]
    fn locate_and_trace_share_the_exact_implementation_fragment() {
        let locate_ir = NeedIr::parse(
            "@@need\n\
             @route locate.implementation\n\
             @subject cli-option:\"--glob-case-insensitive\"\n\
             @require implementation-location selection=primary granularity=exact-location polarity=positive\n\
             @world source=current features=default\n\
             \n\
             Locate the implementation.\n\
             @@end",
        )
        .unwrap()
        .unwrap();
        let trace_ir = NeedIr::parse(example()).unwrap().unwrap();
        let contracts = built_in_route_contracts();
        let lineage = Digest::blake3(b"repo");
        let locate = compile_need(
            &locate_ir,
            lineage,
            contracts
                .iter()
                .find(|contract| contract.route.as_str() == "locate.implementation")
                .unwrap(),
        )
        .unwrap();
        let trace = compile_need(
            &trace_ir,
            lineage,
            contracts
                .iter()
                .find(|contract| contract.route.as_str() == "trace.state-flow")
                .unwrap(),
        )
        .unwrap();
        let locate_obligation = locate
            .required
            .iter()
            .find(|item| item.predicate == PredicateKind::ImplementationLocation)
            .unwrap()
            .clone();
        let trace_obligation = trace
            .required
            .iter()
            .find(|item| item.predicate == PredicateKind::ImplementationLocation)
            .unwrap()
            .clone();
        assert_eq!(locate_obligation.id, trace_obligation.id);
        assert_eq!(
            need_fragment(&locate, vec![locate_obligation], Vec::new()).id,
            need_fragment(&trace, vec![trace_obligation], Vec::new()).id
        );
    }

    #[test]
    fn world_changes_semantic_identity() {
        let ir = NeedIr::parse(example()).unwrap().unwrap();
        let lineage = Digest::blake3(b"repo");
        let current = compile_need(&ir, lineage, &route()).unwrap();
        let mut other_world = ir;
        other_world.world = vec![
            Facet { key: "source".to_owned(), value: "current".to_owned() },
            Facet { key: "features".to_owned(), value: "all".to_owned() },
        ];
        let changed = compile_need(&other_world, lineage, &route()).unwrap();
        assert_ne!(current.id, changed.id);
    }

    #[test]
    fn owned_ir_cannot_bypass_revision_bounds_or_world_consistency() {
        let lineage = Digest::blake3(b"repo");
        let mut ir = NeedIr::parse(example()).unwrap().unwrap();
        ir.format_revision = NEED_IR_FORMAT_REVISION + 1;
        assert!(matches!(
            compile_need(&ir, lineage, &route()),
            Err(NeedCompileError::UnsupportedFormat)
        ));

        ir.format_revision = NEED_IR_FORMAT_REVISION;
        ir.subjects.extend((0..MAX_NEED_SUBJECTS).map(|index| SubjectExpression {
            kind: SubjectKind::Symbol,
            canonical_name: format!("extra-{index}"),
        }));
        assert!(matches!(compile_need(&ir, lineage, &route()), Err(NeedCompileError::Bounds)));

        let mut duplicate_world = NeedIr::parse(example()).unwrap().unwrap();
        duplicate_world.world.push(Facet { key: "source".to_owned(), value: "other".to_owned() });
        assert!(matches!(
            compile_need(&duplicate_world, lineage, &route()),
            Err(NeedCompileError::UnsupportedWorld)
        ));
    }

    #[test]
    fn artifact_and_claim_capabilities_have_independent_authority() {
        let artifacts = built_in_capability_classes();
        let claims = built_in_claim_capability_classes();
        assert_eq!(artifacts.len(), claims.len());
        for artifact in &artifacts {
            let claim =
                claims.iter().find(|candidate| candidate.predicate == artifact.predicate).unwrap();
            assert_eq!(artifact.reuse_unit, ReuseUnit::Artifact);
            assert_eq!(claim.reuse_unit, ReuseUnit::Claim);
            assert_ne!(artifact.id, claim.id);
            assert_ne!(artifact.definition_digest, claim.definition_digest);
        }
    }

    #[test]
    fn legacy_capability_payload_defaults_to_artifact_reuse() {
        let capability = built_in_capability_classes().remove(0);
        let mut payload = serde_json::to_value(&capability).unwrap();
        payload.as_object_mut().unwrap().remove("reuse_unit");
        let decoded: CapabilityClass = serde_json::from_value(payload).unwrap();
        assert_eq!(decoded.reuse_unit, ReuseUnit::Artifact);
        assert_eq!(decoded.definition_digest, capability.definition_digest);
    }
}
