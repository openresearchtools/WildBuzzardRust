// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Capability-free discovery and pre-request admission for external stylesheets.
//!
//! The planner consumes one immutable DOM snapshot and the exact captured
//! response metadata which created that snapshot. It resolves and policy-checks
//! candidate URLs, but owns no transport, cookie, resolver, report, logging,
//! renderer, callback, or stylesheet-parsing capability.

use std::fmt;

use wild_buzzard_dom::{
    DocumentSnapshot, DocumentVersion, ElementData, Namespace, NodeId, NodeKind, SnapshotNode,
};
use wild_buzzard_net::{GeneralWebTarget, WebScheme};

use crate::{
    CapturedDocumentResponseMetadata, LiveDocumentPage, NavigationCommitMetadata,
    StylePolicyDecision, StylePolicyError, StylePolicySet,
};

/// Maximum HTML `link[rel~=stylesheet]` records in one plan.
pub const MAX_STYLE_RESOURCE_CANDIDATES: usize = 64;
/// Maximum bytes in one canonical base or stylesheet URL retained or used by the plan.
pub const MAX_STYLE_RESOURCE_URL_BYTES: usize = 16 * 1024;
/// Maximum bytes inspected in one content-parsed style-resource attribute.
///
/// This applies to `rel`, `href`, `type`, and `nonce`. Presence-only admission
/// attributes do not require scanning their values.
pub const MAX_STYLE_RESOURCE_ATTRIBUTE_BYTES: usize = 16 * 1024;
/// Maximum privacy-safe diagnostics retained by one plan.
pub const MAX_STYLE_RESOURCE_DIAGNOSTICS: usize = 256;

/// A hard bounded resource owned by stylesheet discovery/admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StyleResourceLimit {
    /// HTML stylesheet candidate records.
    CandidateRecords,
    /// Bytes in one canonical URL.
    CanonicalUrlBytes,
    /// Bytes inspected in one content-parsed attribute.
    AttributeBytes,
    /// Retained privacy-safe diagnostic records.
    Diagnostics,
}

/// Checked aggregate counter maintained by the plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StyleResourceCounter {
    /// Candidate records discovered in document order.
    CandidateRecords,
    /// Retained diagnostic records.
    Diagnostics,
    /// A candidate's diagnostic range.
    DiagnosticRange,
    /// Bytes across admitted request identities.
    RequestUrlBytes,
    /// Enforcing policies which blocked evaluated candidates.
    EnforcingPolicyBlocks,
    /// Report-only policies which would block evaluated candidates.
    ReportOnlyPolicyBlocks,
}

/// One fallibly reserved allocation owned by the plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StyleResourceAllocation {
    /// Candidate-record storage.
    CandidateRecords,
    /// Admitted request-identity storage.
    Requests,
    /// Diagnostic storage.
    Diagnostics,
    /// A canonical URL copied into immutable plan state.
    Url,
}

/// Privacy-safe failure to construct an exact bounded plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StyleResourcePlanError {
    /// The snapshot was not the exact initial response revision.
    DocumentVersionMismatch {
        /// Version carried by the immutable snapshot.
        snapshot: DocumentVersion,
        /// Version bound to the captured final response.
        response: DocumentVersion,
    },
    /// The live DOM could not publish an immutable validated snapshot.
    SnapshotUnavailable,
    /// A snapshot node was not owned by the snapshot's exact document.
    SnapshotOwnershipMismatch,
    /// Internal first-gate state contradicted its validated admission record.
    AdmissionInvariant,
    /// The final commitment or enforcing CSP input failed closed.
    Policy(StylePolicyError),
    /// A count or byte bound was exceeded. Input is never truncated.
    LimitExceeded {
        /// Exhausted resource.
        limit: StyleResourceLimit,
        /// Observed count or byte length.
        actual: usize,
        /// Maximum admitted value.
        maximum: usize,
    },
    /// Checked aggregate arithmetic overflowed.
    CounterOverflow {
        /// Counter which could not represent the result.
        counter: StyleResourceCounter,
    },
    /// A bounded owned value could not be reserved.
    AllocationFailed {
        /// Allocation which failed.
        allocation: StyleResourceAllocation,
    },
}

impl fmt::Display for StyleResourcePlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DocumentVersionMismatch { .. } => {
                formatter.write_str("stylesheet plan document version mismatch")
            }
            Self::SnapshotUnavailable => {
                formatter.write_str("stylesheet plan could not snapshot the live document")
            }
            Self::SnapshotOwnershipMismatch => {
                formatter.write_str("stylesheet plan snapshot ownership mismatch")
            }
            Self::AdmissionInvariant => {
                formatter.write_str("stylesheet plan admission invariant failed")
            }
            Self::Policy(error) => write!(formatter, "stylesheet plan policy failure: {error}"),
            Self::LimitExceeded {
                limit,
                actual,
                maximum,
            } => write!(
                formatter,
                "stylesheet plan exceeded {limit:?} bound ({actual} > {maximum})"
            ),
            Self::CounterOverflow { counter } => {
                write!(formatter, "stylesheet plan {counter:?} counter overflowed")
            }
            Self::AllocationFailed { allocation } => write!(
                formatter,
                "bounded allocation failed while retaining {allocation:?} stylesheet plan data"
            ),
        }
    }
}

impl std::error::Error for StyleResourcePlanError {}

impl From<StylePolicyError> for StyleResourcePlanError {
    fn from(value: StylePolicyError) -> Self {
        Self::Policy(value)
    }
}

