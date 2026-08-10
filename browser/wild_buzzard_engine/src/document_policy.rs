// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fmt;

use wild_buzzard_dom::DocumentVersion;
use wild_buzzard_net::Headers;

use crate::NavigationCommitMetadata;

/// Maximum number of enforcing CSP field lines retained from one response.
pub const MAX_ENFORCING_CSP_FIELDS: usize = 16;
/// Maximum number of report-only CSP field lines retained from one response.
pub const MAX_REPORT_ONLY_CSP_FIELDS: usize = 16;
/// Maximum bytes retained from one CSP field value.
pub const MAX_CSP_FIELD_BYTES: usize = 16 * 1024;
/// Maximum aggregate bytes retained across both CSP field kinds.
pub const MAX_CSP_BYTES: usize = 32 * 1024;
/// Maximum number of Referrer-Policy field lines inspected per response.
pub const MAX_REFERRER_POLICY_FIELDS: usize = 16;
/// Maximum bytes inspected from one Referrer-Policy field value.
pub const MAX_REFERRER_POLICY_FIELD_BYTES: usize = 4 * 1024;
/// Maximum comma-delimited Referrer-Policy tokens inspected per response.
pub const MAX_REFERRER_POLICY_TOKENS: usize = 128;
/// Maximum number of recognized Referrer-Policy inputs retained per response.
pub const MAX_RECOGNIZED_REFERRER_POLICY_INPUTS: usize = 64;
/// Maximum number of Content-Type field lines classified per response.
pub const MAX_CONTENT_TYPE_FIELDS: usize = 8;
/// Maximum bytes inspected from one Content-Type field value.
pub const MAX_CONTENT_TYPE_FIELD_BYTES: usize = 4 * 1024;
/// Maximum charset parameters retained from one Content-Type field value.
pub const MAX_CONTENT_TYPE_CHARSETS: usize = 16;
/// Maximum number of Set-Cookie field lines counted per response.
pub const MAX_SET_COOKIE_FIELDS: usize = 128;
/// Maximum aggregate Set-Cookie value bytes counted per response.
pub const MAX_SET_COOKIE_BYTES: usize = 64 * 1024;
/// Maximum aggregate bytes inspected across all policy-relevant field values.
pub const MAX_DOCUMENT_POLICY_INPUT_BYTES: usize = 64 * 1024;

/// A response field family captured for later policy parsing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentPolicyField {
    /// `Content-Security-Policy`.
    EnforcingContentSecurityPolicy,
    /// `Content-Security-Policy-Report-Only`.
    ReportOnlyContentSecurityPolicy,
    /// `Referrer-Policy`.
    ReferrerPolicy,
    /// `Content-Type`.
    ContentType,
    /// `Set-Cookie`; values are never retained by this layer.
    SetCookie,
    /// All policy-relevant response fields together.
    Aggregate,
}

/// Which bounded resource rejected response-policy capture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentPolicyLimit {
    /// Number of duplicate field lines.
    FieldCount,
    /// Bytes in one field value.
    FieldBytes,
    /// Aggregate bytes in one field family or the whole capture.
    AggregateBytes,
    /// Comma-delimited tokens inspected.
    TokenCount,
    /// Recognized typed inputs retained.
    RetainedInputCount,
    /// Parsed charset parameters retained for one media type.
    CharsetCount,
}

/// Typed, value-redacting failure while capturing response-policy inputs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentPolicyError {
    /// A fixed count, byte, or work bound was exceeded.
    LimitExceeded {
        /// Field family whose bound was exceeded.
        field: DocumentPolicyField,
        /// Kind of resource which reached its limit.
        limit: DocumentPolicyLimit,
        /// Observed count or byte size.
        actual: usize,
        /// Maximum admitted count or byte size.
        maximum: usize,
    },
    /// A checked counter could not represent the next value.
    CounterOverflow {
        /// Field family whose counter overflowed.
        field: DocumentPolicyField,
    },
    /// A bounded owned input could not be allocated.
    AllocationFailed {
        /// Field family whose allocation failed.
        field: DocumentPolicyField,
    },
    /// An internal owner attempted to pair metadata with another response or document.
    BindingMismatch,
}

impl DocumentPolicyError {
    /// Whether the error represents explicit input-count, input-byte, or work exhaustion.
    #[must_use]
    pub const fn is_resource_limit(self) -> bool {
        matches!(
            self,
            Self::LimitExceeded { .. }
                | Self::CounterOverflow { .. }
                | Self::AllocationFailed { .. }
        )
    }

    /// Whether the error is an impossible internal response/document pairing failure.
    #[must_use]
    pub const fn is_binding_mismatch(self) -> bool {
        matches!(self, Self::BindingMismatch)
    }
}

impl fmt::Display for DocumentPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LimitExceeded {
                field,
                limit,
                actual,
                maximum,
            } => write!(
                formatter,
                "document response metadata exceeded {field:?} {limit:?} bound ({actual} > {maximum})"
            ),
            Self::CounterOverflow { field } => {
                write!(
                    formatter,
                    "document response metadata {field:?} counter overflowed"
                )
            }
            Self::AllocationFailed { field } => write!(
                formatter,
                "bounded allocation failed while capturing {field:?} response metadata"
            ),
            Self::BindingMismatch => formatter.write_str(
                "captured response metadata did not match its document/navigation identity",
            ),
        }
    }
}

impl std::error::Error for DocumentPolicyError {}

/// One exact raw CSP field value retained for a later dedicated CSP parser.
///
/// Values remain separate in wire order because CSP field lines are policies,
/// not an HTTP comma-list. The bytes can contain reporting endpoints or other
/// sensitive material, so `Debug` deliberately reports only their length.
#[derive(Eq, PartialEq)]
pub struct CspFieldValue {
    bytes: Vec<u8>,
}

