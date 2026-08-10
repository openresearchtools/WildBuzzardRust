// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bounded Content Security Policy parsing and matching for author styles.
//!
//! This module is deliberately capability-free. It parses only the response
//! metadata already owned by a live document and evaluates already-resolved
//! HTTP(S) candidate URLs or inline nonces. It cannot fetch, mutate a DOM,
//! publish a frame, or enforce a decision in the product pipeline.

use std::fmt;
use std::net::IpAddr;

use wild_buzzard_dom::DocumentVersion;
use wild_buzzard_net::{GeneralWebTarget, WebHost, WebScheme};

use crate::{CapturedDocumentResponseMetadata, NavigationCommitMetadata};

/// Maximum comma-delimited CSP members inspected across both field kinds.
///
/// This custom denial-of-service bound includes leading and interior empty
/// members even though Firefox does not retain a zero-directive policy.
pub const MAX_STYLE_CSP_POLICY_MEMBERS: usize = 16;
/// Maximum nonempty serialized directives inspected in one CSP policy.
pub const MAX_STYLE_CSP_DIRECTIVES_PER_POLICY: usize = 128;
/// Maximum source expressions inspected across relevant directives.
pub const MAX_STYLE_CSP_SOURCE_EXPRESSIONS: usize = 512;
/// Maximum bytes in one serialized policy member.
pub const MAX_STYLE_CSP_POLICY_BYTES: usize = 16 * 1024;
/// Maximum bytes in one relevant source-expression token.
pub const MAX_STYLE_CSP_SOURCE_TOKEN_BYTES: usize = 1024;
/// Maximum parser work units charged to one serialized policy.
pub const MAX_STYLE_CSP_POLICY_WORK: usize = 17 * 1024;
/// Maximum candidate nonce bytes eligible for matching.
pub const MAX_STYLE_CSP_NONCE_BYTES: usize = 1024;

/// Whether a bounded CSP input came from an enforcing or report-only field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StylePolicyInput {
    /// `Content-Security-Policy`.
    Enforcing,
    /// `Content-Security-Policy-Report-Only`.
    ReportOnly,
    /// A limit shared by both field kinds.
    Aggregate,
}

/// Which bounded CSP parser or matcher resource was exhausted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StylePolicyLimit {
    /// Comma-delimited members inspected across both field kinds.
    PolicyMemberCount,
    /// Bytes in one serialized policy member.
    PolicyBytes,
    /// Nonempty directives inspected in one policy.
    DirectiveCount,
    /// Source expressions inspected across relevant directives.
    SourceExpressionCount,
    /// Bytes in one relevant source-expression token.
    SourceTokenBytes,
    /// Work charged while parsing one policy.
    PolicyWork,
}

/// A bounded allocation owned by the pure CSP style-policy layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StylePolicyAllocation {
    /// Policy storage.
    Policy,
    /// A relevant directive's source list.
    SourceList,
    /// A normalized source host.
    Host,
    /// A redacted nonce value.
    Nonce,
    /// Privacy-safe unsupported-source evidence.
    UnsupportedSource,
    /// A temporary bounded URL used to normalize one CSP host source.
    HostNormalization,
}

/// Value-redacting failure from CSP style-policy parsing or matching.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StylePolicyError {
    /// Final navigation metadata was not a coherent canonical HTTP(S) commit.
    InvalidDocumentCommit,
    /// A candidate was not a credential-free HTTP(S) browser URL.
    InvalidCandidateUrl,
    /// A candidate was valid but was not its exact WHATWG serialization.
    NonCanonicalCandidateUrl,
    /// A fixed count, byte, or work limit was exceeded.
    LimitExceeded {
        /// Field family being parsed.
        input: StylePolicyInput,
        /// Exhausted resource.
        limit: StylePolicyLimit,
        /// Observed count, byte length, or work.
        actual: usize,
        /// Maximum admitted value.
        maximum: usize,
    },
    /// Checked count, byte, or work arithmetic overflowed.
    CounterOverflow {
        /// Field family being parsed or evaluated.
        input: StylePolicyInput,
    },
    /// A bounded owned value could not be allocated.
    AllocationFailed {
        /// Allocation which failed.
        allocation: StylePolicyAllocation,
    },
}

impl fmt::Display for StylePolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDocumentCommit => {
                formatter.write_str("invalid final document commitment for style policy")
            }
            Self::InvalidCandidateUrl => {
                formatter.write_str("invalid HTTP(S) candidate for style policy")
            }
            Self::NonCanonicalCandidateUrl => {
                formatter.write_str("noncanonical HTTP(S) candidate for style policy")
            }
            Self::LimitExceeded {
                input,
                limit,
                actual,
                maximum,
            } => write!(
                formatter,
                "style policy exceeded {input:?} {limit:?} bound ({actual} > {maximum})"
            ),
            Self::CounterOverflow { input } => {
                write!(formatter, "style policy {input:?} counter overflowed")
            }
            Self::AllocationFailed { allocation } => write!(
                formatter,
                "bounded allocation failed while retaining {allocation:?} style policy data"
            ),
        }
    }
}

impl std::error::Error for StylePolicyError {}

/// Style operation evaluated against the relevant CSP directive fallback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StylePolicyResource {
    /// A candidate first `base[href]`, governed only by `base-uri`.
    BaseUri,
    /// An external stylesheet request, including a possible link nonce.
    ExternalStyle,
    /// An inline `style` element, including a possible element nonce.
    InlineStyleElement,
    /// An inline `style` attribute; nonces never apply.
    InlineStyleAttribute,
}

/// Privacy-safe category for a retained but deliberately nonmatching source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnsupportedStyleSourceKind {
    /// A syntactically valid `sha256`, `sha384`, or `sha512` source.
    Hash,
    /// A host source with a path other than the nonrestrictive root path.
    HostPath,
    /// A source using a scheme outside the admitted HTTP(S) subset.
    NonHttpScheme,
    /// A recognized or quoted keyword outside this style-policy subset.
    Keyword,
    /// A malformed or otherwise unsupported source expression.
    Malformed,
}

/// Redacted evidence for one unsupported relevant source expression.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnsupportedStyleSource {
    kind: UnsupportedStyleSourceKind,
    byte_len: usize,
}

impl UnsupportedStyleSource {
    /// Non-sensitive reason why the expression cannot match.
    #[must_use]
    pub const fn kind(self) -> UnsupportedStyleSourceKind {
        self.kind
    }

    /// Exact serialized token length without retaining or exposing its bytes.
    #[must_use]
    pub const fn byte_len(self) -> usize {
        self.byte_len
    }
}

/// Intersection result for one style-policy operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StylePolicyDecision {
    resource: StylePolicyResource,
    enforcing_blocked_policies: usize,
    report_only_would_block_policies: usize,
    candidate_nonce_ignored_over_limit: bool,
}

impl StylePolicyDecision {
    /// Operation which was evaluated.
    #[must_use]
    pub const fn resource(self) -> StylePolicyResource {
        self.resource
    }

    /// Whether every enforcing policy admitted the operation.
    #[must_use]
    pub const fn is_allowed(self) -> bool {
        self.enforcing_blocked_policies == 0
    }

    /// Number of enforcing policies which block the operation.
    #[must_use]
    pub const fn enforcing_blocked_policy_count(self) -> usize {
        self.enforcing_blocked_policies
    }

    /// Number of report-only policies which would block the operation.
    #[must_use]
    pub const fn report_only_would_block_policy_count(self) -> usize {
        self.report_only_would_block_policies
    }

    /// Whether at least one report-only policy would diagnose a violation.
    #[must_use]
    pub const fn report_only_would_block(self) -> bool {
        self.report_only_would_block_policies != 0
    }

    /// Whether an over-limit candidate nonce was treated as absent.
    ///
    /// This bit exposes no nonce bytes and never changes URL matching or the
    /// distinction between enforcing and report-only policies.
    #[must_use]
    pub const fn candidate_nonce_ignored_over_limit(self) -> bool {
        self.candidate_nonce_ignored_over_limit
    }
}

/// Immutable CSP subset bound to one final response and initial document.
///
/// The object owns no network, DOM, renderer, logging, or reporting capability.
/// Constructing it does not alter page behavior; a later style-resource owner
/// must explicitly consume its decisions.
pub struct StylePolicySet {
    response_document_version: DocumentVersion,
    navigation_commit: NavigationCommitMetadata,
    document_target: GeneralWebTarget,
    enforcing: Vec<StylePolicy>,
    report_only: Vec<StylePolicy>,
    inspected_policy_members: usize,
    relevant_source_expressions: usize,
    ignored_duplicate_directives: usize,
    unsupported_sources: Vec<UnsupportedStyleSource>,
    report_only_parse_failure: Option<StylePolicyError>,
}

impl StylePolicySet {
    /// Parses the exact separate CSP fields captured from a final response.
    ///
    /// # Errors
    ///
    /// Returns a redacted typed error for an incoherent document commitment or
    /// an enforcing-policy count/byte/work, arithmetic, or allocation failure.
    /// Report-only failures do not return `Err`; they are rolled back and
    /// exposed by [`Self::report_only_parse_failure`]. Input is never
    /// truncated.
    pub fn from_response_metadata(
        metadata: &CapturedDocumentResponseMetadata,
    ) -> Result<Self, StylePolicyError> {
        Self::from_field_bytes(
            metadata.response_document_version(),
            metadata.navigation_commit().clone(),
            metadata
                .enforcing_csp_fields()
                .iter()
                .map(crate::CspFieldValue::as_bytes),
            metadata
                .report_only_csp_fields()
                .iter()
                .map(crate::CspFieldValue::as_bytes),
        )
    }