/// Relevant element attribute associated with a redacted diagnostic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StyleResourceAttribute {
    /// `rel`.
    Rel,
    /// `href`.
    Href,
    /// `type`.
    Type,
    /// `disabled`.
    Disabled,
    /// `crossorigin`.
    CrossOrigin,
    /// `integrity`.
    Integrity,
    /// `title`.
    Title,
    /// `nonce`.
    Nonce,
}

/// The operation associated with one retained diagnostic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StyleResourceDiagnosticSubject {
    /// The response-level report-only policy transaction.
    DocumentPolicy,
    /// The first HTML `base[href]` candidate.
    DocumentBase,
    /// One HTML external stylesheet candidate.
    ExternalStyle,
}

/// Redacted reason associated with one base or stylesheet decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StyleResourceDiagnosticKind {
    /// Report-only policy inputs were discarded transactionally after a safe parse failure.
    ReportOnlyPolicyUnavailable,
    /// A content-parsed attribute exceeded its hard byte bound.
    AttributeTooLong {
        /// Attribute whose bytes were not inspected past the bound.
        attribute: StyleResourceAttribute,
        /// Exact observed byte length.
        actual: usize,
        /// Maximum inspected byte length.
        maximum: usize,
    },
    /// The stylesheet `href` was absent or exactly empty.
    EmptyHref,
    /// A `type` value did not have an absent/empty or `text/css` essence.
    WrongType,
    /// The link had a `disabled` attribute.
    Disabled,
    /// The link had a `crossorigin` attribute, unsupported by this first gate.
    CrossOrigin,
    /// The link had nonempty integrity metadata, unsupported until SRI exists.
    Integrity,
    /// The link had a nonempty title, unsupported until style-set selection exists.
    Titled,
    /// The `rel` token list also contained `alternate`.
    AlternateStylesheet,
    /// Relative resolution or WHATWG parsing failed.
    InvalidUrl,
    /// The resolved URL used a non-HTTP(S) scheme.
    UnsupportedScheme,
    /// The resolved URL contained username or password data.
    CredentialsNotAllowed,
    /// A resolved canonical URL exceeded the per-URL bound.
    CanonicalUrlTooLong {
        /// Exact canonical URL byte length.
        actual: usize,
        /// Maximum retained canonical URL byte length.
        maximum: usize,
    },
    /// An authenticated HTTPS document directly selected a cleartext HTTP stylesheet.
    MixedContent,
    /// One or more enforcing CSP policies blocked the operation.
    PolicyBlocked {
        /// Number of enforcing policies which blocked.
        enforcing: usize,
        /// Number of report-only policies which would also block.
        report_only: usize,
    },
    /// The operation was admitted, but report-only policies would block it.
    ReportOnlyWouldBlock {
        /// Number of report-only policies which would block.
        policies: usize,
    },
    /// A candidate nonce exceeded the matcher cap and was treated as absent.
    NonceIgnoredOverLimit,
}

/// One bounded diagnostic with exact node/version ownership and no content text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StyleResourceDiagnostic {
    document_version: DocumentVersion,
    owner: Option<NodeId>,
    subject: StyleResourceDiagnosticSubject,
    kind: StyleResourceDiagnosticKind,
}

impl StyleResourceDiagnostic {
    /// Exact immutable document revision associated with this diagnostic.
    #[must_use]
    pub const fn document_version(self) -> DocumentVersion {
        self.document_version
    }

    /// Owning base/link node, or `None` for response-level evidence.
    #[must_use]
    pub const fn owner(self) -> Option<NodeId> {
        self.owner
    }

    /// Operation which produced this diagnostic.
    #[must_use]
    pub const fn subject(self) -> StyleResourceDiagnosticSubject {
        self.subject
    }

    /// Redacted diagnostic category and bounded counts.
    #[must_use]
    pub const fn kind(self) -> StyleResourceDiagnosticKind {
        self.kind
    }
}

/// Whether the first base candidate replaced the response fallback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StyleBaseCandidateStatus {
    /// The resolved candidate passed enforcing `base-uri` policy.
    Selected,
    /// The candidate failed a URL, resource, or enforcing-policy gate.
    Rejected,
}

/// Exact owner and policy evidence for the first HTML `base[href]` candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StyleBaseCandidateEvidence {
    document_version: DocumentVersion,
    owner: NodeId,
    status: StyleBaseCandidateStatus,
    policy_decision: Option<StylePolicyDecision>,
}

impl StyleBaseCandidateEvidence {
    /// Exact immutable document revision inspected.
    #[must_use]
    pub const fn document_version(self) -> DocumentVersion {
        self.document_version
    }

    /// First HTML `base[href]` node in document order.
    #[must_use]
    pub const fn owner(self) -> NodeId {
        self.owner
    }

    /// Whether the candidate was selected or rejected.
    #[must_use]
    pub const fn status(self) -> StyleBaseCandidateStatus {
        self.status
    }

    /// CSP result when the candidate reached policy evaluation.
    #[must_use]
    pub const fn policy_decision(self) -> Option<StylePolicyDecision> {
        self.policy_decision
    }
}

/// Immutable canonical identity for a future external stylesheet request.
///
/// This value is data only. It cannot open a socket, resolve a host, attach
/// credentials, send reports, or invoke a callback.
#[derive(Eq, PartialEq)]
pub struct StyleResourceRequestIdentity {
    document_version: DocumentVersion,
    owner: NodeId,
    canonical_url: String,
    policy_decision: StylePolicyDecision,
}

impl StyleResourceRequestIdentity {
    /// Exact immutable document revision which owns this request identity.
    #[must_use]
    pub const fn document_version(&self) -> DocumentVersion {
        self.document_version
    }

    /// Owning HTML link node.
    #[must_use]
    pub const fn owner(&self) -> NodeId {
        self.owner
    }