impl CspFieldValue {
    /// Exact validated HTTP field-value bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl fmt::Debug for CspFieldValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CspFieldValue")
            .field("bytes", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

/// A currently recognized Referrer Policy token.
///
/// The capture layer retains recognized inputs in response order. It does not
/// apply them to any request. A later policy owner must perform the applicable
/// selection and request-context algorithm.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReferrerPolicyInput {
    /// `no-referrer`.
    NoReferrer,
    /// `no-referrer-when-downgrade`.
    NoReferrerWhenDowngrade,
    /// `origin`.
    Origin,
    /// `origin-when-cross-origin`.
    OriginWhenCrossOrigin,
    /// `same-origin`.
    SameOrigin,
    /// `strict-origin`.
    StrictOrigin,
    /// `strict-origin-when-cross-origin`.
    StrictOriginWhenCrossOrigin,
    /// `unsafe-url`.
    UnsafeUrl,
}

/// Bounded Referrer-Policy selection inputs and ignored-token evidence.
#[derive(Debug, Eq, PartialEq)]
pub struct ReferrerPolicyMetadata {
    recognized: Vec<ReferrerPolicyInput>,
    ignored_tokens: usize,
}

impl ReferrerPolicyMetadata {
    /// Recognized inputs in field-line and comma-token order.
    #[must_use]
    pub fn recognized_inputs(&self) -> &[ReferrerPolicyInput] {
        &self.recognized
    }

    /// Number of nonempty unknown or non-ASCII tokens ignored by capture.
    #[must_use]
    pub const fn ignored_token_count(&self) -> usize {
        self.ignored_tokens
    }

    /// Last recognized input, without claiming that requests enforce it.
    #[must_use]
    pub fn last_recognized_input(&self) -> Option<ReferrerPolicyInput> {
        self.recognized.last().copied()
    }
}

/// Why a Content-Type field could not be reduced to typed media/charset inputs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MalformedContentType {
    /// The field contained non-ASCII bytes.
    NonAscii,
    /// The media type was empty or lacked an exact `type/subtype` pair.
    InvalidMediaType,
    /// A parameter name, separator, token, or quoted string was malformed.
    InvalidParameter,
    /// A charset value was empty or not an HTTP token after quote removal.
    InvalidCharset,
}

/// Parsed Content-Type selection inputs from one exact field line.
#[derive(Debug, Eq, PartialEq)]
pub struct ParsedContentType {
    media_type: String,
    charsets: Vec<String>,
}

impl ParsedContentType {
    /// ASCII-lowercase `type/subtype`.
    #[must_use]
    pub fn media_type(&self) -> &str {
        &self.media_type
    }

    /// ASCII-lowercase charset values in parameter order.
    #[must_use]
    pub fn charsets(&self) -> impl ExactSizeIterator<Item = &str> {
        self.charsets.iter().map(String::as_str)
    }
}

/// Classification of one Content-Type field line.
///
/// Duplicate field lines remain separate and no invalid comma join is made.
#[derive(Debug, Eq, PartialEq)]
pub enum ContentTypeInput {
    /// A syntactically reduced media type and its charset parameters.
    Parsed(ParsedContentType),
    /// A malformed field, retained only as a non-sensitive typed reason.
    Malformed(MalformedContentType),
}

/// Privacy-safe evidence that Set-Cookie occurred on the final response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SetCookieMetadata {
    field_count: usize,
    value_bytes: usize,
}

impl SetCookieMetadata {
    /// Whether at least one Set-Cookie field line occurred.
    #[must_use]
    pub const fn was_present(self) -> bool {
        self.field_count != 0
    }

    /// Exact number of final-response Set-Cookie field lines.
    #[must_use]
    pub const fn field_count(self) -> usize {
        self.field_count
    }

    /// Exact aggregate bytes in their field values, without retaining values.
    #[must_use]
    pub const fn value_bytes(self) -> usize {
        self.value_bytes
    }
}

/// Bounded final-response metadata retained with one live document.
///
/// This is an observation envelope, **not** a policy-admission or enforcement
/// result. In particular, captured CSP values are not parsed or enforced yet,
/// recognized referrer inputs do not affect requests, Content-Type does not
/// drive encoding/MIME selection, and Set-Cookie values are neither stored nor
/// applied. The exact initial document version and navigation commitment make
/// cross-response transplantation detectable by the engine owner.
pub struct CapturedDocumentResponseMetadata {
    response_document_version: DocumentVersion,
    navigation_commit: NavigationCommitMetadata,
    enforcing_csp: Vec<CspFieldValue>,
    report_only_csp: Vec<CspFieldValue>,
    referrer_policy: ReferrerPolicyMetadata,
    content_types: Vec<ContentTypeInput>,
    set_cookie: SetCookieMetadata,
}

impl CapturedDocumentResponseMetadata {
    /// Initial immutable DOM revision parsed from this exact final response.
    #[must_use]
    pub const fn response_document_version(&self) -> DocumentVersion {
        self.response_document_version
    }

    /// Exact final navigation identity and transport commitment.
    #[must_use]
    pub const fn navigation_commit(&self) -> &NavigationCommitMetadata {
        &self.navigation_commit
    }

    /// Separate enforcing CSP field values in response order.
    #[must_use]
    pub fn enforcing_csp_fields(&self) -> &[CspFieldValue] {
        &self.enforcing_csp
    }

    /// Separate report-only CSP field values in response order.
    #[must_use]
    pub fn report_only_csp_fields(&self) -> &[CspFieldValue] {
        &self.report_only_csp
    }

    /// Recognized and ignored Referrer-Policy input evidence.
    #[must_use]
    pub const fn referrer_policy(&self) -> &ReferrerPolicyMetadata {
        &self.referrer_policy
    }

    /// Separate Content-Type field classifications in response order.
    #[must_use]
    pub fn content_type_fields(&self) -> &[ContentTypeInput] {
        &self.content_types
    }

    /// Count/byte evidence only; no Set-Cookie value is retained.
    #[must_use]
    pub const fn set_cookie(&self) -> SetCookieMetadata {
        self.set_cookie
    }
}

