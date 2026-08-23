//! Bounded, generation-checked navigation and presentation facade.
//!
//! The facade owns one synchronous executor on one dedicated worker thread. It
//! deliberately does not expose DOM, layout, renderer, headless, or platform
//! window types. A successful result is published as an opaque frame lease only
//! while its navigation generation is still current.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::num::{NonZeroU64, NonZeroUsize};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::thread::{self, JoinHandle};

use wild_buzzard_dom::bindings::{
    CreatedNodeToken, ScriptMutationBatch, ScriptMutationCommand, ScriptMutationLimitKind,
    ScriptMutationLimits, ScriptNode,
};
use wild_buzzard_dom::{DocumentSnapshot, DocumentVersion, NodeId};
use wild_buzzard_headless::RgbaFrame;
use wild_buzzard_net::{
    AlpnOutcome, CommittedResponseAuthority, ConnectionSecurity, GeneralWebConfig,
    GeneralWebResponse, GeneralWebTarget, IpAddressSpace, TlsVersion, TrustStore, WebScheme,
};

use crate::dynamic::DocumentMutationCommit;
use crate::pipeline::{DetachedLiveDocument, PipelineFrame};
use crate::{
    CancellationToken, PipelineError, PipelineStage, PresentationScene, PresentationSceneMetadata,
    RenderedPresentationPage, RenderedStaticPage, StaticPageConfig, StaticPageEngine,
};

/// Hard upper bound for one user-supplied navigation URL.
pub const MAX_NAVIGATION_URL_BYTES: usize = 16 * 1024;

const MAX_QUEUE_CAPACITY: usize = 4_096;
const MAX_CONTEXTS: usize = 1_024;
const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;
const MAX_RETAINED_DOCUMENT_NODES: usize = 64 * 1024 * 1024;
const DEFAULT_RETAINED_DOCUMENT_NODES: usize = 4 * 1024 * 1024;
const MAX_RETAINED_MUTATION_RESULT_NODES: usize = 4 * 1024 * 1024;
const DEFAULT_RETAINED_MUTATION_RESULT_NODES: usize = 64 * 1024;
const MAX_PENDING_MUTATION_PAYLOAD_BYTES: usize = 256 * 1024 * 1024;
const DEFAULT_PENDING_MUTATION_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
const RGBA8_BYTES_PER_PIXEL: usize = 4;

static NEXT_ENGINE_OWNER: AtomicU64 = AtomicU64::new(1);

/// Opaque identity for one top-level browsing context.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TopLevelContextId(NonZeroU64);

impl TopLevelContextId {
    /// Creates an identity. Zero is permanently reserved as invalid.
    #[must_use]
    pub const fn new(raw: u64) -> Option<Self> {
        match NonZeroU64::new(raw) {
            Some(raw) => Some(Self(raw)),
            None => None,
        }
    }

    /// Returns the opaque numeric representation for diagnostics or transport.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Monotonic generation within one [`TopLevelContextId`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NavigationGeneration(NonZeroU64);

impl NavigationGeneration {
    /// First accepted generation for a newly admitted context.
    pub const INITIAL: Self = Self(NonZeroU64::MIN);

    /// Creates a nonzero generation.
    #[must_use]
    pub const fn new(raw: u64) -> Option<Self> {
        match NonZeroU64::new(raw) {
            Some(raw) => Some(Self(raw)),
            None => None,
        }
    }

    /// Returns the numeric generation.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    /// Returns the next generation, or `None` after permanent exhaustion.
    #[must_use]
    pub const fn checked_next(self) -> Option<Self> {
        match self.get().checked_add(1) {
            Some(raw) => Self::new(raw),
            None => None,
        }
    }
}

/// Exact context and generation of one navigation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NavigationId {
    context: TopLevelContextId,
    generation: NavigationGeneration,
}

impl NavigationId {
    /// Creates a typed navigation identity.
    #[must_use]
    pub const fn new(context: TopLevelContextId, generation: NavigationGeneration) -> Self {
        Self {
            context,
            generation,
        }
    }

    /// Returns the top-level context.
    #[must_use]
    pub const fn context(self) -> TopLevelContextId {
        self.context
    }

    /// Returns the context-local generation.
    #[must_use]
    pub const fn generation(self) -> NavigationGeneration {
        self.generation
    }
}

/// Worker-scoped, never-reused identity for one admitted document operation.
///
/// The identity is issued only by [`NavigationEngine`] and remains bound to
/// the exact [`NavigationId`] carried by its admission receipt. It is not a
/// navigation generation and cannot be used with [`EngineCommand::Cancel`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DocumentOperationId {
    owner: NonZeroU64,
    sequence: NonZeroU64,
}

impl DocumentOperationId {
    const fn new(owner: NonZeroU64, sequence: NonZeroU64) -> Self {
        Self { owner, sequence }
    }

    /// Returns the opaque worker-local sequence for diagnostics.
    ///
    /// Equality also includes a private engine-owner incarnation, so this
    /// numeric projection is not a serializable operation identity.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.sequence.get()
    }
}

fn allocate_engine_owner() -> Option<NonZeroU64> {
    allocate_owner_from(&NEXT_ENGINE_OWNER)
}

fn allocate_owner_from(counter: &AtomicU64) -> Option<NonZeroU64> {
    let raw = counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .ok()?;
    NonZeroU64::new(raw)
}

/// Network authority explicitly attached to one navigation request.
///
/// The variants are intentionally not interchangeable: constructing a
/// general-web request never widens the legacy numeric-loopback capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NavigationNetworkCapability {
    /// Cleartext HTTP to a numeric loopback address only.
    NumericLoopback,
    /// Validated HTTP or authenticated HTTPS through the general-web client.
    GeneralWeb,
}

/// TLS version authenticated for the response which committed a navigation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NavigationTlsVersion {
    /// TLS 1.2.
    Tls12,
    /// TLS 1.3.
    Tls13,
}

/// HTTP/1.1 ALPN result authenticated for a committed TLS connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NavigationAlpn {
    /// The peer explicitly selected HTTP/1.1.
    Http11,
    /// The peer selected no ALPN protocol and HTTP/1.1 was used.
    NotNegotiated,
}

/// Connection evidence for the exact response which committed a navigation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NavigationConnectionSecurity {
    /// No authenticated transport evidence was supplied by a deterministic or
    /// transitional executor. Product general-web loads must not use this as a
    /// secure indication.
    Unverified,
    /// The final response arrived over explicit cleartext HTTP.
    Cleartext,
    /// The final response arrived over authenticated TLS.
    AuthenticatedTls {
        /// Negotiated TLS version.
        version: NavigationTlsVersion,
        /// HTTP/1.1 ALPN result.
        alpn: NavigationAlpn,
    },
}

/// Bounded final identity committed together with one successful navigation.
///
/// An authoritative general-web value can only be created from the exact
/// [`GeneralWebResponse`] which supplied the committed bytes. After parsing,
/// its response authority is bound to one exact [`DocumentVersion`]; worker
/// publication additionally binds every clone to one exact [`NavigationId`].
/// Synthetic values created by [`Self::new`] carry no response authority and
/// can never authorize subresource networking.
#[derive(Clone)]
pub struct NavigationCommitMetadata {
    final_url: Arc<str>,
    redirect_count: u8,
    security: NavigationConnectionSecurity,
    had_https_downgrade: bool,
    authority: NavigationResponseAuthority,
}

#[derive(Clone)]
enum NavigationResponseAuthority {
    None,
    FinalResponse(CommittedResponseAuthority),
    CommittedDocument(Arc<CommittedDocumentAuthority>),
}

struct CommittedDocumentAuthority {
    response: CommittedResponseAuthority,
    document_version: DocumentVersion,
    style_document: Arc<StyleDocumentLifecycle>,
}

struct StyleDocumentLifecycle {
    state: Mutex<StyleDocumentLifecycleState>,
}

struct StyleDocumentLifecycleState {
    navigation: Option<NavigationId>,
    status: StyleDocumentStatus,
}

enum StyleDocumentStatus {
    Current(StyleDocumentIssuance),
    Retired,
}

enum StyleDocumentIssuance {
    Available,
    Issued {
        class: StyleDocumentAuthorityClass,
        transaction: StyleDocumentTransaction,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StyleDocumentAuthorityClass {
    Product,
    NonProduct,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StyleDocumentTransaction {
    Ready,
    Active,
    Consumed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StyleDocumentAccessError {
    Retired,
    AlreadyIssued,
    ProductNavigationRequired,
    NonProductNavigationBound,
    TransactionActive,
    TransactionConsumed,
}

/// Unique owner proving that one response document is still current.
///
/// The owner is deliberately non-clone. Moving it transfers current-document
/// ownership; dropping it retires the lifecycle monotonically.
pub(crate) struct StyleDocumentCurrentOwner {
    lifecycle: Arc<StyleDocumentLifecycle>,
}

impl StyleDocumentCurrentOwner {
    fn new(lifecycle: Arc<StyleDocumentLifecycle>) -> Self {
        Self { lifecycle }
    }

    pub(crate) fn retire(&self) {
        self.lifecycle.retire();
    }

    pub(crate) fn retire_if_succeeded<T, E>(
        &self,
        operation: impl FnOnce() -> Result<T, E>,
    ) -> Option<Result<T, E>> {
        self.lifecycle.retire_if_succeeded(operation)
    }
}

impl Drop for StyleDocumentCurrentOwner {
    fn drop(&mut self) {
        self.lifecycle.retire();
    }
}

/// One exact issuance from a live document's shared quota ledger.
///
/// This value is private to the engine and deliberately non-clone.
pub(crate) struct StyleDocumentFetchCapability {
    lifecycle: Arc<StyleDocumentLifecycle>,
    class: StyleDocumentAuthorityClass,
}

impl StyleDocumentFetchCapability {
    pub(crate) fn begin_transaction(
        &self,
    ) -> Result<StyleDocumentTransactionGuard<'_>, StyleDocumentAccessError> {
        self.lifecycle.begin_transaction(self.class)
    }

    pub(crate) fn ensure_current(&self) -> Result<(), StyleDocumentAccessError> {
        self.lifecycle.ensure_current(self.class)
    }
}

pub(crate) struct StyleDocumentTransactionGuard<'a> {
    state: MutexGuard<'a, StyleDocumentLifecycleState>,
    class: StyleDocumentAuthorityClass,
}

impl Drop for StyleDocumentTransactionGuard<'_> {
    fn drop(&mut self) {
        let StyleDocumentStatus::Current(StyleDocumentIssuance::Issued { class, transaction }) =
            &mut self.state.status
        else {
            return;
        };
        if *class == self.class && *transaction == StyleDocumentTransaction::Active {
            *transaction = StyleDocumentTransaction::Consumed;
        }
    }
}

impl StyleDocumentLifecycle {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(StyleDocumentLifecycleState {
                navigation: None,
                status: StyleDocumentStatus::Current(StyleDocumentIssuance::Available),
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, StyleDocumentLifecycleState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn navigation(&self) -> Option<NavigationId> {
        self.lock().navigation
    }

    fn bind_navigation(&self, navigation: NavigationId) -> Result<(), StyleDocumentAccessError> {
        let mut state = self.lock();
        if matches!(state.status, StyleDocumentStatus::Retired) {
            return Err(StyleDocumentAccessError::Retired);
        }
        match state.navigation {
            Some(bound) if bound == navigation => Ok(()),
            Some(_) => Err(StyleDocumentAccessError::ProductNavigationRequired),
            None => {
                if !matches!(
                    state.status,
                    StyleDocumentStatus::Current(StyleDocumentIssuance::Available)
                ) {
                    return Err(StyleDocumentAccessError::AlreadyIssued);
                }
                state.navigation = Some(navigation);
                Ok(())
            }
        }
    }

    fn issue(
        self: &Arc<Self>,
        class: StyleDocumentAuthorityClass,
    ) -> Result<StyleDocumentFetchCapability, StyleDocumentAccessError> {
        let mut state = self.lock();
        if matches!(state.status, StyleDocumentStatus::Retired) {
            return Err(StyleDocumentAccessError::Retired);
        }
        match (class, state.navigation) {
            (StyleDocumentAuthorityClass::Product, None) => {
                return Err(StyleDocumentAccessError::ProductNavigationRequired);
            }
            (StyleDocumentAuthorityClass::NonProduct, Some(_)) => {
                return Err(StyleDocumentAccessError::NonProductNavigationBound);
            }
            (StyleDocumentAuthorityClass::Product, Some(_))
            | (StyleDocumentAuthorityClass::NonProduct, None) => {}
        }
        match &mut state.status {
            StyleDocumentStatus::Current(issuance @ StyleDocumentIssuance::Available) => {
                *issuance = StyleDocumentIssuance::Issued {
                    class,
                    transaction: StyleDocumentTransaction::Ready,
                };
            }
            StyleDocumentStatus::Current(StyleDocumentIssuance::Issued { .. }) => {
                return Err(StyleDocumentAccessError::AlreadyIssued);
            }
            StyleDocumentStatus::Retired => return Err(StyleDocumentAccessError::Retired),
        }
        drop(state);
        Ok(StyleDocumentFetchCapability {
            lifecycle: Arc::clone(self),
            class,
        })
    }

    fn ensure_current(
        &self,
        class: StyleDocumentAuthorityClass,
    ) -> Result<(), StyleDocumentAccessError> {
        let state = self.lock();
        match &state.status {
            StyleDocumentStatus::Retired => Err(StyleDocumentAccessError::Retired),
            StyleDocumentStatus::Current(StyleDocumentIssuance::Issued {
                class: issued,
                transaction: StyleDocumentTransaction::Ready,
            }) if *issued == class => Ok(()),
            StyleDocumentStatus::Current(StyleDocumentIssuance::Issued {
                class: issued,
                transaction: StyleDocumentTransaction::Active,
            }) if *issued == class => Err(StyleDocumentAccessError::TransactionActive),
            StyleDocumentStatus::Current(StyleDocumentIssuance::Issued {
                class: issued,
                transaction: StyleDocumentTransaction::Consumed,
            }) if *issued == class => Err(StyleDocumentAccessError::TransactionConsumed),
            StyleDocumentStatus::Current(_) => Err(StyleDocumentAccessError::AlreadyIssued),
        }
    }

    fn begin_transaction(
        &self,
        class: StyleDocumentAuthorityClass,
    ) -> Result<StyleDocumentTransactionGuard<'_>, StyleDocumentAccessError> {
        let mut state = self.lock();
        match &mut state.status {
            StyleDocumentStatus::Retired => Err(StyleDocumentAccessError::Retired),
            StyleDocumentStatus::Current(StyleDocumentIssuance::Issued {
                class: issued,
                transaction,
            }) if *issued == class => match transaction {
                StyleDocumentTransaction::Ready => {
                    *transaction = StyleDocumentTransaction::Active;
                    Ok(StyleDocumentTransactionGuard { state, class })
                }
                StyleDocumentTransaction::Active => {
                    Err(StyleDocumentAccessError::TransactionActive)
                }
                StyleDocumentTransaction::Consumed => {
                    Err(StyleDocumentAccessError::TransactionConsumed)
                }
            },
            StyleDocumentStatus::Current(_) => Err(StyleDocumentAccessError::AlreadyIssued),
        }
    }

    fn retire(&self) {
        let mut state = self.lock();
        state.status = StyleDocumentStatus::Retired;
    }

    fn retire_if_succeeded<T, E>(
        &self,
        operation: impl FnOnce() -> Result<T, E>,
    ) -> Option<Result<T, E>> {
        let mut state = self.lock();
        if matches!(state.status, StyleDocumentStatus::Retired) {
            return None;
        }
        let result = operation();
        if result.is_ok() {
            state.status = StyleDocumentStatus::Retired;
        }
        Some(result)
    }
}

pub(crate) struct BoundNavigationDocument {
    metadata: NavigationCommitMetadata,
    style_owner: Option<StyleDocumentCurrentOwner>,
}

impl BoundNavigationDocument {
    pub(crate) fn into_parts(
        self,
    ) -> (NavigationCommitMetadata, Option<StyleDocumentCurrentOwner>) {
        (self.metadata, self.style_owner)
    }
}

/// Failed product validation of a general-web navigation commitment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NavigationCommitValidationError {
    /// The final identity was not a credential-free HTTP(S) browser URL.
    InvalidFinalUrl,
    /// The final identity was not the exact canonical WHATWG serialization.
    NonCanonicalFinalUrl,
    /// The claimed redirect count exceeded the engine's exported policy.
    TooManyRedirects,
    /// A product general-web commit supplied no authenticated/cleartext evidence.
    UnverifiedSecurity,
    /// The final scheme contradicted the claimed connection evidence.
    SchemeSecurityMismatch,
    /// No opaque transport-authenticated final-response authority was retained.
    UnverifiedAddressSpace,
    /// The supplied URL/security did not belong to the exact final response.
    ResponseAuthorityMismatch,
    /// The final response was not yet bound to one parsed document revision.
    UnboundDocument,
    /// A response authority was paired with a different document revision.
    DocumentIdentityMismatch,
    /// A committed document was paired with a different navigation identity.
    NavigationIdentityMismatch,
}

impl fmt::Display for NavigationCommitValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid general-web navigation commitment: {self:?}"
        )
    }
}

impl std::error::Error for NavigationCommitValidationError {}

impl NavigationCommitMetadata {
    /// Creates bounded synthetic or legacy metadata without response authority.
    ///
    /// The supplied security value is observational test/transition data only.
    /// This constructor cannot create a product-valid general-web commitment
    /// or authorize subresources; only the private final-response constructor
    /// used by the pipeline can do so.
    ///
    /// # Errors
    ///
    /// Returns [`NavigationRequestError`] for an empty or oversized final URL.
    pub fn new(
        final_url: &str,
        redirect_count: u8,
        security: NavigationConnectionSecurity,
        had_https_downgrade: bool,
    ) -> Result<Self, NavigationRequestError> {
        Self::new_inner(final_url, redirect_count, security, had_https_downgrade)
    }

    pub(crate) fn from_general_web_response(
        final_url: &str,
        redirect_count: u8,
        had_https_downgrade: bool,
        response: &GeneralWebResponse,
    ) -> Result<Self, NavigationCommitValidationError> {
        if final_url.is_empty() || final_url.len() > MAX_NAVIGATION_URL_BYTES {
            return Err(NavigationCommitValidationError::InvalidFinalUrl);
        }
        let (identity, target) = GeneralWebTarget::parse_navigation(final_url)
            .map_err(|_| NavigationCommitValidationError::InvalidFinalUrl)?;
        if identity.as_str() != final_url {
            return Err(NavigationCommitValidationError::NonCanonicalFinalUrl);
        }
        if !response.response_authority().matches_target(&target) {
            return Err(NavigationCommitValidationError::ResponseAuthorityMismatch);
        }
        let security = project_response_security(target.origin().scheme(), response.security())?;
        let commitment = Self {
            final_url: Arc::from(final_url),
            redirect_count,
            security,
            had_https_downgrade,
            authority: NavigationResponseAuthority::FinalResponse(
                response.response_authority().clone(),
            ),
        };
        commitment.validate_general_web()?;
        Ok(commitment)
    }

    fn new_inner(
        final_url: &str,
        redirect_count: u8,
        security: NavigationConnectionSecurity,
        had_https_downgrade: bool,
    ) -> Result<Self, NavigationRequestError> {
        if final_url.is_empty() {
            return Err(NavigationRequestError::EmptyUrl);
        }
        if final_url.len() > MAX_NAVIGATION_URL_BYTES {
            return Err(NavigationRequestError::UrlTooLong {
                actual: final_url.len(),
                maximum: MAX_NAVIGATION_URL_BYTES,
            });
        }
        Ok(Self {
            final_url: Arc::from(final_url),
            redirect_count,
            security,
            had_https_downgrade,
            authority: NavigationResponseAuthority::None,
        })
    }

    fn unverified_requested(request: &NavigationRequest) -> Self {
        Self {
            final_url: Arc::from(request.url.as_ref()),
            redirect_count: 0,
            security: NavigationConnectionSecurity::Unverified,
            had_https_downgrade: false,
            authority: NavigationResponseAuthority::None,
        }
    }

    pub(crate) fn bind_document(
        mut self,
        document_version: DocumentVersion,
    ) -> Result<BoundNavigationDocument, NavigationCommitValidationError> {
        let mut style_owner = None;
        self.authority = match self.authority {
            NavigationResponseAuthority::None => NavigationResponseAuthority::None,
            NavigationResponseAuthority::FinalResponse(response) => {
                let style_document = StyleDocumentLifecycle::new();
                style_owner = Some(StyleDocumentCurrentOwner::new(Arc::clone(&style_document)));
                NavigationResponseAuthority::CommittedDocument(Arc::new(
                    CommittedDocumentAuthority {
                        response,
                        document_version,
                        style_document,
                    },
                ))
            }
            NavigationResponseAuthority::CommittedDocument(authority)
                if authority.document_version == document_version =>
            {
                if matches!(
                    &authority.style_document.lock().status,
                    StyleDocumentStatus::Retired
                ) {
                    return Err(NavigationCommitValidationError::UnboundDocument);
                }
                NavigationResponseAuthority::CommittedDocument(authority)
            }
            NavigationResponseAuthority::CommittedDocument(_) => {
                return Err(NavigationCommitValidationError::DocumentIdentityMismatch);
            }
        };
        Ok(BoundNavigationDocument {
            metadata: self,
            style_owner,
        })
    }

    pub(crate) fn bind_navigation(
        &self,
        navigation: NavigationId,
        document_version: DocumentVersion,
    ) -> Result<(), NavigationCommitValidationError> {
        let NavigationResponseAuthority::CommittedDocument(authority) = &self.authority else {
            return if matches!(self.authority, NavigationResponseAuthority::None) {
                Ok(())
            } else {
                Err(NavigationCommitValidationError::UnboundDocument)
            };
        };
        if authority.document_version != document_version {
            return Err(NavigationCommitValidationError::DocumentIdentityMismatch);
        }
        authority
            .style_document
            .bind_navigation(navigation)
            .map_err(|_| NavigationCommitValidationError::NavigationIdentityMismatch)
    }

    /// Exact normalized URL whose response body was committed.
    #[must_use]
    pub fn final_url(&self) -> &str {
        &self.final_url
    }

    /// Number of HTTP redirects followed before the final response.
    #[must_use]
    pub const fn redirect_count(&self) -> u8 {
        self.redirect_count
    }

    /// Transport evidence for the final response connection.
    #[must_use]
    pub const fn security(&self) -> NavigationConnectionSecurity {
        self.security
    }

    /// Whether any hop changed from authenticated HTTPS to cleartext HTTP.
    #[must_use]
    pub const fn had_https_downgrade(&self) -> bool {
        self.had_https_downgrade
    }

    /// Returns the authenticated final peer address space as observation only.
    ///
    /// This scalar cannot authorize networking; only the private bound response
    /// authority retained by this commitment can be delegated.
    #[must_use]
    pub fn address_space(&self) -> Option<IpAddressSpace> {
        self.response_authority()
            .map(CommittedResponseAuthority::address_space)
    }

    /// Exact parsed document revision bound to the final response, if any.
    #[must_use]
    pub fn document_version(&self) -> Option<DocumentVersion> {
        self.document_authority()
            .map(|authority| authority.document_version)
    }

    /// Exact navigation identity bound during worker publication, if any.
    #[must_use]
    pub fn navigation(&self) -> Option<NavigationId> {
        self.document_authority()
            .and_then(|authority| authority.style_document.navigation())
    }

    /// Validates bounded general-web URL and security structure.
    ///
    /// Browser fragments are retained in `final_url`; the network target used
    /// for validation strips only that fragment. URL spelling never creates
    /// transport evidence.
    ///
    /// # Errors
    ///
    /// This structural check deliberately does not confer response authority;
    /// callers initiating networking must use the exact-document or
    /// exact-navigation validators below.
    ///
    /// Returns [`NavigationCommitValidationError`] for an invalid,
    /// credentialed, non-HTTP(S), noncanonical, over-limit, unverified, or
    /// scheme/security-incoherent record.
    pub fn validate_general_web(&self) -> Result<(), NavigationCommitValidationError> {
        if self.redirect_count > crate::pipeline::MAX_TOP_LEVEL_REDIRECTS {
            return Err(NavigationCommitValidationError::TooManyRedirects);
        }
        let (identity, target) = GeneralWebTarget::parse_navigation(&self.final_url)
            .map_err(|_| NavigationCommitValidationError::InvalidFinalUrl)?;
        if identity.as_str() != self.final_url.as_ref() {
            return Err(NavigationCommitValidationError::NonCanonicalFinalUrl);
        }
        match (target.origin().scheme(), self.security) {
            (_, NavigationConnectionSecurity::Unverified) => {
                return Err(NavigationCommitValidationError::UnverifiedSecurity);
            }
            (WebScheme::Http, NavigationConnectionSecurity::Cleartext)
            | (WebScheme::Https, NavigationConnectionSecurity::AuthenticatedTls { .. }) => {}
            (WebScheme::Http, NavigationConnectionSecurity::AuthenticatedTls { .. })
            | (WebScheme::Https, NavigationConnectionSecurity::Cleartext) => {
                return Err(NavigationCommitValidationError::SchemeSecurityMismatch);
            }
        }
        Ok(())
    }

    /// Validates a commitment before it can initiate product subresources.
    ///
    /// In addition to URL and connection validation, this requires address
    /// space evidence issued for the exact connected final response. Legacy or
    /// synthetic commitments remain usable as non-network test metadata but
    /// cannot authorize a general-web subresource request.
    ///
    /// # Errors
    ///
    /// Returns [`NavigationCommitValidationError`] when ordinary general-web
    /// validation fails or the committed response address space is unverified.
    pub fn validate_general_web_for_subresources(
        &self,
        document_version: DocumentVersion,
    ) -> Result<(), NavigationCommitValidationError> {
        self.validate_general_web()?;
        let (_, target) = GeneralWebTarget::parse_navigation(&self.final_url)
            .map_err(|_| NavigationCommitValidationError::InvalidFinalUrl)?;
        let Some(response_authority) = self.response_authority() else {
            return Err(NavigationCommitValidationError::UnverifiedAddressSpace);
        };
        if !response_authority.matches_target(&target)
            || response_authority.security() != unproject_response_security(self.security)
        {
            return Err(NavigationCommitValidationError::ResponseAuthorityMismatch);
        }
        let Some(authority) = self.document_authority() else {
            return Err(NavigationCommitValidationError::UnboundDocument);
        };
        if authority.document_version != document_version {
            return Err(NavigationCommitValidationError::DocumentIdentityMismatch);
        }
        Ok(())
    }

    /// Validates the exact response, document revision, and worker navigation.
    ///
    /// # Errors
    ///
    /// Returns a redacted mismatch if any authority dimension differs.
    pub fn validate_general_web_for_navigation(
        &self,
        navigation: NavigationId,
        document_version: DocumentVersion,
    ) -> Result<(), NavigationCommitValidationError> {
        self.validate_general_web_for_subresources(document_version)?;
        if self.navigation() != Some(navigation) {
            return Err(NavigationCommitValidationError::NavigationIdentityMismatch);
        }
        Ok(())
    }

    pub(crate) fn committed_response_authority(
        &self,
        document_version: DocumentVersion,
    ) -> Result<&CommittedResponseAuthority, NavigationCommitValidationError> {
        self.validate_general_web_for_subresources(document_version)?;
        self.document_authority()
            .map(|authority| &authority.response)
            .ok_or(NavigationCommitValidationError::UnboundDocument)
    }