    /// Canonical credential-free fragment-free HTTP(S) request URL.
    #[must_use]
    pub fn canonical_url(&self) -> &str {
        &self.canonical_url
    }

    /// Exact enforcing/report-only and bounded-nonce decision evidence.
    #[must_use]
    pub const fn policy_decision(&self) -> StylePolicyDecision {
        self.policy_decision
    }
}

impl fmt::Debug for StyleResourceRequestIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StyleResourceRequestIdentity")
            .field("document_version", &self.document_version)
            .field("owner", &self.owner)
            .field("canonical_url_bytes", &self.canonical_url.len())
            .field("policy_decision", &self.policy_decision)
            .finish_non_exhaustive()
    }
}

/// Outcome for one discovered HTML `link[rel~=stylesheet]` element.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StyleResourceCandidateStatus {
    /// The candidate produced the request at this index in the plan's request slice.
    Admitted {
        /// Index into [`StyleResourcePlan::requests`].
        request_index: usize,
    },
    /// The candidate produced no request identity.
    Rejected,
}

/// One DOM-order stylesheet candidate record without peer-controlled strings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StyleResourceCandidateRecord {
    document_version: DocumentVersion,
    owner: NodeId,
    status: StyleResourceCandidateStatus,
    policy_decision: Option<StylePolicyDecision>,
    first_diagnostic: usize,
    diagnostic_count: usize,
}

impl StyleResourceCandidateRecord {
    /// Exact immutable document revision inspected.
    #[must_use]
    pub const fn document_version(self) -> DocumentVersion {
        self.document_version
    }

    /// Owning HTML link node.
    #[must_use]
    pub const fn owner(self) -> NodeId {
        self.owner
    }

    /// Admission outcome and request index, if admitted.
    #[must_use]
    pub const fn status(self) -> StyleResourceCandidateStatus {
        self.status
    }

    /// CSP result when the candidate reached policy evaluation.
    #[must_use]
    pub const fn policy_decision(self) -> Option<StylePolicyDecision> {
        self.policy_decision
    }

    /// First associated entry in [`StyleResourcePlan::diagnostics`].
    #[must_use]
    pub const fn first_diagnostic_index(self) -> usize {
        self.first_diagnostic
    }

    /// Number of consecutive associated diagnostic entries.
    #[must_use]
    pub const fn diagnostic_count(self) -> usize {
        self.diagnostic_count
    }
}

/// Immutable bounded pre-fetch stylesheet discovery/admission plan.
pub struct StyleResourcePlan {
    document_version: DocumentVersion,
    navigation_commit: NavigationCommitMetadata,
    fallback_base_url: String,
    document_base_url: String,
    base_candidate: Option<StyleBaseCandidateEvidence>,
    candidates: Vec<StyleResourceCandidateRecord>,
    requests: Vec<StyleResourceRequestIdentity>,
    diagnostics: Vec<StyleResourceDiagnostic>,
    enforcing_policy_count: usize,
    report_only_policy_count: usize,
    report_only_parse_failure: Option<StylePolicyError>,
    enforcing_policy_block_count: usize,
    report_only_policy_block_count: usize,
    retained_request_url_bytes: usize,
}

impl StyleResourcePlan {
    /// Snapshots an unmodified live response document and constructs its plan.
    ///
    /// A live document whose revision advanced beyond its captured response is
    /// rejected by the same exact-version check as [`Self::from_snapshot`].
    ///
    /// # Errors
    ///
    /// Returns a typed redacted failure when snapshot publication or plan
    /// construction fails.
    pub fn from_live_document(page: &LiveDocumentPage) -> Result<Self, StyleResourcePlanError> {
        let snapshot = page
            .document
            .snapshot()
            .map_err(|_| StyleResourcePlanError::SnapshotUnavailable)?;
        Self::from_snapshot(&snapshot, page.captured_response_metadata())
    }

    /// Constructs a capability-free plan from an exact immutable response revision.
    ///
    /// # Errors
    ///
    /// Returns a typed redacted failure for version/ownership mismatch, an
    /// invalid final commitment, enforcing-policy failure, resource exhaustion,
    /// checked arithmetic failure, or bounded allocation failure. Hard bounds
    /// reject the complete plan; no partial or truncated plan is returned.
    pub fn from_snapshot(
        snapshot: &DocumentSnapshot,
        metadata: &CapturedDocumentResponseMetadata,
    ) -> Result<Self, StyleResourcePlanError> {
        let mut builder = StyleResourcePlanBuilder::new(snapshot, metadata)?;
        builder.discover_base(snapshot)?;
        builder.discover_stylesheets(snapshot)?;
        Ok(builder.finish())
    }

    /// Exact immutable response revision represented by every record.
    #[must_use]
    pub const fn document_version(&self) -> DocumentVersion {
        self.document_version
    }

    /// Exact validated final navigation commitment.
    #[must_use]
    pub const fn navigation_commit(&self) -> &NavigationCommitMetadata {
        &self.navigation_commit
    }

    /// Canonical exact final response URL used as the document fallback base.
    #[must_use]
    pub fn fallback_base_url(&self) -> &str {
        &self.fallback_base_url
    }

    /// Canonical selected document base, or the unchanged fallback after rejection.
    #[must_use]
    pub fn document_base_url(&self) -> &str {
        &self.document_base_url
    }

    /// Evidence for the first HTML `base[href]`, when present.
    #[must_use]
    pub const fn base_candidate(&self) -> Option<StyleBaseCandidateEvidence> {
        self.base_candidate
    }

    /// Every discovered stylesheet candidate in DOM order.
    #[must_use]
    pub fn candidates(&self) -> &[StyleResourceCandidateRecord] {
        &self.candidates
    }