    fn from_field_bytes<'input, Enforcing, ReportOnly>(
        response_document_version: DocumentVersion,
        navigation_commit: NavigationCommitMetadata,
        enforcing_fields: Enforcing,
        report_only_fields: ReportOnly,
    ) -> Result<Self, StylePolicyError>
    where
        Enforcing: IntoIterator<Item = &'input [u8]>,
        ReportOnly: IntoIterator<Item = &'input [u8]>,
    {
        navigation_commit
            .validate_general_web()
            .map_err(|_| StylePolicyError::InvalidDocumentCommit)?;
        let (identity, document_target) =
            GeneralWebTarget::parse_navigation(navigation_commit.final_url())
                .map_err(|_| StylePolicyError::InvalidDocumentCommit)?;
        if identity.as_str() != navigation_commit.final_url() {
            return Err(StylePolicyError::InvalidDocumentCommit);
        }

        let mut enforcing_parser = StylePolicyParser::new(&document_target);
        enforcing_parser.parse_fields(enforcing_fields, StylePolicyInput::Enforcing)?;
        let parsed = enforcing_parser.finish();

        let mut report_only_parser = StylePolicyParser::for_report_only(
            &document_target,
            parsed.inspected_policy_members,
            parsed.relevant_source_expressions,
        );
        let report_only_result = report_only_parser
            .parse_fields(report_only_fields, StylePolicyInput::ReportOnly)
            .map(|()| report_only_parser.finish());

        Ok(Self::from_parsed_policy_sets(
            response_document_version,
            navigation_commit,
            document_target,
            parsed,
            report_only_result,
        ))
    }

    fn from_parsed_policy_sets(
        response_document_version: DocumentVersion,
        navigation_commit: NavigationCommitMetadata,
        document_target: GeneralWebTarget,
        mut parsed: ParsedPolicySets,
        report_only_result: Result<ParsedPolicySets, StylePolicyError>,
    ) -> Self {
        let report_only_parse_failure = match report_only_result {
            Ok(report_only) => parsed.merge_report_only(report_only).err(),
            Err(failure) => Some(failure),
        };

        Self {
            response_document_version,
            navigation_commit,
            document_target,
            enforcing: parsed.enforcing,
            report_only: parsed.report_only,
            inspected_policy_members: parsed.inspected_policy_members,
            relevant_source_expressions: parsed.relevant_source_expressions,
            ignored_duplicate_directives: parsed.ignored_duplicate_directives,
            unsupported_sources: parsed.unsupported_sources,
            report_only_parse_failure,
        }
    }

    /// Initial immutable document revision bound to the response fields.
    #[must_use]
    pub const fn response_document_version(&self) -> DocumentVersion {
        self.response_document_version
    }

    /// Exact final navigation commitment used as `'self'` and fallback base.
    #[must_use]
    pub const fn navigation_commit(&self) -> &NavigationCommitMetadata {
        &self.navigation_commit
    }

    /// Number of enforcing matcher records retained after zero-directive
    /// members are discarded.
    ///
    /// This style-subset count is not a claim of full Firefox
    /// `GetPolicyCount` parity.
    #[must_use]
    pub fn enforcing_policy_count(&self) -> usize {
        self.enforcing.len()
    }

    /// Number of report-only matcher records retained after zero-directive
    /// members are discarded.
    ///
    /// This style-subset count is not a claim of full Firefox
    /// `GetPolicyCount` parity.
    #[must_use]
    pub fn report_only_policy_count(&self) -> usize {
        self.report_only.len()
    }

    /// Number of comma-delimited members charged to the custom aggregate
    /// denial-of-service bound, including nontrailing empty members.
    #[must_use]
    pub const fn inspected_policy_member_count(&self) -> usize {
        self.inspected_policy_members
    }

    /// Number of tokens inspected in relevant source lists.
    #[must_use]
    pub const fn relevant_source_expression_count(&self) -> usize {
        self.relevant_source_expressions
    }

    /// Number of later duplicate relevant directives ignored by the parser.
    #[must_use]
    pub const fn ignored_duplicate_directive_count(&self) -> usize {
        self.ignored_duplicate_directives
    }

    /// Privacy-safe unsupported expression records in parse order.
    #[must_use]
    pub fn unsupported_sources(&self) -> &[UnsupportedStyleSource] {
        &self.unsupported_sources
    }

    /// Redacted reason report-only diagnostics were discarded transactionally.
    ///
    /// Enforcing policies remain available and authoritative. When this is
    /// `Some`, report-only policy/evidence/count accessors expose no partial
    /// report transaction.
    #[must_use]
    pub const fn report_only_parse_failure(&self) -> Option<StylePolicyError> {
        self.report_only_parse_failure
    }

    /// Evaluates a canonical HTTP(S) base URL. `default-src` never applies.
    ///
    /// # Errors
    ///
    /// Returns a redacted URL or arithmetic failure without retaining the
    /// candidate.
    pub fn evaluate_base_uri(
        &self,
        candidate_url: &str,
    ) -> Result<StylePolicyDecision, StylePolicyError> {
        let candidate = parse_candidate(candidate_url)?;
        self.evaluate(StylePolicyResource::BaseUri, Some(&candidate), None, false)
    }

    /// Evaluates an external stylesheet URL and optional link nonce.
    ///
    /// A valid matching nonce bypasses URL matching for that policy, as
    /// required by the CSP `style-src-elem` pre-request algorithm.
    ///
    /// # Errors
    ///
    /// Returns a redacted URL or arithmetic failure. An over-limit nonce is
    /// treated as absent and recorded in the returned decision.
    pub fn evaluate_external_style(
        &self,
        candidate_url: &str,
        nonce: Option<&str>,
    ) -> Result<StylePolicyDecision, StylePolicyError> {
        let (nonce, candidate_nonce_ignored_over_limit) = bounded_candidate_nonce(nonce);
        let candidate = parse_candidate(candidate_url)?;
        self.evaluate(
            StylePolicyResource::ExternalStyle,
            Some(&candidate),
            nonce,
            candidate_nonce_ignored_over_limit,
        )
    }

    /// Evaluates an inline `style` element and optional element nonce.
    ///
    /// Hash matching is not implemented by this gate. A valid retained hash
    /// source still disables `'unsafe-inline'` and otherwise remains
    /// conservatively nonmatching.
    ///
    /// # Errors
    ///
    /// Returns a checked arithmetic failure. An over-limit nonce is treated as
    /// absent and recorded in the returned decision.
    pub fn evaluate_inline_style_element(
        &self,
        nonce: Option<&str>,
    ) -> Result<StylePolicyDecision, StylePolicyError> {
        let (nonce, candidate_nonce_ignored_over_limit) = bounded_candidate_nonce(nonce);
        self.evaluate(
            StylePolicyResource::InlineStyleElement,
            None,
            nonce,
            candidate_nonce_ignored_over_limit,
        )
    }

    /// Evaluates an inline style attribute. Element nonces never apply.
    ///
    /// # Errors
    ///
    /// Returns a checked arithmetic failure.
    pub fn evaluate_inline_style_attribute(&self) -> Result<StylePolicyDecision, StylePolicyError> {
        self.evaluate(StylePolicyResource::InlineStyleAttribute, None, None, false)
    }

    fn evaluate(
        &self,
        resource: StylePolicyResource,
        candidate: Option<&GeneralWebTarget>,
        nonce: Option<&str>,
        candidate_nonce_ignored_over_limit: bool,
    ) -> Result<StylePolicyDecision, StylePolicyError> {
        let mut enforcing_blocked = 0usize;
        for policy in &self.enforcing {
            if !policy.allows(resource, &self.document_target, candidate, nonce) {
                enforcing_blocked =
                    checked_increment(enforcing_blocked, StylePolicyInput::Enforcing)?;
            }
        }

        let mut report_only_would_block = 0usize;
        for policy in &self.report_only {
            if !policy.allows(resource, &self.document_target, candidate, nonce) {
                report_only_would_block =
                    checked_increment(report_only_would_block, StylePolicyInput::ReportOnly)?;
            }
        }

        Ok(StylePolicyDecision {
            resource,
            enforcing_blocked_policies: enforcing_blocked,
            report_only_would_block_policies: report_only_would_block,
            candidate_nonce_ignored_over_limit,
        })
    }
}

impl fmt::Debug for StylePolicySet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StylePolicySet")
            .field("response_document_version", &self.response_document_version)
            .field("enforcing_policies", &self.enforcing.len())
            .field("report_only_policies", &self.report_only.len())
            .field("inspected_policy_members", &self.inspected_policy_members)
            .field(
                "relevant_source_expressions",
                &self.relevant_source_expressions,
            )
            .field(
                "ignored_duplicate_directives",
                &self.ignored_duplicate_directives,
            )
            .field("unsupported_sources", &self.unsupported_sources)
            .field("report_only_parse_failure", &self.report_only_parse_failure)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum RelevantDirective {
    BaseUri,
    StyleSrcElem,
    StyleSrc,
    StyleSrcAttr,
    DefaultSrc,
}

#[derive(Default)]
struct StylePolicy {
    base_uri: Option<SourceList>,
    style_src_elem: Option<SourceList>,
    style_src: Option<SourceList>,
    style_src_attr: Option<SourceList>,
    default_src: Option<SourceList>,
}

impl StylePolicy {
    fn source_list(&self, resource: StylePolicyResource) -> Option<&SourceList> {
        match resource {
            StylePolicyResource::BaseUri => self.base_uri.as_ref(),
            StylePolicyResource::ExternalStyle | StylePolicyResource::InlineStyleElement => self
                .style_src_elem
                .as_ref()
                .or(self.style_src.as_ref())
                .or(self.default_src.as_ref()),
            StylePolicyResource::InlineStyleAttribute => self
                .style_src_attr
                .as_ref()
                .or(self.style_src.as_ref())
                .or(self.default_src.as_ref()),
        }
    }