    pub(crate) fn issue_product_style_fetch(
        &self,
        navigation: NavigationId,
        document_version: DocumentVersion,
    ) -> Result<StyleDocumentFetchCapability, StyleDocumentAccessError> {
        self.validate_general_web_for_navigation(navigation, document_version)
            .map_err(|_| StyleDocumentAccessError::ProductNavigationRequired)?;
        self.document_authority()
            .ok_or(StyleDocumentAccessError::ProductNavigationRequired)?
            .style_document
            .issue(StyleDocumentAuthorityClass::Product)
    }

    pub(crate) fn issue_non_product_style_fetch(
        &self,
        document_version: DocumentVersion,
    ) -> Result<StyleDocumentFetchCapability, StyleDocumentAccessError> {
        self.validate_general_web_for_subresources(document_version)
            .map_err(|_| StyleDocumentAccessError::Retired)?;
        if self.navigation().is_some() {
            return Err(StyleDocumentAccessError::NonProductNavigationBound);
        }
        self.document_authority()
            .ok_or(StyleDocumentAccessError::Retired)?
            .style_document
            .issue(StyleDocumentAuthorityClass::NonProduct)
    }

    pub(crate) fn retire_style_document(&self) {
        if let Some(authority) = self.document_authority() {
            authority.style_document.retire();
        }
    }

    fn response_authority(&self) -> Option<&CommittedResponseAuthority> {
        match &self.authority {
            NavigationResponseAuthority::None => None,
            NavigationResponseAuthority::FinalResponse(authority) => Some(authority),
            NavigationResponseAuthority::CommittedDocument(authority) => Some(&authority.response),
        }
    }

    fn document_authority(&self) -> Option<&CommittedDocumentAuthority> {
        match &self.authority {
            NavigationResponseAuthority::CommittedDocument(authority) => Some(authority),
            NavigationResponseAuthority::None | NavigationResponseAuthority::FinalResponse(_) => {
                None
            }
        }
    }
}

impl PartialEq for NavigationCommitMetadata {
    fn eq(&self, other: &Self) -> bool {
        self.final_url == other.final_url
            && self.redirect_count == other.redirect_count
            && self.security == other.security
            && self.had_https_downgrade == other.had_https_downgrade
            && match (&self.authority, &other.authority) {
                (NavigationResponseAuthority::None, NavigationResponseAuthority::None) => true,
                (
                    NavigationResponseAuthority::FinalResponse(left),
                    NavigationResponseAuthority::FinalResponse(right),
                ) => left == right,
                (
                    NavigationResponseAuthority::CommittedDocument(left),
                    NavigationResponseAuthority::CommittedDocument(right),
                ) => Arc::ptr_eq(left, right),
                _ => false,
            }
    }
}

impl Eq for NavigationCommitMetadata {}

impl fmt::Debug for NavigationCommitMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NavigationCommitMetadata")
            .field("final_url", &self.final_url)
            .field("redirect_count", &self.redirect_count)
            .field("security", &self.security)
            .field("had_https_downgrade", &self.had_https_downgrade)
            .field("address_space", &self.address_space())
            .field("document_version", &self.document_version())
            .field("navigation", &self.navigation())
            .finish_non_exhaustive()
    }
}

fn project_response_security(
    scheme: WebScheme,
    security: ConnectionSecurity,
) -> Result<NavigationConnectionSecurity, NavigationCommitValidationError> {
    match (scheme, security) {
        (WebScheme::Http, ConnectionSecurity::Cleartext) => {
            Ok(NavigationConnectionSecurity::Cleartext)
        }
        (WebScheme::Https, ConnectionSecurity::Tls { version, alpn }) => {
            let version = match version {
                TlsVersion::Tls12 => NavigationTlsVersion::Tls12,
                TlsVersion::Tls13 => NavigationTlsVersion::Tls13,
            };
            let alpn = match alpn {
                AlpnOutcome::Http11 => NavigationAlpn::Http11,
                AlpnOutcome::NotNegotiated => NavigationAlpn::NotNegotiated,
            };
            Ok(NavigationConnectionSecurity::AuthenticatedTls { version, alpn })
        }
        (WebScheme::Http, ConnectionSecurity::Tls { .. })
        | (WebScheme::Https, ConnectionSecurity::Cleartext) => {
            Err(NavigationCommitValidationError::SchemeSecurityMismatch)
        }
    }
}

const fn unproject_response_security(security: NavigationConnectionSecurity) -> ConnectionSecurity {
    match security {
        NavigationConnectionSecurity::Unverified | NavigationConnectionSecurity::Cleartext => {
            ConnectionSecurity::Cleartext
        }
        NavigationConnectionSecurity::AuthenticatedTls { version, alpn } => {
            let version = match version {
                NavigationTlsVersion::Tls12 => TlsVersion::Tls12,
                NavigationTlsVersion::Tls13 => TlsVersion::Tls13,
            };
            let alpn = match alpn {
                NavigationAlpn::Http11 => AlpnOutcome::Http11,
                NavigationAlpn::NotNegotiated => AlpnOutcome::NotNegotiated,
            };
            ConnectionSecurity::Tls { version, alpn }
        }
    }
}

/// A bounded, owned navigation request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NavigationRequest {
    url: Box<str>,
    network_capability: NavigationNetworkCapability,
}

impl NavigationRequest {
    /// Copies a nonempty numeric-loopback URL after enforcing the hard byte
    /// bound. URL and loopback validation still occur on the engine worker.
    ///
    /// # Errors
    ///
    /// Returns [`NavigationRequestError`] for an empty or oversized URL.
    pub fn new(url: &str) -> Result<Self, NavigationRequestError> {
        Self::with_network_capability(url, NavigationNetworkCapability::NumericLoopback)
    }

    /// Copies a nonempty explicit HTTP(S) general-web URL after enforcing the
    /// hard byte bound. URL, DNS, TCP, and TLS work all remain on the engine
    /// worker rather than the caller/UI thread.
    ///
    /// # Errors
    ///
    /// Returns [`NavigationRequestError`] for an empty or oversized URL.
    pub fn general_web(url: &str) -> Result<Self, NavigationRequestError> {
        Self::with_network_capability(url, NavigationNetworkCapability::GeneralWeb)
    }

    fn with_network_capability(
        url: &str,
        network_capability: NavigationNetworkCapability,
    ) -> Result<Self, NavigationRequestError> {
        if url.is_empty() {
            return Err(NavigationRequestError::EmptyUrl);
        }
        if url.len() > MAX_NAVIGATION_URL_BYTES {
            return Err(NavigationRequestError::UrlTooLong {
                actual: url.len(),
                maximum: MAX_NAVIGATION_URL_BYTES,
            });
        }
        Ok(Self {
            url: url.into(),
            network_capability,
        })
    }

    /// Returns the requested URL without transferring ownership.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Returns the exact network authority selected by the caller.
    #[must_use]
    pub const fn network_capability(&self) -> NavigationNetworkCapability {
        self.network_capability
    }
}

/// Validation failure while constructing a bounded navigation request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NavigationRequestError {
    /// The URL is empty.
    EmptyUrl,
    /// The URL exceeds the hard byte bound.
    UrlTooLong {
        /// Supplied byte length.
        actual: usize,
        /// Accepted byte length.
        maximum: usize,
    },
}

impl fmt::Display for NavigationRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyUrl => formatter.write_str("navigation URL must not be empty"),
            Self::UrlTooLong { actual, maximum } => {
                write!(
                    formatter,
                    "navigation URL has {actual} bytes; maximum is {maximum}"
                )
            }
        }
    }
}

impl std::error::Error for NavigationRequestError {}

/// Fixed resource bounds for one navigation worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EngineLimits {
    command_capacity: NonZeroUsize,
    event_capacity: NonZeroUsize,
    max_contexts: NonZeroUsize,
    max_frame_bytes: NonZeroUsize,
    max_retained_frame_bytes: NonZeroUsize,
    max_retained_document_nodes: NonZeroUsize,
    max_retained_mutation_result_nodes: NonZeroUsize,
    max_pending_mutation_payload_bytes: NonZeroUsize,
}

impl EngineLimits {
    /// Creates checked worker, context, and retained-frame bounds.
    ///
    /// `event_capacity` must be at least three so one undrained started event
    /// cannot prevent the indivisible commit and frame-ready transaction.
    ///
    /// # Errors
    ///
    /// Returns [`EngineLimitsError`] when a value is zero, above its hard cap,
    /// or cannot retain at least one maximum-sized frame.
    pub fn new(
        command_capacity: usize,
        event_capacity: usize,
        max_contexts: usize,
        max_frame_bytes: usize,
        max_retained_frame_bytes: usize,
    ) -> Result<Self, EngineLimitsError> {
        let command_capacity =
            checked_nonzero_bounded("command_capacity", command_capacity, MAX_QUEUE_CAPACITY)?;
        let event_capacity =
            checked_nonzero_bounded("event_capacity", event_capacity, MAX_QUEUE_CAPACITY)?;
        if event_capacity.get() < 3 {
            return Err(EngineLimitsError::TooSmall {
                field: "event_capacity",
                actual: event_capacity.get(),
                minimum: 3,
            });
        }
        let max_contexts = checked_nonzero_bounded("max_contexts", max_contexts, MAX_CONTEXTS)?;
        let max_frame_bytes =
            checked_nonzero_bounded("max_frame_bytes", max_frame_bytes, MAX_FRAME_BYTES)?;
        let max_retained_frame_bytes = checked_nonzero_bounded(
            "max_retained_frame_bytes",
            max_retained_frame_bytes,
            MAX_FRAME_BYTES,
        )?;
        if max_retained_frame_bytes < max_frame_bytes {
            return Err(EngineLimitsError::TooSmall {
                field: "max_retained_frame_bytes",
                actual: max_retained_frame_bytes.get(),
                minimum: max_frame_bytes.get(),
            });
        }
        Ok(Self {
            command_capacity,
            event_capacity,
            max_contexts,
            max_frame_bytes,
            max_retained_frame_bytes,
            max_retained_document_nodes: checked_nonzero_bounded(
                "max_retained_document_nodes",
                DEFAULT_RETAINED_DOCUMENT_NODES,
                MAX_RETAINED_DOCUMENT_NODES,
            )?,
            max_retained_mutation_result_nodes: checked_nonzero_bounded(
                "max_retained_mutation_result_nodes",
                DEFAULT_RETAINED_MUTATION_RESULT_NODES,
                MAX_RETAINED_MUTATION_RESULT_NODES,
            )?,
            max_pending_mutation_payload_bytes: checked_nonzero_bounded(
                "max_pending_mutation_payload_bytes",
                DEFAULT_PENDING_MUTATION_PAYLOAD_BYTES,
                MAX_PENDING_MUTATION_PAYLOAD_BYTES,
            )?,
        })
    }

    /// Maximum queued navigations, excluding the one executing.
    #[must_use]
    pub const fn command_capacity(self) -> usize {
        self.command_capacity.get()
    }

    /// Maximum ordinary queued events. One terminal slot is reserved separately.
    #[must_use]
    pub const fn event_capacity(self) -> usize {
        self.event_capacity.get()
    }

    /// Maximum number of admitted top-level contexts.
    #[must_use]
    pub const fn max_contexts(self) -> usize {
        self.max_contexts.get()
    }

    /// Maximum bytes in one composed executor frame.
    #[must_use]
    pub const fn max_frame_bytes(self) -> usize {
        self.max_frame_bytes.get()
    }

    /// Maximum aggregate bytes retained behind all current frame leases.
    #[must_use]
    pub const fn max_retained_frame_bytes(self) -> usize {
        self.max_retained_frame_bytes.get()
    }

    /// Maximum conservative node charge across retained live documents and
    /// admitted mutation creations.
    #[must_use]
    pub const fn max_retained_document_nodes(self) -> usize {
        self.max_retained_document_nodes.get()
    }

    /// Maximum result units retained behind untaken mutation-result leases.
    /// Each lease consumes at least one unit; nonempty mappings consume one
    /// unit per node.
    #[must_use]
    pub const fn max_retained_mutation_result_nodes(self) -> usize {
        self.max_retained_mutation_result_nodes.get()
    }

    /// Maximum normalized command/string bytes retained by queued mutations.
    #[must_use]
    pub const fn max_pending_mutation_payload_bytes(self) -> usize {
        self.max_pending_mutation_payload_bytes.get()
    }

    /// Narrows or widens the aggregate retained-document node budget within
    /// the process hard cap.
    ///
    /// # Errors
    ///
    /// Returns [`EngineLimitsError`] for zero or a value above the hard cap.
    pub fn with_max_retained_document_nodes(
        mut self,
        maximum: usize,
    ) -> Result<Self, EngineLimitsError> {
        self.max_retained_document_nodes = checked_nonzero_bounded(
            "max_retained_document_nodes",
            maximum,
            MAX_RETAINED_DOCUMENT_NODES,
        )?;
        Ok(self)
    }

    /// Sets the aggregate unit budget retained behind mutation-result leases.
    ///
    /// # Errors
    ///
    /// Returns [`EngineLimitsError`] for zero or a value above the hard cap.
    pub fn with_max_retained_mutation_result_nodes(
        mut self,
        maximum: usize,
    ) -> Result<Self, EngineLimitsError> {
        self.max_retained_mutation_result_nodes = checked_nonzero_bounded(
            "max_retained_mutation_result_nodes",
            maximum,
            MAX_RETAINED_MUTATION_RESULT_NODES,
        )?;
        Ok(self)
    }

    /// Sets the aggregate normalized command/string byte budget for queued and
    /// executing mutation batches.
    ///
    /// # Errors
    ///
    /// Returns [`EngineLimitsError`] for zero or a value above the hard cap.
    pub fn with_max_pending_mutation_payload_bytes(
        mut self,
        maximum: usize,
    ) -> Result<Self, EngineLimitsError> {
        self.max_pending_mutation_payload_bytes = checked_nonzero_bounded(
            "max_pending_mutation_payload_bytes",
            maximum,
            MAX_PENDING_MUTATION_PAYLOAD_BYTES,
        )?;
        Ok(self)
    }
}

impl Default for EngineLimits {
    fn default() -> Self {
        Self::new(16, 32, 16, 128 * 1024 * 1024, 256 * 1024 * 1024)
            .expect("default engine limits are valid")
    }
}

/// Invalid engine bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineLimitsError {
    /// A required bound is zero.
    Zero { field: &'static str },
    /// A bound exceeds its hard cap.
    TooLarge {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
    /// A bound is below a required minimum.
    TooSmall {
        field: &'static str,
        actual: usize,
        minimum: usize,
    },
}

impl fmt::Display for EngineLimitsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero { field } => write!(formatter, "{field} must be nonzero"),
            Self::TooLarge {
                field,
                actual,
                maximum,
            } => {
                write!(formatter, "{field} is {actual}; maximum is {maximum}")
            }
            Self::TooSmall {
                field,
                actual,
                minimum,
            } => {
                write!(formatter, "{field} is {actual}; minimum is {minimum}")
            }
        }
    }
}

impl std::error::Error for EngineLimitsError {}

fn checked_nonzero_bounded(
    field: &'static str,
    value: usize,
    maximum: usize,
) -> Result<NonZeroUsize, EngineLimitsError> {
    let value = NonZeroUsize::new(value).ok_or(EngineLimitsError::Zero { field })?;
    if value.get() > maximum {
        return Err(EngineLimitsError::TooLarge {
            field,
            actual: value.get(),
            maximum,
        });
    }
    Ok(value)
}

/// Typed commands accepted by [`NavigationEngine::try_send`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EngineCommand {
    /// Queue a bounded navigation at an explicitly supplied generation.
    Navigate {
        /// Typed context/generation identity.
        navigation: NavigationId,
        /// Bounded URL request.
        request: NavigationRequest,
    },
    /// Request cancellation of one exact active navigation operation.
    Cancel {
        /// Exact navigation operation to cancel. This command never cancels a
        /// mutation or rerender under the same navigation.
        navigation: NavigationId,
    },
    /// Request cancellation of one exact admitted document operation.
    CancelDocumentOperation {
        /// Navigation to which the operation was bound at admission.
        navigation: NavigationId,
        /// Never-reused operation identity returned by its admission receipt.
        operation: DocumentOperationId,
    },
    /// Queue one bounded, exact-live-version DOM mutation and recomposition.
    MutateDocument {
        /// Navigation which owns the currently published live document.
        navigation: NavigationId,
        /// Engine-neutral, exact-version bounded mutation batch.
        batch: ScriptMutationBatch,
    },
    /// Queue a full recomposition of one exact live DOM revision.
    RerenderDocument {
        /// Navigation which owns the currently published live document.
        navigation: NavigationId,
        /// Exact live revision to recompute without mutation.
        expected_live_version: DocumentVersion,
    },
    /// Close a context and destroy its worker-owned live document.
    CloseContext {
        /// Exact current navigation whose context is to close.
        navigation: NavigationId,
    },
    /// Request deterministic worker shutdown.
    Shutdown,
}

/// Result of accepting a command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandReceipt {
    /// Navigation entered the bounded work queue.
    NavigationQueued(NavigationId),
    /// Navigation cancellation changed the exact live token to cancelled.
    NavigationCancellationRequested(NavigationId),
    /// Document-operation cancellation changed the exact live token.
    DocumentOperationCancellationRequested {
        /// Navigation to which the operation is bound.
        navigation: NavigationId,
        /// Exact operation whose token changed to cancelled.
        operation: DocumentOperationId,
    },
    /// Exact-version mutation entered the bounded work queue.
    DocumentMutationQueued {
        /// Owning navigation.
        navigation: NavigationId,
        /// Never-reused identity of this mutation operation.
        operation: DocumentOperationId,
        /// Exact version named by the batch.
        expected_live_version: DocumentVersion,
    },
    /// Exact-version rerender entered the bounded work queue.
    DocumentRerenderQueued {
        /// Owning navigation.
        navigation: NavigationId,
        /// Never-reused identity of this rerender operation.
        operation: DocumentOperationId,
        /// Exact unchanged live version to recompute.
        expected_live_version: DocumentVersion,
    },
    /// Context close was admitted as a priority control.
    ContextCloseRequested(NavigationId),
    /// Shutdown was requested; repeated requests are reported without side effects.
    ShutdownRequested { already_requested: bool },
}

/// Reason a command was rejected without changing navigation state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandErrorKind {
    /// The bounded worker queue is full.
    QueueFull { capacity: usize },
    /// A new context did not start at generation one.
    InitialGenerationRequired,
    /// A generation was not strictly newer than the last accepted generation.
    NonMonotonicGeneration { latest: NavigationGeneration },
    /// No greater generation can be represented.
    GenerationExhausted,
    /// The configured number of live contexts has been reached.
    ContextLimitReached { maximum: usize },
    /// This numeric context identity was already admitted or lies below a
    /// later admitted identity and can never be opened again by this worker.
    ContextIdentityRetired { latest: TopLevelContextId },
    /// The context has never been admitted.
    UnknownContext,
    /// The cancellation target is not the exact active navigation operation.
    NotCurrentNavigation,
    /// No cancellable navigation operation is active in the context.
    NoActiveNavigation,
    /// No cancellable mutation or rerender is active in the context.
    NoActiveDocumentOperation,
    /// The supplied identity is not the context's exact active document work.
    NotCurrentDocumentOperation {
        /// Exact active document operation.
        current: DocumentOperationId,
    },
    /// The operation identity is active but is bound to another navigation.
    DocumentOperationNavigationMismatch {
        /// Navigation to which the active operation is actually bound.
        current: NavigationId,
    },
    /// No further never-reused document-operation identity can be represented.
    DocumentOperationIdentityExhausted,
    /// The context already has one admitted navigation or document operation.
    ContextBusy,
    /// No successfully published live document belongs to the context.
    NoLiveDocument,
    /// The command names a generation other than the retained live document.
    DocumentNavigationMismatch { current: NavigationId },
    /// The command does not name the exact retained live revision.
    DocumentVersionMismatch { live: DocumentVersion },
    /// A mutation payload exceeds one immutable process hard cap.
    MutationPayloadLimit {
        /// Rejected resource dimension.
        kind: ScriptMutationLimitKind,
        /// Fixed process maximum.
        maximum: usize,
        /// Supplied amount.
        actual: usize,
    },
    /// Conservatively reserving created nodes would exceed retained live-state policy.
    RetainedDocumentNodeLimit { maximum: usize },
    /// Conservatively reserving the created-node result would exceed lease policy.
    MutationResultNodeLimit { maximum: usize },
    /// Queued normalized mutation command/string bytes would exceed policy.
    MutationPayloadBudget { maximum: usize },
    /// A close is already pending for this context.
    ContextClosing,
    /// The event receiver was dropped.
    EventReceiverDropped,
    /// The worker is shutting down or stopped.
    ShuttingDown,
}

/// Rejected command plus its typed reason.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandError {
    kind: CommandErrorKind,
    command: EngineCommand,
}

impl CommandError {
    /// Returns the rejection reason.
    #[must_use]
    pub const fn kind(&self) -> CommandErrorKind {
        self.kind
    }

    /// Recovers ownership of the rejected command.
    #[must_use]
    pub fn into_command(self) -> EngineCommand {
        self.command
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "engine command rejected: {:?}", self.kind)
    }
}

impl std::error::Error for CommandError {}

/// UI-neutral device-pixel dimensions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PixelSize {
    width: u32,
    height: u32,
}

impl PixelSize {
    /// Creates nonzero dimensions with a representable RGBA8 length.
    ///
    /// # Errors
    ///
    /// Returns [`EngineFrameError`] for zero dimensions or byte overflow.
    pub fn new(width: u32, height: u32) -> Result<Self, EngineFrameError> {
        let size = Self { width, height };
        size.rgba8_len()?;
        Ok(size)
    }

    /// Width in device pixels.
    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    /// Height in device pixels.
    #[must_use]
    pub const fn height(self) -> u32 {
        self.height
    }

    fn rgba8_len(self) -> Result<usize, EngineFrameError> {
        if self.width == 0 || self.height == 0 {
            return Err(EngineFrameError::InvalidSize {
                width: self.width,
                height: self.height,
            });
        }
        let pixels = usize::try_from(self.width)
            .ok()
            .and_then(|width| {
                usize::try_from(self.height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .ok_or(EngineFrameError::ByteLengthOverflow)?;
        pixels
            .checked_mul(RGBA8_BYTES_PER_PIXEL)
            .ok_or(EngineFrameError::ByteLengthOverflow)
    }
}

/// Metadata for top-left row-order RGBA8 bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rgba8Metadata {
    size: PixelSize,
    stride: usize,
    byte_len: usize,
}

impl Rgba8Metadata {
    fn checked(size: PixelSize, byte_len: usize) -> Result<Self, EngineFrameError> {
        let expected = size.rgba8_len()?;
        if byte_len != expected {
            return Err(EngineFrameError::WrongByteLength {
                actual: byte_len,
                expected,
            });
        }
        if byte_len > MAX_FRAME_BYTES {
            return Err(EngineFrameError::FrameTooLarge {
                actual: byte_len,
                maximum: MAX_FRAME_BYTES,
            });
        }
        let stride = usize::try_from(size.width)
            .ok()
            .and_then(|width| width.checked_mul(RGBA8_BYTES_PER_PIXEL))
            .ok_or(EngineFrameError::ByteLengthOverflow)?;
        Ok(Self {
            size,
            stride,
            byte_len,
        })
    }

    /// Device-pixel dimensions.
    #[must_use]
    pub const fn size(self) -> PixelSize {
        self.size
    }

    /// Bytes per row.
    #[must_use]
    pub const fn stride(self) -> usize {
        self.stride
    }

    /// Total RGBA8 bytes.
    #[must_use]
    pub const fn byte_len(self) -> usize {
        self.byte_len
    }
}

/// Fixed metadata carried by a frame-ready event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameMetadata {
    output: FrameOutputMetadata,
    document_version: Option<DocumentVersion>,
}

impl FrameMetadata {
    /// Exact output kind and its bounded logical resource charge.
    #[must_use]
    pub const fn output(self) -> FrameOutputMetadata {
        self.output
    }

    /// Composed-frame RGBA8 metadata, only for the explicit headless path.
    #[must_use]
    pub const fn rgba8(self) -> Option<Rgba8Metadata> {
        match self.output {
            FrameOutputMetadata::Rgba8(metadata) => Some(metadata),
            FrameOutputMetadata::Presentation(_) => None,
        }
    }

    /// Immutable scene metadata, only for the explicit presentation path.
    #[must_use]
    pub const fn presentation(self) -> Option<PresentationSceneMetadata> {
        match self.output {
            FrameOutputMetadata::Rgba8(_) => None,
            FrameOutputMetadata::Presentation(metadata) => Some(metadata),
        }
    }

    /// Exact document revision represented by the publication, when this is a
    /// document-backed frame. This remains available even if its one-shot
    /// pixel lease is superseded before the consumer transfers it.
    #[must_use]
    pub const fn document_version(self) -> Option<DocumentVersion> {
        self.document_version
    }

    const fn total_bytes(self) -> usize {
        match self.output {
            FrameOutputMetadata::Rgba8(metadata) => metadata.byte_len,
            FrameOutputMetadata::Presentation(metadata) => metadata.retained_charge_bytes(),
        }
    }
}

/// Fixed output metadata announced before one one-shot frame transfer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameOutputMetadata {
    /// Owned top-left row-order headless pixels.
    Rgba8(Rgba8Metadata),
    /// Renderer-neutral compiled scene plus canonical shaped text.
    Presentation(PresentationSceneMetadata),
}

enum FramePayload {
    Headless(RgbaFrame),
    Owned(Box<[u8]>),
    Presentation(Box<PresentationScene>),
}

impl FramePayload {
    fn rgba8_pixels(&self) -> Option<&[u8]> {
        match self {
            Self::Headless(frame) => Some(frame.pixels()),
            Self::Owned(pixels) => Some(pixels),
            Self::Presentation(_) => None,
        }
    }
}

/// UI-neutral executor result before generation-checked publication.
pub struct EngineFrame {
    metadata: FrameMetadata,
    payload: FramePayload,
    document_version: Option<DocumentVersion>,
}

impl fmt::Debug for EngineFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EngineFrame")
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}

impl EngineFrame {
    /// Creates one composed RGBA8 frame for an executor or deterministic fake.
    ///
    /// # Errors
    ///
    /// Returns [`EngineFrameError`] when dimensions, length, or the hard byte
    /// cap are invalid.
    pub fn from_rgba8(size: PixelSize, pixels: Vec<u8>) -> Result<Self, EngineFrameError> {
        let rgba8 = Rgba8Metadata::checked(size, pixels.len())?;
        let metadata = FrameMetadata {
            output: FrameOutputMetadata::Rgba8(rgba8),
            document_version: None,
        };
        checked_total_frame_bytes(metadata)?;
        Ok(Self {
            metadata,
            payload: FramePayload::Owned(pixels.into_boxed_slice()),
            document_version: None,
        })
    }

    /// Creates a deterministic executor frame tied to one exact document.
    /// This is an executor-test seam; the worker still validates all document
    /// transitions before publication.
    ///
    /// # Errors
    ///
    /// Returns the same bounded RGBA8 validation failures as [`Self::from_rgba8`].
    pub fn from_rgba8_for_document(
        size: PixelSize,
        pixels: Vec<u8>,
        document_version: DocumentVersion,
    ) -> Result<Self, EngineFrameError> {
        let mut frame = Self::from_rgba8(size, pixels)?;
        frame.document_version = Some(document_version);
        frame.metadata.document_version = Some(document_version);
        Ok(frame)
    }