    /// Admitted canonical request identities in DOM order.
    #[must_use]
    pub fn requests(&self) -> &[StyleResourceRequestIdentity] {
        &self.requests
    }

    /// Bounded redacted diagnostics in discovery/evaluation order.
    #[must_use]
    pub fn diagnostics(&self) -> &[StyleResourceDiagnostic] {
        &self.diagnostics
    }

    /// Retained enforcing style-policy record count.
    #[must_use]
    pub const fn enforcing_policy_count(&self) -> usize {
        self.enforcing_policy_count
    }

    /// Retained report-only style-policy record count.
    #[must_use]
    pub const fn report_only_policy_count(&self) -> usize {
        self.report_only_policy_count
    }

    /// Redacted reason report-only policy evidence was discarded transactionally.
    #[must_use]
    pub const fn report_only_parse_failure(&self) -> Option<StylePolicyError> {
        self.report_only_parse_failure
    }

    /// Aggregate enforcing policy blocks across evaluated base/link operations.
    #[must_use]
    pub const fn enforcing_policy_block_count(&self) -> usize {
        self.enforcing_policy_block_count
    }

    /// Aggregate report-only would-block count across evaluated operations.
    #[must_use]
    pub const fn report_only_policy_block_count(&self) -> usize {
        self.report_only_policy_block_count
    }

    /// Aggregate bytes across canonical admitted request identities.
    #[must_use]
    pub const fn retained_request_url_bytes(&self) -> usize {
        self.retained_request_url_bytes
    }
}

impl fmt::Debug for StyleResourcePlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StyleResourcePlan")
            .field("document_version", &self.document_version)
            .field("redirect_count", &self.navigation_commit.redirect_count())
            .field("fallback_base_url_bytes", &self.fallback_base_url.len())
            .field("document_base_url_bytes", &self.document_base_url.len())
            .field("base_candidate", &self.base_candidate)
            .field("candidates", &self.candidates)
            .field("requests", &self.requests)
            .field("diagnostics", &self.diagnostics)
            .field("enforcing_policy_count", &self.enforcing_policy_count)
            .field("report_only_policy_count", &self.report_only_policy_count)
            .field("report_only_parse_failure", &self.report_only_parse_failure)
            .field(
                "enforcing_policy_block_count",
                &self.enforcing_policy_block_count,
            )
            .field(
                "report_only_policy_block_count",
                &self.report_only_policy_block_count,
            )
            .field(
                "retained_request_url_bytes",
                &self.retained_request_url_bytes,
            )
            .finish_non_exhaustive()
    }
}

struct StyleResourcePlanBuilder {
    document_version: DocumentVersion,
    navigation_commit: NavigationCommitMetadata,
    final_document_scheme: WebScheme,
    fallback_base_url: String,
    document_base_url: String,
    base_candidate: Option<StyleBaseCandidateEvidence>,
    candidates: Vec<StyleResourceCandidateRecord>,
    requests: Vec<StyleResourceRequestIdentity>,
    diagnostics: Vec<StyleResourceDiagnostic>,
    policies: StylePolicySet,
    report_only_parse_failure: Option<StylePolicyError>,
    enforcing_policy_block_count: usize,
    report_only_policy_block_count: usize,
    retained_request_url_bytes: usize,
}

impl StyleResourcePlanBuilder {
    fn new(
        snapshot: &DocumentSnapshot,
        metadata: &CapturedDocumentResponseMetadata,
    ) -> Result<Self, StyleResourcePlanError> {
        let document_version = snapshot.version();
        let response_version = metadata.response_document_version();
        if document_version != response_version {
            return Err(StyleResourcePlanError::DocumentVersionMismatch {
                snapshot: document_version,
                response: response_version,
            });
        }
        validate_snapshot_ownership(snapshot)?;

        let policies = StylePolicySet::from_response_metadata(metadata)?;
        if policies.response_document_version() != document_version
            || policies.navigation_commit() != metadata.navigation_commit()
        {
            return Err(StyleResourcePlanError::DocumentVersionMismatch {
                snapshot: document_version,
                response: policies.response_document_version(),
            });
        }

        let final_url = metadata.navigation_commit().final_url();
        let (final_identity, final_target) = GeneralWebTarget::parse_navigation(final_url)
            .map_err(|_| StyleResourcePlanError::Policy(StylePolicyError::InvalidDocumentCommit))?;
        if final_identity.as_str() != final_url {
            return Err(StyleResourcePlanError::Policy(
                StylePolicyError::InvalidDocumentCommit,
            ));
        }
        enforce_limit(
            StyleResourceLimit::CanonicalUrlBytes,
            final_url.len(),
            MAX_STYLE_RESOURCE_URL_BYTES,
        )?;

        let report_only_parse_failure = policies.report_only_parse_failure();
        let mut builder = Self {
            document_version,
            navigation_commit: metadata.navigation_commit().clone(),
            final_document_scheme: final_target.origin().scheme(),
            fallback_base_url: try_copy_url(final_url)?,
            document_base_url: try_copy_url(final_url)?,
            base_candidate: None,
            candidates: try_vec_with_capacity(
                MAX_STYLE_RESOURCE_CANDIDATES,
                StyleResourceAllocation::CandidateRecords,
            )?,
            requests: try_vec_with_capacity(
                MAX_STYLE_RESOURCE_CANDIDATES,
                StyleResourceAllocation::Requests,
            )?,
            diagnostics: try_vec_with_capacity(
                MAX_STYLE_RESOURCE_DIAGNOSTICS,
                StyleResourceAllocation::Diagnostics,
            )?,
            policies,
            report_only_parse_failure,
            enforcing_policy_block_count: 0,
            report_only_policy_block_count: 0,
            retained_request_url_bytes: 0,
        };
        if report_only_parse_failure.is_some() {
            builder.push_diagnostic(
                None,
                StyleResourceDiagnosticSubject::DocumentPolicy,
                StyleResourceDiagnosticKind::ReportOnlyPolicyUnavailable,
            )?;
        }
        Ok(builder)
    }