    fn has(&self, directive: RelevantDirective) -> bool {
        match directive {
            RelevantDirective::BaseUri => self.base_uri.is_some(),
            RelevantDirective::StyleSrcElem => self.style_src_elem.is_some(),
            RelevantDirective::StyleSrc => self.style_src.is_some(),
            RelevantDirective::StyleSrcAttr => self.style_src_attr.is_some(),
            RelevantDirective::DefaultSrc => self.default_src.is_some(),
        }
    }

    fn set(&mut self, directive: RelevantDirective, sources: SourceList) {
        let slot = match directive {
            RelevantDirective::BaseUri => &mut self.base_uri,
            RelevantDirective::StyleSrcElem => &mut self.style_src_elem,
            RelevantDirective::StyleSrc => &mut self.style_src,
            RelevantDirective::StyleSrcAttr => &mut self.style_src_attr,
            RelevantDirective::DefaultSrc => &mut self.default_src,
        };
        debug_assert!(slot.is_none());
        *slot = Some(sources);
    }

    fn allows(
        &self,
        resource: StylePolicyResource,
        document: &GeneralWebTarget,
        candidate: Option<&GeneralWebTarget>,
        nonce: Option<&str>,
    ) -> bool {
        let Some(sources) = self.source_list(resource) else {
            return true;
        };
        match resource {
            StylePolicyResource::BaseUri => {
                candidate.is_some_and(|candidate| sources.permits_url(document, candidate, None))
            }
            StylePolicyResource::ExternalStyle => {
                candidate.is_some_and(|candidate| sources.permits_url(document, candidate, nonce))
            }
            StylePolicyResource::InlineStyleElement => sources.permits_inline_element(nonce),
            StylePolicyResource::InlineStyleAttribute => sources.permits_inline_attribute(),
        }
    }
}

struct SourceList {
    sources: Vec<SourceExpression>,
}

impl SourceList {
    fn permits_url(
        &self,
        document: &GeneralWebTarget,
        candidate: &GeneralWebTarget,
        nonce: Option<&str>,
    ) -> bool {
        if nonce.is_some_and(|nonce| {
            !nonce.is_empty()
                && self
                    .sources
                    .iter()
                    .any(|source| source.matches_nonce(nonce))
        }) {
            return true;
        }
        self.sources
            .iter()
            .any(|source| source.matches_url(document, candidate))
    }

    fn permits_inline_element(&self, nonce: Option<&str>) -> bool {
        if self.allows_all_inline() {
            return true;
        }
        nonce.is_some_and(|nonce| {
            !nonce.is_empty()
                && self
                    .sources
                    .iter()
                    .any(|source| source.matches_nonce(nonce))
        })
    }

    fn permits_inline_attribute(&self) -> bool {
        self.allows_all_inline()
    }

    fn allows_all_inline(&self) -> bool {
        let mut unsafe_inline = false;
        for source in &self.sources {
            if source.invalidates_unsafe_inline() {
                return false;
            }
            if matches!(source, SourceExpression::UnsafeInline) {
                unsafe_inline = true;
            }
        }
        unsafe_inline
    }
}

enum SourceExpression {
    DenyAll,
    SelfOrigin,
    AnyNetwork,
    Scheme(WebScheme),
    Host(HostSource),
    Nonce(SecretNonce),
    UnsafeInline,
    Unsupported {
        evidence: UnsupportedStyleSource,
        invalidates_unsafe_inline: bool,
    },
}

impl SourceExpression {
    fn matches_nonce(&self, candidate: &str) -> bool {
        match self {
            Self::Nonce(nonce) => nonce.matches(candidate),
            _ => false,
        }
    }

    fn invalidates_unsafe_inline(&self) -> bool {
        matches!(self, Self::Nonce(_))
            || matches!(
                self,
                Self::Unsupported {
                    invalidates_unsafe_inline: true,
                    ..
                }
            )
    }

    fn matches_url(&self, document: &GeneralWebTarget, candidate: &GeneralWebTarget) -> bool {
        match self {
            Self::SelfOrigin => self_source_matches(document, candidate),
            Self::AnyNetwork => true,
            Self::Scheme(scheme) => scheme_matches(*scheme, candidate.origin().scheme()),
            Self::Host(host) => host.matches(candidate),
            Self::DenyAll | Self::Nonce(_) | Self::UnsafeInline | Self::Unsupported { .. } => false,
        }
    }
}

struct SecretNonce {
    value: String,
}

impl SecretNonce {
    fn matches(&self, candidate: &str) -> bool {
        let expected = self.value.as_bytes();
        let candidate = candidate.as_bytes();
        if expected.len() != candidate.len() {
            return false;
        }
        expected
            .iter()
            .zip(candidate)
            .fold(0_u8, |difference, (left, right)| {
                difference | (left ^ right)
            })
            == 0
    }
}

struct HostSource {
    scheme: WebScheme,
    host: HostPattern,
    port: PortPattern,
}

impl HostSource {
    fn matches(&self, candidate: &GeneralWebTarget) -> bool {
        scheme_matches(self.scheme, candidate.origin().scheme())
            && self.host.matches(candidate.origin().host())
            && self.port.matches(self.scheme, candidate)
    }
}

enum HostPattern {
    Any,
    Exact(HostIdentity),
    Subdomains(String),
}

impl HostPattern {
    fn matches(&self, candidate: &WebHost) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(expected) => expected.matches(candidate),
            Self::Subdomains(suffix) => match candidate {
                WebHost::Domain(domain) => {
                    domain.len() > suffix.len()
                        && domain.ends_with(suffix.as_str())
                        && domain.as_bytes()[domain.len() - suffix.len() - 1] == b'.'
                }
                WebHost::Ip(_) => false,
            },
        }
    }
}

enum HostIdentity {
    Domain(String),
    Ip(IpAddr),
}

impl HostIdentity {
    fn matches(&self, candidate: &WebHost) -> bool {
        match (self, candidate) {
            (Self::Domain(expected), WebHost::Domain(actual)) => expected == actual,
            (Self::Ip(expected), WebHost::Ip(actual)) => expected == actual,
            (Self::Domain(_), WebHost::Ip(_)) | (Self::Ip(_), WebHost::Domain(_)) => false,
        }
    }
}

enum PortPattern {
    Any,
    Exact(u16),
    Default,
}

impl PortPattern {
    fn matches(&self, source_scheme: WebScheme, candidate: &GeneralWebTarget) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(port) => ports_match(*port, candidate.origin().port()),
            Self::Default => ports_match(default_port(source_scheme), candidate.origin().port()),
        }
    }
}

struct ParsedPolicySets {
    enforcing: Vec<StylePolicy>,
    report_only: Vec<StylePolicy>,
    inspected_policy_members: usize,
    relevant_source_expressions: usize,
    ignored_duplicate_directives: usize,
    unsupported_sources: Vec<UnsupportedStyleSource>,
}

impl ParsedPolicySets {
    fn merge_report_only(&mut self, report_only: Self) -> Result<(), StylePolicyError> {
        debug_assert!(self.report_only.is_empty());
        debug_assert!(report_only.enforcing.is_empty());

        let ignored_duplicate_directives = checked_add(
            self.ignored_duplicate_directives,
            report_only.ignored_duplicate_directives,
            StylePolicyInput::Aggregate,
        )?;
        self.unsupported_sources
            .try_reserve(report_only.unsupported_sources.len())
            .map_err(|_| StylePolicyError::AllocationFailed {
                allocation: StylePolicyAllocation::UnsupportedSource,
            })?;

        // No fallible operation follows the reserve. Commit the whole
        // report-only transaction at once so callers can never observe a
        // truncated diagnostic policy/evidence/counter set.
        self.unsupported_sources
            .extend(report_only.unsupported_sources);
        self.report_only = report_only.report_only;
        self.inspected_policy_members = report_only.inspected_policy_members;
        self.relevant_source_expressions = report_only.relevant_source_expressions;
        self.ignored_duplicate_directives = ignored_duplicate_directives;
        Ok(())
    }
}

struct StylePolicyParser<'document> {
    document: &'document GeneralWebTarget,
    enforcing: Vec<StylePolicy>,
    report_only: Vec<StylePolicy>,
    inspected_policy_members: usize,
    relevant_source_expressions: usize,
    ignored_duplicate_directives: usize,
    unsupported_sources: Vec<UnsupportedStyleSource>,
}

impl<'document> StylePolicyParser<'document> {
    fn new(document: &'document GeneralWebTarget) -> Self {
        Self {
            document,
            enforcing: Vec::new(),
            report_only: Vec::new(),
            inspected_policy_members: 0,
            relevant_source_expressions: 0,
            ignored_duplicate_directives: 0,
            unsupported_sources: Vec::new(),
        }
    }

    fn for_report_only(
        document: &'document GeneralWebTarget,
        enforcing_policy_members: usize,
        enforcing_source_expressions: usize,
    ) -> Self {
        let mut parser = Self::new(document);
        parser.inspected_policy_members = enforcing_policy_members;
        parser.relevant_source_expressions = enforcing_source_expressions;
        parser
    }