impl fmt::Debug for CapturedDocumentResponseMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let enforcing_csp_bytes = csp_byte_count(&self.enforcing_csp);
        let report_only_csp_bytes = csp_byte_count(&self.report_only_csp);
        formatter
            .debug_struct("CapturedDocumentResponseMetadata")
            .field("response_document_version", &self.response_document_version)
            .field("redirect_count", &self.navigation_commit.redirect_count())
            .field("enforcing_csp_fields", &self.enforcing_csp.len())
            .field("enforcing_csp_bytes", &enforcing_csp_bytes)
            .field("report_only_csp_fields", &self.report_only_csp.len())
            .field("report_only_csp_bytes", &report_only_csp_bytes)
            .field(
                "recognized_referrer_policy_inputs",
                &self.referrer_policy.recognized.len(),
            )
            .field("content_type_fields", &self.content_types.len())
            .field("set_cookie", &self.set_cookie)
            .finish_non_exhaustive()
    }
}

pub(crate) struct UnboundDocumentResponseMetadata {
    enforcing_csp: Vec<CspFieldValue>,
    report_only_csp: Vec<CspFieldValue>,
    referrer_policy: ReferrerPolicyMetadata,
    content_types: Vec<ContentTypeInput>,
    set_cookie: SetCookieMetadata,
}

impl UnboundDocumentResponseMetadata {
    pub(crate) fn bind(
        self,
        response_document_version: DocumentVersion,
        navigation_commit: NavigationCommitMetadata,
    ) -> CapturedDocumentResponseMetadata {
        CapturedDocumentResponseMetadata {
            response_document_version,
            navigation_commit,
            enforcing_csp: self.enforcing_csp,
            report_only_csp: self.report_only_csp,
            referrer_policy: self.referrer_policy,
            content_types: self.content_types,
            set_cookie: self.set_cookie,
        }
    }
}

pub(crate) fn capture_document_response_metadata(
    headers: &Headers,
) -> Result<UnboundDocumentResponseMetadata, DocumentPolicyError> {
    let mut capture = MetadataCapture::default();
    for (name, value) in headers.iter() {
        capture.capture_field(name.as_str(), value.as_bytes())?;
    }
    Ok(capture.finish())
}

#[derive(Default)]
struct MetadataCapture {
    enforcing_csp: Vec<CspFieldValue>,
    report_only_csp: Vec<CspFieldValue>,
    recognized_referrer_policy: Vec<ReferrerPolicyInput>,
    ignored_referrer_tokens: usize,
    referrer_fields: usize,
    referrer_tokens: usize,
    content_types: Vec<ContentTypeInput>,
    set_cookie_fields: usize,
    set_cookie_bytes: usize,
    csp_bytes: usize,
    inspected_bytes: usize,
}

impl MetadataCapture {
    fn capture_field(&mut self, name: &str, value: &[u8]) -> Result<(), DocumentPolicyError> {
        let Some(field) = response_policy_field(name) else {
            return Ok(());
        };
        let prospective_cookie = if field == DocumentPolicyField::SetCookie {
            self.preflight_set_cookie(value.len())?
        } else {
            (0, 0)
        };
        let prospective_inspected = checked_add(self.inspected_bytes, value.len(), field)?;
        enforce_limit(
            DocumentPolicyField::Aggregate,
            DocumentPolicyLimit::AggregateBytes,
            prospective_inspected,
            MAX_DOCUMENT_POLICY_INPUT_BYTES,
        )?;
        match field {
            DocumentPolicyField::EnforcingContentSecurityPolicy => capture_csp(
                &mut self.enforcing_csp,
                &mut self.csp_bytes,
                value,
                field,
                MAX_ENFORCING_CSP_FIELDS,
            )?,
            DocumentPolicyField::ReportOnlyContentSecurityPolicy => capture_csp(
                &mut self.report_only_csp,
                &mut self.csp_bytes,
                value,
                field,
                MAX_REPORT_ONLY_CSP_FIELDS,
            )?,
            DocumentPolicyField::ReferrerPolicy => {
                self.referrer_fields = checked_increment(self.referrer_fields, field)?;
                enforce_limit(
                    field,
                    DocumentPolicyLimit::FieldCount,
                    self.referrer_fields,
                    MAX_REFERRER_POLICY_FIELDS,
                )?;
                enforce_limit(
                    field,
                    DocumentPolicyLimit::FieldBytes,
                    value.len(),
                    MAX_REFERRER_POLICY_FIELD_BYTES,
                )?;
                capture_referrer_policy(
                    value,
                    &mut self.recognized_referrer_policy,
                    &mut self.ignored_referrer_tokens,
                    &mut self.referrer_tokens,
                )?;
            }
            DocumentPolicyField::ContentType => {
                let next_count = checked_increment(self.content_types.len(), field)?;
                enforce_limit(
                    field,
                    DocumentPolicyLimit::FieldCount,
                    next_count,
                    MAX_CONTENT_TYPE_FIELDS,
                )?;
                enforce_limit(
                    field,
                    DocumentPolicyLimit::FieldBytes,
                    value.len(),
                    MAX_CONTENT_TYPE_FIELD_BYTES,
                )?;
                let parsed = parse_content_type(value)?;
                try_push(&mut self.content_types, parsed, field)?;
            }
            DocumentPolicyField::SetCookie => {
                let (field_count, value_bytes) = prospective_cookie;
                self.set_cookie_fields = field_count;
                self.set_cookie_bytes = value_bytes;
            }
            DocumentPolicyField::Aggregate => unreachable!(),
        }
        self.inspected_bytes = prospective_inspected;
        Ok(())
    }