    fn discover_base(&mut self, snapshot: &DocumentSnapshot) -> Result<(), StyleResourcePlanError> {
        for node in snapshot.nodes_in_document_order() {
            let NodeKind::Element(element) = &node.kind else {
                continue;
            };
            if is_html_element(element, "base")
                && let Some(href) = element.html_attribute("href")
            {
                self.evaluate_first_base(node.id, href)?;
                break;
            }
        }
        Ok(())
    }

    fn evaluate_first_base(
        &mut self,
        owner: NodeId,
        href: &str,
    ) -> Result<(), StyleResourcePlanError> {
        if href.len() > MAX_STYLE_RESOURCE_ATTRIBUTE_BYTES {
            self.push_diagnostic(
                Some(owner),
                StyleResourceDiagnosticSubject::DocumentBase,
                StyleResourceDiagnosticKind::AttributeTooLong {
                    attribute: StyleResourceAttribute::Href,
                    actual: href.len(),
                    maximum: MAX_STYLE_RESOURCE_ATTRIBUTE_BYTES,
                },
            )?;
            self.set_base_evidence(owner, StyleBaseCandidateStatus::Rejected, None);
            return Ok(());
        }

        let resolved = match resolve_http_url(&self.fallback_base_url, href, true) {
            Ok(resolved) => resolved,
            Err(ResolveHttpUrlError::Rejected(rejection)) => {
                self.push_diagnostic(
                    Some(owner),
                    StyleResourceDiagnosticSubject::DocumentBase,
                    rejection.diagnostic_kind(),
                )?;
                self.set_base_evidence(owner, StyleBaseCandidateStatus::Rejected, None);
                return Ok(());
            }
            Err(ResolveHttpUrlError::Fatal(error)) => return Err(error),
        };

        let decision = self.policies.evaluate_base_uri(&resolved.canonical)?;
        self.accumulate_policy_counts(decision)?;
        if !decision.is_allowed() {
            self.push_diagnostic(
                Some(owner),
                StyleResourceDiagnosticSubject::DocumentBase,
                policy_blocked_diagnostic(decision),
            )?;
            self.set_base_evidence(owner, StyleBaseCandidateStatus::Rejected, Some(decision));
            return Ok(());
        }
        if decision.report_only_would_block() {
            self.push_diagnostic(
                Some(owner),
                StyleResourceDiagnosticSubject::DocumentBase,
                report_only_diagnostic(decision),
            )?;
        }
        self.document_base_url = resolved.canonical;
        self.set_base_evidence(owner, StyleBaseCandidateStatus::Selected, Some(decision));
        Ok(())
    }

    fn discover_stylesheets(
        &mut self,
        snapshot: &DocumentSnapshot,
    ) -> Result<(), StyleResourcePlanError> {
        for node in snapshot.nodes_in_document_order() {
            let NodeKind::Element(element) = &node.kind else {
                continue;
            };
            if !is_html_element(element, "link") {
                continue;
            }
            let Some(rel) = element.html_attribute("rel") else {
                continue;
            };
            enforce_limit(
                StyleResourceLimit::AttributeBytes,
                rel.len(),
                MAX_STYLE_RESOURCE_ATTRIBUTE_BYTES,
            )?;
            if rel_has_token(rel, "stylesheet") {
                self.inspect_stylesheet(node, element, rel)?;
            }
        }
        Ok(())
    }

    fn inspect_stylesheet(
        &mut self,
        node: &SnapshotNode,
        element: &ElementData,
        rel: &str,
    ) -> Result<(), StyleResourcePlanError> {
        checked_increment_limit(
            self.candidates.len(),
            StyleResourceLimit::CandidateRecords,
            MAX_STYLE_RESOURCE_CANDIDATES,
            StyleResourceCounter::CandidateRecords,
        )?;
        let first_diagnostic = self.diagnostics.len();
        let href = element.html_attribute("href");
        let nonce = element.html_attribute("nonce");
        let rejected = self.inspect_link_attributes(node.id, element, rel, href, nonce)?;
        if rejected {
            return self.record_candidate(
                node.id,
                StyleResourceCandidateStatus::Rejected,
                None,
                first_diagnostic,
            );
        }
        let Some(href) = href.filter(|value| !value.is_empty()) else {
            return Err(StyleResourcePlanError::AdmissionInvariant);
        };
        self.evaluate_stylesheet_url(node.id, href, nonce, first_diagnostic)
    }

    fn inspect_link_attributes(
        &mut self,
        owner: NodeId,
        element: &ElementData,
        rel: &str,
        href: Option<&str>,
        nonce: Option<&str>,
    ) -> Result<bool, StyleResourcePlanError> {
        let mut rejected = self.inspect_href(owner, href)?;
        rejected |= self.inspect_type(owner, element.html_attribute("type"))?;
        rejected |= self.reject_if(
            owner,
            element.html_attribute("disabled").is_some(),
            StyleResourceDiagnosticKind::Disabled,
        )?;
        rejected |= self.reject_if(
            owner,
            element.html_attribute("crossorigin").is_some(),
            StyleResourceDiagnosticKind::CrossOrigin,
        )?;
        rejected |= self.reject_if(
            owner,
            element
                .html_attribute("integrity")
                .is_some_and(|value| !value.is_empty()),
            StyleResourceDiagnosticKind::Integrity,
        )?;
        rejected |= self.reject_if(
            owner,
            element
                .html_attribute("title")
                .is_some_and(|value| !value.is_empty()),
            StyleResourceDiagnosticKind::Titled,
        )?;
        rejected |= self.reject_if(
            owner,
            rel_has_token(rel, "alternate"),
            StyleResourceDiagnosticKind::AlternateStylesheet,
        )?;
        if let Some(value) = nonce {
            rejected |= self.diagnose_oversized(owner, StyleResourceAttribute::Nonce, value)?;
        }
        Ok(rejected)
    }