    fn from_rendered(rendered: RenderedStaticPage) -> Result<Self, EngineFrameError> {
        let RenderedStaticPage {
            evidence, frame, ..
        } = rendered;
        Self::from_headless(frame, evidence.document_version)
    }

    fn from_headless(
        frame: RgbaFrame,
        document_version: DocumentVersion,
    ) -> Result<Self, EngineFrameError> {
        let pending_text_runs = frame.pending_text_runs();
        if pending_text_runs != 0 {
            return Err(EngineFrameError::PendingTextRuns {
                actual: pending_text_runs,
            });
        }
        if frame.document_version() != document_version {
            return Err(EngineFrameError::DocumentVersionMismatch {
                expected: document_version,
                actual: frame.document_version(),
            });
        }
        let rgba8 = metadata_from_headless(&frame)?;
        let metadata = FrameMetadata {
            output: FrameOutputMetadata::Rgba8(rgba8),
            document_version: Some(document_version),
        };
        checked_total_frame_bytes(metadata)?;
        Ok(Self {
            metadata,
            payload: FramePayload::Headless(frame),
            document_version: Some(document_version),
        })
    }

    fn from_presentation(scene: PresentationScene) -> Result<Self, EngineFrameError> {
        let metadata = scene.metadata();
        let document_version = metadata.document_version();
        let frame_metadata = FrameMetadata {
            output: FrameOutputMetadata::Presentation(metadata),
            document_version: Some(document_version),
        };
        checked_total_frame_bytes(frame_metadata)?;
        Ok(Self {
            metadata: frame_metadata,
            payload: FramePayload::Presentation(Box::new(scene)),
            document_version: Some(document_version),
        })
    }

    /// Fixed publication metadata.
    #[must_use]
    pub const fn metadata(&self) -> FrameMetadata {
        self.metadata
    }

    /// Exact composed RGBA8 bytes in top-left row order, absent for a
    /// presentation scene which has never been headlessly rasterized.
    #[must_use]
    pub fn rgba8_pixels(&self) -> Option<&[u8]> {
        self.payload.rgba8_pixels()
    }

    /// Exact DOM revision represented by this frame when supplied by the real
    /// page pipeline. Deterministic custom executors may return `None`.
    #[must_use]
    pub const fn document_version(&self) -> Option<DocumentVersion> {
        self.document_version
    }

    fn into_presentation(self) -> Result<PresentationScene, EngineFrameError> {
        match self.payload {
            FramePayload::Presentation(scene) => Ok(*scene),
            FramePayload::Headless(_) | FramePayload::Owned(_) => {
                Err(EngineFrameError::WrongOutputKind)
            }
        }
    }
}

fn metadata_from_headless(frame: &RgbaFrame) -> Result<Rgba8Metadata, EngineFrameError> {
    let size = frame.size();
    let size = PixelSize::new(size.width(), size.height())?;
    Rgba8Metadata::checked(size, frame.pixels().len())
}

fn checked_total_frame_bytes(metadata: FrameMetadata) -> Result<usize, EngineFrameError> {
    let total = metadata.total_bytes();
    if total > MAX_FRAME_BYTES {
        return Err(EngineFrameError::FrameTooLarge {
            actual: total,
            maximum: MAX_FRAME_BYTES,
        });
    }
    Ok(total)
}

/// Invalid executor frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineFrameError {
    /// Zero width or height.
    InvalidSize { width: u32, height: u32 },
    /// Checked RGBA8 length arithmetic overflowed.
    ByteLengthOverflow,
    /// Supplied pixel bytes do not match dimensions.
    WrongByteLength { actual: usize, expected: usize },
    /// Frame exceeds the absolute construction cap.
    FrameTooLarge { actual: usize, maximum: usize },
    /// A successful pipeline result still omitted finalized text.
    PendingTextRuns { actual: usize },
    /// The owned frame does not represent the executor-declared document.
    DocumentVersionMismatch {
        /// Version required by the executor result.
        expected: DocumentVersion,
        /// Version encoded by the frame.
        actual: DocumentVersion,
    },
    /// A caller attempted to consume pixels as a scene or a scene as pixels.
    WrongOutputKind,
}

impl fmt::Display for EngineFrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid engine frame: {self:?}")
    }
}

impl std::error::Error for EngineFrameError {}

/// Coarse, UI-safe stage for a navigation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NavigationStage {
    /// URL validation and the explicitly selected HTTP transport.
    Fetch,
    /// HTML parsing and immutable DOM creation.
    Document,
    /// Style computation.
    Style,
    /// Layout and text shaping.
    Layout,
    /// Scene compilation and rendering.
    Render,
    /// Executor shutdown.
    Shutdown,
}

/// Bounded failure category returned by an executor and carried in events.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionFailureKind {
    /// Cancellation was observed.
    Cancelled,
    /// A bounded deadline elapsed.
    DeadlineExceeded,
    /// Input or policy rejected the request.
    Rejected,
    /// Network transport failed.
    Network,
    /// Document processing failed.
    Document,
    /// Rendering failed.
    Rendering,
    /// A configured resource limit rejected work.
    ResourceLimit,
    /// Executor invariant failed without exposing private diagnostics.
    Internal,
}

/// Fixed-size executor failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutionFailure {
    kind: ExecutionFailureKind,
    stage: NavigationStage,
    renderer_unusable: bool,
}

impl ExecutionFailure {
    /// Creates a fixed-size failure.
    #[must_use]
    pub const fn new(kind: ExecutionFailureKind, stage: NavigationStage) -> Self {
        Self {
            kind,
            stage,
            renderer_unusable: false,
        }
    }

    /// Failure category.
    #[must_use]
    pub const fn kind(self) -> ExecutionFailureKind {
        self.kind
    }

    /// Last meaningful stage.
    #[must_use]
    pub const fn stage(self) -> NavigationStage {
        self.stage
    }

    /// Whether the executor's renderer became terminally unusable.
    #[must_use]
    pub const fn renderer_unusable(self) -> bool {
        self.renderer_unusable
    }

    fn mark_renderer_unusable(mut self) -> Self {
        self.renderer_unusable = true;
        self
    }
}

/// Stable, fixed-size reason a live-document worker operation failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentOperationFailure {
    /// No retained live document exists for the exact context.
    NoLiveDocument,
    /// The renderer is terminally unusable.
    RendererUnavailable,
    /// The caller's document identity or revision was stale.
    VersionMismatch,
    /// The bounded DOM transaction rejected without committing.
    MutationRejected,
    /// Cancellation was observed at a defined checkpoint.
    Cancelled,
    /// The operation deadline elapsed.
    DeadlineExceeded,
    /// Snapshot, style, layout, or other document processing failed.
    Document,
    /// Scene compilation, composition, or readback failed.
    Rendering,
    /// A configured worker or pipeline resource bound rejected work.
    ResourceLimit,
    /// An executor invariant failed without exposing private diagnostics.
    Internal,
}

/// Opaque proof that one executor-owned DOM snapshot exists at an exact
/// version and connected-node charge.
///
/// Safe code can construct this value only from an actual [`DocumentSnapshot`]
/// (or through the crate-private real pipeline adapter). It prevents a custom
/// executor from inventing document admission metadata independently of a DOM
/// state it owns.
#[derive(Clone, Copy, Debug)]
pub struct DocumentLoadProof {
    version: DocumentVersion,
    node_charge: usize,
}

impl DocumentLoadProof {
    /// Derives admission evidence from one immutable DOM snapshot.
    #[must_use]
    pub fn from_snapshot(snapshot: &DocumentSnapshot) -> Self {
        Self {
            version: snapshot.version(),
            node_charge: snapshot.nodes_in_document_order().len(),
        }
    }

    fn from_pipeline(version: DocumentVersion, node_charge: usize) -> Self {
        Self {
            version,
            node_charge,
        }
    }

    /// Exact document version proved by the snapshot.
    #[must_use]
    pub const fn version(&self) -> DocumentVersion {
        self.version
    }

    /// Conservative connected-node charge proved by the snapshot.
    #[must_use]
    pub const fn node_charge(&self) -> usize {
        self.node_charge
    }
}

/// Worker-private execution outcome for one exact-version mutation.
///
/// Created-node mappings cross only the executor/worker stack. Public events
/// carry a bounded one-shot lease instead of embedding this variable payload.
#[derive(Debug)]
pub enum ExecutorDocumentMutation {
    /// DOM committed and a replacement frame was produced.
    Rendered {
        previous_live_version: DocumentVersion,
        previous_frame_version: DocumentVersion,
        commit: DocumentMutationCommit,
        frame: EngineFrame,
    },
    /// No DOM mutation committed.
    Rejected {
        live_version: Option<DocumentVersion>,
        frame_version: Option<DocumentVersion>,
        failure: DocumentOperationFailure,
    },
    /// DOM committed, but no replacement frame was returned.
    CommittedWithoutFrame {
        previous_live_version: DocumentVersion,
        frame_version: DocumentVersion,
        commit: DocumentMutationCommit,
        failure: DocumentOperationFailure,
    },
    /// Hidden executor state changed but cannot be represented by a valid
    /// publishable outcome. The worker must invalidate the page and stop.
    Invalidated,
}

impl ExecutorDocumentMutation {
    fn changed_hidden_state(&self) -> bool {
        !matches!(self, Self::Rejected { .. })
    }

    fn renderer_unusable(&self) -> bool {
        matches!(
            self,
            Self::Rejected {
                failure: DocumentOperationFailure::RendererUnavailable,
                ..
            } | Self::CommittedWithoutFrame {
                failure: DocumentOperationFailure::RendererUnavailable,
                ..
            }
        )
    }
}

/// Worker-private execution outcome for one exact-version no-mutation rerender.
#[derive(Debug)]
pub enum ExecutorDocumentRerender {
    /// The unchanged live revision produced a replacement frame.
    Rendered {
        live_version: DocumentVersion,
        previous_frame_version: DocumentVersion,
        frame: EngineFrame,
    },
    /// No frame was returned and neither tracked document version advanced.
    Rejected {
        live_version: Option<DocumentVersion>,
        frame_version: Option<DocumentVersion>,
        failure: DocumentOperationFailure,
    },
    /// Hidden frame state changed but no valid frame can be published.
    Invalidated,
}

impl ExecutorDocumentRerender {
    fn changed_hidden_state(&self) -> bool {
        matches!(self, Self::Rendered { .. } | Self::Invalidated)
    }

    fn renderer_unusable(&self) -> bool {
        matches!(
            self,
            Self::Rejected {
                failure: DocumentOperationFailure::RendererUnavailable,
                ..
            }
        )
    }
}

/// Successful executor output before generation-gated publication.
#[derive(Debug)]
pub struct ExecutorOutput {
    http_status: u16,
    frame: EngineFrame,
    document_node_charge: Option<usize>,
    navigation_commit: Option<NavigationCommitMetadata>,
}

impl ExecutorOutput {
    /// Creates a success result with a 2xx HTTP status.
    ///
    /// # Errors
    ///
    /// Returns [`ExecutionFailure`] if the status is not successful.
    pub fn new(http_status: u16, frame: EngineFrame) -> Result<Self, ExecutionFailure> {
        if !(200..=299).contains(&http_status) {
            return Err(ExecutionFailure::new(
                ExecutionFailureKind::Rejected,
                NavigationStage::Fetch,
            ));
        }
        Ok(Self {
            http_status,
            frame,
            document_node_charge: None,
            navigation_commit: None,
        })
    }

    /// Creates a typed real/deterministic document load result with its
    /// conservative retained-node charge.
    ///
    /// # Errors
    ///
    /// Returns a bounded internal failure if the frame lacks an exact document
    /// version, or the usual status rejection from [`Self::new`].
    pub fn new_document(
        http_status: u16,
        frame: EngineFrame,
        proof: DocumentLoadProof,
    ) -> Result<Self, ExecutionFailure> {
        if frame.document_version() != Some(proof.version) {
            return Err(ExecutionFailure::new(
                ExecutionFailureKind::Internal,
                NavigationStage::Document,
            ));
        }
        Self::new(http_status, frame)
            .map(|output| output.with_document_node_charge(proof.node_charge))
    }

    fn with_document_node_charge(mut self, node_charge: usize) -> Self {
        self.document_node_charge = Some(node_charge);
        self
    }

    fn with_navigation_commit(mut self, commit: NavigationCommitMetadata) -> Self {
        self.navigation_commit = Some(commit);
        self
    }
}

/// Dedicated-worker execution seam used by the real static pipeline and deterministic tests.
///
/// The executor itself need not be [`Send`]: its `Send` factory crosses the
/// thread boundary, then constructs, uses, and destroys the executor on that
/// one worker. This permits thread-affine EGL and renderer state without an
/// unsafe cross-thread assertion.
pub trait NavigationExecutor: 'static {
    /// Executes one request synchronously. Implementations must observe bounded
    /// cancellation and must never publish frames themselves.
    ///
    /// # Errors
    ///
    /// Returns a fixed [`ExecutionFailure`] when execution cannot produce a
    /// publishable response and frame.
    fn execute(
        &mut self,
        navigation: NavigationId,
        request: &NavigationRequest,
        cancellation: &CancellationToken,
    ) -> Result<ExecutorOutput, ExecutionFailure>;

    /// Applies one exact-version mutation to the document retained for
    /// `navigation`. The default fail-closed implementation exposes no live
    /// document, keeping deterministic navigation-only executors source
    /// compatible.
    fn mutate_document(
        &mut self,
        _navigation: NavigationId,
        _batch: ScriptMutationBatch,
        _cancellation: &CancellationToken,
    ) -> ExecutorDocumentMutation {
        ExecutorDocumentMutation::Rejected {
            live_version: None,
            frame_version: None,
            failure: DocumentOperationFailure::NoLiveDocument,
        }
    }

    /// Recomputes one exact retained document without fetching, parsing, or
    /// mutation. Navigation-only executors fail closed by default.
    fn rerender_document(
        &mut self,
        _navigation: NavigationId,
        _expected_live_version: DocumentVersion,
        _cancellation: &CancellationToken,
    ) -> ExecutorDocumentRerender {
        ExecutorDocumentRerender::Rejected {
            live_version: None,
            frame_version: None,
            failure: DocumentOperationFailure::NoLiveDocument,
        }
    }

    /// Completes the real executor's pending old/new page transaction after
    /// the worker either published or suppressed one navigation result.
    fn acknowledge_navigation_publication(&mut self, _navigation: NavigationId, _published: bool) {}

    /// Permanently discards a page whose hidden executor state changed after
    /// its navigation was superseded. Called only on the worker owner thread.
    fn invalidate_document(&mut self, _context: TopLevelContextId) {}

    /// Releases one context's retained page on the executor owner thread.
    fn close_context(&mut self, _context: TopLevelContextId) {}

    /// Releases all executor-owned resources on the same worker thread.
    ///
    /// # Errors
    ///
    /// Returns a fixed [`ExecutionFailure`] when cleanup is incomplete.
    fn shutdown(&mut self) -> Result<(), ExecutionFailure>;
}

/// Opaque one-shot identity for a published frame.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FrameLeaseId(NonZeroU64);

impl FrameLeaseId {
    /// Returns the opaque numeric representation.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Opaque one-shot identity for one bounded created-node mapping.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MutationResultLeaseId(NonZeroU64);

impl MutationResultLeaseId {
    /// Returns the opaque numeric representation.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Monotonic event sequence assigned by the worker.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EventSequence(NonZeroU64);

impl EventSequence {
    /// Returns the sequence number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Reason the worker stopped accepting and executing commands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerStopReason {
    /// Explicit shutdown was requested.
    Requested,
    /// The only event receiver was dropped.
    EventReceiverDropped,
    /// An indivisible event publication could not fit its bounded queue.
    EventQueueSaturated,
    /// Internal event order validation rejected a transition.
    EventOrderViolation,
    /// A monotonic event or frame-lease identity was exhausted.
    IdentityExhausted,
    /// Executor code panicked; the worker caught the unwind.
    ExecutorPanicked,
    /// The renderer is terminally unusable and the executor must be replaced.
    RendererUnavailable,
    /// A custom executor returned state which violated its typed contract.
    ExecutorContractViolation,
}

/// Outcome of executor cleanup performed on its owner thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutorShutdownStatus {
    /// Executor construction did not complete, so no cleanup was possible.
    NotStarted,
    /// Executor cleanup completed.
    Clean,
    /// Executor cleanup returned a bounded error.
    Failed(ExecutionFailure),
    /// Executor cleanup panicked and was contained by the worker.
    Panicked,
}

/// Stable, repeatable result of joining the worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EngineShutdownStatus {
    reason: WorkerStopReason,
    executor: ExecutorShutdownStatus,
}

impl EngineShutdownStatus {
    /// Why command processing stopped.
    #[must_use]
    pub const fn reason(self) -> WorkerStopReason {
        self.reason
    }

    /// Same-thread executor cleanup result.
    #[must_use]
    pub const fn executor(self) -> ExecutorShutdownStatus {
        self.executor
    }
}

/// Fixed-size event payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineEventKind {
    /// The worker began executing a navigation.
    NavigationStarted { navigation: NavigationId },
    /// The synchronous pipeline committed a successful response.
    NavigationCommitted {
        navigation: NavigationId,
        http_status: u16,
    },
    /// A current frame was published behind a one-shot lease.
    FrameReady {
        navigation: NavigationId,
        lease: FrameLeaseId,
        metadata: FrameMetadata,
    },
    /// Cancellation or supersession prevented publication.
    NavigationCancelled { navigation: NavigationId },
    /// Navigation failed before publication.
    NavigationFailed {
        navigation: NavigationId,
        failure: ExecutionFailure,
    },
    /// A DOM batch committed and its exact replacement frame was published.
    DocumentMutationRendered {
        navigation: NavigationId,
        operation: DocumentOperationId,
        previous_live_version: DocumentVersion,
        previous_frame_version: DocumentVersion,
        live_version: DocumentVersion,
        result: MutationResultLeaseId,
        created_nodes: usize,
        frame: FrameLeaseId,
        metadata: FrameMetadata,
    },
    /// A DOM batch committed, but downstream work returned no replacement frame.
    DocumentMutationCommittedWithoutFrame {
        navigation: NavigationId,
        operation: DocumentOperationId,
        previous_live_version: DocumentVersion,
        live_version: DocumentVersion,
        frame_version: DocumentVersion,
        result: MutationResultLeaseId,
        created_nodes: usize,
        failure: DocumentOperationFailure,
    },
    /// A mutation committed no DOM state.
    DocumentMutationRejected {
        navigation: NavigationId,
        operation: DocumentOperationId,
        live_version: Option<DocumentVersion>,
        frame_version: Option<DocumentVersion>,
        failure: DocumentOperationFailure,
    },
    /// The unchanged exact live revision was recomputed and published.
    DocumentRerendered {
        navigation: NavigationId,
        operation: DocumentOperationId,
        live_version: DocumentVersion,
        previous_frame_version: DocumentVersion,
        frame: FrameLeaseId,
        metadata: FrameMetadata,
    },
    /// An exact rerender returned no frame and changed no DOM revision.
    DocumentRerenderRejected {
        navigation: NavigationId,
        operation: DocumentOperationId,
        live_version: Option<DocumentVersion>,
        frame_version: Option<DocumentVersion>,
        failure: DocumentOperationFailure,
    },
    /// Executor-owned state for the context was destroyed on its owner thread.
    ContextClosed { navigation: NavigationId },
    /// Reserved terminal event; it does not consume ordinary queue capacity.
    ShutdownComplete { status: EngineShutdownStatus },
}

/// Sequenced event from the dedicated worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EngineEvent {
    sequence: EventSequence,
    kind: EngineEventKind,
}

impl EngineEvent {
    /// Monotonic worker-assigned event sequence.
    #[must_use]
    pub const fn sequence(self) -> EventSequence {
        self.sequence
    }

    /// Typed event payload.
    #[must_use]
    pub const fn kind(self) -> EngineEventKind {
        self.kind
    }
}

/// Event queue state when no event was returned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventReceiveError {
    /// No event is currently queued, but the worker can still make progress.
    Empty,
    /// The worker stopped and no further events remain.
    Closed(EngineShutdownStatus),
}

/// Exact final identity transferred in response to `NavigationCommitted`.
#[derive(Debug, Eq, PartialEq)]
pub struct NavigationCommit {
    navigation: NavigationId,
    metadata: NavigationCommitMetadata,
}

impl NavigationCommit {
    /// Navigation whose successful response installed this identity.
    #[must_use]
    pub const fn navigation(&self) -> NavigationId {
        self.navigation
    }

    /// Bounded final URL, redirect, downgrade, and transport evidence.
    #[must_use]
    pub const fn metadata(&self) -> &NavigationCommitMetadata {
        &self.metadata
    }

    /// Consumes the transfer into its bounded metadata.
    #[must_use]
    pub fn into_metadata(self) -> NavigationCommitMetadata {
        self.metadata
    }
}

/// A frame transferred out of the current-lease store exactly once.
#[derive(Debug)]
pub struct FrameLease {
    navigation: NavigationId,
    lease: FrameLeaseId,
    frame: EngineFrame,
}

impl FrameLease {
    /// Navigation which produced the frame.
    #[must_use]
    pub const fn navigation(&self) -> NavigationId {
        self.navigation
    }

    /// Lease identity consumed by this transfer.
    #[must_use]
    pub const fn lease_id(&self) -> FrameLeaseId {
        self.lease
    }

    /// Fixed metadata announced in `FrameReady`.
    #[must_use]
    pub const fn metadata(&self) -> FrameMetadata {
        self.frame.metadata()
    }

    /// Composed RGBA8 bytes in top-left row order, absent for an immutable
    /// native-presentation scene.
    #[must_use]
    pub fn rgba8_pixels(&self) -> Option<&[u8]> {
        self.frame.rgba8_pixels()
    }

    /// Exact DOM revision represented by this frame when the executor supplied
    /// one. Real pipeline frames always return `Some`.
    #[must_use]
    pub const fn document_version(&self) -> Option<DocumentVersion> {
        self.frame.document_version()
    }

    /// Consumes this exact one-shot lease into its renderer-neutral page
    /// scene. Headless and deterministic RGBA8 leases fail explicitly.
    ///
    /// # Errors
    ///
    /// Returns [`EngineFrameError::WrongOutputKind`] unless this lease owns a
    /// presentation scene produced by the explicit presentation engine mode.
    pub fn into_presentation(self) -> Result<PresentationScene, EngineFrameError> {
        self.frame.into_presentation()
    }
}

/// One bounded created-node mapping transferred exactly once.
#[derive(Debug)]
pub struct MutationResultLease {
    navigation: NavigationId,
    operation: DocumentOperationId,
    live_version: DocumentVersion,
    lease: MutationResultLeaseId,
    created_nodes: Box<[NodeId]>,
}

impl MutationResultLease {
    /// Navigation which owns the committed mutation.
    #[must_use]
    pub const fn navigation(&self) -> NavigationId {
        self.navigation
    }

    /// Exact mutation operation which produced this mapping.
    #[must_use]
    pub const fn operation(&self) -> DocumentOperationId {
        self.operation
    }

    /// Newly committed live document version.
    #[must_use]
    pub const fn live_version(&self) -> DocumentVersion {
        self.live_version
    }

    /// One-shot lease consumed by this transfer.
    #[must_use]
    pub const fn lease_id(&self) -> MutationResultLeaseId {
        self.lease
    }

    /// Dense created-node mapping in token-index order.
    #[must_use]
    pub fn created_nodes(&self) -> &[NodeId] {
        &self.created_nodes
    }

    /// Resolves one dense batch-local created token.
    #[must_use]
    pub fn created_node(&self, token: CreatedNodeToken) -> Option<NodeId> {
        self.created_nodes.get(token.index() as usize).copied()
    }
}

/// Failed one-shot lease transfer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameLeaseError {
    /// A lower, already-replaced or already-consumed lease cannot affect the current frame.
    Stale,
    /// The lease has never been issued by this worker.
    Unknown,
    /// The event receiver has already been detached.
    ReceiverClosed,
}

impl fmt::Display for FrameLeaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "frame lease unavailable: {self:?}")
    }
}

impl std::error::Error for FrameLeaseError {}

/// Failed exact-navigation commitment transfer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NavigationCommitError {
    /// The exact or an older navigation commitment was replaced or consumed.
    Stale,
    /// No commitment has ever been published for that navigation identity.
    Unknown,
    /// The event receiver has already been detached.
    ReceiverClosed,
}

impl fmt::Display for NavigationCommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "navigation commitment unavailable: {self:?}")
    }
}

impl std::error::Error for NavigationCommitError {}

/// Failed mutation-result lease transfer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationResultLeaseError {
    /// A lower, replaced, or already-consumed lease is stale.
    Stale,
    /// The lease has never been issued by this worker.
    Unknown,
    /// The event receiver has already been detached.
    ReceiverClosed,
}

impl fmt::Display for MutationResultLeaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "mutation-result lease unavailable: {self:?}")
    }
}

impl std::error::Error for MutationResultLeaseError {}

/// Startup failure before a usable worker/receiver pair exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineStartError {
    /// No never-reused process-local worker owner can be represented.
    IdentityExhausted,
    /// The operating system rejected thread creation.
    ThreadSpawn,
    /// Executor construction returned a bounded failure.
    Executor(ExecutionFailure),
    /// Executor construction panicked and was contained.
    ExecutorPanicked,
    /// Worker initialization ended without a status message.
    WorkerExited,
}

impl fmt::Display for EngineStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "navigation worker failed to start: {self:?}")
    }
}

impl std::error::Error for EngineStartError {}

struct StoredFrame {
    lease: FrameLeaseId,
    navigation: NavigationId,
    frame: EngineFrame,
}

struct StoredMutationResult {
    lease: MutationResultLeaseId,
    navigation: NavigationId,
    operation: DocumentOperationId,
    live_version: DocumentVersion,
    created_nodes: Box<[NodeId]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RetainedDocumentState {
    navigation: NavigationId,
    live_version: DocumentVersion,
    frame_version: DocumentVersion,
    node_charge: usize,
}

enum ActiveCancellation {
    Navigation {
        navigation: NavigationId,
        source: crate::CancellationSource,
    },
    Document {
        navigation: NavigationId,
        operation: DocumentOperationId,
        source: crate::CancellationSource,
    },
}

impl ActiveCancellation {
    fn cancel(&self) -> bool {
        match self {
            Self::Navigation { source, .. } | Self::Document { source, .. } => source.cancel(),
        }
    }

    fn is_navigation(&self, navigation: NavigationId) -> bool {
        matches!(
            self,
            Self::Navigation {
                navigation: active,
                ..
            } if *active == navigation
        )
    }

    fn is_document(&self, navigation: NavigationId, operation: DocumentOperationId) -> bool {
        matches!(
            self,
            Self::Document {
                navigation: active_navigation,
                operation: active_operation,
                ..
            } if *active_navigation == navigation && *active_operation == operation
        )
    }
}

struct ContextState {
    latest_generation: NavigationGeneration,
    active_cancellation: Option<ActiveCancellation>,
    current_frame: Option<StoredFrame>,
    document: Option<RetainedDocumentState>,
}

struct NavigationWork {
    navigation: NavigationId,
    request: NavigationRequest,
    cancellation: CancellationToken,
}

struct DocumentMutationWork {
    navigation: NavigationId,
    operation: DocumentOperationId,
    batch: ScriptMutationBatch,
    cancellation: CancellationToken,
    reserved_created_nodes: usize,
    reserved_result_units: usize,
    reserved_payload_bytes: usize,
}

struct DocumentRerenderWork {
    navigation: NavigationId,
    operation: DocumentOperationId,
    expected_live_version: DocumentVersion,
    cancellation: CancellationToken,
}

enum EngineWork {
    Navigate(NavigationWork),
    Mutate(DocumentMutationWork),
    Rerender(DocumentRerenderWork),
}

impl EngineWork {
    const fn context(&self) -> TopLevelContextId {
        match self {
            Self::Navigate(work) => work.navigation.context(),
            Self::Mutate(work) => work.navigation.context(),
            Self::Rerender(work) => work.navigation.context(),
        }
    }