    fn parse_fields<'input, Fields>(
        &mut self,
        fields: Fields,
        input: StylePolicyInput,
    ) -> Result<(), StylePolicyError>
    where
        Fields: IntoIterator<Item = &'input [u8]>,
    {
        for field in fields {
            let mut member_start = 0usize;
            for (index, byte) in field.iter().enumerate() {
                if *byte == b',' {
                    self.parse_policy_member(
                        trim_header_list_whitespace(&field[member_start..index]),
                        input,
                    )?;
                    member_start = checked_increment(index, input)?;
                }
            }
            let tail = trim_header_list_whitespace(&field[member_start..]);
            if !tail.is_empty() {
                self.parse_policy_member(tail, input)?;
            }
        }
        Ok(())
    }

    fn parse_policy_member(
        &mut self,
        member: &[u8],
        input: StylePolicyInput,
    ) -> Result<(), StylePolicyError> {
        self.inspected_policy_members =
            checked_increment(self.inspected_policy_members, StylePolicyInput::Aggregate)?;
        enforce_limit(
            StylePolicyInput::Aggregate,
            StylePolicyLimit::PolicyMemberCount,
            self.inspected_policy_members,
            MAX_STYLE_CSP_POLICY_MEMBERS,
        )?;
        enforce_limit(
            input,
            StylePolicyLimit::PolicyBytes,
            member.len(),
            MAX_STYLE_CSP_POLICY_BYTES,
        )?;

        let Some(policy) = self.parse_policy(member, input)? else {
            return Ok(());
        };
        let output = match input {
            StylePolicyInput::Enforcing => &mut self.enforcing,
            StylePolicyInput::ReportOnly => &mut self.report_only,
            StylePolicyInput::Aggregate => unreachable!(),
        };
        try_push(output, policy, StylePolicyAllocation::Policy)
    }

    fn parse_policy(
        &mut self,
        member: &[u8],
        input: StylePolicyInput,
    ) -> Result<Option<StylePolicy>, StylePolicyError> {
        let mut work = PolicyWork::new(input);
        work.charge(member.len())?;
        let mut policy = StylePolicy::default();
        let mut directives = 0usize;
        let mut retains_policy = false;

        for serialized in member.split(|byte| *byte == b';') {
            let serialized = trim_csp_whitespace(serialized);
            if serialized.is_empty() {
                continue;
            }
            directives = checked_increment(directives, input)?;
            enforce_limit(
                input,
                StylePolicyLimit::DirectiveCount,
                directives,
                MAX_STYLE_CSP_DIRECTIVES_PER_POLICY,
            )?;
            work.charge(1)?;

            if !serialized.iter().copied().all(is_directive_byte) {
                continue;
            }
            let name_end = serialized
                .iter()
                .position(|byte| is_csp_whitespace(*byte))
                .unwrap_or(serialized.len());
            let name = &serialized[..name_end];
            let Some(directive) = relevant_directive(name) else {
                // A policy containing a recognized non-style directive still
                // exists in Firefox and must therefore remain one neutral
                // matcher record. Specialized non-style directive values are
                // deliberately outside this bounded style parser, so this is
                // not exposed as full GetPolicyCount parity.
                retains_policy |= is_recognized_neutral_directive(name);
                continue;
            };
            retains_policy = true;
            if policy.has(directive) {
                self.ignored_duplicate_directives =
                    checked_increment(self.ignored_duplicate_directives, input)?;
                continue;
            }

            let value = trim_csp_whitespace(&serialized[name_end..]);
            let sources = self.parse_source_list(value, input, &mut work)?;
            policy.set(directive, sources);
        }
        Ok(retains_policy.then_some(policy))
    }

    fn parse_source_list(
        &mut self,
        value: &[u8],
        input: StylePolicyInput,
        work: &mut PolicyWork,
    ) -> Result<SourceList, StylePolicyError> {
        let mut sources = Vec::new();
        for token in value.split(|byte| is_csp_whitespace(*byte)) {
            if token.is_empty() {
                continue;
            }
            self.relevant_source_expressions = checked_increment(
                self.relevant_source_expressions,
                StylePolicyInput::Aggregate,
            )?;
            enforce_limit(
                StylePolicyInput::Aggregate,
                StylePolicyLimit::SourceExpressionCount,
                self.relevant_source_expressions,
                MAX_STYLE_CSP_SOURCE_EXPRESSIONS,
            )?;
            enforce_limit(
                input,
                StylePolicyLimit::SourceTokenBytes,
                token.len(),
                MAX_STYLE_CSP_SOURCE_TOKEN_BYTES,
            )?;
            work.charge(checked_increment(token.len(), input)?)?;

            if token.eq_ignore_ascii_case(b"'none'") {
                continue;
            }
            let source = parse_source_expression(token, self.document.origin().scheme())?;
            if let SourceExpression::Unsupported { evidence, .. } = source {
                try_push(
                    &mut self.unsupported_sources,
                    evidence,
                    StylePolicyAllocation::UnsupportedSource,
                )?;
            }
            try_push(&mut sources, source, StylePolicyAllocation::SourceList)?;
        }

        if sources.is_empty() {
            try_push(
                &mut sources,
                SourceExpression::DenyAll,
                StylePolicyAllocation::SourceList,
            )?;
        }
        Ok(SourceList { sources })
    }

    fn finish(self) -> ParsedPolicySets {
        ParsedPolicySets {
            enforcing: self.enforcing,
            report_only: self.report_only,
            inspected_policy_members: self.inspected_policy_members,
            relevant_source_expressions: self.relevant_source_expressions,
            ignored_duplicate_directives: self.ignored_duplicate_directives,
            unsupported_sources: self.unsupported_sources,
        }
    }
}

struct PolicyWork {
    input: StylePolicyInput,
    used: usize,
}

impl PolicyWork {
    const fn new(input: StylePolicyInput) -> Self {
        Self { input, used: 0 }
    }

    fn charge(&mut self, amount: usize) -> Result<(), StylePolicyError> {
        self.used = checked_add(self.used, amount, self.input)?;
        enforce_limit(
            self.input,
            StylePolicyLimit::PolicyWork,
            self.used,
            MAX_STYLE_CSP_POLICY_WORK,
        )
    }
}

fn parse_source_expression(
    token: &[u8],
    document_scheme: WebScheme,
) -> Result<SourceExpression, StylePolicyError> {
    if token.eq_ignore_ascii_case(b"'self'") {
        return Ok(SourceExpression::SelfOrigin);
    }
    if token.eq_ignore_ascii_case(b"'unsafe-inline'") {
        return Ok(SourceExpression::UnsafeInline);
    }
    if let Some(nonce) = nonce_value(token) {
        return Ok(SourceExpression::Nonce(SecretNonce {
            value: try_copy_ascii(nonce, StylePolicyAllocation::Nonce)?,
        }));
    }
    if is_valid_hash_source(token) {
        return Ok(unsupported_source(
            token,
            UnsupportedStyleSourceKind::Hash,
            true,
        ));
    }
    if token.len() >= 2 && token.first() == Some(&b'\'') && token.last() == Some(&b'\'') {
        let body = &token[1..token.len() - 1];
        let kind = if starts_ignore_ascii_case(body, b"nonce-")
            || starts_ignore_ascii_case(body, b"sha256-")
            || starts_ignore_ascii_case(body, b"sha384-")
            || starts_ignore_ascii_case(body, b"sha512-")
        {
            UnsupportedStyleSourceKind::Malformed
        } else {
            UnsupportedStyleSourceKind::Keyword
        };
        return Ok(unsupported_source(token, kind, false));
    }
    if token == b"*" {
        return Ok(SourceExpression::AnyNetwork);
    }
    if token.eq_ignore_ascii_case(b"http:") {
        return Ok(SourceExpression::Scheme(WebScheme::Http));
    }
    if token.eq_ignore_ascii_case(b"https:") {
        return Ok(SourceExpression::Scheme(WebScheme::Https));
    }

    parse_host_source(token, document_scheme)
}

fn parse_host_source(
    token: &[u8],
    document_scheme: WebScheme,
) -> Result<SourceExpression, StylePolicyError> {
    let (scheme, mut authority) = if starts_ignore_ascii_case(token, b"http://") {
        (WebScheme::Http, &token[b"http://".len()..])
    } else if starts_ignore_ascii_case(token, b"https://") {
        (WebScheme::Https, &token[b"https://".len()..])
    } else if token.windows(3).any(|window| window == b"://") || token.ends_with(b":") {
        return Ok(unsupported_source(
            token,
            UnsupportedStyleSourceKind::NonHttpScheme,
            false,
        ));
    } else {
        (document_scheme, token)
    };

    if let Some(path_start) = authority.iter().position(|byte| *byte == b'/') {
        if &authority[path_start..] != b"/" {
            return Ok(unsupported_source(
                token,
                UnsupportedStyleSourceKind::HostPath,
                false,
            ));
        }
        authority = &authority[..path_start];
    }
    if authority.is_empty() || authority.iter().any(|byte| matches!(byte, b'?' | b'#')) {
        return Ok(unsupported_source(
            token,
            UnsupportedStyleSourceKind::Malformed,
            false,
        ));
    }

    let Some((host_bytes, port)) = split_host_and_port(authority) else {
        return Ok(unsupported_source(
            token,
            UnsupportedStyleSourceKind::Malformed,
            false,
        ));
    };
    let Some(port) = parse_port_pattern(port) else {
        return Ok(unsupported_source(
            token,
            UnsupportedStyleSourceKind::Malformed,
            false,
        ));
    };

    let host = if host_bytes == b"*" {
        HostPattern::Any
    } else if let Some(suffix) = host_bytes.strip_prefix(b"*.") {
        let Some(HostIdentity::Domain(domain)) = normalize_host(suffix, scheme)? else {
            return Ok(unsupported_source(
                token,
                UnsupportedStyleSourceKind::Malformed,
                false,
            ));
        };
        HostPattern::Subdomains(domain)
    } else {
        let Some(host) = normalize_host(host_bytes, scheme)? else {
            return Ok(unsupported_source(
                token,
                UnsupportedStyleSourceKind::Malformed,
                false,
            ));
        };
        HostPattern::Exact(host)
    };

    Ok(SourceExpression::Host(HostSource { scheme, host, port }))
}