    fn inspect_href(
        &mut self,
        owner: NodeId,
        href: Option<&str>,
    ) -> Result<bool, StyleResourcePlanError> {
        match href {
            None | Some("") => self.reject_if(owner, true, StyleResourceDiagnosticKind::EmptyHref),
            Some(value) => self.diagnose_oversized(owner, StyleResourceAttribute::Href, value),
        }
    }

    fn inspect_type(
        &mut self,
        owner: NodeId,
        type_value: Option<&str>,
    ) -> Result<bool, StyleResourcePlanError> {
        let Some(value) = type_value else {
            return Ok(false);
        };
        if self.diagnose_oversized(owner, StyleResourceAttribute::Type, value)? {
            return Ok(true);
        }
        self.reject_if(
            owner,
            !type_has_css_essence(value),
            StyleResourceDiagnosticKind::WrongType,
        )
    }

    fn evaluate_stylesheet_url(
        &mut self,
        owner: NodeId,
        href: &str,
        nonce: Option<&str>,
        first_diagnostic: usize,
    ) -> Result<(), StyleResourcePlanError> {
        let resolved = match resolve_http_url(&self.document_base_url, href, false) {
            Ok(resolved) => resolved,
            Err(ResolveHttpUrlError::Rejected(rejection)) => {
                return self.reject_candidate(
                    owner,
                    first_diagnostic,
                    None,
                    rejection.diagnostic_kind(),
                );
            }
            Err(ResolveHttpUrlError::Fatal(error)) => return Err(error),
        };
        if self.final_document_scheme == WebScheme::Https && resolved.scheme == WebScheme::Http {
            return self.reject_candidate(
                owner,
                first_diagnostic,
                None,
                StyleResourceDiagnosticKind::MixedContent,
            );
        }
        self.evaluate_stylesheet_policy(owner, resolved, nonce, first_diagnostic)
    }

    fn evaluate_stylesheet_policy(
        &mut self,
        owner: NodeId,
        resolved: ResolvedHttpUrl,
        nonce: Option<&str>,
        first_diagnostic: usize,
    ) -> Result<(), StyleResourcePlanError> {
        let decision = self
            .policies
            .evaluate_external_style(&resolved.canonical, nonce)?;
        self.accumulate_policy_counts(decision)?;
        if decision.candidate_nonce_ignored_over_limit() {
            self.push_external_diagnostic(
                owner,
                StyleResourceDiagnosticKind::NonceIgnoredOverLimit,
            )?;
        }
        if !decision.is_allowed() {
            return self.reject_candidate(
                owner,
                first_diagnostic,
                Some(decision),
                policy_blocked_diagnostic(decision),
            );
        }
        if decision.report_only_would_block() {
            self.push_external_diagnostic(owner, report_only_diagnostic(decision))?;
        }

        self.retained_request_url_bytes = checked_add_counter(
            self.retained_request_url_bytes,
            resolved.canonical.len(),
            StyleResourceCounter::RequestUrlBytes,
        )?;
        let request_index = self.requests.len();
        self.requests.push(StyleResourceRequestIdentity {
            document_version: self.document_version,
            owner,
            canonical_url: resolved.canonical,
            policy_decision: decision,
        });
        self.record_candidate(
            owner,
            StyleResourceCandidateStatus::Admitted { request_index },
            Some(decision),
            first_diagnostic,
        )
    }

    fn reject_candidate(
        &mut self,
        owner: NodeId,
        first_diagnostic: usize,
        decision: Option<StylePolicyDecision>,
        kind: StyleResourceDiagnosticKind,
    ) -> Result<(), StyleResourcePlanError> {
        self.push_external_diagnostic(owner, kind)?;
        self.record_candidate(
            owner,
            StyleResourceCandidateStatus::Rejected,
            decision,
            first_diagnostic,
        )
    }

    fn record_candidate(
        &mut self,
        owner: NodeId,
        status: StyleResourceCandidateStatus,
        decision: Option<StylePolicyDecision>,
        first_diagnostic: usize,
    ) -> Result<(), StyleResourcePlanError> {
        let record = candidate_record(
            self.document_version,
            owner,
            status,
            decision,
            first_diagnostic,
            self.diagnostics.len(),
        )?;
        push_candidate_record(&mut self.candidates, record);
        Ok(())
    }

    fn reject_if(
        &mut self,
        owner: NodeId,
        rejected: bool,
        kind: StyleResourceDiagnosticKind,
    ) -> Result<bool, StyleResourcePlanError> {
        if rejected {
            self.push_external_diagnostic(owner, kind)?;
        }
        Ok(rejected)
    }

    fn diagnose_oversized(
        &mut self,
        owner: NodeId,
        attribute: StyleResourceAttribute,
        value: &str,
    ) -> Result<bool, StyleResourcePlanError> {
        diagnose_attribute_length(
            &mut self.diagnostics,
            self.document_version,
            owner,
            StyleResourceDiagnosticSubject::ExternalStyle,
            attribute,
            value,
        )
    }

    fn push_external_diagnostic(
        &mut self,
        owner: NodeId,
        kind: StyleResourceDiagnosticKind,
    ) -> Result<(), StyleResourcePlanError> {
        self.push_diagnostic(
            Some(owner),
            StyleResourceDiagnosticSubject::ExternalStyle,
            kind,
        )
    }