    fn preflight_set_cookie(
        &self,
        value_bytes: usize,
    ) -> Result<(usize, usize), DocumentPolicyError> {
        let field = DocumentPolicyField::SetCookie;
        let field_count = checked_increment(self.set_cookie_fields, field)?;
        enforce_limit(
            field,
            DocumentPolicyLimit::FieldCount,
            field_count,
            MAX_SET_COOKIE_FIELDS,
        )?;
        let value_bytes = checked_add(self.set_cookie_bytes, value_bytes, field)?;
        enforce_limit(
            field,
            DocumentPolicyLimit::AggregateBytes,
            value_bytes,
            MAX_SET_COOKIE_BYTES,
        )?;
        Ok((field_count, value_bytes))
    }

    fn finish(self) -> UnboundDocumentResponseMetadata {
        UnboundDocumentResponseMetadata {
            enforcing_csp: self.enforcing_csp,
            report_only_csp: self.report_only_csp,
            referrer_policy: ReferrerPolicyMetadata {
                recognized: self.recognized_referrer_policy,
                ignored_tokens: self.ignored_referrer_tokens,
            },
            content_types: self.content_types,
            set_cookie: SetCookieMetadata {
                field_count: self.set_cookie_fields,
                value_bytes: self.set_cookie_bytes,
            },
        }
    }
}

fn response_policy_field(name: &str) -> Option<DocumentPolicyField> {
    match name {
        "content-security-policy" => Some(DocumentPolicyField::EnforcingContentSecurityPolicy),
        "content-security-policy-report-only" => {
            Some(DocumentPolicyField::ReportOnlyContentSecurityPolicy)
        }
        "referrer-policy" => Some(DocumentPolicyField::ReferrerPolicy),
        "content-type" => Some(DocumentPolicyField::ContentType),
        "set-cookie" => Some(DocumentPolicyField::SetCookie),
        _ => None,
    }
}

fn capture_csp(
    output: &mut Vec<CspFieldValue>,
    csp_bytes: &mut usize,
    value: &[u8],
    field: DocumentPolicyField,
    maximum_fields: usize,
) -> Result<(), DocumentPolicyError> {
    let next_count = checked_increment(output.len(), field)?;
    enforce_limit(
        field,
        DocumentPolicyLimit::FieldCount,
        next_count,
        maximum_fields,
    )?;
    enforce_limit(
        field,
        DocumentPolicyLimit::FieldBytes,
        value.len(),
        MAX_CSP_FIELD_BYTES,
    )?;
    let prospective_csp_bytes = checked_add(*csp_bytes, value.len(), field)?;
    enforce_limit(
        field,
        DocumentPolicyLimit::AggregateBytes,
        prospective_csp_bytes,
        MAX_CSP_BYTES,
    )?;
    let bytes = try_copy_bytes(value, field)?;
    try_push(output, CspFieldValue { bytes }, field)?;
    *csp_bytes = prospective_csp_bytes;
    Ok(())
}

fn capture_referrer_policy(
    value: &[u8],
    recognized: &mut Vec<ReferrerPolicyInput>,
    ignored_tokens: &mut usize,
    total_tokens: &mut usize,
) -> Result<(), DocumentPolicyError> {
    let field = DocumentPolicyField::ReferrerPolicy;
    for token in value.split(|byte| *byte == b',') {
        let token = trim_ascii_whitespace(token);
        if token.is_empty() {
            continue;
        }
        *total_tokens = checked_increment(*total_tokens, field)?;
        enforce_limit(
            field,
            DocumentPolicyLimit::TokenCount,
            *total_tokens,
            MAX_REFERRER_POLICY_TOKENS,
        )?;
        if let Some(input) = recognized_referrer_policy(token) {
            let next = checked_increment(recognized.len(), field)?;
            enforce_limit(
                field,
                DocumentPolicyLimit::RetainedInputCount,
                next,
                MAX_RECOGNIZED_REFERRER_POLICY_INPUTS,
            )?;
            try_push(recognized, input, field)?;
        } else {
            *ignored_tokens = checked_increment(*ignored_tokens, field)?;
        }
    }
    Ok(())
}

fn recognized_referrer_policy(token: &[u8]) -> Option<ReferrerPolicyInput> {
    if token.eq_ignore_ascii_case(b"no-referrer") {
        Some(ReferrerPolicyInput::NoReferrer)
    } else if token.eq_ignore_ascii_case(b"no-referrer-when-downgrade") {
        Some(ReferrerPolicyInput::NoReferrerWhenDowngrade)
    } else if token.eq_ignore_ascii_case(b"origin") {
        Some(ReferrerPolicyInput::Origin)
    } else if token.eq_ignore_ascii_case(b"origin-when-cross-origin") {
        Some(ReferrerPolicyInput::OriginWhenCrossOrigin)
    } else if token.eq_ignore_ascii_case(b"same-origin") {
        Some(ReferrerPolicyInput::SameOrigin)
    } else if token.eq_ignore_ascii_case(b"strict-origin") {
        Some(ReferrerPolicyInput::StrictOrigin)
    } else if token.eq_ignore_ascii_case(b"strict-origin-when-cross-origin") {
        Some(ReferrerPolicyInput::StrictOriginWhenCrossOrigin)
    } else if token.eq_ignore_ascii_case(b"unsafe-url") {
        Some(ReferrerPolicyInput::UnsafeUrl)
    } else {
        None
    }
}

fn parse_content_type(value: &[u8]) -> Result<ContentTypeInput, DocumentPolicyError> {
    if !value.is_ascii() {
        return Ok(ContentTypeInput::Malformed(MalformedContentType::NonAscii));
    }
    let value = trim_ascii_whitespace(value);
    let Some((media_type, media_end)) = parse_media_type(value)? else {
        return Ok(ContentTypeInput::Malformed(
            MalformedContentType::InvalidMediaType,
        ));
    };
    let charsets = match parse_content_type_parameters(value, media_end)? {
        ParsedCharsetParameters::Parsed(charsets) => charsets,
        ParsedCharsetParameters::Malformed(reason) => {
            return Ok(ContentTypeInput::Malformed(reason));
        }
    };
    Ok(ContentTypeInput::Parsed(ParsedContentType {
        media_type,
        charsets,
    }))
}