fn normalize_host(
    host: &[u8],
    scheme: WebScheme,
) -> Result<Option<HostIdentity>, StylePolicyError> {
    if host.is_empty() || host.contains(&b'%') || !valid_source_host_spelling(host) {
        return Ok(None);
    }
    let Ok(host) = std::str::from_utf8(host) else {
        return Ok(None);
    };
    let scheme_text = scheme.as_str();
    let capacity = checked_add(
        checked_add(scheme_text.len(), 3, StylePolicyInput::Aggregate)?,
        checked_increment(host.len(), StylePolicyInput::Aggregate)?,
        StylePolicyInput::Aggregate,
    )?;
    let mut synthetic = String::new();
    synthetic
        .try_reserve_exact(capacity)
        .map_err(|_| StylePolicyError::AllocationFailed {
            allocation: StylePolicyAllocation::HostNormalization,
        })?;
    synthetic.push_str(scheme_text);
    synthetic.push_str("://");
    synthetic.push_str(host);
    synthetic.push('/');

    let Ok(target) = GeneralWebTarget::parse(&synthetic) else {
        return Ok(None);
    };
    match target.origin().host() {
        WebHost::Domain(domain) => Ok(Some(HostIdentity::Domain(try_copy_str(
            domain,
            StylePolicyAllocation::Host,
        )?))),
        WebHost::Ip(address) => {
            let source_address = host.parse::<IpAddr>().ok();
            if source_address != Some(*address) {
                return Ok(None);
            }
            Ok(Some(HostIdentity::Ip(*address)))
        }
    }
}

fn valid_source_host_spelling(host: &[u8]) -> bool {
    !host.starts_with(b".")
        && !host.ends_with(b".")
        && !host.windows(2).any(|pair| pair == b"..")
        && host
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
}

fn split_host_and_port(authority: &[u8]) -> Option<(&[u8], Option<&[u8]>)> {
    let mut separators = authority
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b':');
    let first = separators.next();
    if separators.next().is_some() {
        return None;
    }
    match first {
        Some((index, _)) => Some((&authority[..index], Some(&authority[index + 1..]))),
        None => Some((authority, None)),
    }
}

fn parse_port_pattern(port: Option<&[u8]>) -> Option<PortPattern> {
    let Some(port) = port else {
        return Some(PortPattern::Default);
    };
    if port == b"*" {
        return Some(PortPattern::Any);
    }
    if port.is_empty() || !port.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let port = std::str::from_utf8(port).ok()?.parse::<u16>().ok()?;
    Some(PortPattern::Exact(port))
}

fn nonce_value(token: &[u8]) -> Option<&[u8]> {
    if token.len() <= b"'nonce-'".len()
        || token.first() != Some(&b'\'')
        || token.last() != Some(&b'\'')
        || !starts_ignore_ascii_case(&token[1..], b"nonce-")
    {
        return None;
    }
    let value = &token[b"'nonce-".len()..token.len() - 1];
    is_valid_base64_value(value).then_some(value)
}

fn is_valid_hash_source(token: &[u8]) -> bool {
    if token.len() <= b"'sha256-'".len()
        || token.first() != Some(&b'\'')
        || token.last() != Some(&b'\'')
    {
        return false;
    }
    let body = &token[1..token.len() - 1];
    for algorithm in [b"sha256-".as_slice(), b"sha384-", b"sha512-"] {
        if starts_ignore_ascii_case(body, algorithm) {
            return is_valid_base64_value(&body[algorithm.len()..]);
        }
    }
    false
}

fn is_valid_base64_value(mut value: &[u8]) -> bool {
    if value.ends_with(b"=") {
        value = &value[..value.len() - 1];
    }
    if value.ends_with(b"=") {
        value = &value[..value.len() - 1];
    }
    !value.is_empty()
        && value
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'-' | b'_'))
}

fn unsupported_source(
    token: &[u8],
    kind: UnsupportedStyleSourceKind,
    invalidates_unsafe_inline: bool,
) -> SourceExpression {
    SourceExpression::Unsupported {
        evidence: UnsupportedStyleSource {
            kind,
            byte_len: token.len(),
        },
        invalidates_unsafe_inline,
    }
}

fn relevant_directive(name: &[u8]) -> Option<RelevantDirective> {
    if name.eq_ignore_ascii_case(b"base-uri") {
        Some(RelevantDirective::BaseUri)
    } else if name.eq_ignore_ascii_case(b"style-src-elem") {
        Some(RelevantDirective::StyleSrcElem)
    } else if name.eq_ignore_ascii_case(b"style-src") {
        Some(RelevantDirective::StyleSrc)
    } else if name.eq_ignore_ascii_case(b"style-src-attr") {
        Some(RelevantDirective::StyleSrcAttr)
    } else if name.eq_ignore_ascii_case(b"default-src") {
        Some(RelevantDirective::DefaultSrc)
    } else {
        None
    }
}

fn is_recognized_neutral_directive(name: &[u8]) -> bool {
    // Pinned ESR153 `CSPStrDirectives`, excluding the five directives this
    // style subset evaluates and `reflected-xss`, which Firefox recognizes by
    // spelling but deliberately discards as unsupported. Value validation for
    // these neutral directives belongs to their future subsystem parsers.
    const NAMES: [&[u8]; 21] = [
        b"script-src",
        b"object-src",
        b"img-src",
        b"media-src",
        b"frame-src",
        b"font-src",
        b"connect-src",
        b"report-uri",
        b"frame-ancestors",
        b"form-action",
        b"manifest-src",
        b"upgrade-insecure-requests",
        b"child-src",
        b"block-all-mixed-content",
        b"sandbox",
        b"worker-src",
        b"script-src-elem",
        b"script-src-attr",
        b"require-trusted-types-for",
        b"trusted-types",
        b"report-to",
    ];
    NAMES
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

fn self_source_matches(document: &GeneralWebTarget, candidate: &GeneralWebTarget) -> bool {
    let source = document.origin();
    let candidate = candidate.origin();
    scheme_matches(source.scheme(), candidate.scheme())
        && same_host(source.host(), candidate.host())
        && ports_match(source.port(), candidate.port())
}

fn same_host(left: &WebHost, right: &WebHost) -> bool {
    match (left, right) {
        (WebHost::Domain(left), WebHost::Domain(right)) => left == right,
        (WebHost::Ip(left), WebHost::Ip(right)) => left == right,
        (WebHost::Domain(_), WebHost::Ip(_)) | (WebHost::Ip(_), WebHost::Domain(_)) => false,
    }
}

const fn scheme_matches(source: WebScheme, candidate: WebScheme) -> bool {
    matches!(
        (source, candidate),
        (WebScheme::Http, WebScheme::Http | WebScheme::Https)
            | (WebScheme::Https, WebScheme::Https)
    )
}

const fn ports_match(source_port: u16, candidate_port: u16) -> bool {
    source_port == candidate_port || (source_port == 80 && candidate_port == 443)
}

const fn default_port(scheme: WebScheme) -> u16 {
    match scheme {
        WebScheme::Http => 80,
        WebScheme::Https => 443,
    }
}

fn parse_candidate(candidate: &str) -> Result<GeneralWebTarget, StylePolicyError> {
    let (identity, target) = GeneralWebTarget::parse_navigation(candidate)
        .map_err(|_| StylePolicyError::InvalidCandidateUrl)?;
    if identity.as_str() != candidate {
        return Err(StylePolicyError::NonCanonicalCandidateUrl);
    }
    Ok(target)
}

fn bounded_candidate_nonce(nonce: Option<&str>) -> (Option<&str>, bool) {
    match nonce {
        Some(nonce) if nonce.len() > MAX_STYLE_CSP_NONCE_BYTES => (None, true),
        candidate => (candidate, false),
    }
}

fn try_copy_ascii(
    value: &[u8],
    allocation: StylePolicyAllocation,
) -> Result<String, StylePolicyError> {
    debug_assert!(value.is_ascii());
    let mut output = String::new();
    output
        .try_reserve_exact(value.len())
        .map_err(|_| StylePolicyError::AllocationFailed { allocation })?;
    output.extend(value.iter().map(|byte| char::from(*byte)));
    Ok(output)
}

fn try_copy_str(
    value: &str,
    allocation: StylePolicyAllocation,
) -> Result<String, StylePolicyError> {
    let mut output = String::new();
    output
        .try_reserve_exact(value.len())
        .map_err(|_| StylePolicyError::AllocationFailed { allocation })?;
    output.push_str(value);
    Ok(output)
}

fn try_push<T>(
    values: &mut Vec<T>,
    value: T,
    allocation: StylePolicyAllocation,
) -> Result<(), StylePolicyError> {
    values
        .try_reserve(1)
        .map_err(|_| StylePolicyError::AllocationFailed { allocation })?;
    values.push(value);
    Ok(())
}

fn checked_increment(current: usize, input: StylePolicyInput) -> Result<usize, StylePolicyError> {
    checked_add(current, 1, input)
}

fn checked_add(
    current: usize,
    addition: usize,
    input: StylePolicyInput,
) -> Result<usize, StylePolicyError> {
    current
        .checked_add(addition)
        .ok_or(StylePolicyError::CounterOverflow { input })
}

fn enforce_limit(
    input: StylePolicyInput,
    limit: StylePolicyLimit,
    actual: usize,
    maximum: usize,
) -> Result<(), StylePolicyError> {
    if actual > maximum {
        Err(StylePolicyError::LimitExceeded {
            input,
            limit,
            actual,
            maximum,
        })
    } else {
        Ok(())
    }
}

fn starts_ignore_ascii_case(value: &[u8], prefix: &[u8]) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
}

const fn is_csp_whitespace(byte: u8) -> bool {
    matches!(byte, b'\t' | b'\n' | 0x0c | b'\r' | b' ')
}