    fn push_diagnostic(
        &mut self,
        owner: Option<NodeId>,
        subject: StyleResourceDiagnosticSubject,
        kind: StyleResourceDiagnosticKind,
    ) -> Result<(), StyleResourcePlanError> {
        push_diagnostic(
            &mut self.diagnostics,
            StyleResourceDiagnostic {
                document_version: self.document_version,
                owner,
                subject,
                kind,
            },
        )
    }

    fn accumulate_policy_counts(
        &mut self,
        decision: StylePolicyDecision,
    ) -> Result<(), StyleResourcePlanError> {
        accumulate_policy_counts(
            decision,
            &mut self.enforcing_policy_block_count,
            &mut self.report_only_policy_block_count,
        )
    }

    fn set_base_evidence(
        &mut self,
        owner: NodeId,
        status: StyleBaseCandidateStatus,
        policy_decision: Option<StylePolicyDecision>,
    ) {
        self.base_candidate = Some(StyleBaseCandidateEvidence {
            document_version: self.document_version,
            owner,
            status,
            policy_decision,
        });
    }

    fn finish(self) -> StyleResourcePlan {
        StyleResourcePlan {
            document_version: self.document_version,
            navigation_commit: self.navigation_commit,
            fallback_base_url: self.fallback_base_url,
            document_base_url: self.document_base_url,
            base_candidate: self.base_candidate,
            candidates: self.candidates,
            requests: self.requests,
            diagnostics: self.diagnostics,
            enforcing_policy_count: self.policies.enforcing_policy_count(),
            report_only_policy_count: self.policies.report_only_policy_count(),
            report_only_parse_failure: self.report_only_parse_failure,
            enforcing_policy_block_count: self.enforcing_policy_block_count,
            report_only_policy_block_count: self.report_only_policy_block_count,
            retained_request_url_bytes: self.retained_request_url_bytes,
        }
    }
}

fn is_html_element(element: &ElementData, local_name: &str) -> bool {
    element.name.namespace == Namespace::Html && element.name.local_name == local_name
}

const fn policy_blocked_diagnostic(decision: StylePolicyDecision) -> StyleResourceDiagnosticKind {
    StyleResourceDiagnosticKind::PolicyBlocked {
        enforcing: decision.enforcing_blocked_policy_count(),
        report_only: decision.report_only_would_block_policy_count(),
    }
}

const fn report_only_diagnostic(decision: StylePolicyDecision) -> StyleResourceDiagnosticKind {
    StyleResourceDiagnosticKind::ReportOnlyWouldBlock {
        policies: decision.report_only_would_block_policy_count(),
    }
}

fn validate_snapshot_ownership(snapshot: &DocumentSnapshot) -> Result<(), StyleResourcePlanError> {
    if snapshot.document_node().document_id() != snapshot.document_id()
        || snapshot
            .nodes_in_document_order()
            .iter()
            .any(|node| node.id.document_id() != snapshot.document_id())
    {
        return Err(StyleResourcePlanError::SnapshotOwnershipMismatch);
    }
    Ok(())
}

#[derive(Debug)]
struct ResolvedHttpUrl {
    canonical: String,
    scheme: WebScheme,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UrlRejection {
    Invalid,
    UnsupportedScheme,
    Credentials,
    TooLong { actual: usize },
}

#[derive(Debug)]
enum ResolveHttpUrlError {
    Rejected(UrlRejection),
    Fatal(StyleResourcePlanError),
}

impl UrlRejection {
    const fn diagnostic_kind(self) -> StyleResourceDiagnosticKind {
        match self {
            Self::Invalid => StyleResourceDiagnosticKind::InvalidUrl,
            Self::UnsupportedScheme => StyleResourceDiagnosticKind::UnsupportedScheme,
            Self::Credentials => StyleResourceDiagnosticKind::CredentialsNotAllowed,
            Self::TooLong { actual } => StyleResourceDiagnosticKind::CanonicalUrlTooLong {
                actual,
                maximum: MAX_STYLE_RESOURCE_URL_BYTES,
            },
        }
    }
}

fn resolve_http_url(
    base_url: &str,
    href: &str,
    retain_fragment: bool,
) -> Result<ResolvedHttpUrl, ResolveHttpUrlError> {
    let (base_identity, _) = GeneralWebTarget::parse_navigation(base_url)
        .map_err(|_| ResolveHttpUrlError::Rejected(UrlRejection::Invalid))?;
    let resolved = base_identity
        .join(href)
        .map_err(|_| ResolveHttpUrlError::Rejected(UrlRejection::Invalid))?;
    let scheme = match resolved.scheme() {
        "http" => WebScheme::Http,
        "https" => WebScheme::Https,
        _ => {
            return Err(ResolveHttpUrlError::Rejected(
                UrlRejection::UnsupportedScheme,
            ));
        }
    };
    if !resolved.username().is_empty() || resolved.password().is_some() {
        return Err(ResolveHttpUrlError::Rejected(UrlRejection::Credentials));
    }
    let (identity, target) = GeneralWebTarget::from_navigation_url(resolved)
        .map_err(|_| ResolveHttpUrlError::Rejected(UrlRejection::Invalid))?;
    let canonical = if retain_fragment {
        identity.as_str()
    } else {
        target.url().as_str()
    };
    if canonical.len() > MAX_STYLE_RESOURCE_URL_BYTES {
        return Err(ResolveHttpUrlError::Rejected(UrlRejection::TooLong {
            actual: canonical.len(),
        }));
    }
    let canonical = try_copy_url(canonical).map_err(ResolveHttpUrlError::Fatal)?;
    Ok(ResolvedHttpUrl { canonical, scheme })
}

fn rel_has_token(value: &str, expected: &str) -> bool {
    value
        .split(is_html_ascii_whitespace)
        .any(|token| token.eq_ignore_ascii_case(expected))
}

const fn is_html_ascii_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '
    )
}