fn parse_media_type(value: &[u8]) -> Result<Option<(String, usize)>, DocumentPolicyError> {
    let media_end = value
        .iter()
        .position(|byte| *byte == b';')
        .unwrap_or(value.len());
    let media = trim_ascii_whitespace(&value[..media_end]);
    let Some(slash) = media.iter().position(|byte| *byte == b'/') else {
        return Ok(None);
    };
    if slash == 0
        || slash + 1 == media.len()
        || media[slash + 1..].contains(&b'/')
        || !media[..slash].iter().copied().all(is_http_token_byte)
        || !media[slash + 1..].iter().copied().all(is_http_token_byte)
    {
        return Ok(None);
    }
    let media_type = try_ascii_lowercase_string(media, DocumentPolicyField::ContentType)?;
    Ok(Some((media_type, media_end)))
}

enum ParsedCharsetParameters {
    Parsed(Vec<String>),
    Malformed(MalformedContentType),
}

fn parse_content_type_parameters(
    value: &[u8],
    mut cursor: usize,
) -> Result<ParsedCharsetParameters, DocumentPolicyError> {
    let mut charsets = Vec::new();
    while cursor < value.len() {
        let Some(parameter) = parse_content_type_parameter(value, cursor) else {
            return Ok(ParsedCharsetParameters::Malformed(
                MalformedContentType::InvalidParameter,
            ));
        };
        cursor = parameter.next;

        if parameter.name.eq_ignore_ascii_case(b"charset") {
            let charset = if parameter.quoted {
                unescape_quoted_charset(parameter.value)?
            } else {
                try_ascii_lowercase_string(parameter.value, DocumentPolicyField::ContentType)?
            };
            if charset.is_empty() || !charset.as_bytes().iter().copied().all(is_http_token_byte) {
                return Ok(ParsedCharsetParameters::Malformed(
                    MalformedContentType::InvalidCharset,
                ));
            }
            let next_count = checked_increment(charsets.len(), DocumentPolicyField::ContentType)?;
            enforce_limit(
                DocumentPolicyField::ContentType,
                DocumentPolicyLimit::CharsetCount,
                next_count,
                MAX_CONTENT_TYPE_CHARSETS,
            )?;
            try_push(&mut charsets, charset, DocumentPolicyField::ContentType)?;
        }
    }
    Ok(ParsedCharsetParameters::Parsed(charsets))
}

struct ContentTypeParameter<'input> {
    name: &'input [u8],
    value: &'input [u8],
    quoted: bool,
    next: usize,
}

fn parse_content_type_parameter(
    value: &[u8],
    semicolon: usize,
) -> Option<ContentTypeParameter<'_>> {
    if value.get(semicolon) != Some(&b';') {
        return None;
    }
    let mut cursor = semicolon.checked_add(1)?;
    skip_ows(value, &mut cursor);
    let name_start = cursor;
    while cursor < value.len() && is_http_token_byte(value[cursor]) {
        cursor += 1;
    }
    if cursor == name_start {
        return None;
    }
    let name = &value[name_start..cursor];
    skip_ows(value, &mut cursor);
    if value.get(cursor) != Some(&b'=') {
        return None;
    }
    cursor += 1;
    skip_ows(value, &mut cursor);
    let (parameter_value, next_cursor, quoted) = match value.get(cursor) {
        Some(b'"') => {
            let (parameter_value, next) = parse_quoted_parameter(value, cursor)?;
            (parameter_value, next, true)
        }
        Some(_) => {
            let start = cursor;
            while cursor < value.len() && is_http_token_byte(value[cursor]) {
                cursor += 1;
            }
            (start != cursor).then_some((&value[start..cursor], cursor, false))?
        }
        None => return None,
    };
    cursor = next_cursor;
    skip_ows(value, &mut cursor);
    if cursor < value.len() && value[cursor] != b';' {
        return None;
    }
    Some(ContentTypeParameter {
        name,
        value: parameter_value,
        quoted,
        next: cursor,
    })
}

fn parse_quoted_parameter(value: &[u8], opening_quote: usize) -> Option<(&[u8], usize)> {
    let start = opening_quote.checked_add(1)?;
    let mut cursor = start;
    let mut escaped = false;
    while cursor < value.len() {
        let byte = value[cursor];
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            return Some((&value[start..cursor], cursor + 1));
        }
        cursor += 1;
    }
    None
}

fn unescape_quoted_charset(value: &[u8]) -> Result<String, DocumentPolicyError> {
    let field = DocumentPolicyField::ContentType;
    let mut output = String::new();
    output
        .try_reserve_exact(value.len())
        .map_err(|_| DocumentPolicyError::AllocationFailed { field })?;
    let mut cursor = 0usize;
    while cursor < value.len() {
        let byte = value[cursor];
        if byte == b'\\' {
            cursor = cursor
                .checked_add(1)
                .ok_or(DocumentPolicyError::CounterOverflow { field })?;
            let Some(escaped) = value.get(cursor) else {
                return Ok(String::new());
            };
            output.push(char::from(escaped.to_ascii_lowercase()));
        } else {
            output.push(char::from(byte.to_ascii_lowercase()));
        }
        cursor = cursor
            .checked_add(1)
            .ok_or(DocumentPolicyError::CounterOverflow { field })?;
    }
    Ok(output)
}

fn try_ascii_lowercase_string(
    value: &[u8],
    field: DocumentPolicyField,
) -> Result<String, DocumentPolicyError> {
    debug_assert!(value.is_ascii());
    let mut output = String::new();
    output
        .try_reserve_exact(value.len())
        .map_err(|_| DocumentPolicyError::AllocationFailed { field })?;
    output.extend(
        value
            .iter()
            .map(|byte| char::from(byte.to_ascii_lowercase())),
    );
    Ok(output)
}