const fn is_directive_byte(byte: u8) -> bool {
    is_csp_whitespace(byte) || (byte >= 0x21 && byte <= 0x7e)
}

fn trim_csp_whitespace(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(|byte| is_csp_whitespace(*byte)) {
        value = &value[1..];
    }
    while value.last().is_some_and(|byte| is_csp_whitespace(*byte)) {
        value = &value[..value.len() - 1];
    }
    value
}

fn trim_header_list_whitespace(mut value: &[u8]) -> &[u8] {
    while value
        .first()
        .is_some_and(|byte| matches!(byte, b'\t' | b'\n' | b'\r' | b' '))
    {
        value = &value[1..];
    }
    while value
        .last()
        .is_some_and(|byte| matches!(byte, b'\t' | b'\n' | b'\r' | b' '))
    {
        value = &value[..value.len() - 1];
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use wild_buzzard_dom::Document;

    fn policy_set<'a>(
        document_url: &str,
        enforcing: impl IntoIterator<Item = &'a [u8]>,
        report_only: impl IntoIterator<Item = &'a [u8]>,
    ) -> Result<StylePolicySet, StylePolicyError> {
        let commit = NavigationCommitMetadata::new(
            document_url,
            0,
            crate::NavigationConnectionSecurity::Cleartext,
            false,
        )
        .unwrap();
        StylePolicySet::from_field_bytes(Document::new().version(), commit, enforcing, report_only)
    }

    fn policy_set_with_forced_report_failure<'a>(
        document_url: &str,
        enforcing: impl IntoIterator<Item = &'a [u8]>,
        failure: StylePolicyError,
    ) -> Result<StylePolicySet, StylePolicyError> {
        let response_document_version = Document::new().version();
        let commit = NavigationCommitMetadata::new(
            document_url,
            0,
            crate::NavigationConnectionSecurity::Cleartext,
            false,
        )
        .unwrap();
        let (_, document_target) = GeneralWebTarget::parse_navigation(commit.final_url())
            .map_err(|_| StylePolicyError::InvalidDocumentCommit)?;
        let mut parser = StylePolicyParser::new(&document_target);
        parser.parse_fields(enforcing, StylePolicyInput::Enforcing)?;
        let parsed = parser.finish();
        Ok(StylePolicySet::from_parsed_policy_sets(
            response_document_version,
            commit,
            document_target,
            parsed,
            Err(failure),
        ))
    }

    #[test]
    fn empty_comma_members_are_inspected_but_zero_directive_policies_are_dropped() {
        let fields = [
            b"style-src 'self',,style-src * ,".as_slice(),
            b" STYLE-SRC https://blocked.test ".as_slice(),
        ];
        let policies = policy_set("http://example.test/page", fields, []).unwrap();
        assert_eq!(policies.enforcing_policy_count(), 3);
        assert_eq!(policies.inspected_policy_member_count(), 4);
        let decision = policies
            .evaluate_external_style("http://cross.test/a.css", None)
            .unwrap();
        assert_eq!(decision.enforcing_blocked_policy_count(), 2);

        let form_feed_tail =
            policy_set("http://example.test/", [b"style-src *,\x0c".as_slice()], []).unwrap();
        assert_eq!(form_feed_tail.enforcing_policy_count(), 1);
        assert_eq!(form_feed_tail.inspected_policy_member_count(), 2);

        let neutral = policy_set(
            "http://example.test/",
            [b",unknown-src value,reflected-xss value,script-src 'none'".as_slice()],
            [],
        )
        .unwrap();
        assert_eq!(neutral.enforcing_policy_count(), 1);
        assert_eq!(neutral.inspected_policy_member_count(), 4);
        assert!(
            neutral
                .evaluate_external_style("http://cross.test/a.css", None)
                .unwrap()
                .is_allowed(),
            "a recognized non-style policy is a neutral style matcher record"
        );

        let one_per_member = vec![b"default-src *".as_slice(); MAX_STYLE_CSP_POLICY_MEMBERS];
        assert!(policy_set("http://example.test/", one_per_member, []).is_ok());
        let over = vec![b"default-src *".as_slice(); MAX_STYLE_CSP_POLICY_MEMBERS + 1];
        assert!(matches!(
            policy_set("http://example.test/", over, []),
            Err(StylePolicyError::LimitExceeded {
                input: StylePolicyInput::Aggregate,
                limit: StylePolicyLimit::PolicyMemberCount,
                actual,
                maximum: MAX_STYLE_CSP_POLICY_MEMBERS,
            }) if actual == MAX_STYLE_CSP_POLICY_MEMBERS + 1
        ));

        let empty_edge = ",".repeat(MAX_STYLE_CSP_POLICY_MEMBERS);
        let empty = policy_set("http://example.test/", [empty_edge.as_bytes()], []).unwrap();
        assert_eq!(empty.enforcing_policy_count(), 0);
        assert_eq!(
            empty.inspected_policy_member_count(),
            MAX_STYLE_CSP_POLICY_MEMBERS
        );
        let empty_over = ",".repeat(MAX_STYLE_CSP_POLICY_MEMBERS + 1);
        assert!(matches!(
            policy_set("http://example.test/", [empty_over.as_bytes()], []),
            Err(StylePolicyError::LimitExceeded {
                input: StylePolicyInput::Aggregate,
                limit: StylePolicyLimit::PolicyMemberCount,
                actual,
                maximum: MAX_STYLE_CSP_POLICY_MEMBERS,
            }) if actual == MAX_STYLE_CSP_POLICY_MEMBERS + 1
        ));
    }

    #[test]
    fn report_only_failures_roll_back_without_weakening_or_becoming_enforcing() {
        let report_members = vec![b"style-src *".as_slice(); MAX_STYLE_CSP_POLICY_MEMBERS];
        let member_failure = policy_set(
            "http://example.test/",
            [b"style-src 'none'".as_slice()],
            report_members,
        )
        .unwrap();
        assert!(matches!(
            member_failure.report_only_parse_failure(),
            Some(StylePolicyError::LimitExceeded {
                input: StylePolicyInput::Aggregate,
                limit: StylePolicyLimit::PolicyMemberCount,
                actual,
                maximum: MAX_STYLE_CSP_POLICY_MEMBERS,
            }) if actual == MAX_STYLE_CSP_POLICY_MEMBERS + 1
        ));
        assert_eq!(member_failure.enforcing_policy_count(), 1);
        assert_eq!(member_failure.report_only_policy_count(), 0);
        assert_eq!(member_failure.inspected_policy_member_count(), 1);
        let member_decision = member_failure
            .evaluate_external_style("http://cross.test/a.css", None)
            .unwrap();
        assert!(!member_decision.is_allowed());
        assert_eq!(member_decision.enforcing_blocked_policy_count(), 1);
        assert_eq!(member_decision.report_only_would_block_policy_count(), 0);

        let source_over = format!(
            "style-src {}",
            std::iter::repeat_n("*", MAX_STYLE_CSP_SOURCE_EXPRESSIONS + 1)
                .collect::<Vec<_>>()
                .join(" ")
        );
        let source_failure =
            policy_set("http://example.test/", [], [source_over.as_bytes()]).unwrap();
        assert!(matches!(
            source_failure.report_only_parse_failure(),
            Some(StylePolicyError::LimitExceeded {
                input: StylePolicyInput::Aggregate,
                limit: StylePolicyLimit::SourceExpressionCount,
                actual,
                maximum: MAX_STYLE_CSP_SOURCE_EXPRESSIONS,
            }) if actual == MAX_STYLE_CSP_SOURCE_EXPRESSIONS + 1
        ));
        assert_eq!(source_failure.enforcing_policy_count(), 0);
        assert_eq!(source_failure.report_only_policy_count(), 0);
        assert_eq!(source_failure.inspected_policy_member_count(), 0);
        assert_eq!(source_failure.relevant_source_expression_count(), 0);
        let source_decision = source_failure
            .evaluate_external_style("http://cross.test/a.css", None)
            .unwrap();
        assert!(source_decision.is_allowed());
        assert_eq!(source_decision.report_only_would_block_policy_count(), 0);

        let work_token = "a".repeat(MAX_STYLE_CSP_SOURCE_TOKEN_BYTES);
        let work_core = format!("style-src {work_token}");
        let work_member_len =
            MAX_STYLE_CSP_POLICY_WORK - 1 - (MAX_STYLE_CSP_SOURCE_TOKEN_BYTES + 1);
        let work_edge = format!(
            "{}{}",
            ";".repeat(work_member_len - work_core.len()),
            work_core
        );
        let work_over = format!(";{work_edge}");
        let work_failure = policy_set(
            "http://example.test/",
            [b"style-src 'none'".as_slice()],
            [work_over.as_bytes()],
        )
        .unwrap();
        assert!(matches!(
            work_failure.report_only_parse_failure(),
            Some(StylePolicyError::LimitExceeded {
                input: StylePolicyInput::ReportOnly,
                limit: StylePolicyLimit::PolicyWork,
                actual,
                maximum: MAX_STYLE_CSP_POLICY_WORK,
            }) if actual == MAX_STYLE_CSP_POLICY_WORK + 1
        ));
        let work_decision = work_failure.evaluate_inline_style_element(None).unwrap();
        assert!(!work_decision.is_allowed());
        assert_eq!(work_decision.report_only_would_block_policy_count(), 0);

        let allocation_failure = StylePolicyError::AllocationFailed {
            allocation: StylePolicyAllocation::UnsupportedSource,
        };
        let allocation =
            policy_set_with_forced_report_failure("http://example.test/", [], allocation_failure)
                .unwrap();
        assert_eq!(
            allocation.report_only_parse_failure(),
            Some(allocation_failure)
        );
        assert_eq!(allocation.report_only_policy_count(), 0);
        let allocation_decision = allocation.evaluate_inline_style_element(None).unwrap();
        assert!(allocation_decision.is_allowed());
        assert_eq!(
            allocation_decision.report_only_would_block_policy_count(),
            0
        );
    }

    #[test]
    fn duplicate_first_directive_case_and_ascii_whitespace_match_firefox() {
        let policies = policy_set(
            "http://example.test/page",
            [b"STYLE-SRC\t'self'; style-src *; unknown-src *; \x0bstyle-src *".as_slice()],
            [],
        )
        .unwrap();
        assert_eq!(policies.ignored_duplicate_directive_count(), 1);
        assert!(
            policies
                .evaluate_external_style("http://example.test/a.css", None)
                .unwrap()
                .is_allowed()
        );
        assert!(
            !policies
                .evaluate_external_style("http://cross.test/a.css", None)
                .unwrap()
                .is_allowed()
        );
    }

    #[test]
    fn none_fallback_and_policy_intersection_are_exact() {
        let policies = policy_set(
            "http://example.test/page",
            [
                b"default-src *; style-src 'none' https://cdn.test; style-src-attr 'unsafe-inline'"
                    .as_slice(),
                b"style-src-elem https:".as_slice(),
            ],
            [b"style-src-elem 'none'".as_slice()],
        )
        .unwrap();
        let allowed = policies
            .evaluate_external_style("https://cdn.test/a.css", None)
            .unwrap();
        assert!(allowed.is_allowed());
        assert_eq!(allowed.report_only_would_block_policy_count(), 1);
        assert!(
            !policies
                .evaluate_external_style("https://other.test/a.css", None)
                .unwrap()
                .is_allowed()
        );
        assert!(
            policies
                .evaluate_inline_style_attribute()
                .unwrap()
                .is_allowed()
        );
        assert!(
            policies
                .evaluate_base_uri("https://unrestricted.test/base/")
                .unwrap()
                .is_allowed(),
            "base-uri has no default-src fallback"
        );
    }

    #[test]
    fn schemes_hosts_ports_wildcards_and_ip_kinds_do_not_cross_match() {
        let policies = policy_set(
            "http://example.test/page",
            [b"style-src http://upgrade.test:80 https://secure.test *.cdn.test:* 127.0.0.1 [::1]"
                .as_slice()],
            [],
        )
        .unwrap();
        for allowed in [
            "https://upgrade.test/a.css",
            "https://secure.test/a.css",
            "http://a.cdn.test:8080/a.css",
            "http://127.0.0.1/a.css",
        ] {
            assert!(
                policies
                    .evaluate_external_style(allowed, None)
                    .unwrap()
                    .is_allowed(),
                "expected allowed: {allowed}"
            );
        }
        for blocked in [
            "http://secure.test/a.css",
            "http://cdn.test/a.css",
            "http://127.0.0.2/a.css",
            "http://[::1]/a.css",
            "http://[::2]/a.css",
        ] {
            assert!(
                !policies
                    .evaluate_external_style(blocked, None)
                    .unwrap()
                    .is_allowed(),
                "expected blocked: {blocked}"
            );
        }
        assert_eq!(
            policies.unsupported_sources(),
            [UnsupportedStyleSource {
                kind: UnsupportedStyleSourceKind::Malformed,
                byte_len: b"[::1]".len(),
            }]
        );
    }

    #[test]
    fn firefox_port_80_to_443_quirk_and_standards_forward_zero_stripping_are_explicit() {
        let firefox_quirk = policy_set(
            "http://example.test/page",
            [b"style-src https://quirk.test:80".as_slice()],
            [],
        )
        .unwrap();
        assert!(
            firefox_quirk
                .evaluate_external_style("https://quirk.test/a.css", None)
                .unwrap()
                .is_allowed(),
            "ESR153 permits an enforcement port of 80 against resource port 443 after scheme admission"
        );
        for blocked in ["http://quirk.test/a.css", "https://quirk.test:444/a.css"] {
            assert!(
                !firefox_quirk
                    .evaluate_external_style(blocked, None)
                    .unwrap()
                    .is_allowed(),
                "expected non-upgrade control to remain blocked: {blocked}"
            );
        }

        let leading_zero = policy_set(
            "http://example.test/page",
            [b"style-src https://zero.test:000443".as_slice()],
            [],
        )
        .unwrap();
        assert!(
            leading_zero
                .evaluate_external_style("https://zero.test/a.css", None)
                .unwrap()
                .is_allowed(),
            "Rust normalizes a numeric source port; pinned Firefox's corresponding WPT is expected FAIL"
        );
    }

    #[test]
    fn self_and_scheme_upgrade_preserve_exact_host_and_port() {
        let policies = policy_set(
            "http://example.test/page",
            [b"style-src 'self'".as_slice()],
            [],
        )
        .unwrap();
        assert!(
            policies
                .evaluate_external_style("https://example.test/a.css", None)
                .unwrap()
                .is_allowed()
        );
        assert!(
            !policies
                .evaluate_external_style("https://sub.example.test/a.css", None)
                .unwrap()
                .is_allowed()
        );
        assert!(
            !policies
                .evaluate_external_style("https://example.test:444/a.css", None)
                .unwrap()
                .is_allowed()
        );
    }

    #[test]
    fn link_and_style_element_nonces_are_separate_from_attributes() {
        let policies = policy_set(
            "http://example.test/page",
            [b"style-src 'nonce-Correct+/nonce=' 'unsafe-inline'".as_slice()],
            [],
        )
        .unwrap();
        assert!(
            policies
                .evaluate_external_style("http://blocked.test/a.css", Some("Correct+/nonce="))
                .unwrap()
                .is_allowed()
        );
        assert!(
            policies
                .evaluate_inline_style_element(Some("Correct+/nonce="))
                .unwrap()
                .is_allowed()
        );
        assert!(
            !policies
                .evaluate_inline_style_element(Some("correct+/nonce="))
                .unwrap()
                .is_allowed()
        );
        assert!(
            !policies
                .evaluate_inline_style_attribute()
                .unwrap()
                .is_allowed(),
            "nonce presence invalidates unsafe-inline and never admits attributes"
        );
    }

    #[test]
    fn retained_nonce_and_host_strings_do_not_borrow_serialized_input() {
        let mut serialized = b"style-src 'nonce-OwnedSecret' *.owned.test".to_vec();
        let policies = policy_set("http://example.test/page", [serialized.as_slice()], []).unwrap();
        serialized.fill(b'x');
        drop(serialized);

        assert!(
            policies
                .evaluate_inline_style_element(Some("OwnedSecret"))
                .unwrap()
                .is_allowed()
        );
        assert!(
            policies
                .evaluate_external_style("https://sub.owned.test/a.css", None)
                .unwrap()
                .is_allowed()
        );
        assert!(!format!("{policies:?}").contains("OwnedSecret"));
    }

    #[test]
    fn overlimit_candidate_nonce_is_absent_without_bypassing_policy() {
        let nonce_over = "n".repeat(MAX_STYLE_CSP_NONCE_BYTES + 1);
        let enforcing = policy_set(
            "http://example.test/",
            [
                b"style-src 'none'".as_slice(),
                b"style-src 'nonce-short'".as_slice(),
            ],
            [],
        )
        .unwrap();
        let blocked = enforcing
            .evaluate_external_style("https://blocked.test/a.css", Some(&nonce_over))
            .unwrap();
        assert!(!blocked.is_allowed());
        assert_eq!(blocked.enforcing_blocked_policy_count(), 2);
        assert!(blocked.candidate_nonce_ignored_over_limit());

        let url_allowlist = policy_set(
            "http://example.test/",
            [b"style-src https://allowed.test 'nonce-short'".as_slice()],
            [],
        )
        .unwrap();
        let allowed = url_allowlist
            .evaluate_external_style("https://allowed.test/a.css", Some(&nonce_over))
            .unwrap();
        assert!(allowed.is_allowed());
        assert!(allowed.candidate_nonce_ignored_over_limit());
        assert!(
            !url_allowlist
                .evaluate_external_style("https://blocked.test/a.css", Some(&nonce_over))
                .unwrap()
                .is_allowed(),
            "an over-limit nonce cannot replace URL admission"
        );

        let diagnostic_only =
            policy_set("http://example.test/", [], [b"style-src 'none'".as_slice()]).unwrap();
        let diagnostic = diagnostic_only
            .evaluate_inline_style_element(Some(&nonce_over))
            .unwrap();
        assert!(diagnostic.is_allowed());
        assert_eq!(diagnostic.report_only_would_block_policy_count(), 1);
        assert!(diagnostic.candidate_nonce_ignored_over_limit());
    }

    #[test]
    fn only_valid_supported_hash_grammar_disables_unsafe_inline() {
        let valid = policy_set(
            "http://example.test/page",
            [b"style-src 'unsafe-inline' 'sha256-Abc_-'".as_slice()],
            [],
        )
        .unwrap();
        assert!(
            !valid
                .evaluate_inline_style_element(None)
                .unwrap()
                .is_allowed()
        );
        assert_eq!(
            valid.unsupported_sources()[0].kind(),
            UnsupportedStyleSourceKind::Hash
        );

        for malformed in [
            b"style-src 'unsafe-inline' 'sha256-'".as_slice(),
            b"style-src 'unsafe-inline' 'sha1-Abc'".as_slice(),
            b"style-src 'unsafe-inline' 'sha256-Abc==='".as_slice(),
        ] {
            let policy = policy_set("http://example.test/page", [malformed], []).unwrap();
            assert!(
                policy
                    .evaluate_inline_style_element(None)
                    .unwrap()
                    .is_allowed()
            );
        }

        let unsupported_keyword = policy_set(
            "http://example.test/page",
            [b"style-src 'unsafe-inline' 'unsafe-eval'".as_slice()],
            [],
        )
        .unwrap();
        assert_eq!(
            unsupported_keyword.unsupported_sources()[0].kind(),
            UnsupportedStyleSourceKind::Keyword
        );
        assert!(
            unsupported_keyword
                .evaluate_inline_style_element(None)
                .unwrap()
                .is_allowed()
        );
    }

    #[test]
    fn one_quote_and_empty_quoted_tokens_are_bounded_nonmatching_sources() {
        for (serialized, expected_kind, token_len) in [
            (
                b"style-src '".as_slice(),
                UnsupportedStyleSourceKind::Malformed,
                1,
            ),
            (
                b"style-src ''".as_slice(),
                UnsupportedStyleSourceKind::Keyword,
                2,
            ),
        ] {
            let policies = policy_set("http://example.test/", [serialized], []).unwrap();
            assert_eq!(
                policies.unsupported_sources(),
                [UnsupportedStyleSource {
                    kind: expected_kind,
                    byte_len: token_len,
                }]
            );
            assert!(
                !policies
                    .evaluate_external_style("http://cross.test/a.css", None)
                    .unwrap()
                    .is_allowed()
            );
        }
    }

    #[test]
    fn malformed_source_bytes_never_become_document_commit_failures() {
        assert!(normalize_host(b"\xff", WebScheme::Http).unwrap().is_none());

        let invalid_directive_then_valid = policy_set(
            "http://example.test/page",
            [b"style-src \xff; style-src 'none'".as_slice()],
            [],
        )
        .unwrap();
        assert!(
            !invalid_directive_then_valid
                .evaluate_external_style("http://example.test/a.css", None)
                .unwrap()
                .is_allowed(),
            "Firefox skips the invalid non-ASCII directive, so the later valid directive wins"
        );

        let malformed_ascii = policy_set(
            "http://example.test/page",
            [b"style-src https://bad%2eexample".as_slice()],
            [],
        )
        .unwrap();
        assert_eq!(
            malformed_ascii.unsupported_sources()[0].kind(),
            UnsupportedStyleSourceKind::Malformed
        );
        assert!(
            !malformed_ascii
                .evaluate_external_style("https://bad.example/a.css", None)
                .unwrap()
                .is_allowed()
        );
    }

    #[test]
    fn paths_are_typed_nonmatching_and_debug_redacts_raw_and_nonce() {
        let policies = policy_set(
            "http://example.test/private-document",
            [b"style-src https://secret.example/private/path 'nonce-do-not-print'".as_slice()],
            [],
        )
        .unwrap();
        assert_eq!(
            policies.unsupported_sources(),
            [UnsupportedStyleSource {
                kind: UnsupportedStyleSourceKind::HostPath,
                byte_len: b"https://secret.example/private/path".len(),
            }]
        );
        let debug = format!("{policies:?}");
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("private"));
        assert!(!debug.contains("do-not-print"));
    }

    #[test]
    fn canonical_candidates_reject_credentials_and_ambiguous_spelling() {
        let policies =
            policy_set("http://example.test/page", [b"style-src *".as_slice()], []).unwrap();
        assert_eq!(
            policies.evaluate_external_style("http://user@example.test/a.css", None),
            Err(StylePolicyError::InvalidCandidateUrl)
        );
        assert_eq!(
            policies.evaluate_external_style("HTTP://EXAMPLE.TEST/a.css", None),
            Err(StylePolicyError::NonCanonicalCandidateUrl)
        );
    }

    #[test]
    fn policy_byte_and_directive_bounds_have_exact_edge_and_next_unit_evidence() {
        let policy_byte_edge = vec![b';'; MAX_STYLE_CSP_POLICY_BYTES];
        assert!(policy_set("http://example.test/", [policy_byte_edge.as_slice()], []).is_ok());
        let policy_byte_over = vec![b';'; MAX_STYLE_CSP_POLICY_BYTES + 1];
        assert!(matches!(
            policy_set(
                "http://example.test/",
                [policy_byte_over.as_slice()],
                []
            ),
            Err(StylePolicyError::LimitExceeded {
                input: StylePolicyInput::Enforcing,
                limit: StylePolicyLimit::PolicyBytes,
                actual,
                maximum: MAX_STYLE_CSP_POLICY_BYTES,
            }) if actual == MAX_STYLE_CSP_POLICY_BYTES + 1
        ));

        let directive_edge =
            std::iter::repeat_n("unknown-src", MAX_STYLE_CSP_DIRECTIVES_PER_POLICY)
                .collect::<Vec<_>>()
                .join(";");
        assert!(policy_set("http://example.test/", [directive_edge.as_bytes()], []).is_ok());
        let directive_over =
            std::iter::repeat_n("unknown-src", MAX_STYLE_CSP_DIRECTIVES_PER_POLICY + 1)
                .collect::<Vec<_>>()
                .join(";");
        assert!(matches!(
            policy_set(
                "http://example.test/",
                [directive_over.as_bytes()],
                []
            ),
            Err(StylePolicyError::LimitExceeded {
                input: StylePolicyInput::Enforcing,
                limit: StylePolicyLimit::DirectiveCount,
                actual,
                maximum: MAX_STYLE_CSP_DIRECTIVES_PER_POLICY,
            }) if actual == MAX_STYLE_CSP_DIRECTIVES_PER_POLICY + 1
        ));
    }

    #[test]
    fn source_expression_and_token_bounds_have_exact_edge_and_next_unit_evidence() {
        let source_edge = format!(
            "style-src {}",
            std::iter::repeat_n("*", MAX_STYLE_CSP_SOURCE_EXPRESSIONS)
                .collect::<Vec<_>>()
                .join(" ")
        );
        let parsed_source_edge =
            policy_set("http://example.test/", [source_edge.as_bytes()], []).unwrap();
        assert_eq!(
            parsed_source_edge.relevant_source_expression_count(),
            MAX_STYLE_CSP_SOURCE_EXPRESSIONS
        );
        let source_over = format!(
            "style-src {}",
            std::iter::repeat_n("*", MAX_STYLE_CSP_SOURCE_EXPRESSIONS + 1)
                .collect::<Vec<_>>()
                .join(" ")
        );
        assert!(matches!(
            policy_set(
                "http://example.test/",
                [source_over.as_bytes()],
                []
            ),
            Err(StylePolicyError::LimitExceeded {
                input: StylePolicyInput::Aggregate,
                limit: StylePolicyLimit::SourceExpressionCount,
                actual,
                maximum: MAX_STYLE_CSP_SOURCE_EXPRESSIONS,
            }) if actual == MAX_STYLE_CSP_SOURCE_EXPRESSIONS + 1
        ));

        let token_edge = format!("style-src {}", "a".repeat(MAX_STYLE_CSP_SOURCE_TOKEN_BYTES));
        assert!(policy_set("http://example.test/", [token_edge.as_bytes()], []).is_ok());
        let token_over = format!(
            "style-src {}",
            "a".repeat(MAX_STYLE_CSP_SOURCE_TOKEN_BYTES + 1)
        );
        assert!(matches!(
            policy_set(
                "http://example.test/",
                [token_over.as_bytes()],
                []
            ),
            Err(StylePolicyError::LimitExceeded {
                input: StylePolicyInput::Enforcing,
                limit: StylePolicyLimit::SourceTokenBytes,
                actual,
                maximum: MAX_STYLE_CSP_SOURCE_TOKEN_BYTES,
            }) if actual == MAX_STYLE_CSP_SOURCE_TOKEN_BYTES + 1
        ));
    }

    #[test]
    fn policy_work_bound_has_exact_edge_and_next_unit_evidence() {
        let work_token = "a".repeat(MAX_STYLE_CSP_SOURCE_TOKEN_BYTES);
        let work_core = format!("style-src {work_token}");
        let work_member_len =
            MAX_STYLE_CSP_POLICY_WORK - 1 - (MAX_STYLE_CSP_SOURCE_TOKEN_BYTES + 1);
        let work_edge = format!(
            "{}{}",
            ";".repeat(work_member_len - work_core.len()),
            work_core
        );
        assert_eq!(work_edge.len(), work_member_len);
        assert!(policy_set("http://example.test/", [work_edge.as_bytes()], []).is_ok());
        let work_over = format!(";{work_edge}");
        assert!(matches!(
            policy_set(
                "http://example.test/",
                [work_over.as_bytes()],
                []
            ),
            Err(StylePolicyError::LimitExceeded {
                input: StylePolicyInput::Enforcing,
                limit: StylePolicyLimit::PolicyWork,
                actual,
                maximum: MAX_STYLE_CSP_POLICY_WORK,
            }) if actual == MAX_STYLE_CSP_POLICY_WORK + 1
        ));
    }

    #[test]
    fn candidate_nonce_cap_and_counter_overflow_have_exact_evidence() {
        let policies = policy_set("http://example.test/", [b"style-src *".as_slice()], []).unwrap();
        let nonce_edge = "n".repeat(MAX_STYLE_CSP_NONCE_BYTES);
        let nonce_edge_decision = policies
            .evaluate_inline_style_element(Some(&nonce_edge))
            .unwrap();
        assert!(!nonce_edge_decision.candidate_nonce_ignored_over_limit());
        let nonce_over = "n".repeat(MAX_STYLE_CSP_NONCE_BYTES + 1);
        let nonce_over_decision = policies
            .evaluate_inline_style_element(Some(&nonce_over))
            .unwrap();
        assert!(!nonce_over_decision.is_allowed());
        assert!(nonce_over_decision.candidate_nonce_ignored_over_limit());

        assert_eq!(
            checked_add(usize::MAX, 1, StylePolicyInput::Aggregate),
            Err(StylePolicyError::CounterOverflow {
                input: StylePolicyInput::Aggregate,
            })
        );
    }
}