fn type_has_css_essence(value: &str) -> bool {
    let trimmed = value.trim_matches(is_html_ascii_whitespace);
    if trimmed.is_empty() {
        return true;
    }
    let essence = trimmed
        .split_once(';')
        .map_or(trimmed, |(essence, _)| essence)
        .trim_matches(is_html_ascii_whitespace);
    essence.eq_ignore_ascii_case("text/css")
}

fn diagnose_attribute_length(
    diagnostics: &mut Vec<StyleResourceDiagnostic>,
    document_version: DocumentVersion,
    owner: NodeId,
    subject: StyleResourceDiagnosticSubject,
    attribute: StyleResourceAttribute,
    value: &str,
) -> Result<bool, StyleResourcePlanError> {
    if value.len() <= MAX_STYLE_RESOURCE_ATTRIBUTE_BYTES {
        return Ok(false);
    }
    push_node_diagnostic(
        diagnostics,
        document_version,
        owner,
        subject,
        StyleResourceDiagnosticKind::AttributeTooLong {
            attribute,
            actual: value.len(),
            maximum: MAX_STYLE_RESOURCE_ATTRIBUTE_BYTES,
        },
    )?;
    Ok(true)
}

fn accumulate_policy_counts(
    decision: StylePolicyDecision,
    enforcing: &mut usize,
    report_only: &mut usize,
) -> Result<(), StyleResourcePlanError> {
    *enforcing = checked_add_counter(
        *enforcing,
        decision.enforcing_blocked_policy_count(),
        StyleResourceCounter::EnforcingPolicyBlocks,
    )?;
    *report_only = checked_add_counter(
        *report_only,
        decision.report_only_would_block_policy_count(),
        StyleResourceCounter::ReportOnlyPolicyBlocks,
    )?;
    Ok(())
}

fn candidate_record(
    document_version: DocumentVersion,
    owner: NodeId,
    status: StyleResourceCandidateStatus,
    policy_decision: Option<StylePolicyDecision>,
    first_diagnostic: usize,
    diagnostics_end: usize,
) -> Result<StyleResourceCandidateRecord, StyleResourcePlanError> {
    let diagnostic_count = diagnostics_end.checked_sub(first_diagnostic).ok_or(
        StyleResourcePlanError::CounterOverflow {
            counter: StyleResourceCounter::DiagnosticRange,
        },
    )?;
    Ok(StyleResourceCandidateRecord {
        document_version,
        owner,
        status,
        policy_decision,
        first_diagnostic,
        diagnostic_count,
    })
}

fn push_candidate_record(
    candidates: &mut Vec<StyleResourceCandidateRecord>,
    record: StyleResourceCandidateRecord,
) {
    debug_assert!(candidates.len() < MAX_STYLE_RESOURCE_CANDIDATES);
    candidates.push(record);
}

fn push_node_diagnostic(
    diagnostics: &mut Vec<StyleResourceDiagnostic>,
    document_version: DocumentVersion,
    owner: NodeId,
    subject: StyleResourceDiagnosticSubject,
    kind: StyleResourceDiagnosticKind,
) -> Result<(), StyleResourcePlanError> {
    push_diagnostic(
        diagnostics,
        StyleResourceDiagnostic {
            document_version,
            owner: Some(owner),
            subject,
            kind,
        },
    )
}

fn push_diagnostic(
    diagnostics: &mut Vec<StyleResourceDiagnostic>,
    diagnostic: StyleResourceDiagnostic,
) -> Result<(), StyleResourcePlanError> {
    checked_increment_limit(
        diagnostics.len(),
        StyleResourceLimit::Diagnostics,
        MAX_STYLE_RESOURCE_DIAGNOSTICS,
        StyleResourceCounter::Diagnostics,
    )?;
    diagnostics.push(diagnostic);
    Ok(())
}

fn try_vec_with_capacity<T>(
    capacity: usize,
    allocation: StyleResourceAllocation,
) -> Result<Vec<T>, StyleResourcePlanError> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(capacity)
        .map_err(|_| StyleResourcePlanError::AllocationFailed { allocation })?;
    Ok(values)
}

fn try_copy_url(value: &str) -> Result<String, StyleResourcePlanError> {
    let mut output = String::new();
    output.try_reserve_exact(value.len()).map_err(|_| {
        StyleResourcePlanError::AllocationFailed {
            allocation: StyleResourceAllocation::Url,
        }
    })?;
    output.push_str(value);
    Ok(output)
}

fn checked_increment_limit(
    current: usize,
    limit: StyleResourceLimit,
    maximum: usize,
    counter: StyleResourceCounter,
) -> Result<usize, StyleResourcePlanError> {
    let actual = current
        .checked_add(1)
        .ok_or(StyleResourcePlanError::CounterOverflow { counter })?;
    enforce_limit(limit, actual, maximum)?;
    Ok(actual)
}

fn enforce_limit(
    limit: StyleResourceLimit,
    actual: usize,
    maximum: usize,
) -> Result<(), StyleResourcePlanError> {
    if actual > maximum {
        Err(StyleResourcePlanError::LimitExceeded {
            limit,
            actual,
            maximum,
        })
    } else {
        Ok(())
    }
}

fn checked_add_counter(
    current: usize,
    addition: usize,
    counter: StyleResourceCounter,
) -> Result<usize, StyleResourcePlanError> {
    current
        .checked_add(addition)
        .ok_or(StyleResourcePlanError::CounterOverflow { counter })
}