fn try_copy_bytes(
    value: &[u8],
    field: DocumentPolicyField,
) -> Result<Vec<u8>, DocumentPolicyError> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(value.len())
        .map_err(|_| DocumentPolicyError::AllocationFailed { field })?;
    output.extend_from_slice(value);
    Ok(output)
}

fn try_push<T>(
    values: &mut Vec<T>,
    value: T,
    field: DocumentPolicyField,
) -> Result<(), DocumentPolicyError> {
    values
        .try_reserve(1)
        .map_err(|_| DocumentPolicyError::AllocationFailed { field })?;
    values.push(value);
    Ok(())
}

fn checked_increment(
    current: usize,
    field: DocumentPolicyField,
) -> Result<usize, DocumentPolicyError> {
    checked_add(current, 1, field)
}

fn checked_add(
    current: usize,
    addition: usize,
    field: DocumentPolicyField,
) -> Result<usize, DocumentPolicyError> {
    current
        .checked_add(addition)
        .ok_or(DocumentPolicyError::CounterOverflow { field })
}

fn enforce_limit(
    field: DocumentPolicyField,
    limit: DocumentPolicyLimit,
    actual: usize,
    maximum: usize,
) -> Result<(), DocumentPolicyError> {
    if actual > maximum {
        Err(DocumentPolicyError::LimitExceeded {
            field,
            limit,
            actual,
            maximum,
        })
    } else {
        Ok(())
    }
}

fn trim_ascii_whitespace(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

fn skip_ows(value: &[u8], cursor: &mut usize) {
    while value
        .get(*cursor)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        *cursor += 1;
    }
}

const fn is_http_token_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'#'..=b'\'' | b'*' | b'+' | b'-' | b'.' | b'0'..=b'9' | b'A'..=b'Z'
            | b'^'..=b'z' | b'|' | b'~'
    )
}