    const fn reserved_created_nodes(&self) -> usize {
        match self {
            Self::Mutate(work) => work.reserved_created_nodes,
            Self::Navigate(_) | Self::Rerender(_) => 0,
        }
    }

    const fn reserved_result_units(&self) -> usize {
        match self {
            Self::Mutate(work) => work.reserved_result_units,
            Self::Navigate(_) | Self::Rerender(_) => 0,
        }
    }

    const fn reserved_payload_bytes(&self) -> usize {
        match self {
            Self::Mutate(work) => work.reserved_payload_bytes,
            Self::Navigate(_) | Self::Rerender(_) => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lifecycle {
    Running,
    Stopping(WorkerStopReason),
    Stopped(EngineShutdownStatus),
}

struct SharedState {
    lifecycle: Lifecycle,
    receiver_open: bool,
    commands: VecDeque<EngineWork>,
    context_closures: VecDeque<NavigationId>,
    closing_contexts: BTreeSet<TopLevelContextId>,
    events: VecDeque<EngineEvent>,
    terminal_event: Option<EngineEvent>,
    contexts: BTreeMap<TopLevelContextId, ContextState>,
    latest_new_context: Option<TopLevelContextId>,
    navigation_commits: BTreeMap<NavigationId, NavigationCommitMetadata>,
    current_style_documents: BTreeMap<TopLevelContextId, NavigationCommitMetadata>,
    mutation_results: BTreeMap<MutationResultLeaseId, StoredMutationResult>,
    retained_frame_bytes: usize,
    retained_document_nodes: usize,
    pending_document_nodes: usize,
    retained_mutation_result_nodes: usize,
    pending_mutation_result_nodes: usize,
    pending_mutation_payload_bytes: usize,
    next_event_sequence: u64,
    next_frame_lease: u64,
    next_mutation_result_lease: u64,
    document_operation_owner: NonZeroU64,
    next_document_operation_sequence: u64,
}

struct Shared {
    limits: EngineLimits,
    state: Mutex<SharedState>,
    command_ready: Condvar,
    event_ready: Condvar,
}

impl Shared {
    fn new(limits: EngineLimits, document_operation_owner: NonZeroU64) -> Self {
        Self {
            limits,
            state: Mutex::new(SharedState {
                lifecycle: Lifecycle::Running,
                receiver_open: true,
                commands: VecDeque::with_capacity(limits.command_capacity()),
                context_closures: VecDeque::new(),
                closing_contexts: BTreeSet::new(),
                events: VecDeque::with_capacity(limits.event_capacity()),
                terminal_event: None,
                contexts: BTreeMap::new(),
                latest_new_context: None,
                navigation_commits: BTreeMap::new(),
                current_style_documents: BTreeMap::new(),
                mutation_results: BTreeMap::new(),
                retained_frame_bytes: 0,
                retained_document_nodes: 0,
                pending_document_nodes: 0,
                retained_mutation_result_nodes: 0,
                pending_mutation_result_nodes: 0,
                pending_mutation_payload_bytes: 0,
                next_event_sequence: 1,
                next_frame_lease: 1,
                next_mutation_result_lease: 1,
                document_operation_owner,
                next_document_operation_sequence: 1,
            }),
            command_ready: Condvar::new(),
            event_ready: Condvar::new(),
        }
    }
}

/// Controller for one dedicated, bounded navigation worker.
pub struct NavigationEngine {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    joined_status: Option<EngineShutdownStatus>,
}

impl NavigationEngine {
    /// Spawns the real static pipeline, constructing and owning it entirely on
    /// the dedicated worker thread.
    ///
    /// # Errors
    ///
    /// Returns [`EngineStartError`] if thread or pipeline initialization fails.
    pub fn spawn(
        config: StaticPageConfig,
        limits: EngineLimits,
    ) -> Result<(Self, EngineEventReceiver), EngineStartError> {
        Self::spawn_with_executor(limits, move || {
            StaticPipelineExecutor::new(config, limits, StaticPipelineOutput::Headless)
        })
    }

    /// Spawns the real headless pipeline with the distinct DNS/authenticated
    /// HTTPS general-web capability.
    ///
    /// Only [`NavigationRequest::general_web`] requests are admitted by this
    /// executor. The legacy constructor continues to accept only the numeric
    /// loopback capability.
    ///
    /// # Errors
    ///
    /// Returns [`EngineStartError`] if thread, DNS/TLS policy, or pipeline
    /// initialization fails.
    pub fn spawn_general_web(
        config: StaticPageConfig,
        general_web: GeneralWebConfig,
        trust_store: TrustStore,
        limits: EngineLimits,
    ) -> Result<(Self, EngineEventReceiver), EngineStartError> {
        Self::spawn_with_executor(limits, move || {
            StaticPipelineExecutor::new_general_web(
                config,
                general_web,
                trust_store,
                limits,
                StaticPipelineOutput::Headless,
            )
        })
    }

    /// Spawns the real pipeline in its explicit scene-presentation mode.
    ///
    /// Each successful frame lease owns the exact `CompiledScene` and
    /// canonical shaped-text inventory. This mode constructs no headless
    /// renderer and produces no RGBA8 pixel buffer.
    ///
    /// # Errors
    ///
    /// Returns [`EngineStartError`] if thread or pipeline initialization fails.
    pub fn spawn_for_presentation(
        config: StaticPageConfig,
        limits: EngineLimits,
    ) -> Result<(Self, EngineEventReceiver), EngineStartError> {
        Self::spawn_with_executor(limits, move || {
            StaticPipelineExecutor::new(config, limits, StaticPipelineOutput::Presentation)
        })
    }

    /// Spawns the presentation-only pipeline with the distinct
    /// DNS/authenticated HTTPS general-web capability.
    ///
    /// # Errors
    ///
    /// Returns [`EngineStartError`] if thread, DNS/TLS policy, or pipeline
    /// initialization fails.
    pub fn spawn_general_web_for_presentation(
        config: StaticPageConfig,
        general_web: GeneralWebConfig,
        trust_store: TrustStore,
        limits: EngineLimits,
    ) -> Result<(Self, EngineEventReceiver), EngineStartError> {
        Self::spawn_with_executor(limits, move || {
            StaticPipelineExecutor::new_general_web(
                config,
                general_web,
                trust_store,
                limits,
                StaticPipelineOutput::Presentation,
            )
        })
    }

    /// Spawns a worker around a bounded executor factory. This seam exists for
    /// deterministic contract tests and future engine adapters; the factory,
    /// executor, and executor shutdown all run on the worker thread.
    ///
    /// # Errors
    ///
    /// Returns [`EngineStartError`] if thread or executor initialization fails.
    pub fn spawn_with_executor<E, F>(
        limits: EngineLimits,
        factory: F,
    ) -> Result<(Self, EngineEventReceiver), EngineStartError>
    where
        E: NavigationExecutor,
        F: FnOnce() -> Result<E, ExecutionFailure> + Send + 'static,
    {
        let owner = allocate_engine_owner().ok_or(EngineStartError::IdentityExhausted)?;
        let shared = Arc::new(Shared::new(limits, owner));
        let worker_shared = Arc::clone(&shared);
        let (init_sender, init_receiver) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("wild-buzzard-engine".into())
            .spawn(move || worker_main(&worker_shared, factory, &init_sender))
            .map_err(|_| EngineStartError::ThreadSpawn)?;

        match init_receiver.recv() {
            Ok(WorkerInit::Ready) => Ok((
                Self {
                    shared: Arc::clone(&shared),
                    worker: Some(worker),
                    joined_status: None,
                },
                EngineEventReceiver {
                    shared,
                    attached: true,
                },
            )),
            Ok(WorkerInit::Failed(error)) => {
                let _ = worker.join();
                Err(EngineStartError::Executor(error))
            }
            Ok(WorkerInit::Panicked) => {
                let _ = worker.join();
                Err(EngineStartError::ExecutorPanicked)
            }
            Err(_) => {
                let _ = worker.join();
                Err(EngineStartError::WorkerExited)
            }
        }
    }

    /// Sends a typed command without blocking.
    ///
    /// Navigate admission is transactional: failure does not create a context,
    /// advance its generation, cancel prior work, or consume queue capacity.
    /// Cancellation and shutdown are priority controls and therefore remain
    /// available when the navigation queue is saturated.
    ///
    /// # Errors
    ///
    /// Returns [`CommandError`] with ownership of the rejected command.
    pub fn try_send(&self, command: EngineCommand) -> Result<CommandReceipt, CommandError> {
        let result = match &command {
            EngineCommand::Navigate {
                navigation,
                request,
            } => self.try_queue_navigation(*navigation, request.clone()),
            EngineCommand::Cancel { navigation } => self.try_cancel_navigation(*navigation),
            EngineCommand::CancelDocumentOperation {
                navigation,
                operation,
            } => self.try_cancel_document_operation(*navigation, *operation),
            EngineCommand::MutateDocument { navigation, batch } => {
                self.try_queue_document_mutation(*navigation, batch.clone())
            }
            EngineCommand::RerenderDocument {
                navigation,
                expected_live_version,
            } => self.try_queue_document_rerender(*navigation, *expected_live_version),
            EngineCommand::CloseContext { navigation } => self.try_close_context(*navigation),
            EngineCommand::Shutdown => Ok(self.request_shutdown()),
        };
        result.map_err(|kind| CommandError { kind, command })
    }

    /// Allocates and queues the exact next generation for a context.
    /// New context identities must increase monotonically and are never reused
    /// for the lifetime of this worker; existing contexts retain their own
    /// monotonic navigation-generation sequence.
    ///
    /// # Errors
    ///
    /// Returns the same transactional errors as [`Self::try_send`].
    pub fn navigate(
        &self,
        context: TopLevelContextId,
        request: NavigationRequest,
    ) -> Result<NavigationId, CommandError> {
        let mut state = lock_unpoisoned(&self.shared.state);
        if state.closing_contexts.contains(&context) {
            return Err(CommandError {
                kind: CommandErrorKind::ContextClosing,
                command: EngineCommand::Navigate {
                    navigation: NavigationId::new(context, NavigationGeneration::INITIAL),
                    request,
                },
            });
        }
        let generation = match state.contexts.get(&context) {
            Some(context_state) => match context_state.latest_generation.checked_next() {
                Some(generation) => generation,
                None => {
                    return Err(CommandError {
                        kind: CommandErrorKind::GenerationExhausted,
                        command: EngineCommand::Navigate {
                            navigation: NavigationId::new(context, context_state.latest_generation),
                            request,
                        },
                    });
                }
            },
            None => NavigationGeneration::INITIAL,
        };
        let navigation = NavigationId::new(context, generation);
        if let Err(kind) =
            queue_navigation_locked(&mut state, self.shared.limits, navigation, request.clone())
        {
            return Err(CommandError {
                kind,
                command: EngineCommand::Navigate {
                    navigation,
                    request,
                },
            });
        }
        drop(state);
        self.shared.command_ready.notify_one();
        Ok(navigation)
    }

    /// Last accepted generation for a context.
    #[must_use]
    pub fn latest_generation(&self, context: TopLevelContextId) -> Option<NavigationGeneration> {
        lock_unpoisoned(&self.shared.state)
            .contexts
            .get(&context)
            .map(|context| context.latest_generation)
    }

    /// Cancels one exact active navigation operation. Mutations and rerenders
    /// require [`Self::cancel_document_operation`] even when they belong to the
    /// same navigation.
    ///
    /// # Errors
    ///
    /// Returns a transactional [`CommandError`] when the navigation is not the
    /// exact active navigation operation or controls are no longer accepted.
    pub fn cancel_navigation(
        &self,
        navigation: NavigationId,
    ) -> Result<CommandReceipt, CommandError> {
        self.try_send(EngineCommand::Cancel { navigation })
    }

    /// Cancels one exact admitted mutation or rerender operation.
    ///
    /// # Errors
    ///
    /// Returns a transactional [`CommandError`] for a completed, stale,
    /// foreign-owner, wrong-navigation, or otherwise inactive operation.
    pub fn cancel_document_operation(
        &self,
        navigation: NavigationId,
        operation: DocumentOperationId,
    ) -> Result<CommandReceipt, CommandError> {
        self.try_send(EngineCommand::CancelDocumentOperation {
            navigation,
            operation,
        })
    }

    /// Queues one bounded mutation for the exact retained live document.
    ///
    /// # Errors
    ///
    /// Returns a transactional [`CommandError`] without changing queue,
    /// cancellation, document, or reservation state.
    pub fn mutate_document(
        &self,
        navigation: NavigationId,
        batch: ScriptMutationBatch,
    ) -> Result<CommandReceipt, CommandError> {
        self.try_send(EngineCommand::MutateDocument { navigation, batch })
    }

    /// Queues a no-fetch, no-parse, no-mutation rerender of one exact revision.
    ///
    /// # Errors
    ///
    /// Returns a transactional [`CommandError`] when context, generation,
    /// version, queue, or lifecycle admission fails.
    pub fn rerender_document(
        &self,
        navigation: NavigationId,
        expected_live_version: DocumentVersion,
    ) -> Result<CommandReceipt, CommandError> {
        self.try_send(EngineCommand::RerenderDocument {
            navigation,
            expected_live_version,
        })
    }

    /// Closes one context as a priority control and schedules same-thread page
    /// destruction even when the ordinary work queue is saturated.
    ///
    /// # Errors
    ///
    /// Returns a transactional [`CommandError`] for an unknown/already-closing
    /// context, a stale navigation generation, or a worker which no longer
    /// accepts control commands.
    pub fn close_context(&self, navigation: NavigationId) -> Result<CommandReceipt, CommandError> {
        self.try_send(EngineCommand::CloseContext { navigation })
    }

    /// Requests shutdown, joins the worker exactly once, and returns a stable
    /// status. Repeated calls return the same status. There is no join
    /// deadline; an executor which ignores cancellation can block this call
    /// indefinitely.
    #[must_use]
    pub fn shutdown(&mut self) -> EngineShutdownStatus {
        if let Some(status) = self.joined_status {
            return status;
        }
        let _ = self.request_shutdown();
        let join_result = self.worker.take().map(JoinHandle::join);
        if matches!(join_result, Some(Err(_))) {
            force_worker_stopped(
                &self.shared,
                EngineShutdownStatus {
                    reason: WorkerStopReason::ExecutorPanicked,
                    executor: ExecutorShutdownStatus::Panicked,
                },
            );
        }
        let status = match lock_unpoisoned(&self.shared.state).lifecycle {
            Lifecycle::Stopped(status) => status,
            Lifecycle::Running | Lifecycle::Stopping(_) => EngineShutdownStatus {
                reason: WorkerStopReason::ExecutorPanicked,
                executor: ExecutorShutdownStatus::Panicked,
            },
        };
        self.joined_status = Some(status);
        status
    }

    /// Requests one-way worker shutdown without joining the worker thread.
    ///
    /// This split phase lets an owner release receiver-held event, frame, and
    /// mutation-result resources before entering the potentially blocking
    /// join in [`Self::shutdown`]. Repeated requests preserve the first stop
    /// reason and report whether shutdown had already been requested.
    #[must_use]
    pub fn request_shutdown(&self) -> CommandReceipt {
        let mut state = lock_unpoisoned(&self.shared.state);
        let already_requested = !matches!(state.lifecycle, Lifecycle::Running);
        request_stop_locked(&mut state, WorkerStopReason::Requested);
        drop(state);
        self.shared.command_ready.notify_all();
        CommandReceipt::ShutdownRequested { already_requested }
    }

    fn try_queue_navigation(
        &self,
        navigation: NavigationId,
        request: NavigationRequest,
    ) -> Result<CommandReceipt, CommandErrorKind> {
        let mut state = lock_unpoisoned(&self.shared.state);
        let receipt = queue_navigation_locked(&mut state, self.shared.limits, navigation, request)?;
        drop(state);
        self.shared.command_ready.notify_one();
        Ok(receipt)
    }

    fn try_cancel_navigation(
        &self,
        navigation: NavigationId,
    ) -> Result<CommandReceipt, CommandErrorKind> {
        let state = lock_unpoisoned(&self.shared.state);
        ensure_accepting(&state)?;
        let context = state
            .contexts
            .get(&navigation.context())
            .ok_or(CommandErrorKind::UnknownContext)?;
        match context.active_cancellation.as_ref() {
            Some(ActiveCancellation::Navigation {
                navigation: active,
                source,
            }) if *active == navigation => {
                if !source.cancel() {
                    return Err(CommandErrorKind::NoActiveNavigation);
                }
            }
            Some(ActiveCancellation::Navigation { .. }) => {
                return Err(CommandErrorKind::NotCurrentNavigation);
            }
            Some(ActiveCancellation::Document { .. }) | None => {
                if context.latest_generation != navigation.generation() {
                    return Err(CommandErrorKind::NotCurrentNavigation);
                }
                return Err(CommandErrorKind::NoActiveNavigation);
            }
        }
        Ok(CommandReceipt::NavigationCancellationRequested(navigation))
    }

    fn try_cancel_document_operation(
        &self,
        navigation: NavigationId,
        operation: DocumentOperationId,
    ) -> Result<CommandReceipt, CommandErrorKind> {
        let state = lock_unpoisoned(&self.shared.state);
        ensure_accepting(&state)?;
        let context = state
            .contexts
            .get(&navigation.context())
            .ok_or(CommandErrorKind::UnknownContext)?;
        match context.active_cancellation.as_ref() {
            Some(ActiveCancellation::Document {
                navigation: active_navigation,
                operation: active_operation,
                source,
            }) => {
                if *active_operation != operation {
                    return Err(CommandErrorKind::NotCurrentDocumentOperation {
                        current: *active_operation,
                    });
                }
                if *active_navigation != navigation {
                    return Err(CommandErrorKind::DocumentOperationNavigationMismatch {
                        current: *active_navigation,
                    });
                }
                if !source.cancel() {
                    return Err(CommandErrorKind::NoActiveDocumentOperation);
                }
            }
            Some(ActiveCancellation::Navigation { .. }) | None => {
                if context.latest_generation != navigation.generation() {
                    return Err(CommandErrorKind::NotCurrentNavigation);
                }
                return Err(CommandErrorKind::NoActiveDocumentOperation);
            }
        }
        Ok(CommandReceipt::DocumentOperationCancellationRequested {
            navigation,
            operation,
        })
    }

    fn try_queue_document_mutation(
        &self,
        navigation: NavigationId,
        batch: ScriptMutationBatch,
    ) -> Result<CommandReceipt, CommandErrorKind> {
        let MutationPayloadReservation {
            created_nodes: reserved_created_nodes,
            payload_bytes: reserved_payload_bytes,
        } = validate_mutation_payload(&batch)?;
        let reserved_result_units = reserved_created_nodes.max(1);
        let expected_live_version = batch.expected_version();
        let mut state = lock_unpoisoned(&self.shared.state);
        ensure_document_admission(
            &state,
            self.shared.limits,
            navigation,
            expected_live_version,
        )?;
        let retained_and_pending = state
            .retained_document_nodes
            .checked_add(state.pending_document_nodes)
            .and_then(|value| value.checked_add(reserved_created_nodes))
            .ok_or(CommandErrorKind::RetainedDocumentNodeLimit {
                maximum: self.shared.limits.max_retained_document_nodes(),
            })?;
        if retained_and_pending > self.shared.limits.max_retained_document_nodes() {
            return Err(CommandErrorKind::RetainedDocumentNodeLimit {
                maximum: self.shared.limits.max_retained_document_nodes(),
            });
        }
        let result_nodes = state
            .retained_mutation_result_nodes
            .checked_add(state.pending_mutation_result_nodes)
            .and_then(|value| value.checked_add(reserved_result_units))
            .ok_or(CommandErrorKind::MutationResultNodeLimit {
                maximum: self.shared.limits.max_retained_mutation_result_nodes(),
            })?;
        if result_nodes > self.shared.limits.max_retained_mutation_result_nodes() {
            return Err(CommandErrorKind::MutationResultNodeLimit {
                maximum: self.shared.limits.max_retained_mutation_result_nodes(),
            });
        }
        let payload_bytes = state
            .pending_mutation_payload_bytes
            .checked_add(reserved_payload_bytes)
            .ok_or(CommandErrorKind::MutationPayloadBudget {
                maximum: self.shared.limits.max_pending_mutation_payload_bytes(),
            })?;
        if payload_bytes > self.shared.limits.max_pending_mutation_payload_bytes() {
            return Err(CommandErrorKind::MutationPayloadBudget {
                maximum: self.shared.limits.max_pending_mutation_payload_bytes(),
            });
        }

        let operation = reserve_document_operation_id(&mut state)?;
        let cancellation = crate::CancellationSource::new();
        state
            .commands
            .push_back(EngineWork::Mutate(DocumentMutationWork {
                navigation,
                operation,
                batch,
                cancellation: cancellation.token(),
                reserved_created_nodes,
                reserved_result_units,
                reserved_payload_bytes,
            }));
        state.pending_document_nodes = state
            .pending_document_nodes
            .checked_add(reserved_created_nodes)
            .expect("admission proved the retained-document reservation");
        state.pending_mutation_result_nodes = state
            .pending_mutation_result_nodes
            .checked_add(reserved_result_units)
            .expect("admission proved the mutation-result reservation");
        state.pending_mutation_payload_bytes = payload_bytes;
        state
            .contexts
            .get_mut(&navigation.context())
            .expect("document admission proved the context")
            .active_cancellation = Some(ActiveCancellation::Document {
            navigation,
            operation,
            source: cancellation,
        });
        drop(state);
        self.shared.command_ready.notify_one();
        Ok(CommandReceipt::DocumentMutationQueued {
            navigation,
            operation,
            expected_live_version,
        })
    }

    fn try_queue_document_rerender(
        &self,
        navigation: NavigationId,
        expected_live_version: DocumentVersion,
    ) -> Result<CommandReceipt, CommandErrorKind> {
        let mut state = lock_unpoisoned(&self.shared.state);
        ensure_document_admission(
            &state,
            self.shared.limits,
            navigation,
            expected_live_version,
        )?;
        let operation = reserve_document_operation_id(&mut state)?;
        let cancellation = crate::CancellationSource::new();
        state
            .commands
            .push_back(EngineWork::Rerender(DocumentRerenderWork {
                navigation,
                operation,
                expected_live_version,
                cancellation: cancellation.token(),
            }));
        state
            .contexts
            .get_mut(&navigation.context())
            .expect("document admission proved the context")
            .active_cancellation = Some(ActiveCancellation::Document {
            navigation,
            operation,
            source: cancellation,
        });
        drop(state);
        self.shared.command_ready.notify_one();
        Ok(CommandReceipt::DocumentRerenderQueued {
            navigation,
            operation,
            expected_live_version,
        })
    }

    fn try_close_context(
        &self,
        navigation: NavigationId,
    ) -> Result<CommandReceipt, CommandErrorKind> {
        let mut state = lock_unpoisoned(&self.shared.state);
        ensure_accepting(&state)?;
        let context_id = navigation.context();
        if state.closing_contexts.contains(&context_id) {
            return Err(CommandErrorKind::ContextClosing);
        }
        let latest = state
            .contexts
            .get(&context_id)
            .ok_or(CommandErrorKind::UnknownContext)?
            .latest_generation;
        if latest != navigation.generation() {
            return Err(CommandErrorKind::NotCurrentNavigation);
        }
        retire_current_style_document(&mut state, context_id);
        let mut context = state
            .contexts
            .remove(&context_id)
            .ok_or(CommandErrorKind::UnknownContext)?;
        if let Some(active) = context.active_cancellation.take() {
            active.cancel();
        }
        if let Some(frame) = context.current_frame.take() {
            state.retained_frame_bytes = state
                .retained_frame_bytes
                .checked_sub(frame.frame.metadata().total_bytes())
                .expect("stored frame bytes are included in the aggregate");
        }
        if let Some(document) = context.document.take() {
            state.retained_document_nodes = state
                .retained_document_nodes
                .checked_sub(document.node_charge)
                .expect("stored document charge is included in the aggregate");
        }

        let mut removed_created_nodes = 0usize;
        let mut removed_result_units = 0usize;
        let mut removed_payload_bytes = 0usize;
        state.commands.retain(|work| {
            if work.context() == context_id {
                removed_created_nodes = removed_created_nodes
                    .checked_add(work.reserved_created_nodes())
                    .expect("queued reservation total is representable");
                removed_result_units = removed_result_units
                    .checked_add(work.reserved_result_units())
                    .expect("queued result reservation total is representable");
                removed_payload_bytes = removed_payload_bytes
                    .checked_add(work.reserved_payload_bytes())
                    .expect("queued payload reservation total is representable");
                false
            } else {
                true
            }
        });
        release_pending_mutation_reservations(
            &mut state,
            removed_created_nodes,
            removed_result_units,
            removed_payload_bytes,
        );
        remove_context_mutation_results(&mut state, context_id);
        state.closing_contexts.insert(context_id);
        state.context_closures.push_back(navigation);
        drop(state);
        self.shared.command_ready.notify_one();
        Ok(CommandReceipt::ContextCloseRequested(navigation))
    }
}

impl Drop for NavigationEngine {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Unique bounded event consumer and frame-lease transfer endpoint.
pub struct EngineEventReceiver {
    shared: Arc<Shared>,
    attached: bool,
}

impl EngineEventReceiver {
    /// Receives the next event without blocking.
    ///
    /// # Errors
    ///
    /// Returns [`EventReceiveError::Empty`] while the worker can still produce
    /// events, or [`EventReceiveError::Closed`] after terminal drain.
    pub fn try_recv(&mut self) -> Result<EngineEvent, EventReceiveError> {
        receive_event_locked(&self.shared, false)
    }

    /// Blocks on a condition variable until an event or terminal status exists.
    ///
    /// # Errors
    ///
    /// Returns [`EventReceiveError::Closed`] after all events are drained.
    pub fn recv(&mut self) -> Result<EngineEvent, EventReceiveError> {
        receive_event_locked(&self.shared, true)
    }

    /// Atomically transfers the bounded commitment installed before the exact
    /// `NavigationCommitted` event was enqueued.
    ///
    /// A stale identity never removes a newer context commitment.
    ///
    /// # Errors
    ///
    /// Returns [`NavigationCommitError`] for a stale, unknown, or detached
    /// navigation identity.
    pub fn take_navigation_commit(
        &mut self,
        navigation: NavigationId,
    ) -> Result<NavigationCommit, NavigationCommitError> {
        if !self.attached {
            return Err(NavigationCommitError::ReceiverClosed);
        }
        let mut state = lock_unpoisoned(&self.shared.state);
        if let Some(metadata) = state.navigation_commits.remove(&navigation) {
            return Ok(NavigationCommit {
                navigation,
                metadata,
            });
        }
        if state
            .contexts
            .get(&navigation.context())
            .is_some_and(|context| navigation.generation() <= context.latest_generation)
        {
            Err(NavigationCommitError::Stale)
        } else {
            Err(NavigationCommitError::Unknown)
        }
    }

    /// Atomically transfers the current matching frame out of the bounded store.
    /// A stale lease never removes a newer current frame.
    ///
    /// # Errors
    ///
    /// Returns [`FrameLeaseError`] when the lease is stale, unknown, or this
    /// receiver is detached.
    pub fn take_frame(&mut self, lease: FrameLeaseId) -> Result<FrameLease, FrameLeaseError> {
        if !self.attached {
            return Err(FrameLeaseError::ReceiverClosed);
        }
        let mut state = lock_unpoisoned(&self.shared.state);
        let matching_context = state.contexts.iter().find_map(|(context_id, context)| {
            let stored = context.current_frame.as_ref()?;
            if stored.lease != lease {
                return None;
            }
            Some((*context_id, stored.frame.metadata().total_bytes()))
        });
        if let Some((context_id, bytes)) = matching_context {
            let Some(retained_after) = state.retained_frame_bytes.checked_sub(bytes) else {
                return Err(FrameLeaseError::Unknown);
            };
            let Some(stored) = state
                .contexts
                .get_mut(&context_id)
                .and_then(|context| context.current_frame.take())
            else {
                return Err(FrameLeaseError::Unknown);
            };
            state.retained_frame_bytes = retained_after;
            return Ok(FrameLease {
                navigation: stored.navigation,
                lease: stored.lease,
                frame: stored.frame,
            });
        }
        if lease.get() < state.next_frame_lease {
            Err(FrameLeaseError::Stale)
        } else {
            Err(FrameLeaseError::Unknown)
        }
    }

    /// Atomically transfers one exact bounded created-node mapping out of the
    /// result store. Frame and result leases are independent and one-shot.
    ///
    /// # Errors
    ///
    /// Returns [`MutationResultLeaseError`] for a stale, unknown, or detached
    /// lease.
    pub fn take_mutation_result(
        &mut self,
        lease: MutationResultLeaseId,
    ) -> Result<MutationResultLease, MutationResultLeaseError> {
        if !self.attached {
            return Err(MutationResultLeaseError::ReceiverClosed);
        }
        let mut state = lock_unpoisoned(&self.shared.state);
        if let Some(result_units) = state
            .mutation_results
            .get(&lease)
            .map(|stored| stored.created_nodes.len().max(1))
        {
            let Some(retained_after) = state
                .retained_mutation_result_nodes
                .checked_sub(result_units)
            else {
                return Err(MutationResultLeaseError::Unknown);
            };
            let Some(stored) = state.mutation_results.remove(&lease) else {
                return Err(MutationResultLeaseError::Unknown);
            };
            state.retained_mutation_result_nodes = retained_after;
            return Ok(MutationResultLease {
                navigation: stored.navigation,
                operation: stored.operation,
                live_version: stored.live_version,
                lease: stored.lease,
                created_nodes: stored.created_nodes,
            });
        }
        if lease.get() < state.next_mutation_result_lease {
            Err(MutationResultLeaseError::Stale)
        } else {
            Err(MutationResultLeaseError::Unknown)
        }
    }
}

impl Drop for EngineEventReceiver {
    fn drop(&mut self) {
        if !self.attached {
            return;
        }
        self.attached = false;
        let mut state = lock_unpoisoned(&self.shared.state);
        state.receiver_open = false;
        state.events.clear();
        state.terminal_event = None;
        for context in state.contexts.values_mut() {
            if let Some(active) = context.active_cancellation.take() {
                active.cancel();
            }
            context.current_frame = None;
            context.document = None;
        }
        state.retained_frame_bytes = 0;
        state.navigation_commits.clear();
        state.mutation_results.clear();
        state.retained_mutation_result_nodes = 0;
        state.pending_mutation_result_nodes = 0;
        state.pending_mutation_payload_bytes = 0;
        state.pending_document_nodes = 0;
        state.retained_document_nodes = 0;
        state.commands.clear();
        state.context_closures.clear();
        state.closing_contexts.clear();
        request_stop_locked(&mut state, WorkerStopReason::EventReceiverDropped);
        drop(state);
        self.shared.command_ready.notify_all();
        self.shared.event_ready.notify_all();
    }
}

fn receive_event_locked(shared: &Shared, blocking: bool) -> Result<EngineEvent, EventReceiveError> {
    let mut state = lock_unpoisoned(&shared.state);
    loop {
        if let Some(event) = state.events.pop_front() {
            if let EngineEventKind::FrameReady { navigation, .. } = event.kind {
                // Commitment transfer is intentionally tied to the immediately
                // preceding commit event. A consumer which advances to the
                // frame without taking it has declined that one-shot record;
                // retaining it after the paired event would permit unbounded
                // metadata growth in non-browser consumers.
                state.navigation_commits.remove(&navigation);
            }
            return Ok(event);
        }
        if let Some(event) = state.terminal_event.take() {
            return Ok(event);
        }
        if let Lifecycle::Stopped(status) = state.lifecycle {
            return Err(EventReceiveError::Closed(status));
        }
        if !blocking {
            return Err(EventReceiveError::Empty);
        }
        state = shared
            .event_ready
            .wait(state)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}

fn ensure_accepting(state: &SharedState) -> Result<(), CommandErrorKind> {
    if !state.receiver_open {
        return Err(CommandErrorKind::EventReceiverDropped);
    }
    if !matches!(state.lifecycle, Lifecycle::Running) {
        return Err(CommandErrorKind::ShuttingDown);
    }
    Ok(())
}

fn reserve_document_operation_id(
    state: &mut SharedState,
) -> Result<DocumentOperationId, CommandErrorKind> {
    let raw = state.next_document_operation_sequence;
    let next = raw
        .checked_add(1)
        .ok_or(CommandErrorKind::DocumentOperationIdentityExhausted)?;
    let sequence =
        NonZeroU64::new(raw).ok_or(CommandErrorKind::DocumentOperationIdentityExhausted)?;
    let operation = DocumentOperationId::new(state.document_operation_owner, sequence);
    state.next_document_operation_sequence = next;
    Ok(operation)
}

fn ensure_document_admission(
    state: &SharedState,
    limits: EngineLimits,
    navigation: NavigationId,
    expected_live_version: DocumentVersion,
) -> Result<(), CommandErrorKind> {
    ensure_accepting(state)?;
    if state.closing_contexts.contains(&navigation.context()) {
        return Err(CommandErrorKind::ContextClosing);
    }
    if state.commands.len() >= limits.command_capacity() {
        return Err(CommandErrorKind::QueueFull {
            capacity: limits.command_capacity(),
        });
    }
    let context = state
        .contexts
        .get(&navigation.context())
        .ok_or(CommandErrorKind::UnknownContext)?;
    if context.active_cancellation.is_some() {
        return Err(CommandErrorKind::ContextBusy);
    }
    let document = context.document.ok_or(CommandErrorKind::NoLiveDocument)?;
    if document.navigation != navigation {
        return Err(CommandErrorKind::DocumentNavigationMismatch {
            current: document.navigation,
        });
    }
    if document.live_version != expected_live_version {
        return Err(CommandErrorKind::DocumentVersionMismatch {
            live: document.live_version,
        });
    }
    Ok(())
}

struct MutationPayloadReservation {
    created_nodes: usize,
    payload_bytes: usize,
}

fn validate_mutation_payload(
    batch: &ScriptMutationBatch,
) -> Result<MutationPayloadReservation, CommandErrorKind> {
    let commands = batch.commands();
    if commands.len() > ScriptMutationLimits::HARD_MAX_COMMANDS {
        return Err(CommandErrorKind::MutationPayloadLimit {
            kind: ScriptMutationLimitKind::Commands,
            maximum: ScriptMutationLimits::HARD_MAX_COMMANDS,
            actual: commands.len(),
        });
    }
    let mut created_nodes = 0usize;
    let mut total_string_bytes = 0usize;
    for command in commands {
        match command {
            ScriptMutationCommand::CreateHtmlElement { local_name, .. } => {
                created_nodes =
                    created_nodes
                        .checked_add(1)
                        .ok_or(CommandErrorKind::MutationPayloadLimit {
                            kind: ScriptMutationLimitKind::CreatedNodes,
                            maximum: ScriptMutationLimits::HARD_MAX_CREATED_NODES,
                            actual: usize::MAX,
                        })?;
                account_mutation_string(local_name, &mut total_string_bytes)?;
            }
            ScriptMutationCommand::CreateText { data, .. } => {
                created_nodes =
                    created_nodes
                        .checked_add(1)
                        .ok_or(CommandErrorKind::MutationPayloadLimit {
                            kind: ScriptMutationLimitKind::CreatedNodes,
                            maximum: ScriptMutationLimits::HARD_MAX_CREATED_NODES,
                            actual: usize::MAX,
                        })?;
                account_mutation_string(data, &mut total_string_bytes)?;
            }
            ScriptMutationCommand::SetHtmlAttribute {
                local_name, value, ..
            } => {
                account_mutation_string(local_name, &mut total_string_bytes)?;
                account_mutation_string(value, &mut total_string_bytes)?;
            }
            ScriptMutationCommand::RemoveHtmlAttribute { local_name, .. } => {
                account_mutation_string(local_name, &mut total_string_bytes)?;
            }
            ScriptMutationCommand::SetCharacterData { data, .. } => {
                account_mutation_string(data, &mut total_string_bytes)?;
            }
            ScriptMutationCommand::AppendChild { .. }
            | ScriptMutationCommand::InsertBefore { .. }
            | ScriptMutationCommand::RemoveChild { .. } => {}
        }
    }
    if created_nodes > ScriptMutationLimits::HARD_MAX_CREATED_NODES {
        return Err(CommandErrorKind::MutationPayloadLimit {
            kind: ScriptMutationLimitKind::CreatedNodes,
            maximum: ScriptMutationLimits::HARD_MAX_CREATED_NODES,
            actual: created_nodes,
        });
    }
    if total_string_bytes > ScriptMutationLimits::HARD_MAX_TOTAL_STRING_BYTES {
        return Err(CommandErrorKind::MutationPayloadLimit {
            kind: ScriptMutationLimitKind::TotalStringBytes,
            maximum: ScriptMutationLimits::HARD_MAX_TOTAL_STRING_BYTES,
            actual: total_string_bytes,
        });
    }
    let payload_bytes = std::mem::size_of_val(commands)
        .checked_add(total_string_bytes)
        .ok_or(CommandErrorKind::MutationPayloadBudget {
            maximum: MAX_PENDING_MUTATION_PAYLOAD_BYTES,
        })?;
    Ok(MutationPayloadReservation {
        created_nodes,
        payload_bytes,
    })
}

fn account_mutation_string(
    value: &str,
    total_string_bytes: &mut usize,
) -> Result<(), CommandErrorKind> {
    if value.len() > ScriptMutationLimits::HARD_MAX_STRING_BYTES {
        return Err(CommandErrorKind::MutationPayloadLimit {
            kind: ScriptMutationLimitKind::StringBytes,
            maximum: ScriptMutationLimits::HARD_MAX_STRING_BYTES,
            actual: value.len(),
        });
    }
    *total_string_bytes = total_string_bytes.checked_add(value.len()).ok_or(
        CommandErrorKind::MutationPayloadLimit {
            kind: ScriptMutationLimitKind::TotalStringBytes,
            maximum: ScriptMutationLimits::HARD_MAX_TOTAL_STRING_BYTES,
            actual: usize::MAX,
        },
    )?;
    Ok(())
}

fn release_pending_mutation_reservations(
    state: &mut SharedState,
    created_nodes: usize,
    result_units: usize,
    payload_bytes: usize,
) {
    state.pending_document_nodes = state
        .pending_document_nodes
        .checked_sub(created_nodes)
        .expect("pending document-node reservation is exact");
    state.pending_mutation_result_nodes = state
        .pending_mutation_result_nodes
        .checked_sub(result_units)
        .expect("pending result-node reservation is exact");
    state.pending_mutation_payload_bytes = state
        .pending_mutation_payload_bytes
        .checked_sub(payload_bytes)
        .expect("pending payload-byte reservation is exact");
}

fn remove_context_mutation_results(state: &mut SharedState, context: TopLevelContextId) {
    let mut removed_nodes = 0usize;
    state.mutation_results.retain(|_, result| {
        if result.navigation.context() == context {
            removed_nodes = removed_nodes
                .checked_add(result.created_nodes.len().max(1))
                .expect("retained mutation-result total is representable");
            false
        } else {
            true
        }
    });
    state.retained_mutation_result_nodes = state
        .retained_mutation_result_nodes
        .checked_sub(removed_nodes)
        .expect("removed result nodes were retained");
}

fn queue_navigation_locked(
    state: &mut SharedState,
    limits: EngineLimits,
    navigation: NavigationId,
    request: NavigationRequest,
) -> Result<CommandReceipt, CommandErrorKind> {
    ensure_accepting(state)?;

    let context_id = navigation.context();
    if state.closing_contexts.contains(&context_id) {
        return Err(CommandErrorKind::ContextClosing);
    }
    let generation = navigation.generation();
    let occupied_context_slots = state
        .contexts
        .len()
        .saturating_add(state.closing_contexts.len());
    if !state.contexts.contains_key(&context_id)
        && let Some(latest) = state.latest_new_context
        && context_id <= latest
    {
        return Err(CommandErrorKind::ContextIdentityRetired { latest });
    }
    match state.contexts.get(&context_id) {
        Some(existing) if existing.latest_generation.checked_next().is_none() => {
            return Err(CommandErrorKind::GenerationExhausted);
        }
        Some(existing) if generation <= existing.latest_generation => {
            return Err(CommandErrorKind::NonMonotonicGeneration {
                latest: existing.latest_generation,
            });
        }
        None if generation != NavigationGeneration::INITIAL => {
            return Err(CommandErrorKind::InitialGenerationRequired);
        }
        None if occupied_context_slots >= limits.max_contexts() => {
            return Err(CommandErrorKind::ContextLimitReached {
                maximum: limits.max_contexts(),
            });
        }
        Some(_) | None => {}
    }
    if state.commands.len() >= limits.command_capacity() {
        return Err(CommandErrorKind::QueueFull {
            capacity: limits.command_capacity(),
        });
    }

    let cancellation = crate::CancellationSource::new();
    state
        .commands
        .push_back(EngineWork::Navigate(NavigationWork {
            navigation,
            request,
            cancellation: cancellation.token(),
        }));
    if let Some(context) = state.contexts.get_mut(&context_id) {
        let active = ActiveCancellation::Navigation {
            navigation,
            source: cancellation,
        };
        if let Some(previous) = context.active_cancellation.replace(active) {
            previous.cancel();
        }
        context.latest_generation = generation;
    } else {
        state.contexts.insert(
            context_id,
            ContextState {
                latest_generation: generation,
                active_cancellation: Some(ActiveCancellation::Navigation {
                    navigation,
                    source: cancellation,
                }),
                current_frame: None,
                document: None,
            },
        );
        state.latest_new_context = Some(context_id);
    }
    Ok(CommandReceipt::NavigationQueued(navigation))
}

fn request_stop_locked(state: &mut SharedState, reason: WorkerStopReason) {
    if matches!(state.lifecycle, Lifecycle::Running) {
        for commitment in state.current_style_documents.values() {
            commitment.retire_style_document();
        }
        state.current_style_documents.clear();
        state.lifecycle = Lifecycle::Stopping(reason);
        for context in state.contexts.values() {
            if let Some(active) = &context.active_cancellation {
                active.cancel();
            }
        }
    }
}

enum WorkerInit {
    Ready,
    Failed(ExecutionFailure),
    Panicked,
}

fn worker_main<E, F>(shared: &Shared, factory: F, init_sender: &mpsc::SyncSender<WorkerInit>)
where
    E: NavigationExecutor,
    F: FnOnce() -> Result<E, ExecutionFailure>,
{
    let executor = catch_unwind(AssertUnwindSafe(factory));
    let mut executor = match executor {
        Ok(Ok(executor)) => executor,
        Ok(Err(error)) => {
            let _ = init_sender.send(WorkerInit::Failed(error));
            finish_worker(
                shared,
                EngineShutdownStatus {
                    reason: WorkerStopReason::Requested,
                    executor: ExecutorShutdownStatus::NotStarted,
                },
            );
            return;
        }
        Err(_) => {
            let _ = init_sender.send(WorkerInit::Panicked);
            finish_worker(
                shared,
                EngineShutdownStatus {
                    reason: WorkerStopReason::ExecutorPanicked,
                    executor: ExecutorShutdownStatus::NotStarted,
                },
            );
            return;
        }
    };
    if init_sender.send(WorkerInit::Ready).is_err() {
        let finalization = finalize_executor(executor);
        finish_worker(
            shared,
            EngineShutdownStatus {
                reason: if finalization.drop_panicked {
                    WorkerStopReason::ExecutorPanicked
                } else {
                    WorkerStopReason::EventReceiverDropped
                },
                executor: finalization.status,
            },
        );
        return;
    }

    let mut reason = catch_unwind(AssertUnwindSafe(|| worker_loop(shared, &mut executor)))
        .unwrap_or(WorkerStopReason::ExecutorPanicked);
    let finalization = finalize_executor(executor);
    if finalization.drop_panicked {
        reason = WorkerStopReason::ExecutorPanicked;
    }
    finish_worker(
        shared,
        EngineShutdownStatus {
            reason,
            executor: finalization.status,
        },
    );
}

struct ExecutorFinalization {
    status: ExecutorShutdownStatus,
    drop_panicked: bool,
}

fn finalize_executor<E: NavigationExecutor>(mut executor: E) -> ExecutorFinalization {
    let shutdown = match catch_unwind(AssertUnwindSafe(|| executor.shutdown())) {
        Ok(Ok(())) => ExecutorShutdownStatus::Clean,
        Ok(Err(error)) => ExecutorShutdownStatus::Failed(error),
        Err(_) => ExecutorShutdownStatus::Panicked,
    };
    let drop_panicked = catch_unwind(AssertUnwindSafe(|| drop(executor))).is_err();
    ExecutorFinalization {
        status: if drop_panicked {
            ExecutorShutdownStatus::Panicked
        } else {
            shutdown
        },
        drop_panicked,
    }
}

fn worker_loop<E: NavigationExecutor>(shared: &Shared, executor: &mut E) -> WorkerStopReason {
    loop {
        let work = match dequeue_work(shared) {
            Ok(work) => work,
            Err(reason) => return reason,
        };

        match work {
            DequeuedWork::CloseContext(navigation) => {
                executor.close_context(navigation.context());
                if let Err(reason) = publish_context_closed(shared, navigation) {
                    return reason;
                }
            }
            DequeuedWork::Engine(EngineWork::Navigate(work)) => {
                let mut phase = NavigationEventPhase::Queued;
                match begin_navigation(shared, &work, &mut phase) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(reason) => return reason,
                }

                let result = executor.execute(work.navigation, &work.request, &work.cancellation);
                if let Err(reason) = finish_navigation(shared, executor, &work, &mut phase, result)
                {
                    return reason;
                }
            }
            DequeuedWork::Engine(EngineWork::Mutate(work)) => {
                let reservation = match begin_document_mutation(shared, &work) {
                    Ok(Some(reservation)) => reservation,
                    Ok(None) => continue,
                    Err(reason) => return reason,
                };
                let outcome = executor.mutate_document(
                    work.navigation,
                    work.batch.clone(),
                    &work.cancellation,
                );
                if let Err(reason) =
                    finish_document_mutation(shared, executor, &work, reservation, outcome)
                {
                    return reason;
                }
            }
            DequeuedWork::Engine(EngineWork::Rerender(work)) => {
                let reservation = match begin_document_rerender(shared, &work) {
                    Ok(Some(reservation)) => reservation,
                    Ok(None) => continue,
                    Err(reason) => return reason,
                };
                let outcome = executor.rerender_document(
                    work.navigation,
                    work.expected_live_version,
                    &work.cancellation,
                );
                if let Err(reason) =
                    finish_document_rerender(shared, executor, &work, reservation, outcome)
                {
                    return reason;
                }
            }
        }
    }
}

enum DequeuedWork {
    CloseContext(NavigationId),
    Engine(EngineWork),
}

fn dequeue_work(shared: &Shared) -> Result<DequeuedWork, WorkerStopReason> {
    let mut state = lock_unpoisoned(&shared.state);
    loop {
        match state.lifecycle {
            Lifecycle::Stopping(reason) => return Err(reason),
            Lifecycle::Stopped(status) => return Err(status.reason),
            Lifecycle::Running => {}
        }
        if let Some(context) = state.context_closures.pop_front() {
            return Ok(DequeuedWork::CloseContext(context));
        }
        if let Some(work) = state.commands.pop_front() {
            return Ok(DequeuedWork::Engine(work));
        }
        state = shared
            .command_ready
            .wait(state)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}

fn begin_navigation(
    shared: &Shared,
    work: &NavigationWork,
    phase: &mut NavigationEventPhase,
) -> Result<bool, WorkerStopReason> {
    let mut state = lock_unpoisoned(&shared.state);
    if let Lifecycle::Stopping(reason) = state.lifecycle {
        return Err(reason);
    }
    let should_execute = is_current(&state, work.navigation) && !work.cancellation.is_cancelled();
    let kind = if should_execute {
        EngineEventKind::NavigationStarted {
            navigation: work.navigation,
        }
    } else {
        EngineEventKind::NavigationCancelled {
            navigation: work.navigation,
        }
    };
    if let Err(reason) = enqueue_one(&mut state, shared.limits, phase, kind) {
        request_stop_locked(&mut state, reason);
        return Err(reason);
    }
    if !should_execute {
        clear_navigation_cancellation_if_current(&mut state, work.navigation);
    }
    drop(state);
    shared.event_ready.notify_one();
    Ok(should_execute)
}

fn finish_navigation(
    shared: &Shared,
    executor: &mut impl NavigationExecutor,
    work: &NavigationWork,
    phase: &mut NavigationEventPhase,
    result: Result<ExecutorOutput, ExecutionFailure>,
) -> Result<(), WorkerStopReason> {
    let renderer_unusable = result
        .as_ref()
        .is_err_and(|failure| failure.renderer_unusable());
    let mut state = lock_unpoisoned(&shared.state);
    if let Lifecycle::Stopping(reason) = state.lifecycle {
        return Err(reason);
    }
    let (publication, published) =
        if !is_current(&state, work.navigation) || work.cancellation.is_cancelled() {
            let result = enqueue_one(
                &mut state,
                shared.limits,
                phase,
                EngineEventKind::NavigationCancelled {
                    navigation: work.navigation,
                },
            );
            clear_navigation_cancellation_if_current(&mut state, work.navigation);
            (result, false)
        } else {
            match publish_execution_result(
                &mut state,
                shared.limits,
                phase,
                work.navigation,
                &work.request,
                result,
            ) {
                Ok(published) => (Ok(()), published),
                Err(reason) => (Err(reason), false),
            }
        };
    executor.acknowledge_navigation_publication(work.navigation, published);
    if let Err(reason) = publication {
        request_stop_locked(&mut state, reason);
        return Err(reason);
    }
    if renderer_unusable {
        request_stop_locked(&mut state, WorkerStopReason::RendererUnavailable);
    }
    drop(state);
    shared.event_ready.notify_all();
    Ok(())
}

fn publish_execution_result(
    state: &mut SharedState,
    limits: EngineLimits,
    phase: &mut NavigationEventPhase,
    navigation: NavigationId,
    request: &NavigationRequest,
    result: Result<ExecutorOutput, ExecutionFailure>,
) -> Result<bool, WorkerStopReason> {
    match result {
        Ok(output) => publish_success(state, limits, phase, navigation, request, output),
        Err(failure) if failure.kind() == ExecutionFailureKind::Cancelled => {
            enqueue_one(
                state,
                limits,
                phase,
                EngineEventKind::NavigationCancelled { navigation },
            )?;
            clear_navigation_cancellation_if_current(state, navigation);
            Ok(false)
        }
        Err(failure) => {
            enqueue_one(
                state,
                limits,
                phase,
                EngineEventKind::NavigationFailed {
                    navigation,
                    failure,
                },
            )?;
            clear_navigation_cancellation_if_current(state, navigation);
            Ok(false)
        }
    }
}

fn is_current(state: &SharedState, navigation: NavigationId) -> bool {
    state
        .contexts
        .get(&navigation.context())
        .is_some_and(|context| context.latest_generation == navigation.generation())
}

fn clear_navigation_cancellation_if_current(state: &mut SharedState, navigation: NavigationId) {
    let Some(context) = state.contexts.get_mut(&navigation.context()) else {
        return;
    };
    if context
        .active_cancellation
        .as_ref()
        .is_some_and(|active| active.is_navigation(navigation))
    {
        context.active_cancellation = None;
    }
}

fn clear_document_cancellation_if_current(
    state: &mut SharedState,
    navigation: NavigationId,
    operation: DocumentOperationId,
) {
    let Some(context) = state.contexts.get_mut(&navigation.context()) else {
        return;
    };
    if context
        .active_cancellation
        .as_ref()
        .is_some_and(|active| active.is_document(navigation, operation))
    {
        context.active_cancellation = None;
    }
}

#[derive(Clone, Copy)]
struct DocumentPublicationReservation {
    sequence: EventSequence,
    frame: FrameLeaseId,
    result: Option<MutationResultLeaseId>,
}

fn begin_document_mutation(
    shared: &Shared,
    work: &DocumentMutationWork,
) -> Result<Option<DocumentPublicationReservation>, WorkerStopReason> {
    let expected = work.batch.expected_version();
    let mut state = lock_unpoisoned(&shared.state);
    if let Lifecycle::Stopping(reason) = state.lifecycle {
        return Err(reason);
    }
    if !document_work_is_current(&state, work.navigation, expected, work.operation) {
        release_pending_mutation_reservations(
            &mut state,
            work.reserved_created_nodes,
            work.reserved_result_units,
            work.reserved_payload_bytes,
        );
        clear_document_cancellation_if_current(&mut state, work.navigation, work.operation);
        return Ok(None);
    }
    let reservation = reserve_document_publication(&mut state, shared.limits, true)?;
    let (live_version, frame_version) = current_document_versions(&state, work.navigation)
        .ok_or(WorkerStopReason::ExecutorContractViolation)?;
    let failure = if work.cancellation.is_cancelled() {
        Some(DocumentOperationFailure::Cancelled)
    } else if !document_node_budget_is_valid(&state, shared.limits)
        || !reservation_can_replace_frame(&state, shared.limits, work.navigation)
    {
        Some(DocumentOperationFailure::ResourceLimit)
    } else {
        None
    };
    if let Some(failure) = failure {
        release_pending_mutation_reservations(
            &mut state,
            work.reserved_created_nodes,
            work.reserved_result_units,
            work.reserved_payload_bytes,
        );
        clear_document_cancellation_if_current(&mut state, work.navigation, work.operation);
        push_reserved_event(
            &mut state,
            shared.limits,
            reservation.sequence,
            EngineEventKind::DocumentMutationRejected {
                navigation: work.navigation,
                operation: work.operation,
                live_version: Some(live_version),
                frame_version: Some(frame_version),
                failure,
            },
        )?;
        drop(state);
        shared.event_ready.notify_one();
        return Ok(None);
    }
    Ok(Some(reservation))
}

fn document_node_budget_is_valid(state: &SharedState, limits: EngineLimits) -> bool {
    state
        .retained_document_nodes
        .checked_add(state.pending_document_nodes)
        .is_some_and(|total| total <= limits.max_retained_document_nodes())
}

fn begin_document_rerender(
    shared: &Shared,
    work: &DocumentRerenderWork,
) -> Result<Option<DocumentPublicationReservation>, WorkerStopReason> {
    let mut state = lock_unpoisoned(&shared.state);
    if let Lifecycle::Stopping(reason) = state.lifecycle {
        return Err(reason);
    }
    if !document_work_is_current(
        &state,
        work.navigation,
        work.expected_live_version,
        work.operation,
    ) {
        clear_document_cancellation_if_current(&mut state, work.navigation, work.operation);
        return Ok(None);
    }
    let reservation = reserve_document_publication(&mut state, shared.limits, false)?;
    let (live_version, frame_version) = current_document_versions(&state, work.navigation)
        .ok_or(WorkerStopReason::ExecutorContractViolation)?;
    let failure = if work.cancellation.is_cancelled() {
        Some(DocumentOperationFailure::Cancelled)
    } else if !reservation_can_replace_frame(&state, shared.limits, work.navigation) {
        Some(DocumentOperationFailure::ResourceLimit)
    } else {
        None
    };
    if let Some(failure) = failure {
        clear_document_cancellation_if_current(&mut state, work.navigation, work.operation);
        push_reserved_event(
            &mut state,
            shared.limits,
            reservation.sequence,
            EngineEventKind::DocumentRerenderRejected {
                navigation: work.navigation,
                operation: work.operation,
                live_version: Some(live_version),
                frame_version: Some(frame_version),
                failure,
            },
        )?;
        drop(state);
        shared.event_ready.notify_one();
        return Ok(None);
    }
    Ok(Some(reservation))
}

fn reserve_document_publication(
    state: &mut SharedState,
    limits: EngineLimits,
    needs_result: bool,
) -> Result<DocumentPublicationReservation, WorkerStopReason> {
    if state.events.len() >= limits.event_capacity() {
        return Err(WorkerStopReason::EventQueueSaturated);
    }
    let sequence_raw = state.next_event_sequence;
    let _next_sequence = sequence_raw
        .checked_add(1)
        .ok_or(WorkerStopReason::IdentityExhausted)?;
    let frame_raw = state.next_frame_lease;
    let _next_frame = frame_raw
        .checked_add(1)
        .ok_or(WorkerStopReason::IdentityExhausted)?;
    let result_raw = state.next_mutation_result_lease;
    let _next_result = if needs_result {
        Some(
            result_raw
                .checked_add(1)
                .ok_or(WorkerStopReason::IdentityExhausted)?,
        )
    } else {
        None
    };
    let sequence =
        EventSequence(NonZeroU64::new(sequence_raw).ok_or(WorkerStopReason::IdentityExhausted)?);
    let frame =
        FrameLeaseId(NonZeroU64::new(frame_raw).ok_or(WorkerStopReason::IdentityExhausted)?);
    let result = if needs_result {
        Some(MutationResultLeaseId(
            NonZeroU64::new(result_raw).ok_or(WorkerStopReason::IdentityExhausted)?,
        ))
    } else {
        None
    };
    Ok(DocumentPublicationReservation {
        sequence,
        frame,
        result,
    })
}

fn reservation_can_replace_frame(
    state: &SharedState,
    limits: EngineLimits,
    navigation: NavigationId,
) -> bool {
    let old_bytes = state
        .contexts
        .get(&navigation.context())
        .and_then(|context| context.current_frame.as_ref())
        .map_or(0, |stored| stored.frame.metadata().total_bytes());
    state
        .retained_frame_bytes
        .checked_sub(old_bytes)
        .and_then(|without_old| without_old.checked_add(limits.max_frame_bytes()))
        .is_some_and(|retained| retained <= limits.max_retained_frame_bytes())
}

fn document_work_is_current(
    state: &SharedState,
    navigation: NavigationId,
    expected_live_version: DocumentVersion,
    operation: DocumentOperationId,
) -> bool {
    state
        .contexts
        .get(&navigation.context())
        .is_some_and(|context| {
            context.document.is_some_and(|document| {
                document.navigation == navigation && document.live_version == expected_live_version
            }) && context
                .active_cancellation
                .as_ref()
                .is_some_and(|active| active.is_document(navigation, operation))
        })
}

fn current_document_versions(
    state: &SharedState,
    navigation: NavigationId,
) -> Option<(DocumentVersion, DocumentVersion)> {
    state
        .contexts
        .get(&navigation.context())?
        .document
        .filter(|document| document.navigation == navigation)
        .map(|document| (document.live_version, document.frame_version))
}

fn push_reserved_event(
    state: &mut SharedState,
    limits: EngineLimits,
    sequence: EventSequence,
    kind: EngineEventKind,
) -> Result<(), WorkerStopReason> {
    if state.events.len() >= limits.event_capacity() {
        return Err(WorkerStopReason::ExecutorContractViolation);
    }
    if state.next_event_sequence != sequence.get() {
        return Err(WorkerStopReason::ExecutorContractViolation);
    }
    state.next_event_sequence = sequence
        .get()
        .checked_add(1)
        .ok_or(WorkerStopReason::IdentityExhausted)?;
    state.events.push_back(EngineEvent { sequence, kind });
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn finish_document_mutation(
    shared: &Shared,
    executor: &mut impl NavigationExecutor,
    work: &DocumentMutationWork,
    reservation: DocumentPublicationReservation,
    outcome: ExecutorDocumentMutation,
) -> Result<(), WorkerStopReason> {
    let expected = work.batch.expected_version();
    let renderer_unusable = outcome.renderer_unusable();
    let mut state = lock_unpoisoned(&shared.state);
    if let Lifecycle::Stopping(reason) = state.lifecycle {
        return Err(reason);
    }
    if !document_work_is_current(&state, work.navigation, expected, work.operation) {
        release_pending_mutation_reservations(
            &mut state,
            work.reserved_created_nodes,
            work.reserved_result_units,
            work.reserved_payload_bytes,
        );
        clear_document_cancellation_if_current(&mut state, work.navigation, work.operation);
        if outcome.changed_hidden_state() {
            executor.invalidate_document(work.navigation.context());
            invalidate_shared_document(&mut state, work.navigation);
        }
        if renderer_unusable {
            request_stop_locked(&mut state, WorkerStopReason::RendererUnavailable);
            return Err(WorkerStopReason::RendererUnavailable);
        }
        return Ok(());
    }

    match outcome {
        ExecutorDocumentMutation::Rendered {
            previous_live_version,
            previous_frame_version,
            commit,
            frame,
        } => {
            let live_version = commit.version();
            if validate_mutation_transition(
                &state,
                work,
                previous_live_version,
                previous_frame_version,
                live_version,
                &frame,
                commit.created_nodes(),
            )
            .is_err()
            {
                release_pending_mutation_reservations(
                    &mut state,
                    work.reserved_created_nodes,
                    work.reserved_result_units,
                    work.reserved_payload_bytes,
                );
                invalidate_executor_and_shared_document(&mut state, executor, work.navigation);
                return Err(WorkerStopReason::ExecutorContractViolation);
            }
            let Ok(metadata) = publish_reserved_frame(
                &mut state,
                shared.limits,
                work.navigation,
                reservation.frame,
                frame,
            ) else {
                release_pending_mutation_reservations(
                    &mut state,
                    work.reserved_created_nodes,
                    work.reserved_result_units,
                    work.reserved_payload_bytes,
                );
                invalidate_executor_and_shared_document(&mut state, executor, work.navigation);
                return Err(WorkerStopReason::ExecutorContractViolation);
            };
            if commit_mutation_result(
                &mut state,
                shared.limits,
                work,
                reservation,
                live_version,
                commit.into_created_nodes(),
                true,
            )
            .is_err()
            {
                release_pending_mutation_reservations(
                    &mut state,
                    work.reserved_created_nodes,
                    work.reserved_result_units,
                    work.reserved_payload_bytes,
                );
                invalidate_executor_and_shared_document(&mut state, executor, work.navigation);
                return Err(WorkerStopReason::ExecutorContractViolation);
            }
            push_reserved_event(
                &mut state,
                shared.limits,
                reservation.sequence,
                EngineEventKind::DocumentMutationRendered {
                    navigation: work.navigation,
                    operation: work.operation,
                    previous_live_version,
                    previous_frame_version,
                    live_version,
                    result: reservation
                        .result
                        .ok_or(WorkerStopReason::ExecutorContractViolation)?,
                    created_nodes: work.reserved_created_nodes,
                    frame: reservation.frame,
                    metadata,
                },
            )?;
        }
        ExecutorDocumentMutation::Invalidated => {
            release_pending_mutation_reservations(
                &mut state,
                work.reserved_created_nodes,
                work.reserved_result_units,
                work.reserved_payload_bytes,
            );
            invalidate_executor_and_shared_document(&mut state, executor, work.navigation);
            return Err(WorkerStopReason::ExecutorContractViolation);
        }
        ExecutorDocumentMutation::CommittedWithoutFrame {
            previous_live_version,
            frame_version,
            commit,
            failure,
        } => {
            let live_version = commit.version();
            if validate_committed_mutation(
                &state,
                work,
                previous_live_version,
                live_version,
                frame_version,
                commit.created_nodes(),
            )
            .is_err()
            {
                release_pending_mutation_reservations(
                    &mut state,
                    work.reserved_created_nodes,
                    work.reserved_result_units,
                    work.reserved_payload_bytes,
                );
                invalidate_executor_and_shared_document(&mut state, executor, work.navigation);
                return Err(WorkerStopReason::ExecutorContractViolation);
            }
            if commit_mutation_result(
                &mut state,
                shared.limits,
                work,
                reservation,
                live_version,
                commit.into_created_nodes(),
                false,
            )
            .is_err()
            {
                release_pending_mutation_reservations(
                    &mut state,
                    work.reserved_created_nodes,
                    work.reserved_result_units,
                    work.reserved_payload_bytes,
                );
                invalidate_executor_and_shared_document(&mut state, executor, work.navigation);
                return Err(WorkerStopReason::ExecutorContractViolation);
            }
            push_reserved_event(
                &mut state,
                shared.limits,
                reservation.sequence,
                EngineEventKind::DocumentMutationCommittedWithoutFrame {
                    navigation: work.navigation,
                    operation: work.operation,
                    previous_live_version,
                    live_version,
                    frame_version,
                    result: reservation
                        .result
                        .ok_or(WorkerStopReason::ExecutorContractViolation)?,
                    created_nodes: work.reserved_created_nodes,
                    failure,
                },
            )?;
        }
        ExecutorDocumentMutation::Rejected {
            live_version,
            frame_version,
            failure,
        } => {
            let expected_versions = current_document_versions(&state, work.navigation)
                .ok_or(WorkerStopReason::ExecutorContractViolation)?;
            if (live_version, frame_version)
                != (Some(expected_versions.0), Some(expected_versions.1))
            {
                release_pending_mutation_reservations(
                    &mut state,
                    work.reserved_created_nodes,
                    work.reserved_result_units,
                    work.reserved_payload_bytes,
                );
                invalidate_executor_and_shared_document(&mut state, executor, work.navigation);
                return Err(WorkerStopReason::ExecutorContractViolation);
            }
            release_pending_mutation_reservations(
                &mut state,
                work.reserved_created_nodes,
                work.reserved_result_units,
                work.reserved_payload_bytes,
            );
            push_reserved_event(
                &mut state,
                shared.limits,
                reservation.sequence,
                EngineEventKind::DocumentMutationRejected {
                    navigation: work.navigation,
                    operation: work.operation,
                    live_version,
                    frame_version,
                    failure,
                },
            )?;
        }
    }
    clear_document_cancellation_if_current(&mut state, work.navigation, work.operation);
    if renderer_unusable {
        request_stop_locked(&mut state, WorkerStopReason::RendererUnavailable);
    }
    drop(state);
    shared.event_ready.notify_all();
    Ok(())
}

fn finish_document_rerender(
    shared: &Shared,
    executor: &mut impl NavigationExecutor,
    work: &DocumentRerenderWork,
    reservation: DocumentPublicationReservation,
    outcome: ExecutorDocumentRerender,
) -> Result<(), WorkerStopReason> {
    let renderer_unusable = outcome.renderer_unusable();
    let mut state = lock_unpoisoned(&shared.state);
    if let Lifecycle::Stopping(reason) = state.lifecycle {
        return Err(reason);
    }
    if !document_work_is_current(
        &state,
        work.navigation,
        work.expected_live_version,
        work.operation,
    ) {
        return finish_stale_document_rerender(
            &mut state,
            executor,
            work,
            outcome.changed_hidden_state(),
            renderer_unusable,
        );
    }

    match outcome {
        ExecutorDocumentRerender::Rendered {
            live_version,
            previous_frame_version,
            frame,
        } => {
            let current = current_document_versions(&state, work.navigation)
                .ok_or(WorkerStopReason::ExecutorContractViolation)?;
            if live_version != work.expected_live_version
                || previous_frame_version != current.1
                || frame.document_version() != Some(live_version)
            {
                invalidate_executor_and_shared_document(&mut state, executor, work.navigation);
                return Err(WorkerStopReason::ExecutorContractViolation);
            }
            let Ok(metadata) = publish_reserved_frame(
                &mut state,
                shared.limits,
                work.navigation,
                reservation.frame,
                frame,
            ) else {
                invalidate_executor_and_shared_document(&mut state, executor, work.navigation);
                return Err(WorkerStopReason::ExecutorContractViolation);
            };
            state
                .contexts
                .get_mut(&work.navigation.context())
                .and_then(|context| context.document.as_mut())
                .ok_or(WorkerStopReason::ExecutorContractViolation)?
                .frame_version = live_version;
            push_reserved_event(
                &mut state,
                shared.limits,
                reservation.sequence,
                EngineEventKind::DocumentRerendered {
                    navigation: work.navigation,
                    operation: work.operation,
                    live_version,
                    previous_frame_version,
                    frame: reservation.frame,
                    metadata,
                },
            )?;
        }
        ExecutorDocumentRerender::Rejected {
            live_version,
            frame_version,
            failure,
        } => {
            let current = current_document_versions(&state, work.navigation)
                .ok_or(WorkerStopReason::ExecutorContractViolation)?;
            if (live_version, frame_version) != (Some(current.0), Some(current.1)) {
                invalidate_executor_and_shared_document(&mut state, executor, work.navigation);
                return Err(WorkerStopReason::ExecutorContractViolation);
            }
            push_reserved_event(
                &mut state,
                shared.limits,
                reservation.sequence,
                EngineEventKind::DocumentRerenderRejected {
                    navigation: work.navigation,
                    operation: work.operation,
                    live_version,
                    frame_version,
                    failure,
                },
            )?;
        }
        ExecutorDocumentRerender::Invalidated => {
            invalidate_executor_and_shared_document(&mut state, executor, work.navigation);
            return Err(WorkerStopReason::ExecutorContractViolation);
        }
    }
    clear_document_cancellation_if_current(&mut state, work.navigation, work.operation);
    if renderer_unusable {
        request_stop_locked(&mut state, WorkerStopReason::RendererUnavailable);
    }
    drop(state);
    shared.event_ready.notify_all();
    Ok(())
}

fn finish_stale_document_rerender(
    state: &mut SharedState,
    executor: &mut impl NavigationExecutor,
    work: &DocumentRerenderWork,
    changed_hidden_state: bool,
    renderer_unusable: bool,
) -> Result<(), WorkerStopReason> {
    clear_document_cancellation_if_current(state, work.navigation, work.operation);
    if changed_hidden_state {
        executor.invalidate_document(work.navigation.context());
        invalidate_shared_document(state, work.navigation);
    }
    if renderer_unusable {
        request_stop_locked(state, WorkerStopReason::RendererUnavailable);
        return Err(WorkerStopReason::RendererUnavailable);
    }
    Ok(())
}

fn validate_mutation_transition(
    state: &SharedState,
    work: &DocumentMutationWork,
    previous_live_version: DocumentVersion,
    previous_frame_version: DocumentVersion,
    live_version: DocumentVersion,
    frame: &EngineFrame,
    created_nodes: &[NodeId],
) -> Result<(), WorkerStopReason> {
    validate_committed_mutation(
        state,
        work,
        previous_live_version,
        live_version,
        previous_frame_version,
        created_nodes,
    )?;
    if frame.document_version() != Some(live_version) {
        return Err(WorkerStopReason::ExecutorContractViolation);
    }
    Ok(())
}

fn validate_committed_mutation(
    state: &SharedState,
    work: &DocumentMutationWork,
    previous_live_version: DocumentVersion,
    live_version: DocumentVersion,
    frame_version: DocumentVersion,
    created_nodes: &[NodeId],
) -> Result<(), WorkerStopReason> {
    let current = current_document_versions(state, work.navigation)
        .ok_or(WorkerStopReason::ExecutorContractViolation)?;
    if previous_live_version != current.0
        || frame_version != current.1
        || live_version.document_id() != previous_live_version.document_id()
        || live_version.revision()
            != previous_live_version
                .revision()
                .checked_add(1)
                .ok_or(WorkerStopReason::ExecutorContractViolation)?
        || created_nodes.len() != work.reserved_created_nodes
        || !mutation_batch_has_valid_node_topology(&work.batch)
        || created_nodes
            .iter()
            .any(|node| node.document_id() != live_version.document_id())
        || !created_nodes_are_distinct(created_nodes)
    {
        return Err(WorkerStopReason::ExecutorContractViolation);
    }
    Ok(())
}

fn mutation_batch_has_valid_node_topology(batch: &ScriptMutationBatch) -> bool {
    let document = batch.expected_version().document_id();
    let mut created_nodes = 0usize;
    for command in batch.commands() {
        let valid = match command {
            ScriptMutationCommand::CreateHtmlElement { token, .. }
            | ScriptMutationCommand::CreateText { token, .. } => {
                usize::try_from(token.index()) == Ok(created_nodes)
            }
            ScriptMutationCommand::AppendChild { parent, child }
            | ScriptMutationCommand::RemoveChild { parent, child } => {
                script_node_is_available(*parent, document, created_nodes)
                    && script_node_is_available(*child, document, created_nodes)
            }
            ScriptMutationCommand::InsertBefore {
                parent,
                child,
                reference,
            } => {
                script_node_is_available(*parent, document, created_nodes)
                    && script_node_is_available(*child, document, created_nodes)
                    && reference
                        .is_none_or(|node| script_node_is_available(node, document, created_nodes))
            }
            ScriptMutationCommand::SetHtmlAttribute { element, .. }
            | ScriptMutationCommand::RemoveHtmlAttribute { element, .. } => {
                script_node_is_available(*element, document, created_nodes)
            }
            ScriptMutationCommand::SetCharacterData { node, .. } => {
                script_node_is_available(*node, document, created_nodes)
            }
        };
        if !valid {
            return false;
        }
        if matches!(
            command,
            ScriptMutationCommand::CreateHtmlElement { .. }
                | ScriptMutationCommand::CreateText { .. }
        ) {
            let Some(next) = created_nodes.checked_add(1) else {
                return false;
            };
            created_nodes = next;
        }
    }
    created_nodes <= ScriptMutationLimits::HARD_MAX_CREATED_NODES
}

fn script_node_is_available(
    node: ScriptNode,
    document: wild_buzzard_dom::DocumentId,
    created_nodes: usize,
) -> bool {
    match node {
        ScriptNode::Existing(node) => node.document_id() == document,
        ScriptNode::Created(token) => {
            usize::try_from(token.index()).is_ok_and(|index| index < created_nodes)
        }
    }
}

fn created_nodes_are_distinct(created_nodes: &[NodeId]) -> bool {
    let mut unique = BTreeSet::new();
    created_nodes.iter().all(|node| unique.insert(*node))
}

fn invalidate_executor_and_shared_document(
    state: &mut SharedState,
    executor: &mut impl NavigationExecutor,
    navigation: NavigationId,
) {
    executor.invalidate_document(navigation.context());
    invalidate_shared_document(state, navigation);
    request_stop_locked(state, WorkerStopReason::ExecutorContractViolation);
}

fn publish_reserved_frame(
    state: &mut SharedState,
    limits: EngineLimits,
    navigation: NavigationId,
    lease: FrameLeaseId,
    frame: EngineFrame,
) -> Result<FrameMetadata, WorkerStopReason> {
    if state.next_frame_lease != lease.get() {
        return Err(WorkerStopReason::ExecutorContractViolation);
    }
    let retained_after = retained_after_replacement(state, limits, navigation, &frame)?
        .ok_or(WorkerStopReason::ExecutorContractViolation)?;
    let metadata = frame.metadata();
    state
        .contexts
        .get_mut(&navigation.context())
        .ok_or(WorkerStopReason::ExecutorContractViolation)?
        .current_frame = Some(StoredFrame {
        lease,
        navigation,
        frame,
    });
    state.retained_frame_bytes = retained_after;
    state.next_frame_lease = lease
        .get()
        .checked_add(1)
        .ok_or(WorkerStopReason::IdentityExhausted)?;
    Ok(metadata)
}

fn commit_mutation_result(
    state: &mut SharedState,
    limits: EngineLimits,
    work: &DocumentMutationWork,
    reservation: DocumentPublicationReservation,
    live_version: DocumentVersion,
    created_nodes: Box<[NodeId]>,
    rendered: bool,
) -> Result<(), WorkerStopReason> {
    let lease = reservation
        .result
        .ok_or(WorkerStopReason::ExecutorContractViolation)?;
    if state.next_mutation_result_lease != lease.get() {
        return Err(WorkerStopReason::ExecutorContractViolation);
    }
    let result_units = created_nodes.len().max(1);
    if result_units != work.reserved_result_units || state.mutation_results.contains_key(&lease) {
        return Err(WorkerStopReason::ExecutorContractViolation);
    }
    let retained_after = state
        .retained_mutation_result_nodes
        .checked_add(result_units)
        .ok_or(WorkerStopReason::ExecutorContractViolation)?;
    let pending_result_after = state
        .pending_mutation_result_nodes
        .checked_sub(work.reserved_result_units)
        .ok_or(WorkerStopReason::ExecutorContractViolation)?;
    if retained_after
        .checked_add(pending_result_after)
        .is_none_or(|total| total > limits.max_retained_mutation_result_nodes())
    {
        return Err(WorkerStopReason::ExecutorContractViolation);
    }
    let pending_payload_after = state
        .pending_mutation_payload_bytes
        .checked_sub(work.reserved_payload_bytes)
        .ok_or(WorkerStopReason::ExecutorContractViolation)?;
    let pending_document_after = state
        .pending_document_nodes
        .checked_sub(work.reserved_created_nodes)
        .ok_or(WorkerStopReason::ExecutorContractViolation)?;
    let retained_document_after = state
        .retained_document_nodes
        .checked_add(work.reserved_created_nodes)
        .ok_or(WorkerStopReason::ExecutorContractViolation)?;
    if retained_document_after
        .checked_add(pending_document_after)
        .is_none_or(|total| total > limits.max_retained_document_nodes())
    {
        return Err(WorkerStopReason::ExecutorContractViolation);
    }
    let document = state
        .contexts
        .get(&work.navigation.context())
        .and_then(|context| context.document)
        .filter(|document| document.navigation == work.navigation)
        .ok_or(WorkerStopReason::ExecutorContractViolation)?;
    let document_charge_after = document
        .node_charge
        .checked_add(work.reserved_created_nodes)
        .ok_or(WorkerStopReason::ExecutorContractViolation)?;
    let next_lease = lease
        .get()
        .checked_add(1)
        .ok_or(WorkerStopReason::IdentityExhausted)?;

    state.pending_document_nodes = pending_document_after;
    state.retained_document_nodes = retained_document_after;
    state.pending_mutation_result_nodes = pending_result_after;
    state.pending_mutation_payload_bytes = pending_payload_after;
    state.retained_mutation_result_nodes = retained_after;
    let retained_document = state
        .contexts
        .get_mut(&work.navigation.context())
        .and_then(|context| context.document.as_mut())
        .expect("the prevalidated retained document remains present under the worker lock");
    retained_document.live_version = live_version;
    if rendered {
        retained_document.frame_version = live_version;
    }
    retained_document.node_charge = document_charge_after;
    let replaced = state.mutation_results.insert(
        lease,
        StoredMutationResult {
            lease,
            navigation: work.navigation,
            operation: work.operation,
            live_version,
            created_nodes,
        },
    );
    debug_assert!(replaced.is_none());
    state.next_mutation_result_lease = next_lease;
    debug_assert!(
        state.retained_document_nodes + state.pending_document_nodes
            <= limits.max_retained_document_nodes()
    );
    Ok(())
}

fn invalidate_shared_document(state: &mut SharedState, navigation: NavigationId) {
    retire_current_style_document(state, navigation.context());
    let Some((frame_bytes, document_charge)) = state
        .contexts
        .get_mut(&navigation.context())
        .and_then(|context| {
            if context
                .document
                .is_none_or(|document| document.navigation != navigation)
            {
                return None;
            }
            let frame_bytes = context
                .current_frame
                .take()
                .map(|frame| frame.frame.metadata().total_bytes());
            let document_charge = context.document.take().map(|document| document.node_charge);
            Some((frame_bytes, document_charge))
        })
    else {
        return;
    };
    if let Some(frame_bytes) = frame_bytes {
        state.retained_frame_bytes = state
            .retained_frame_bytes
            .checked_sub(frame_bytes)
            .expect("invalidated frame was retained");
    }
    if let Some(document_charge) = document_charge {
        state.retained_document_nodes = state
            .retained_document_nodes
            .checked_sub(document_charge)
            .expect("invalidated document charge was retained");
    }
    remove_context_mutation_results(state, navigation.context());
}

fn retire_current_style_document(state: &mut SharedState, context: TopLevelContextId) {
    if let Some(commitment) = state.current_style_documents.remove(&context) {
        commitment.retire_style_document();
    }
}

fn publish_context_closed(
    shared: &Shared,
    navigation: NavigationId,
) -> Result<(), WorkerStopReason> {
    let mut state = lock_unpoisoned(&shared.state);
    if let Lifecycle::Stopping(reason) = state.lifecycle {
        return Err(reason);
    }
    if !state.closing_contexts.remove(&navigation.context()) {
        return Err(WorkerStopReason::ExecutorContractViolation);
    }
    if state.events.len() >= shared.limits.event_capacity() {
        request_stop_locked(&mut state, WorkerStopReason::EventQueueSaturated);
        return Err(WorkerStopReason::EventQueueSaturated);
    }
    let sequence = reserve_event_sequence(&mut state)?;
    state.events.push_back(EngineEvent {
        sequence,
        kind: EngineEventKind::ContextClosed { navigation },
    });
    drop(state);
    shared.event_ready.notify_one();
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NavigationEventPhase {
    Queued,
    Started,
    Committed,
    Terminal,
}

fn transition_event(
    phase: NavigationEventPhase,
    kind: EngineEventKind,
) -> Result<NavigationEventPhase, WorkerStopReason> {
    match (phase, kind) {
        (NavigationEventPhase::Queued, EngineEventKind::NavigationStarted { .. }) => {
            Ok(NavigationEventPhase::Started)
        }
        (
            NavigationEventPhase::Queued | NavigationEventPhase::Started,
            EngineEventKind::NavigationCancelled { .. },
        )
        | (NavigationEventPhase::Started, EngineEventKind::NavigationFailed { .. })
        | (NavigationEventPhase::Committed, EngineEventKind::FrameReady { .. }) => {
            Ok(NavigationEventPhase::Terminal)
        }
        (NavigationEventPhase::Started, EngineEventKind::NavigationCommitted { .. }) => {
            Ok(NavigationEventPhase::Committed)
        }
        _ => Err(WorkerStopReason::EventOrderViolation),
    }
}

fn enqueue_one(
    state: &mut SharedState,
    limits: EngineLimits,
    phase: &mut NavigationEventPhase,
    kind: EngineEventKind,
) -> Result<(), WorkerStopReason> {
    let next_phase = transition_event(*phase, kind)?;
    if state.events.len() >= limits.event_capacity() {
        return Err(WorkerStopReason::EventQueueSaturated);
    }
    let sequence = reserve_event_sequence(state)?;
    state.events.push_back(EngineEvent { sequence, kind });
    *phase = next_phase;
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn publish_success(
    state: &mut SharedState,
    limits: EngineLimits,
    phase: &mut NavigationEventPhase,
    navigation: NavigationId,
    request: &NavigationRequest,
    mut output: ExecutorOutput,
) -> Result<bool, WorkerStopReason> {
    if !is_current(state, navigation) {
        return Err(WorkerStopReason::EventOrderViolation);
    }
    let Some(retained_after) =
        retained_after_replacement(state, limits, navigation, &output.frame)?
    else {
        reject_navigation_resource_limit(
            state,
            limits,
            phase,
            navigation,
            NavigationStage::Render,
        )?;
        return Ok(false);
    };
    if limits.event_capacity().saturating_sub(state.events.len()) < 2 {
        return Err(WorkerStopReason::EventQueueSaturated);
    }
    let old_charge = state
        .contexts
        .get(&navigation.context())
        .and_then(|context| context.document)
        .map_or(0, |document| document.node_charge);
    let retained_without_old = state
        .retained_document_nodes
        .checked_sub(old_charge)
        .ok_or(WorkerStopReason::ExecutorContractViolation)?;
    let (replacement_document, retained_document_nodes) =
        match (output.frame.document_version(), output.document_node_charge) {
            (Some(version), Some(node_charge)) => {
                let retained_after = retained_without_old
                    .checked_add(node_charge)
                    .ok_or(WorkerStopReason::ExecutorContractViolation)?;
                (
                    Some(RetainedDocumentState {
                        navigation,
                        live_version: version,
                        frame_version: version,
                        node_charge,
                    }),
                    retained_after,
                )
            }
            (None, None) => (None, retained_without_old),
            (Some(_), None) | (None, Some(_)) => {
                return Err(WorkerStopReason::ExecutorContractViolation);
            }
        };
    if retained_document_nodes
        .checked_add(state.pending_document_nodes)
        .is_none_or(|total| total > limits.max_retained_document_nodes())
    {
        reject_navigation_resource_limit(
            state,
            limits,
            phase,
            navigation,
            NavigationStage::Document,
        )?;
        return Ok(false);
    }

    let navigation_commit = output
        .navigation_commit
        .take()
        .unwrap_or_else(|| NavigationCommitMetadata::unverified_requested(request));

    let commit_kind = EngineEventKind::NavigationCommitted {
        navigation,
        http_status: output.http_status,
    };
    let committed_phase = transition_event(*phase, commit_kind)?;
    let lease_raw = state
        .next_frame_lease
        .checked_add(1)
        .ok_or(WorkerStopReason::IdentityExhausted)?;
    let lease = FrameLeaseId(
        NonZeroU64::new(state.next_frame_lease).ok_or(WorkerStopReason::IdentityExhausted)?,
    );
    let frame_kind = EngineEventKind::FrameReady {
        navigation,
        lease,
        metadata: output.frame.metadata(),
    };
    let terminal_phase = transition_event(committed_phase, frame_kind)?;
    if state.navigation_commits.contains_key(&navigation)
        || state.navigation_commits.len() >= limits.event_capacity()
    {
        return Err(WorkerStopReason::ExecutorContractViolation);
    }
    let sequences = reserve_event_pair(state)?;

    retire_current_style_document(state, navigation.context());
    if replacement_document.is_some()
        && state
            .current_style_documents
            .insert(navigation.context(), navigation_commit.clone())
            .is_some()
    {
        return Err(WorkerStopReason::ExecutorContractViolation);
    }
    remove_context_mutation_results(state, navigation.context());

    {
        let context = state
            .contexts
            .get_mut(&navigation.context())
            .ok_or(WorkerStopReason::EventOrderViolation)?;
        if context.latest_generation != navigation.generation() {
            return Err(WorkerStopReason::EventOrderViolation);
        }
        context.current_frame = Some(StoredFrame {
            lease,
            navigation,
            frame: output.frame,
        });
        if context
            .active_cancellation
            .as_ref()
            .is_some_and(|active| active.is_navigation(navigation))
        {
            context.active_cancellation = None;
        }
        context.document = replacement_document;
    }
    state.retained_frame_bytes = retained_after;
    state.retained_document_nodes = retained_document_nodes;
    state.next_frame_lease = lease_raw;
    if state
        .navigation_commits
        .insert(navigation, navigation_commit)
        .is_some()
    {
        return Err(WorkerStopReason::ExecutorContractViolation);
    }
    state.events.push_back(EngineEvent {
        sequence: sequences[0],
        kind: commit_kind,
    });
    state.events.push_back(EngineEvent {
        sequence: sequences[1],
        kind: frame_kind,
    });
    *phase = terminal_phase;
    Ok(true)
}

fn retained_after_replacement(
    state: &SharedState,
    limits: EngineLimits,
    navigation: NavigationId,
    frame: &EngineFrame,
) -> Result<Option<usize>, WorkerStopReason> {
    let frame_bytes = frame.metadata().total_bytes();
    if frame_bytes > limits.max_frame_bytes() {
        return Ok(None);
    }
    let old_bytes = state
        .contexts
        .get(&navigation.context())
        .and_then(|context| context.current_frame.as_ref())
        .map_or(0, |stored| stored.frame.metadata().total_bytes());
    let retained_without_old = state
        .retained_frame_bytes
        .checked_sub(old_bytes)
        .ok_or(WorkerStopReason::IdentityExhausted)?;
    let retained_after = retained_without_old
        .checked_add(frame_bytes)
        .ok_or(WorkerStopReason::IdentityExhausted)?;
    if retained_after > limits.max_retained_frame_bytes() {
        return Ok(None);
    }
    Ok(Some(retained_after))
}

fn reject_navigation_resource_limit(
    state: &mut SharedState,
    limits: EngineLimits,
    phase: &mut NavigationEventPhase,
    navigation: NavigationId,
    failure_stage: NavigationStage,
) -> Result<(), WorkerStopReason> {
    enqueue_one(
        state,
        limits,
        phase,
        EngineEventKind::NavigationFailed {
            navigation,
            failure: ExecutionFailure::new(ExecutionFailureKind::ResourceLimit, failure_stage),
        },
    )?;
    clear_navigation_cancellation_if_current(state, navigation);
    Ok(())
}

fn reserve_event_sequence(state: &mut SharedState) -> Result<EventSequence, WorkerStopReason> {
    let raw = state.next_event_sequence;
    let next = raw
        .checked_add(1)
        .ok_or(WorkerStopReason::IdentityExhausted)?;
    let sequence = EventSequence(NonZeroU64::new(raw).ok_or(WorkerStopReason::IdentityExhausted)?);
    state.next_event_sequence = next;
    Ok(sequence)
}

fn reserve_event_pair(state: &mut SharedState) -> Result<[EventSequence; 2], WorkerStopReason> {
    let first_raw = state.next_event_sequence;
    let second_raw = first_raw
        .checked_add(1)
        .ok_or(WorkerStopReason::IdentityExhausted)?;
    let next = second_raw
        .checked_add(1)
        .ok_or(WorkerStopReason::IdentityExhausted)?;
    let first =
        EventSequence(NonZeroU64::new(first_raw).ok_or(WorkerStopReason::IdentityExhausted)?);
    let second =
        EventSequence(NonZeroU64::new(second_raw).ok_or(WorkerStopReason::IdentityExhausted)?);
    state.next_event_sequence = next;
    Ok([first, second])
}

fn finish_worker(shared: &Shared, status: EngineShutdownStatus) {
    let mut state = lock_unpoisoned(&shared.state);
    if matches!(state.lifecycle, Lifecycle::Stopped(_)) {
        return;
    }
    for context in state.contexts.values_mut() {
        if let Some(active) = context.active_cancellation.take() {
            active.cancel();
        }
    }
    for commitment in state.current_style_documents.values() {
        commitment.retire_style_document();
    }
    state.current_style_documents.clear();
    state.commands.clear();
    state.context_closures.clear();
    state.closing_contexts.clear();
    state.pending_document_nodes = 0;
    state.pending_mutation_result_nodes = 0;
    state.pending_mutation_payload_bytes = 0;
    state.lifecycle = Lifecycle::Stopped(status);
    if state.receiver_open {
        if let Ok(sequence) = reserve_event_sequence(&mut state) {
            state.terminal_event = Some(EngineEvent {
                sequence,
                kind: EngineEventKind::ShutdownComplete { status },
            });
        }
    } else {
        for context in state.contexts.values_mut() {
            context.current_frame = None;
        }
        state.retained_frame_bytes = 0;
    }
    drop(state);
    shared.event_ready.notify_all();
    shared.command_ready.notify_all();
}

fn force_worker_stopped(shared: &Shared, status: EngineShutdownStatus) {
    let mut state = lock_unpoisoned(&shared.state);
    if !matches!(state.lifecycle, Lifecycle::Stopped(_)) {
        state.lifecycle = Lifecycle::Stopped(status);
    }
    drop(state);
    shared.event_ready.notify_all();
}

struct RetainedExecutorDocument {
    document: DetachedLiveDocument,
    node_charge: usize,
}

struct PendingNavigationDocument {
    navigation: NavigationId,
    previous: Option<RetainedExecutorDocument>,
    replacement: RetainedExecutorDocument,
    retained_nodes_after: usize,
}

struct StaticPipelineExecutor {
    engine: Option<StaticPageEngine>,
    output: StaticPipelineOutput,
    network_capability: NavigationNetworkCapability,
    documents: BTreeMap<TopLevelContextId, RetainedExecutorDocument>,
    pending_navigation: Option<PendingNavigationDocument>,
    retained_document_nodes: usize,
    max_retained_document_nodes: usize,
}

type LoadedNavigation = (
    u16,
    NavigationCommitMetadata,
    DocumentVersion,
    usize,
    EngineFrame,
);

#[derive(Clone, Copy)]
enum StaticPipelineOutput {
    Headless,
    Presentation,
}

impl StaticPipelineExecutor {
    fn new(
        config: StaticPageConfig,
        limits: EngineLimits,
        output: StaticPipelineOutput,
    ) -> Result<Self, ExecutionFailure> {
        Self::validate_frame_limit(&config, limits)?;
        let engine = match output {
            StaticPipelineOutput::Headless => StaticPageEngine::new(config),
            StaticPipelineOutput::Presentation => StaticPageEngine::new_for_presentation(config),
        }
        .map_err(|error| map_pipeline_error(&error))?;
        Ok(Self::from_engine(
            engine,
            output,
            NavigationNetworkCapability::NumericLoopback,
            limits,
        ))
    }

    fn new_general_web(
        config: StaticPageConfig,
        general_web: GeneralWebConfig,
        trust_store: TrustStore,
        limits: EngineLimits,
        output: StaticPipelineOutput,
    ) -> Result<Self, ExecutionFailure> {
        Self::validate_frame_limit(&config, limits)?;
        let engine = match output {
            StaticPipelineOutput::Headless => {
                StaticPageEngine::new_general_web(config, general_web, trust_store)
            }
            StaticPipelineOutput::Presentation => {
                StaticPageEngine::new_general_web_for_presentation(config, general_web, trust_store)
            }
        }
        .map_err(|error| map_pipeline_error(&error))?;
        Ok(Self::from_engine(
            engine,
            output,
            NavigationNetworkCapability::GeneralWeb,
            limits,
        ))
    }

    fn validate_frame_limit(
        config: &StaticPageConfig,
        limits: EngineLimits,
    ) -> Result<(), ExecutionFailure> {
        let configured_frame_bytes = usize::try_from(config.viewport_width)
            .ok()
            .and_then(|width| {
                usize::try_from(config.viewport_height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(RGBA8_BYTES_PER_PIXEL))
            .ok_or_else(|| {
                ExecutionFailure::new(ExecutionFailureKind::ResourceLimit, NavigationStage::Render)
            })?;
        if configured_frame_bytes > limits.max_frame_bytes() {
            return Err(ExecutionFailure::new(
                ExecutionFailureKind::ResourceLimit,
                NavigationStage::Render,
            ));
        }
        Ok(())
    }

    fn from_engine(
        engine: StaticPageEngine,
        output: StaticPipelineOutput,
        network_capability: NavigationNetworkCapability,
        limits: EngineLimits,
    ) -> Self {
        Self {
            engine: Some(engine),
            output,
            network_capability,
            documents: BTreeMap::new(),
            pending_navigation: None,
            retained_document_nodes: 0,
            max_retained_document_nodes: limits.max_retained_document_nodes(),
        }
    }

    fn engine_mut(&mut self) -> Result<&mut StaticPageEngine, ExecutionFailure> {
        self.engine.as_mut().ok_or_else(|| {
            ExecutionFailure::new(ExecutionFailureKind::Internal, NavigationStage::Render)
        })
    }

    fn restore_document(
        &mut self,
        context: TopLevelContextId,
        document: Option<RetainedExecutorDocument>,
    ) {
        if let Some(document) = document {
            let replaced = self.documents.insert(context, document);
            debug_assert!(replaced.is_none());
        }
    }

    fn renderer_unusable(&self) -> bool {
        self.engine
            .as_ref()
            .is_some_and(|engine| !engine.renderer_is_usable())
    }

    fn retain_committed_document(
        &mut self,
        context: TopLevelContextId,
        document: DetachedLiveDocument,
        previous_node_charge: usize,
        created_nodes: usize,
    ) -> Result<(), ()> {
        let node_charge = previous_node_charge.checked_add(created_nodes).ok_or(())?;
        let retained_document_nodes = self
            .retained_document_nodes
            .checked_add(created_nodes)
            .filter(|total| *total <= self.max_retained_document_nodes)
            .ok_or(())?;
        if self.documents.contains_key(&context) {
            return Err(());
        }
        self.documents.insert(
            context,
            RetainedExecutorDocument {
                document,
                node_charge,
            },
        );
        self.retained_document_nodes = retained_document_nodes;
        Ok(())
    }

    fn load_navigation(
        &mut self,
        request: &NavigationRequest,
        cancellation: &CancellationToken,
    ) -> Option<Result<LoadedNavigation, PipelineError>> {
        let output = self.output;
        let network_capability = self.network_capability;
        let engine = self.engine.as_mut()?;
        Some(match (output, network_capability) {
            (StaticPipelineOutput::Headless, NavigationNetworkCapability::NumericLoopback) => {
                engine
                    .load(request.url(), cancellation)
                    .and_then(frame_from_headless_page)
            }
            (StaticPipelineOutput::Headless, NavigationNetworkCapability::GeneralWeb) => engine
                .load_general_web(request.url(), cancellation)
                .and_then(frame_from_headless_page),
            (StaticPipelineOutput::Presentation, NavigationNetworkCapability::NumericLoopback) => {
                engine
                    .load_for_presentation(request.url(), cancellation)
                    .and_then(frame_from_presentation_page)
            }
            (StaticPipelineOutput::Presentation, NavigationNetworkCapability::GeneralWeb) => engine
                .load_general_web_for_presentation(request.url(), cancellation)
                .and_then(frame_from_presentation_page),
        })
    }
}

fn frame_from_headless_page(
    rendered: RenderedStaticPage,
) -> Result<
    (
        u16,
        NavigationCommitMetadata,
        DocumentVersion,
        usize,
        EngineFrame,
    ),
    PipelineError,
> {
    let http_status = rendered.evidence.http_status;
    let navigation_commit = rendered.evidence.navigation_commit.clone();
    let document_version = rendered.evidence.document_version;
    let node_charge = rendered.evidence.dom_nodes;
    EngineFrame::from_rendered(rendered)
        .map(|frame| {
            (
                http_status,
                navigation_commit,
                document_version,
                node_charge,
                frame,
            )
        })
        .map_err(|_| PipelineError::InvalidConfiguration {
            field: "engine_frame",
            detail: "headless pipeline produced an invalid frame lease",
        })
}

fn frame_from_presentation_page(
    rendered: RenderedPresentationPage,
) -> Result<
    (
        u16,
        NavigationCommitMetadata,
        DocumentVersion,
        usize,
        EngineFrame,
    ),
    PipelineError,
> {
    let RenderedPresentationPage {
        evidence, scene, ..
    } = rendered;
    let http_status = evidence.http_status;
    let navigation_commit = evidence.navigation_commit;
    let document_version = evidence.document_version;
    let node_charge = evidence.dom_nodes;
    EngineFrame::from_presentation(scene)
        .map(|frame| {
            (
                http_status,
                navigation_commit,
                document_version,
                node_charge,
                frame,
            )
        })
        .map_err(|_| PipelineError::InvalidConfiguration {
            field: "engine_frame",
            detail: "presentation pipeline produced an invalid scene lease",
        })
}

impl NavigationExecutor for StaticPipelineExecutor {
    fn execute(
        &mut self,
        navigation: NavigationId,
        request: &NavigationRequest,
        cancellation: &CancellationToken,
    ) -> Result<ExecutorOutput, ExecutionFailure> {
        if request.network_capability() != self.network_capability {
            return Err(ExecutionFailure::new(
                ExecutionFailureKind::Rejected,
                NavigationStage::Fetch,
            ));
        }
        if self.pending_navigation.is_some() {
            return Err(ExecutionFailure::new(
                ExecutionFailureKind::Internal,
                NavigationStage::Document,
            ));
        }
        let context = navigation.context();
        let previous = self.documents.remove(&context);
        if self.engine_mut()?.replace_live_document(None).is_some() {
            self.restore_document(context, previous);
            return Err(ExecutionFailure::new(
                ExecutionFailureKind::Internal,
                NavigationStage::Document,
            ));
        }
        let Some(loaded) = self.load_navigation(request, cancellation) else {
            self.restore_document(context, previous);
            return Err(ExecutionFailure::new(
                ExecutionFailureKind::Internal,
                NavigationStage::Render,
            ));
        };
        let (http_status, navigation_commit, document_version, node_charge, frame) = match loaded {
            Ok(loaded) => loaded,
            Err(error) => {
                self.restore_document(context, previous);
                let mut failure = map_pipeline_error(&error);
                if self.renderer_unusable() {
                    failure = failure.mark_renderer_unusable();
                }
                return Err(failure);
            }
        };
        if navigation_commit
            .bind_navigation(navigation, document_version)
            .is_err()
        {
            if let Some(engine) = self.engine.as_mut() {
                drop(engine.replace_live_document(None));
            }
            self.restore_document(context, previous);
            return Err(ExecutionFailure::new(
                ExecutionFailureKind::Internal,
                NavigationStage::Document,
            ));
        }
        let Some(replacement_document) = self.engine_mut()?.replace_live_document(None) else {
            self.restore_document(context, previous);
            return Err(ExecutionFailure::new(
                ExecutionFailureKind::Internal,
                NavigationStage::Document,
            ));
        };
        let old_charge = previous.as_ref().map_or(0, |document| document.node_charge);
        let retained_nodes_after = self
            .retained_document_nodes
            .checked_sub(old_charge)
            .and_then(|retained| retained.checked_add(node_charge));
        let Some(retained_nodes_after) =
            retained_nodes_after.filter(|retained| *retained <= self.max_retained_document_nodes)
        else {
            self.restore_document(context, previous);
            return Err(ExecutionFailure::new(
                ExecutionFailureKind::ResourceLimit,
                NavigationStage::Document,
            ));
        };
        let output = match ExecutorOutput::new_document(
            http_status,
            frame,
            DocumentLoadProof::from_pipeline(document_version, node_charge),
        ) {
            Ok(output) => output.with_navigation_commit(navigation_commit),
            Err(error) => {
                self.restore_document(context, previous);
                return Err(error);
            }
        };
        self.pending_navigation = Some(PendingNavigationDocument {
            navigation,
            previous,
            replacement: RetainedExecutorDocument {
                document: replacement_document,
                node_charge,
            },
            retained_nodes_after,
        });
        Ok(output)
    }

    #[allow(clippy::too_many_lines)]
    fn mutate_document(
        &mut self,
        navigation: NavigationId,
        batch: ScriptMutationBatch,
        cancellation: &CancellationToken,
    ) -> ExecutorDocumentMutation {
        let context = navigation.context();
        let Some(retained) = self.documents.remove(&context) else {
            return ExecutorDocumentMutation::Rejected {
                live_version: None,
                frame_version: None,
                failure: DocumentOperationFailure::NoLiveDocument,
            };
        };
        let node_charge = retained.node_charge;
        let Some(engine) = self.engine.as_mut() else {
            self.restore_document(context, Some(retained));
            return ExecutorDocumentMutation::Rejected {
                live_version: None,
                frame_version: None,
                failure: DocumentOperationFailure::Internal,
            };
        };
        if engine
            .replace_live_document(Some(retained.document))
            .is_some()
        {
            return ExecutorDocumentMutation::Rejected {
                live_version: None,
                frame_version: None,
                failure: DocumentOperationFailure::Internal,
            };
        }
        let result = engine.apply_for_navigation(batch, cancellation);
        let renderer_unusable = !engine.renderer_is_usable();
        let document = engine
            .replace_live_document(None)
            .expect("a dynamic operation retains its activated page");

        match result {
            Ok(rendered) => {
                let live_version = rendered.evidence.document_version;
                let commit = rendered.commit;
                let frame = match rendered.frame {
                    PipelineFrame::Headless(frame) => {
                        EngineFrame::from_headless(frame, live_version)
                    }
                    PipelineFrame::Presentation(scene) => EngineFrame::from_presentation(*scene),
                };
                let Ok(frame) = frame else {
                    return ExecutorDocumentMutation::Invalidated;
                };
                if self
                    .retain_committed_document(
                        context,
                        document,
                        node_charge,
                        commit.created_nodes().len(),
                    )
                    .is_err()
                {
                    return ExecutorDocumentMutation::Invalidated;
                }
                ExecutorDocumentMutation::Rendered {
                    previous_live_version: rendered.previous_live_version,
                    previous_frame_version: rendered.previous_last_returned_frame_version,
                    commit,
                    frame,
                }
            }
            Err(crate::DocumentUpdateError::Committed {
                previous_live_version,
                last_returned_frame_version,
                commit,
                source,
            }) => {
                if self
                    .retain_committed_document(
                        context,
                        document,
                        node_charge,
                        commit.created_nodes().len(),
                    )
                    .is_err()
                {
                    return ExecutorDocumentMutation::Invalidated;
                }
                ExecutorDocumentMutation::CommittedWithoutFrame {
                    previous_live_version,
                    frame_version: last_returned_frame_version,
                    commit,
                    failure: if renderer_unusable {
                        DocumentOperationFailure::RendererUnavailable
                    } else {
                        map_document_pipeline_error(&source)
                    },
                }
            }
            Err(crate::DocumentUpdateError::Rejected {
                live_version,
                last_returned_frame_version,
                reason,
            }) => {
                self.documents.insert(
                    context,
                    RetainedExecutorDocument {
                        document,
                        node_charge,
                    },
                );
                ExecutorDocumentMutation::Rejected {
                    live_version,
                    frame_version: last_returned_frame_version,
                    failure: map_document_rejection(&reason),
                }
            }
        }
    }

    fn rerender_document(
        &mut self,
        navigation: NavigationId,
        expected_live_version: DocumentVersion,
        cancellation: &CancellationToken,
    ) -> ExecutorDocumentRerender {
        let context = navigation.context();
        let Some(retained) = self.documents.remove(&context) else {
            return ExecutorDocumentRerender::Rejected {
                live_version: None,
                frame_version: None,
                failure: DocumentOperationFailure::NoLiveDocument,
            };
        };
        let node_charge = retained.node_charge;
        let Some(engine) = self.engine.as_mut() else {
            self.restore_document(context, Some(retained));
            return ExecutorDocumentRerender::Rejected {
                live_version: None,
                frame_version: None,
                failure: DocumentOperationFailure::Internal,
            };
        };
        if engine
            .replace_live_document(Some(retained.document))
            .is_some()
        {
            return ExecutorDocumentRerender::Rejected {
                live_version: None,
                frame_version: None,
                failure: DocumentOperationFailure::Internal,
            };
        }
        let result = engine.rerender_for_navigation(expected_live_version, cancellation);
        let renderer_unusable = !engine.renderer_is_usable();
        let document = engine
            .replace_live_document(None)
            .expect("a rerender retains its activated page");
        self.documents.insert(
            context,
            RetainedExecutorDocument {
                document,
                node_charge,
            },
        );
        match result {
            Ok(rendered) => {
                let live_version = rendered.evidence.document_version;
                let frame = match rendered.frame {
                    PipelineFrame::Headless(frame) => {
                        EngineFrame::from_headless(frame, live_version)
                    }
                    PipelineFrame::Presentation(scene) => EngineFrame::from_presentation(*scene),
                };
                let Ok(frame) = frame else {
                    self.invalidate_document(context);
                    return ExecutorDocumentRerender::Invalidated;
                };
                ExecutorDocumentRerender::Rendered {
                    live_version,
                    previous_frame_version: rendered.previous_last_returned_frame_version,
                    frame,
                }
            }
            Err(crate::DocumentUpdateError::Rejected {
                live_version,
                last_returned_frame_version,
                reason,
            }) => ExecutorDocumentRerender::Rejected {
                live_version,
                frame_version: last_returned_frame_version,
                failure: if renderer_unusable {
                    DocumentOperationFailure::RendererUnavailable
                } else {
                    map_document_rejection(&reason)
                },
            },
            Err(crate::DocumentUpdateError::Committed { .. }) => {
                self.invalidate_document(context);
                ExecutorDocumentRerender::Invalidated
            }
        }
    }

    fn acknowledge_navigation_publication(&mut self, navigation: NavigationId, published: bool) {
        let Some(pending) = self.pending_navigation.take() else {
            return;
        };
        if pending.navigation != navigation {
            self.restore_document(pending.navigation.context(), pending.previous);
            return;
        }
        let context = navigation.context();
        if published {
            self.retained_document_nodes = pending.retained_nodes_after;
            self.documents.insert(context, pending.replacement);
        } else {
            self.restore_document(context, pending.previous);
        }
    }

    fn invalidate_document(&mut self, context: TopLevelContextId) {
        if let Some(document) = self.documents.remove(&context) {
            if let Some(retained) = self
                .retained_document_nodes
                .checked_sub(document.node_charge)
            {
                self.retained_document_nodes = retained;
            } else {
                self.retained_document_nodes = 0;
            }
        }
        if self
            .pending_navigation
            .as_ref()
            .is_some_and(|pending| pending.navigation.context() == context)
        {
            self.pending_navigation = None;
        }
    }

    fn close_context(&mut self, context: TopLevelContextId) {
        self.invalidate_document(context);
    }

    fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
        self.pending_navigation = None;
        self.documents.clear();
        self.retained_document_nodes = 0;
        let Some(engine) = self.engine.take() else {
            return Ok(());
        };
        engine.shutdown().map(|_| ()).map_err(|error| {
            let mut failure = map_pipeline_error(&error);
            failure.stage = NavigationStage::Shutdown;
            failure
        })
    }
}

fn map_pipeline_error(error: &PipelineError) -> ExecutionFailure {
    match error {
        PipelineError::Cancelled { stage } => {
            ExecutionFailure::new(ExecutionFailureKind::Cancelled, map_pipeline_stage(*stage))
        }
        PipelineError::DeadlineExceeded { stage } => ExecutionFailure::new(
            ExecutionFailureKind::DeadlineExceeded,
            map_pipeline_stage(*stage),
        ),
        PipelineError::Network(_) => {
            ExecutionFailure::new(ExecutionFailureKind::Network, NavigationStage::Fetch)
        }
        PipelineError::DocumentPolicy(crate::DocumentPolicyError::BindingMismatch) => {
            ExecutionFailure::new(ExecutionFailureKind::Internal, NavigationStage::Document)
        }
        PipelineError::DocumentPolicy(
            crate::DocumentPolicyError::LimitExceeded { .. }
            | crate::DocumentPolicyError::CounterOverflow { .. }
            | crate::DocumentPolicyError::AllocationFailed { .. },
        ) => ExecutionFailure::new(ExecutionFailureKind::ResourceLimit, NavigationStage::Fetch),
        PipelineError::InvalidConfiguration { .. }
        | PipelineError::DeadlineOverflow
        | PipelineError::EvidenceOverflow
        | PipelineError::EpochExhausted
        | PipelineError::PresentationRevisionExhausted => {
            ExecutionFailure::new(ExecutionFailureKind::ResourceLimit, NavigationStage::Render)
        }
        PipelineError::RedirectLocation(_)
        | PipelineError::UnsupportedRedirectStatus { .. }
        | PipelineError::RedirectLoop
        | PipelineError::TooManyRedirects { .. }
        | PipelineError::TransportSecurityMismatch => {
            ExecutionFailure::new(ExecutionFailureKind::Rejected, NavigationStage::Fetch)
        }
        PipelineError::HttpStatus(_) | PipelineError::NonUtf8Html => {
            ExecutionFailure::new(ExecutionFailureKind::Rejected, NavigationStage::Document)
        }
        PipelineError::Html(_) | PipelineError::Dom(_) => {
            ExecutionFailure::new(ExecutionFailureKind::Document, NavigationStage::Document)
        }
        PipelineError::Style(_) => {
            ExecutionFailure::new(ExecutionFailureKind::Document, NavigationStage::Style)
        }
        PipelineError::Layout(_) | PipelineError::Text(_) => {
            ExecutionFailure::new(ExecutionFailureKind::Document, NavigationStage::Layout)
        }
        PipelineError::Scene(_) | PipelineError::Headless(_) => {
            ExecutionFailure::new(ExecutionFailureKind::Rendering, NavigationStage::Render)
        }
    }
}

fn map_document_rejection(reason: &crate::DocumentUpdateRejection) -> DocumentOperationFailure {
    match reason {
        crate::DocumentUpdateRejection::NoLiveDocument => DocumentOperationFailure::NoLiveDocument,
        crate::DocumentUpdateRejection::RendererUnavailable => {
            DocumentOperationFailure::RendererUnavailable
        }
        crate::DocumentUpdateRejection::LiveVersionMismatch { .. } => {
            DocumentOperationFailure::VersionMismatch
        }
        crate::DocumentUpdateRejection::Mutation(_) => DocumentOperationFailure::MutationRejected,
        crate::DocumentUpdateRejection::Pipeline(error) => map_document_pipeline_error(error),
    }
}

fn map_document_pipeline_error(error: &PipelineError) -> DocumentOperationFailure {
    match map_pipeline_error(error).kind() {
        ExecutionFailureKind::Cancelled => DocumentOperationFailure::Cancelled,
        ExecutionFailureKind::DeadlineExceeded => DocumentOperationFailure::DeadlineExceeded,
        ExecutionFailureKind::Rejected | ExecutionFailureKind::Document => {
            DocumentOperationFailure::Document
        }
        ExecutionFailureKind::Rendering => DocumentOperationFailure::Rendering,
        ExecutionFailureKind::ResourceLimit => DocumentOperationFailure::ResourceLimit,
        ExecutionFailureKind::Internal | ExecutionFailureKind::Network => {
            DocumentOperationFailure::Internal
        }
    }
}

const fn map_pipeline_stage(stage: PipelineStage) -> NavigationStage {
    match stage {
        PipelineStage::Fetch => NavigationStage::Fetch,
        PipelineStage::Parse | PipelineStage::Snapshot => NavigationStage::Document,
        PipelineStage::Style => NavigationStage::Style,
        PipelineStage::Layout | PipelineStage::TextShaping => NavigationStage::Layout,
        PipelineStage::SceneCompilation | PipelineStage::ComposedRender => NavigationStage::Render,
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn navigation_commit_clones_share_the_exact_final_url_allocation() {
        let commitment = NavigationCommitMetadata::new(
            "https://example.com/final?q=rust#section",
            7,
            NavigationConnectionSecurity::AuthenticatedTls {
                version: NavigationTlsVersion::Tls13,
                alpn: NavigationAlpn::Http11,
            },
            true,
        )
        .unwrap();
        let cloned = commitment.clone();

        assert!(Arc::ptr_eq(&commitment.final_url, &cloned.final_url));
        assert_eq!(Arc::strong_count(&commitment.final_url), 2);
        assert_eq!(cloned.final_url(), commitment.final_url());
        assert_eq!(cloned.redirect_count(), commitment.redirect_count());
        assert_eq!(cloned.security(), commitment.security());
        assert_eq!(
            cloned.had_https_downgrade(),
            commitment.had_https_downgrade()
        );
        assert_eq!(cloned, commitment);
    }

    #[test]
    fn style_document_ledger_has_one_issuance_and_one_transaction() {
        let lifecycle = StyleDocumentLifecycle::new();
        let current_owner = StyleDocumentCurrentOwner::new(Arc::clone(&lifecycle));
        let capability = lifecycle
            .issue(StyleDocumentAuthorityClass::NonProduct)
            .expect("issue direct authority once");
        assert!(matches!(
            lifecycle.issue(StyleDocumentAuthorityClass::NonProduct),
            Err(StyleDocumentAccessError::AlreadyIssued)
        ));

        drop(
            capability
                .begin_transaction()
                .expect("begin the sole transaction"),
        );
        assert_eq!(
            capability.begin_transaction().err(),
            Some(StyleDocumentAccessError::TransactionConsumed)
        );

        drop(current_owner);
        assert_eq!(
            capability.ensure_current(),
            Err(StyleDocumentAccessError::Retired)
        );
    }

    #[test]
    fn product_style_ledger_requires_one_exact_navigation_binding() {
        let lifecycle = StyleDocumentLifecycle::new();
        let current_owner = StyleDocumentCurrentOwner::new(Arc::clone(&lifecycle));
        let exact_navigation = navigation();
        let foreign_navigation = NavigationId::new(
            TopLevelContextId::new(2).unwrap(),
            NavigationGeneration::INITIAL,
        );

        assert!(matches!(
            lifecycle.issue(StyleDocumentAuthorityClass::Product),
            Err(StyleDocumentAccessError::ProductNavigationRequired)
        ));
        lifecycle
            .bind_navigation(exact_navigation)
            .expect("bind exact product navigation");
        lifecycle
            .bind_navigation(exact_navigation)
            .expect("same binding is idempotent");
        assert_eq!(
            lifecycle.bind_navigation(foreign_navigation),
            Err(StyleDocumentAccessError::ProductNavigationRequired)
        );
        assert!(matches!(
            lifecycle.issue(StyleDocumentAuthorityClass::NonProduct),
            Err(StyleDocumentAccessError::NonProductNavigationBound)
        ));
        let capability = lifecycle
            .issue(StyleDocumentAuthorityClass::Product)
            .expect("issue exact product authority");

        current_owner.retire();
        assert_eq!(
            capability.ensure_current(),
            Err(StyleDocumentAccessError::Retired)
        );
    }

    #[test]
    fn active_style_transaction_linearizes_before_retirement() {
        let lifecycle = StyleDocumentLifecycle::new();
        let current_owner = StyleDocumentCurrentOwner::new(Arc::clone(&lifecycle));
        let capability = lifecycle
            .issue(StyleDocumentAuthorityClass::NonProduct)
            .expect("issue direct authority");
        let (entered_sender, entered_receiver) = mpsc::sync_channel(1);
        let (release_sender, release_receiver) = mpsc::sync_channel(1);
        let transaction = std::thread::spawn(move || {
            let guard = capability
                .begin_transaction()
                .expect("begin active transaction");
            entered_sender.send(()).unwrap();
            release_receiver.recv().unwrap();
            drop(guard);
        });
        entered_receiver.recv().unwrap();

        let (retired_sender, retired_receiver) = mpsc::sync_channel(1);
        let retirement = std::thread::spawn(move || {
            current_owner.retire();
            retired_sender.send(()).unwrap();
        });
        assert_eq!(
            retired_receiver.recv_timeout(Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );

        release_sender.send(()).unwrap();
        transaction.join().unwrap();
        retired_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("retirement completes after transaction release");
        retirement.join().unwrap();
        assert!(matches!(
            &lifecycle.lock().status,
            StyleDocumentStatus::Retired
        ));
    }

    struct GatedSuccessExecutor {
        entered: mpsc::Sender<NavigationId>,
        release: mpsc::Receiver<()>,
    }

    impl NavigationExecutor for GatedSuccessExecutor {
        fn execute(
            &mut self,
            navigation: NavigationId,
            _request: &NavigationRequest,
            _cancellation: &CancellationToken,
        ) -> Result<ExecutorOutput, ExecutionFailure> {
            self.entered.send(navigation).unwrap();
            self.release.recv().unwrap();
            ExecutorOutput::new(200, frame(7))
        }

        fn shutdown(&mut self) -> Result<(), ExecutionFailure> {
            Ok(())
        }
    }

    fn context() -> TopLevelContextId {
        TopLevelContextId::new(1).unwrap()
    }

    fn navigation() -> NavigationId {
        NavigationId::new(context(), NavigationGeneration::INITIAL)
    }

    fn frame(marker: u8) -> EngineFrame {
        EngineFrame::from_rgba8(PixelSize::new(1, 1).unwrap(), vec![marker, 0, 0, 255]).unwrap()
    }

    fn state_with_prior_frame() -> (SharedState, EngineLimits, NavigationId) {
        let limits = EngineLimits::new(1, 3, 1, 4, 4).unwrap();
        let navigation = navigation();
        let cancellation = crate::CancellationSource::new();
        let mut contexts = BTreeMap::new();
        contexts.insert(
            navigation.context(),
            ContextState {
                latest_generation: navigation.generation(),
                active_cancellation: Some(ActiveCancellation::Navigation {
                    navigation,
                    source: cancellation,
                }),
                current_frame: Some(StoredFrame {
                    lease: FrameLeaseId(NonZeroU64::new(1).unwrap()),
                    navigation,
                    frame: frame(1),
                }),
                document: None,
            },
        );
        (
            SharedState {
                lifecycle: Lifecycle::Running,
                receiver_open: true,
                commands: VecDeque::new(),
                context_closures: VecDeque::new(),
                closing_contexts: BTreeSet::new(),
                events: VecDeque::new(),
                terminal_event: None,
                contexts,
                latest_new_context: Some(navigation.context()),
                mutation_results: BTreeMap::new(),
                navigation_commits: BTreeMap::new(),
                current_style_documents: BTreeMap::new(),
                retained_frame_bytes: 4,
                retained_document_nodes: 0,
                pending_document_nodes: 0,
                retained_mutation_result_nodes: 0,
                pending_mutation_result_nodes: 0,
                pending_mutation_payload_bytes: 0,
                next_event_sequence: 1,
                next_frame_lease: 2,
                next_mutation_result_lease: 1,
                document_operation_owner: NonZeroU64::MIN,
                next_document_operation_sequence: 1,
            },
            limits,
            navigation,
        )
    }

    fn assert_prior_frame_unchanged(state: &SharedState, navigation: NavigationId) {
        let stored = state
            .contexts
            .get(&navigation.context())
            .unwrap()
            .current_frame
            .as_ref()
            .unwrap();
        assert_eq!(stored.lease.get(), 1);
        assert_eq!(stored.navigation, navigation);
        assert_eq!(stored.frame.rgba8_pixels(), Some(&[1, 0, 0, 255][..]));
        assert_eq!(state.retained_frame_bytes, 4);
        assert!(state.events.is_empty());
    }

    #[test]
    fn minimum_event_capacity_holds_started_and_atomic_success_without_a_drain() {
        assert_eq!(
            EngineLimits::new(1, 2, 1, 4, 4),
            Err(EngineLimitsError::TooSmall {
                field: "event_capacity",
                actual: 2,
                minimum: 3,
            })
        );
        let limits = EngineLimits::new(1, 3, 1, 4, 4).unwrap();
        let (entered_sender, entered_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let (mut engine, mut receiver) = NavigationEngine::spawn_with_executor(limits, move || {
            Ok(GatedSuccessExecutor {
                entered: entered_sender,
                release: release_receiver,
            })
        })
        .unwrap();
        let navigation = engine
            .navigate(context(), NavigationRequest::new("loopback").unwrap())
            .unwrap();
        assert_eq!(entered_receiver.recv().unwrap(), navigation);
        release_sender.send(()).unwrap();

        let mut state = lock_unpoisoned(&engine.shared.state);
        while state.events.len() < 3 {
            assert_eq!(state.lifecycle, Lifecycle::Running);
            state = engine
                .shared
                .event_ready
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        assert_eq!(state.events.len(), 3);
        drop(state);

        assert_eq!(
            receiver.recv().unwrap().kind(),
            EngineEventKind::NavigationStarted { navigation }
        );
        assert_eq!(
            receiver.recv().unwrap().kind(),
            EngineEventKind::NavigationCommitted {
                navigation,
                http_status: 200,
            }
        );
        let ready = receiver.recv().unwrap();
        let EngineEventKind::FrameReady {
            navigation: ready_navigation,
            lease,
            ..
        } = ready.kind()
        else {
            panic!("expected frame-ready event");
        };
        assert_eq!(ready_navigation, navigation);
        assert_eq!(receiver.take_frame(lease).unwrap().navigation(), navigation);
        assert_eq!(engine.shutdown().reason(), WorkerStopReason::Requested);
    }

    #[test]
    fn malformed_event_reordering_is_rejected() {
        let navigation = navigation();
        let failure =
            ExecutionFailure::new(ExecutionFailureKind::Internal, NavigationStage::Document);
        let lease = FrameLeaseId(NonZeroU64::new(1).unwrap());
        let metadata = frame(1).metadata();
        let invalid = [
            (
                NavigationEventPhase::Queued,
                EngineEventKind::NavigationCommitted {
                    navigation,
                    http_status: 200,
                },
            ),
            (
                NavigationEventPhase::Queued,
                EngineEventKind::NavigationFailed {
                    navigation,
                    failure,
                },
            ),
            (
                NavigationEventPhase::Started,
                EngineEventKind::FrameReady {
                    navigation,
                    lease,
                    metadata,
                },
            ),
            (
                NavigationEventPhase::Committed,
                EngineEventKind::NavigationCancelled { navigation },
            ),
            (
                NavigationEventPhase::Terminal,
                EngineEventKind::NavigationStarted { navigation },
            ),
            (
                NavigationEventPhase::Started,
                EngineEventKind::ShutdownComplete {
                    status: EngineShutdownStatus {
                        reason: WorkerStopReason::Requested,
                        executor: ExecutorShutdownStatus::Clean,
                    },
                },
            ),
        ];
        for (phase, event) in invalid {
            assert_eq!(
                transition_event(phase, event),
                Err(WorkerStopReason::EventOrderViolation)
            );
        }

        let started = transition_event(
            NavigationEventPhase::Queued,
            EngineEventKind::NavigationStarted { navigation },
        )
        .unwrap();
        let committed = transition_event(
            started,
            EngineEventKind::NavigationCommitted {
                navigation,
                http_status: 200,
            },
        )
        .unwrap();
        assert_eq!(
            transition_event(
                committed,
                EngineEventKind::FrameReady {
                    navigation,
                    lease,
                    metadata,
                },
            ),
            Ok(NavigationEventPhase::Terminal)
        );
    }

    #[test]
    fn rendered_conversion_rejects_a_real_pending_text_frame() {
        use wild_buzzard_headless::{FrameRequest, FrameSize, HeadlessLimits, HeadlessRenderer};
        use wild_buzzard_html::parse_document;
        use wild_buzzard_layout::{
            InitialStyleResolver, MonospaceTextMeasurer, Viewport, layout_document,
        };
        use wild_buzzard_renderer::{CompileRequest, PipelineKey, SceneCompiler};

        const WIDTH: u32 = 96;
        const HEIGHT: u32 = 64;
        const SOURCE: &str = "<html><body>pending text</body></html>";

        let parsed = parse_document(SOURCE).expect("pending-text fixture must parse");
        let snapshot = parsed
            .document
            .snapshot()
            .expect("pending-text fixture must snapshot");
        let layout = layout_document(
            &snapshot,
            Viewport::from_css_pixels(WIDTH.cast_signed(), HEIGHT.cast_signed()),
            &InitialStyleResolver,
            &MonospaceTextMeasurer,
        )
        .expect("pending-text fixture must lay out");
        let document_version = layout.document_version;
        let compiled = SceneCompiler::default()
            .compile(
                &layout,
                CompileRequest::new(document_version, PipelineKey::new(0x5742, 99)),
            )
            .expect("pending-text fixture scene must compile");
        let pending_text_runs = compiled.scene().pending_text().len();
        assert!(pending_text_runs > 0);
        let scene_items = compiled.scene().items().len();
        let pre_composition_display_list_bytes = compiled.built_display_list().size_in_bytes();

        let size = FrameSize::new(WIDTH, HEIGHT).expect("fixture dimensions must be valid");
        let mut diagnostic_renderer = HeadlessRenderer::new(size, HeadlessLimits::default())
            .expect("host must provide a Linux EGL pbuffer");
        let frame = diagnostic_renderer
            .render(compiled, FrameRequest::new(document_version, 1))
            .expect("the diagnostic renderer must preserve pending text");
        assert_eq!(frame.pending_text_runs(), pending_text_runs);
        diagnostic_renderer
            .shutdown()
            .expect("the diagnostic renderer must shut down cleanly");

        let pipeline_result = RenderedStaticPage {
            evidence: crate::PipelineEvidence {
                document_version,
                http_status: 200,
                navigation_commit: NavigationCommitMetadata::new(
                    "http://127.0.0.1/",
                    0,
                    NavigationConnectionSecurity::Cleartext,
                    false,
                )
                .unwrap(),
                source_bytes: SOURCE.len(),
                dom_nodes: snapshot.nodes_in_document_order().len(),
                html_diagnostics: parsed.errors.len(),
                stylo_style_entries: 0,
                style_diagnostics: 0,
                dropped_style_diagnostics: 0,
                layout_boxes: layout.boxes.len(),
                layout_warnings: layout.warnings.len(),
                scene_items,
                pre_composition_display_list_bytes,
            },
            text: crate::TextEvidence {
                layout_measurement_requests: 1,
                shaped_runs: 0,
                glyphs: 0,
                clusters: 0,
            },
            frame,
        };
        assert_eq!(
            EngineFrame::from_rendered(pipeline_result).unwrap_err(),
            EngineFrameError::PendingTextRuns {
                actual: pending_text_runs
            }
        );
    }

    #[test]
    fn event_sequence_exhaustion_aborts_multi_event_publication_atomically() {
        let (mut state, limits, navigation) = state_with_prior_frame();
        state.next_event_sequence = u64::MAX - 1;
        let mut phase = NavigationEventPhase::Started;
        let result = publish_success(
            &mut state,
            limits,
            &mut phase,
            navigation,
            &NavigationRequest::new("http://127.0.0.1/").unwrap(),
            ExecutorOutput::new(200, frame(2)).unwrap(),
        );

        assert_eq!(result, Err(WorkerStopReason::IdentityExhausted));
        assert_eq!(phase, NavigationEventPhase::Started);
        assert_eq!(state.next_event_sequence, u64::MAX - 1);
        assert_eq!(state.next_frame_lease, 2);
        assert_prior_frame_unchanged(&state, navigation);
    }

    #[test]
    fn document_operation_identity_exhaustion_is_transactional_and_never_wraps() {
        let (mut state, _, _) = state_with_prior_frame();
        state.next_document_operation_sequence = u64::MAX;
        assert_eq!(
            reserve_document_operation_id(&mut state),
            Err(CommandErrorKind::DocumentOperationIdentityExhausted)
        );
        assert_eq!(state.next_document_operation_sequence, u64::MAX);
        assert!(state.commands.is_empty());
        assert_eq!(state.pending_document_nodes, 0);
        assert_eq!(state.pending_mutation_result_nodes, 0);
        assert_eq!(state.pending_mutation_payload_bytes, 0);
    }

    #[test]
    fn engine_owner_identity_exhaustion_is_fail_closed_without_wrap() {
        let counter = AtomicU64::new(u64::MAX - 1);
        assert_eq!(allocate_owner_from(&counter).unwrap().get(), u64::MAX - 1);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
        assert_eq!(allocate_owner_from(&counter), None);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn dropping_receiver_clears_active_document_operation_and_pending_reservations() {
        let limits = EngineLimits::new(2, 8, 1, 4, 4).unwrap();
        let owner = NonZeroU64::MIN;
        let shared = Arc::new(Shared::new(limits, owner));
        let navigation = navigation();
        let operation = DocumentOperationId::new(owner, NonZeroU64::MIN);
        let cancellation = crate::CancellationSource::new();
        let token = cancellation.token();
        let version = wild_buzzard_dom::Document::new().version();
        {
            let mut state = lock_unpoisoned(&shared.state);
            state.contexts.insert(
                navigation.context(),
                ContextState {
                    latest_generation: navigation.generation(),
                    active_cancellation: Some(ActiveCancellation::Document {
                        navigation,
                        operation,
                        source: cancellation,
                    }),
                    current_frame: None,
                    document: Some(RetainedDocumentState {
                        navigation,
                        live_version: version,
                        frame_version: version,
                        node_charge: 1,
                    }),
                },
            );
            state
                .commands
                .push_back(EngineWork::Mutate(DocumentMutationWork {
                    navigation,
                    operation,
                    batch: ScriptMutationBatch::new(version, Vec::new()),
                    cancellation: token.clone(),
                    reserved_created_nodes: 0,
                    reserved_result_units: 1,
                    reserved_payload_bytes: 0,
                }));
            state.retained_document_nodes = 1;
            state.pending_mutation_result_nodes = 1;
        }
        let receiver = EngineEventReceiver {
            shared: Arc::clone(&shared),
            attached: true,
        };

        drop(receiver);

        assert!(token.is_cancelled());
        let state = lock_unpoisoned(&shared.state);
        assert!(!state.receiver_open);
        assert_eq!(
            state.lifecycle,
            Lifecycle::Stopping(WorkerStopReason::EventReceiverDropped)
        );
        assert!(state.commands.is_empty());
        assert!(
            state
                .contexts
                .get(&navigation.context())
                .unwrap()
                .active_cancellation
                .is_none()
        );
        assert!(
            state
                .contexts
                .get(&navigation.context())
                .unwrap()
                .document
                .is_none()
        );
        assert_eq!(state.pending_document_nodes, 0);
        assert_eq!(state.pending_mutation_result_nodes, 0);
        assert_eq!(state.pending_mutation_payload_bytes, 0);
        assert_eq!(state.retained_mutation_result_nodes, 0);
        assert_eq!(state.retained_document_nodes, 0);
    }

    #[test]
    fn requested_shutdown_drops_real_receiver_resources_before_join() {
        let limits = EngineLimits::new(2, 8, 1, 16, 16)
            .unwrap()
            .with_max_retained_document_nodes(16)
            .unwrap()
            .with_max_retained_mutation_result_nodes(16)
            .unwrap();
        let (entered_sender, entered_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let (mut engine, receiver) = NavigationEngine::spawn_with_executor(limits, move || {
            Ok(GatedSuccessExecutor {
                entered: entered_sender,
                release: release_receiver,
            })
        })
        .unwrap();
        let navigation = engine
            .navigate(
                context(),
                NavigationRequest::new("shutdown-resources").unwrap(),
            )
            .unwrap();
        assert_eq!(entered_receiver.recv().unwrap(), navigation);

        let mut document = wild_buzzard_dom::Document::new();
        let node = document.create_text("retained").unwrap();
        let version = document.version();
        let operation_owner = lock_unpoisoned(&engine.shared.state).document_operation_owner;
        let operation = DocumentOperationId::new(operation_owner, NonZeroU64::MIN);
        {
            let mut state = lock_unpoisoned(&engine.shared.state);
            let context = state.contexts.get_mut(&navigation.context()).unwrap();
            context.current_frame = Some(StoredFrame {
                lease: FrameLeaseId(NonZeroU64::MIN),
                navigation,
                frame: frame(9),
            });
            context.document = Some(RetainedDocumentState {
                navigation,
                live_version: version,
                frame_version: version,
                node_charge: 1,
            });
            state.mutation_results.insert(
                MutationResultLeaseId(NonZeroU64::MIN),
                StoredMutationResult {
                    lease: MutationResultLeaseId(NonZeroU64::MIN),
                    navigation,
                    operation,
                    live_version: version,
                    created_nodes: vec![node].into_boxed_slice(),
                },
            );
            state.retained_frame_bytes = 4;
            state.retained_document_nodes = 1;
            state.pending_document_nodes = 2;
            state.retained_mutation_result_nodes = 1;
            state.pending_mutation_result_nodes = 3;
            state.pending_mutation_payload_bytes = 5;
            state.context_closures.push_back(navigation);
            state.closing_contexts.insert(navigation.context());
            assert!(!state.events.is_empty());
        }

        assert_eq!(
            engine.request_shutdown(),
            CommandReceipt::ShutdownRequested {
                already_requested: false,
            }
        );
        drop(receiver);
        {
            let state = lock_unpoisoned(&engine.shared.state);
            assert_eq!(
                state.lifecycle,
                Lifecycle::Stopping(WorkerStopReason::Requested)
            );
            assert!(state.events.is_empty());
            assert!(state.terminal_event.is_none());
            assert!(state.commands.is_empty());
            assert!(state.context_closures.is_empty());
            assert!(state.closing_contexts.is_empty());
            assert!(state.mutation_results.is_empty());
            let context = state.contexts.get(&navigation.context()).unwrap();
            assert!(context.current_frame.is_none());
            assert!(context.document.is_none());
            assert!(context.active_cancellation.is_none());
            assert_eq!(state.retained_frame_bytes, 0);
            assert_eq!(state.retained_document_nodes, 0);
            assert_eq!(state.pending_document_nodes, 0);
            assert_eq!(state.retained_mutation_result_nodes, 0);
            assert_eq!(state.pending_mutation_result_nodes, 0);
            assert_eq!(state.pending_mutation_payload_bytes, 0);
        }

        release_sender.send(()).unwrap();
        let status = engine.shutdown();
        assert_eq!(status.reason(), WorkerStopReason::Requested);
        assert_eq!(status.executor(), ExecutorShutdownStatus::Clean);
    }

    #[test]
    fn frame_lease_exhaustion_aborts_multi_event_publication_atomically() {
        let (mut state, limits, navigation) = state_with_prior_frame();
        state.next_frame_lease = u64::MAX;
        let mut phase = NavigationEventPhase::Started;
        let result = publish_success(
            &mut state,
            limits,
            &mut phase,
            navigation,
            &NavigationRequest::new("http://127.0.0.1/").unwrap(),
            ExecutorOutput::new(200, frame(2)).unwrap(),
        );

        assert_eq!(result, Err(WorkerStopReason::IdentityExhausted));
        assert_eq!(phase, NavigationEventPhase::Started);
        assert_eq!(state.next_event_sequence, 1);
        assert_eq!(state.next_frame_lease, u64::MAX);
        assert_prior_frame_unchanged(&state, navigation);
    }
}