fn csp_byte_count(values: &[CspFieldValue]) -> usize {
    values.iter().fold(0usize, |total, value| {
        total.saturating_add(value.bytes.len())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wild_buzzard_net::{HeaderName, HeaderValue};

    fn headers(fields: &[(&str, &[u8])]) -> Headers {
        let mut headers = Headers::new();
        for (name, value) in fields {
            append(&mut headers, name, value);
        }
        headers
    }

    fn append(headers: &mut Headers, name: &str, value: &[u8]) {
        headers.append(
            HeaderName::new(name).unwrap(),
            HeaderValue::from_bytes(value.to_vec()).unwrap(),
        );
    }

    #[test]
    fn duplicates_remain_separate_and_sensitive_debug_is_redacted() {
        let captured = capture_document_response_metadata(&headers(&[
            (
                "CONTENT-SECURITY-POLICY",
                b"default-src 'self'; report-uri https://secret.invalid/a",
            ),
            ("content-security-policy", b"img-src https:"),
            ("Content-Security-Policy-Report-Only", b"script-src 'none'"),
            ("Referrer-Policy", b"unknown, ORIGIN, no-referrer"),
            ("CONTENT-TYPE", b"Text/HTML; Charset=\"UTF-8\""),
            ("content-type", b"application/xhtml+xml; charset=iso-8859-1"),
            ("Set-Cookie", b"session=top-secret; HttpOnly"),
        ]))
        .unwrap();

        assert_eq!(captured.enforcing_csp.len(), 2);
        assert_eq!(captured.enforcing_csp[1].as_bytes(), b"img-src https:");
        assert_eq!(captured.report_only_csp.len(), 1);
        assert_eq!(
            captured.referrer_policy.recognized,
            [ReferrerPolicyInput::Origin, ReferrerPolicyInput::NoReferrer]
        );
        assert_eq!(captured.referrer_policy.ignored_tokens, 1);
        assert_eq!(captured.content_types.len(), 2);
        assert_eq!(captured.set_cookie.field_count, 1);
        assert_eq!(
            captured.set_cookie.value_bytes,
            b"session=top-secret; HttpOnly".len()
        );

        let csp_debug = format!("{:?}", captured.enforcing_csp[0]);
        assert!(!csp_debug.contains("secret"));
    }

    #[test]
    fn malformed_content_types_are_typed_without_retaining_raw_values() {
        let captured = capture_document_response_metadata(&headers(&[
            ("Content-Type", b"not-a-media-type"),
            ("Content-Type", b"text/html; charset=\"unterminated"),
            ("Content-Type", b"text/html; charset=\"has space\""),
            ("Content-Type", b"text/\xffhtml"),
        ]))
        .unwrap();
        assert_eq!(
            captured.content_types,
            [
                ContentTypeInput::Malformed(MalformedContentType::InvalidMediaType),
                ContentTypeInput::Malformed(MalformedContentType::InvalidParameter),
                ContentTypeInput::Malformed(MalformedContentType::InvalidCharset),
                ContentTypeInput::Malformed(MalformedContentType::NonAscii),
            ]
        );
    }

    #[test]
    fn checked_overflow_and_count_limits_are_typed() {
        assert_eq!(
            checked_add(
                usize::MAX,
                1,
                DocumentPolicyField::EnforcingContentSecurityPolicy
            ),
            Err(DocumentPolicyError::CounterOverflow {
                field: DocumentPolicyField::EnforcingContentSecurityPolicy
            })
        );

        let mut many = Headers::new();
        for _ in 0..=MAX_ENFORCING_CSP_FIELDS {
            many.append(
                HeaderName::new("Content-Security-Policy").unwrap(),
                HeaderValue::from_text("default-src 'self'").unwrap(),
            );
        }
        assert!(matches!(
            capture_document_response_metadata(&many),
            Err(DocumentPolicyError::LimitExceeded {
                field: DocumentPolicyField::EnforcingContentSecurityPolicy,
                limit: DocumentPolicyLimit::FieldCount,
                actual,
                maximum: MAX_ENFORCING_CSP_FIELDS,
            }) if actual == MAX_ENFORCING_CSP_FIELDS + 1
        ));
    }

    #[test]
    fn oversized_values_fail_before_copying() {
        let value = vec![b'a'; MAX_CSP_FIELD_BYTES + 1];
        let captured =
            capture_document_response_metadata(&headers(&[("Content-Security-Policy", &value)]));
        assert!(matches!(
            captured,
            Err(DocumentPolicyError::LimitExceeded {
                field: DocumentPolicyField::EnforcingContentSecurityPolicy,
                limit: DocumentPolicyLimit::FieldBytes,
                actual,
                maximum: MAX_CSP_FIELD_BYTES,
            }) if actual == MAX_CSP_FIELD_BYTES + 1
        ));
    }

    #[test]
    fn every_csp_bound_admits_its_edge_and_rejects_the_next_unit() {
        let maximum_value = vec![b'a'; MAX_CSP_FIELD_BYTES];
        assert!(
            capture_document_response_metadata(&headers(&[(
                "Content-Security-Policy",
                &maximum_value
            ),]))
            .is_ok()
        );
        let oversized_value = vec![b'a'; MAX_CSP_FIELD_BYTES + 1];
        assert!(matches!(
            capture_document_response_metadata(&headers(&[(
                "Content-Security-Policy",
                &oversized_value
            ),])),
            Err(DocumentPolicyError::LimitExceeded {
                field: DocumentPolicyField::EnforcingContentSecurityPolicy,
                limit: DocumentPolicyLimit::FieldBytes,
                maximum: MAX_CSP_FIELD_BYTES,
                ..
            })
        ));

        for (name, maximum) in [
            ("Content-Security-Policy", MAX_ENFORCING_CSP_FIELDS),
            (
                "Content-Security-Policy-Report-Only",
                MAX_REPORT_ONLY_CSP_FIELDS,
            ),
        ] {
            let mut exact = Headers::new();
            for _ in 0..maximum {
                append(&mut exact, name, b"");
            }
            assert!(capture_document_response_metadata(&exact).is_ok());
            append(&mut exact, name, b"");
            assert!(matches!(
                capture_document_response_metadata(&exact),
                Err(DocumentPolicyError::LimitExceeded {
                    limit: DocumentPolicyLimit::FieldCount,
                    actual,
                    maximum: expected,
                    ..
                }) if actual == maximum + 1 && expected == maximum
            ));
        }

        let mut aggregate = Headers::new();
        append(&mut aggregate, "Content-Security-Policy", &maximum_value);
        append(
            &mut aggregate,
            "Content-Security-Policy-Report-Only",
            &maximum_value,
        );
        assert!(capture_document_response_metadata(&aggregate).is_ok());
        append(&mut aggregate, "Content-Security-Policy", b"x");
        assert!(matches!(
            capture_document_response_metadata(&aggregate),
            Err(DocumentPolicyError::LimitExceeded {
                limit: DocumentPolicyLimit::AggregateBytes,
                actual,
                maximum: MAX_CSP_BYTES,
                ..
            }) if actual == MAX_CSP_BYTES + 1
        ));
    }

    #[test]
    fn every_referrer_bound_admits_its_edge_and_rejects_the_next_unit() {
        let mut fields = Headers::new();
        for _ in 0..MAX_REFERRER_POLICY_FIELDS {
            append(&mut fields, "Referrer-Policy", b"");
        }
        assert!(capture_document_response_metadata(&fields).is_ok());
        append(&mut fields, "Referrer-Policy", b"");
        assert!(matches!(
            capture_document_response_metadata(&fields),
            Err(DocumentPolicyError::LimitExceeded {
                limit: DocumentPolicyLimit::FieldCount,
                actual,
                maximum: MAX_REFERRER_POLICY_FIELDS,
                ..
            }) if actual == MAX_REFERRER_POLICY_FIELDS + 1
        ));

        let exact_bytes = vec![b'x'; MAX_REFERRER_POLICY_FIELD_BYTES];
        assert!(
            capture_document_response_metadata(&headers(&[("Referrer-Policy", &exact_bytes),]))
                .is_ok()
        );
        let excess_bytes = vec![b'x'; MAX_REFERRER_POLICY_FIELD_BYTES + 1];
        assert!(matches!(
            capture_document_response_metadata(&headers(&[("Referrer-Policy", &excess_bytes),])),
            Err(DocumentPolicyError::LimitExceeded {
                limit: DocumentPolicyLimit::FieldBytes,
                maximum: MAX_REFERRER_POLICY_FIELD_BYTES,
                ..
            })
        ));

        let exact_tokens = vec!["unknown"; MAX_REFERRER_POLICY_TOKENS].join(",");
        let exact = capture_document_response_metadata(&headers(&[(
            "Referrer-Policy",
            exact_tokens.as_bytes(),
        )]))
        .unwrap();
        assert_eq!(
            exact.referrer_policy.ignored_tokens,
            MAX_REFERRER_POLICY_TOKENS
        );
        let excess_tokens = vec!["unknown"; MAX_REFERRER_POLICY_TOKENS + 1].join(",");
        assert!(matches!(
            capture_document_response_metadata(&headers(&[
                ("Referrer-Policy", excess_tokens.as_bytes()),
            ])),
            Err(DocumentPolicyError::LimitExceeded {
                limit: DocumentPolicyLimit::TokenCount,
                actual,
                maximum: MAX_REFERRER_POLICY_TOKENS,
                ..
            }) if actual == MAX_REFERRER_POLICY_TOKENS + 1
        ));

        let exact_recognized = vec!["origin"; MAX_RECOGNIZED_REFERRER_POLICY_INPUTS].join(",");
        let exact = capture_document_response_metadata(&headers(&[(
            "Referrer-Policy",
            exact_recognized.as_bytes(),
        )]))
        .unwrap();
        assert_eq!(
            exact.referrer_policy.recognized.len(),
            MAX_RECOGNIZED_REFERRER_POLICY_INPUTS
        );
        let excess_recognized = vec!["origin"; MAX_RECOGNIZED_REFERRER_POLICY_INPUTS + 1].join(",");
        assert!(matches!(
            capture_document_response_metadata(&headers(&[
                ("Referrer-Policy", excess_recognized.as_bytes()),
            ])),
            Err(DocumentPolicyError::LimitExceeded {
                limit: DocumentPolicyLimit::RetainedInputCount,
                actual,
                maximum: MAX_RECOGNIZED_REFERRER_POLICY_INPUTS,
                ..
            }) if actual == MAX_RECOGNIZED_REFERRER_POLICY_INPUTS + 1
        ));
    }

    #[test]
    fn every_content_type_bound_admits_its_edge_and_rejects_the_next_unit() {
        let mut fields = Headers::new();
        for _ in 0..MAX_CONTENT_TYPE_FIELDS {
            append(&mut fields, "Content-Type", b"text/html");
        }
        assert!(capture_document_response_metadata(&fields).is_ok());
        append(&mut fields, "Content-Type", b"text/plain");
        assert!(matches!(
            capture_document_response_metadata(&fields),
            Err(DocumentPolicyError::LimitExceeded {
                limit: DocumentPolicyLimit::FieldCount,
                actual,
                maximum: MAX_CONTENT_TYPE_FIELDS,
                ..
            }) if actual == MAX_CONTENT_TYPE_FIELDS + 1
        ));

        let exact_bytes = vec![b'x'; MAX_CONTENT_TYPE_FIELD_BYTES];
        assert!(
            capture_document_response_metadata(&headers(&[("Content-Type", &exact_bytes),]))
                .is_ok()
        );
        let excess_bytes = vec![b'x'; MAX_CONTENT_TYPE_FIELD_BYTES + 1];
        assert!(matches!(
            capture_document_response_metadata(&headers(&[("Content-Type", &excess_bytes),])),
            Err(DocumentPolicyError::LimitExceeded {
                limit: DocumentPolicyLimit::FieldBytes,
                maximum: MAX_CONTENT_TYPE_FIELD_BYTES,
                ..
            })
        ));

        let exact_charsets = format!(
            "text/html{}",
            ";charset=utf-8".repeat(MAX_CONTENT_TYPE_CHARSETS)
        );
        let exact = capture_document_response_metadata(&headers(&[(
            "Content-Type",
            exact_charsets.as_bytes(),
        )]))
        .unwrap();
        let ContentTypeInput::Parsed(parsed) = &exact.content_types[0] else {
            panic!("exact charset edge must parse");
        };
        assert_eq!(parsed.charsets.len(), MAX_CONTENT_TYPE_CHARSETS);
        let excess_charsets = format!(
            "text/html{}",
            ";charset=utf-8".repeat(MAX_CONTENT_TYPE_CHARSETS + 1)
        );
        assert!(matches!(
            capture_document_response_metadata(&headers(&[
                ("Content-Type", excess_charsets.as_bytes()),
            ])),
            Err(DocumentPolicyError::LimitExceeded {
                limit: DocumentPolicyLimit::CharsetCount,
                actual,
                maximum: MAX_CONTENT_TYPE_CHARSETS,
                ..
            }) if actual == MAX_CONTENT_TYPE_CHARSETS + 1
        ));
    }

    #[test]
    fn cookie_and_global_bounds_admit_their_edges_and_reject_the_next_unit() {
        let mut fields = Headers::new();
        for _ in 0..MAX_SET_COOKIE_FIELDS {
            append(&mut fields, "Set-Cookie", b"");
        }
        assert!(capture_document_response_metadata(&fields).is_ok());
        append(&mut fields, "Set-Cookie", b"");
        assert!(matches!(
            capture_document_response_metadata(&fields),
            Err(DocumentPolicyError::LimitExceeded {
                field: DocumentPolicyField::SetCookie,
                limit: DocumentPolicyLimit::FieldCount,
                actual,
                maximum: MAX_SET_COOKIE_FIELDS,
            }) if actual == MAX_SET_COOKIE_FIELDS + 1
        ));

        let exact_cookie_bytes = vec![b'x'; MAX_SET_COOKIE_BYTES];
        let exact =
            capture_document_response_metadata(&headers(&[("Set-Cookie", &exact_cookie_bytes)]))
                .unwrap();
        assert_eq!(exact.set_cookie.value_bytes, MAX_SET_COOKIE_BYTES);
        let excess_cookie_bytes = vec![b'x'; MAX_SET_COOKIE_BYTES + 1];
        assert!(matches!(
            capture_document_response_metadata(&headers(&[
                ("Set-Cookie", &excess_cookie_bytes),
            ])),
            Err(DocumentPolicyError::LimitExceeded {
                field: DocumentPolicyField::SetCookie,
                limit: DocumentPolicyLimit::AggregateBytes,
                actual,
                maximum: MAX_SET_COOKIE_BYTES,
            }) if actual == MAX_SET_COOKIE_BYTES + 1
        ));

        let half_global = vec![b'x'; MAX_DOCUMENT_POLICY_INPUT_BYTES / 2];
        let mut global = Headers::new();
        append(
            &mut global,
            "Content-Security-Policy",
            &half_global[..MAX_CSP_FIELD_BYTES],
        );
        append(
            &mut global,
            "Content-Security-Policy-Report-Only",
            &half_global[..MAX_CSP_FIELD_BYTES],
        );
        append(&mut global, "Set-Cookie", &half_global);
        assert!(capture_document_response_metadata(&global).is_ok());
        append(&mut global, "Referrer-Policy", b"x");
        assert!(matches!(
            capture_document_response_metadata(&global),
            Err(DocumentPolicyError::LimitExceeded {
                field: DocumentPolicyField::Aggregate,
                limit: DocumentPolicyLimit::AggregateBytes,
                actual,
                maximum: MAX_DOCUMENT_POLICY_INPUT_BYTES,
            }) if actual == MAX_DOCUMENT_POLICY_INPUT_BYTES + 1
        ));
    }
}
