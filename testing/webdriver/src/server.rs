/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::Parameters;
use crate::command::{WebDriverCommand, WebDriverMessage};
use crate::error::{ErrorStatus, WebDriverError, WebDriverResult};
use crate::httpapi::{
    Route, VoidWebDriverExtensionRoute, WebDriverExtensionRoute, standard_routes,
};
use crate::response::{CloseWindowResponse, WebDriverResponse};
use bytes::Bytes;
use http::{Method, StatusCode};
use std::convert::Infallible;
use std::fmt;
use std::io::{self, Read, Write};
use std::marker::PhantomData;
use std::net::{SocketAddr, TcpListener as StdTcpListener, TcpStream};
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use url::{Host, Url};
use warp::{Filter, Rejection, Reply};

const LEGACY_MAX_REQUEST_BODY_BYTES: usize = 1_048_576;
const LEGACY_MAX_DISPATCH_QUEUE_DEPTH: usize = 64;
const LEGACY_REQUEST_DEADLINE: Duration = Duration::from_secs(120);
const DEFAULT_MAX_REQUEST_BODY_BYTES: usize = 65_536;
const DEFAULT_MAX_DISPATCH_QUEUE_DEPTH: usize = 16;
const DEFAULT_REQUEST_DEADLINE: Duration = Duration::from_secs(30);
const MAX_REQUEST_BODY_BYTES: usize = 1_048_576;
const MAX_DISPATCH_QUEUE_DEPTH: usize = 64;
const MAX_REQUEST_DEADLINE: Duration = Duration::from_secs(120);
const DISPATCHER_SHUTDOWN_POLL: Duration = Duration::from_millis(20);
const BEARER_TOKEN_HEX_BYTES: usize = 64;
const AUTHENTICATED_MAX_HEADER_BYTES: usize = 16_384;
const AUTHENTICATED_MAX_CONNECTION_WORKERS: usize = 8;
const AUTHENTICATED_CONNECTION_QUEUE_PER_WORKER: usize = 0;
const AUTHENTICATED_IO_POLL: Duration = Duration::from_millis(20);
const AUTHENTICATED_ACCEPT_POLL: Duration = Duration::from_millis(5);
const AUTHENTICATED_RESPONSE_WRITE_RESERVE: Duration = Duration::from_millis(20);

/// Heap-backed secret storage whose bytes are erased by `zeroize` on drop.
///
/// The allocation is populated only after it reaches its final heap address,
/// so moving this wrapper moves a pointer rather than copying secret bytes.
/// This type deliberately implements neither `Clone`, `Debug`, nor `Display`.
pub struct SecretBytes<const N: usize> {
    bytes: Box<[u8; N]>,
    #[cfg(test)]
    drop_observer: Option<Arc<Mutex<Vec<u8>>>>,
}

impl<const N: usize> SecretBytes<N> {
    /// Allocates an all-zero secret buffer at its final heap address.
    #[must_use]
    pub fn zeroed() -> Self {
        Self {
            bytes: Box::new([0; N]),
            #[cfg(test)]
            drop_observer: None,
        }
    }

    /// Exposes the initialized bytes for bounded parsing or comparison.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    /// Exposes the initialized bytes for one bounded secret read.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.bytes.as_mut_slice()
    }

    fn zeroize(&mut self) {
        zeroize::Zeroize::zeroize(self.bytes.as_mut_slice());
    }

    #[cfg(test)]
    fn with_drop_observer(drop_observer: Arc<Mutex<Vec<u8>>>) -> Self {
        Self {
            bytes: Box::new([0; N]),
            drop_observer: Some(drop_observer),
        }
    }
}

impl<const N: usize> Drop for SecretBytes<N> {
    fn drop(&mut self) {
        self.zeroize();
        #[cfg(test)]
        if let Some(observer) = &self.drop_observer {
            *observer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = self.bytes.to_vec();
        }
    }
}

/// A validated 256-bit bearer token represented as exactly 64 lowercase
/// hexadecimal bytes.
///
/// The secret has deliberately redacted debug output and no `Display`
/// implementation. It is compared without data-dependent early returns.
pub struct BearerToken(SecretBytes<BEARER_TOKEN_HEX_BYTES>);

impl BearerToken {
    /// Validates one lowercase hexadecimal encoding of exactly 32 bytes.
    ///
    /// # Errors
    ///
    /// Returns [`BearerTokenError`] without including any part of `value`.
    pub fn from_lower_hex(value: &[u8]) -> Result<Self, BearerTokenError> {
        if value.len() != BEARER_TOKEN_HEX_BYTES
            || value
                .iter()
                .any(|byte| !matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(BearerTokenError);
        }
        let mut token = SecretBytes::zeroed();
        token.as_mut_slice().copy_from_slice(value);
        Ok(Self(token))
    }

    fn authorizes(&self, header: Option<&str>) -> bool {
        let Some(header) = header else {
            return false;
        };
        let bytes = header.as_bytes();
        let expected_len = "Bearer ".len() + BEARER_TOKEN_HEX_BYTES;
        if bytes.len() != expected_len {
            return false;
        }
        let mut difference = 0_u8;
        for (received, expected) in bytes["Bearer ".len()..].iter().zip(self.0.as_slice()) {
            difference |= received ^ expected;
        }
        bytes.starts_with(b"Bearer ") && difference == 0
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BearerToken(<redacted>)")
    }
}

/// A bearer-token validation failure which never reflects secret input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BearerTokenError;

impl fmt::Display for BearerTokenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("bearer token must be 64 lowercase hexadecimal bytes")
    }
}

impl std::error::Error for BearerTokenError {}

/// Mandatory admission policy for an authenticated embedded `WebDriver` server.
pub struct ServerSecurityPolicy {
    bind_address: SocketAddr,
    bearer_token: Arc<BearerToken>,
    allowed_origins: Box<[Box<str>]>,
    max_request_body_bytes: usize,
    max_dispatch_queue_depth: usize,
    request_deadline: Duration,
}

impl ServerSecurityPolicy {
    /// Creates the default bounded policy for one explicit loopback address.
    ///
    /// A zero port is allowed for deterministic tests and is replaced with the
    /// operating-system-assigned port before Host and Origin admission.
    ///
    /// # Errors
    ///
    /// Rejects every non-loopback bind address.
    pub fn new(bind_address: SocketAddr, bearer_token: BearerToken) -> io::Result<Self> {
        if !bind_address.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "authenticated WebDriver may bind only to a loopback address",
            ));
        }
        Ok(Self {
            bind_address,
            bearer_token: Arc::new(bearer_token),
            allowed_origins: Box::new([]),
            max_request_body_bytes: DEFAULT_MAX_REQUEST_BODY_BYTES,
            max_dispatch_queue_depth: DEFAULT_MAX_DISPATCH_QUEUE_DEPTH,
            request_deadline: DEFAULT_REQUEST_DEADLINE,
        })
    }

    /// Replaces the exact allowed Origin values. An absent Origin remains
    /// valid for non-browser `WebDriver` clients; any present Origin must equal
    /// one of these byte-for-byte.
    ///
    /// # Errors
    ///
    /// Rejects duplicate, malformed, non-origin, or excessively long values.
    pub fn with_allowed_origins<I, S>(mut self, origins: I) -> io::Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut admitted: Vec<Box<str>> = Vec::new();
        for origin in origins {
            let origin = origin.as_ref();
            if origin.is_empty() || origin.len() > 512 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "allowed WebDriver Origin is empty or exceeds 512 bytes",
                ));
            }
            let parsed = Url::parse(origin).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "allowed WebDriver Origin is not an absolute URL",
                )
            })?;
            if parsed.username() != ""
                || parsed.password().is_some()
                || parsed.query().is_some()
                || parsed.fragment().is_some()
                || parsed.path() != "/"
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "allowed WebDriver Origin contains non-origin components",
                ));
            }
            if admitted
                .iter()
                .any(|candidate| candidate.as_ref() == origin)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "allowed WebDriver Origin is duplicated",
                ));
            }
            admitted.push(origin.into());
        }
        self.allowed_origins = admitted.into_boxed_slice();
        Ok(self)
    }

    /// Replaces request body, dispatch queue, and deadline limits.
    ///
    /// One monotonic absolute deadline bounds strict HTTP header/body admission,
    /// dispatcher queuing, handler/owner work, and response correlation. No
    /// stage starts a fresh budget. Authenticated admission uses at most the
    /// smaller of eight workers or `max_dispatch_queue_depth + 1`; it queues no
    /// accepted connection behind a busy worker. Headers are capped at 16 KiB,
    /// and a body is allocated and read only after bearer, Host, and Origin
    /// admission succeeds.
    ///
    /// # Errors
    ///
    /// Rejects zero values and values above the crate's hard ceilings.
    pub fn with_limits(
        mut self,
        max_request_body_bytes: usize,
        max_dispatch_queue_depth: usize,
        request_deadline: Duration,
    ) -> io::Result<Self> {
        if max_request_body_bytes == 0 || max_request_body_bytes > MAX_REQUEST_BODY_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "WebDriver request body limit is outside the hard range",
            ));
        }
        if max_dispatch_queue_depth == 0 || max_dispatch_queue_depth > MAX_DISPATCH_QUEUE_DEPTH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "WebDriver dispatch queue limit is outside the hard range",
            ));
        }
        if request_deadline.is_zero() || request_deadline > MAX_REQUEST_DEADLINE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "WebDriver request deadline is outside the hard range",
            ));
        }
        self.max_request_body_bytes = max_request_body_bytes;
        self.max_dispatch_queue_depth = max_dispatch_queue_depth;
        self.request_deadline = request_deadline;
        Ok(self)
    }

    /// Exact requested loopback bind address.
    #[must_use]
    pub const fn bind_address(&self) -> SocketAddr {
        self.bind_address
    }

    /// Per-request deadline applied before an HTTP connection can retain a
    /// dispatcher waiter indefinitely.
    #[must_use]
    pub const fn request_deadline(&self) -> Duration {
        self.request_deadline
    }
}

impl fmt::Debug for ServerSecurityPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerSecurityPolicy")
            .field("bind_address", &self.bind_address)
            .field("bearer_token", &"<redacted>")
            .field("allowed_origins", &self.allowed_origins)
            .field("max_request_body_bytes", &self.max_request_body_bytes)
            .field("max_dispatch_queue_depth", &self.max_dispatch_queue_depth)
            .field("request_deadline", &self.request_deadline)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DispatchPhase {
    Active,
    Completed,
    Cancelled,
}

struct DispatchLifetimeInner {
    deadline: Instant,
    phase: Mutex<DispatchPhase>,
}

/// Shared absolute lifetime and cancellation authority for one authenticated
/// dispatch.
///
/// `Completed` is selected atomically against external cancellation. A
/// completed New Session lifetime is then retained as the exact session
/// authority and may still be authoritatively revoked during teardown.
#[derive(Clone)]
pub struct DispatchLifetime {
    inner: Arc<DispatchLifetimeInner>,
}

impl DispatchLifetime {
    /// Creates one active lifetime ending at the supplied monotonic deadline.
    #[must_use]
    pub fn new(deadline: Instant) -> Self {
        Self {
            inner: Arc::new(DispatchLifetimeInner {
                deadline,
                phase: Mutex::new(DispatchPhase::Active),
            }),
        }
    }

    /// Returns the exact monotonic deadline inherited from HTTP admission.
    #[must_use]
    pub fn deadline(&self) -> Instant {
        self.inner.deadline
    }

    /// Returns the remaining active lifetime. Completed or cancelled
    /// lifetimes have no further command budget.
    #[must_use]
    pub fn remaining(&self) -> Option<Duration> {
        if *self.phase() != DispatchPhase::Active {
            return None;
        }
        self.inner.deadline.checked_duration_since(Instant::now())
    }

    /// Returns whether cancellation or teardown revoked this exact authority.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.phase() == DispatchPhase::Cancelled
    }

    /// Cancels an incomplete dispatch. A response which already won the
    /// completion arbitration remains completed.
    #[must_use]
    pub fn cancel(&self) -> bool {
        let mut phase = self.phase();
        if *phase == DispatchPhase::Active {
            *phase = DispatchPhase::Cancelled;
            true
        } else {
            false
        }
    }

    /// Revokes this authority even if its initial request already completed.
    pub fn revoke(&self) {
        *self.phase() = DispatchPhase::Cancelled;
    }

    /// Cancels an active dispatch once its one absolute deadline has elapsed.
    #[must_use]
    pub fn cancel_if_expired(&self) -> bool {
        let mut phase = self.phase();
        if *phase == DispatchPhase::Active && Instant::now() >= self.inner.deadline {
            *phase = DispatchPhase::Cancelled;
            true
        } else {
            *phase == DispatchPhase::Cancelled
        }
    }

    /// Runs one bounded, nonblocking state transition only while this request
    /// is active and before its exact deadline.
    ///
    /// Cancellation and completion arbitration use the same lock, so an
    /// external waiter cannot report cancellation while this transition is
    /// still running. Callers must keep `operation` to an immediate local
    /// transition and must not call back into this lifetime from it.
    pub fn run_if_active<R>(&self, operation: impl FnOnce() -> R) -> Option<R> {
        let mut phase = self.phase();
        if *phase != DispatchPhase::Active || Instant::now() >= self.inner.deadline {
            if *phase == DispatchPhase::Active {
                *phase = DispatchPhase::Cancelled;
            }
            return None;
        }
        Some(operation())
    }

    /// Runs one transition only while this request and one exact established
    /// session authority are atomically admitted.
    ///
    /// The request must still be active and before its absolute deadline. The
    /// session authority may be active or completed, but must not have been
    /// revoked. Both phase locks remain held through `operation`, so concurrent
    /// request cancellation or exact-session revocation has one deterministic
    /// winner. Callers must not re-enter either lifetime from `operation`.
    pub fn run_if_active_with_authority<R>(
        &self,
        authority: &Self,
        operation: impl FnOnce() -> R,
    ) -> Option<R> {
        self.run_with_authority_phase(authority, false, operation)
    }

    /// Runs one fail-closed teardown transition only while this request is
    /// active and the exact established session authority is already revoked.
    ///
    /// This is reserved for bounded owner-thread revocation commands. Both
    /// phase locks remain held through `operation`; callers must not re-enter
    /// either lifetime from it.
    pub fn run_if_active_with_revoked_authority<R>(
        &self,
        authority: &Self,
        operation: impl FnOnce() -> R,
    ) -> Option<R> {
        self.run_with_authority_phase(authority, true, operation)
    }

    fn run_with_authority_phase<R>(
        &self,
        authority: &Self,
        require_revoked: bool,
        operation: impl FnOnce() -> R,
    ) -> Option<R> {
        if Arc::ptr_eq(&self.inner, &authority.inner) {
            let mut request_phase = self.phase();
            let authority_phase = *request_phase;
            if !self.admit_phases(&mut request_phase, authority_phase, require_revoked) {
                return None;
            }
            return Some(operation());
        }

        if Arc::as_ptr(&self.inner) < Arc::as_ptr(&authority.inner) {
            let mut request_phase = self.phase();
            let authority_phase = authority.phase();
            if !self.admit_phases(&mut request_phase, *authority_phase, require_revoked) {
                return None;
            }
            return Some(operation());
        }

        let authority_phase = authority.phase();
        let mut request_phase = self.phase();
        if !self.admit_phases(&mut request_phase, *authority_phase, require_revoked) {
            return None;
        }
        Some(operation())
    }

    fn admit_phases(
        &self,
        request_phase: &mut DispatchPhase,
        authority_phase: DispatchPhase,
        require_revoked: bool,
    ) -> bool {
        if *request_phase != DispatchPhase::Active || Instant::now() >= self.inner.deadline {
            if *request_phase == DispatchPhase::Active {
                *request_phase = DispatchPhase::Cancelled;
            }
            return false;
        }
        (authority_phase == DispatchPhase::Cancelled) == require_revoked
    }

    fn try_complete(&self) -> bool {
        let mut phase = self.phase();
        if *phase != DispatchPhase::Active || Instant::now() >= self.inner.deadline {
            if *phase == DispatchPhase::Active {
                *phase = DispatchPhase::Cancelled;
            }
            return false;
        }
        *phase = DispatchPhase::Completed;
        true
    }

    fn phase(&self) -> MutexGuard<'_, DispatchPhase> {
        self.inner
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl fmt::Debug for DispatchLifetime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DispatchLifetime")
            .field("deadline", &self.inner.deadline)
            .field("phase", &*self.phase())
            .finish()
    }
}

struct DispatchResponse {
    response: WebDriverResult<WebDriverResponse>,
}

struct DispatchRequest<U: WebDriverExtensionRoute> {
    message: WebDriverMessage<U>,
    response: Sender<DispatchResponse>,
    lifetime: Option<DispatchLifetime>,
    delivery: Option<ResponseDelivery>,
}

enum DispatchMessage<U: WebDriverExtensionRoute> {
    HandleWebDriver(Box<DispatchRequest<U>>),
    Quit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResponseDeliveryPhase {
    Pending,
    Delivered,
    Abandoned,
}

struct ResponseDeliveryState {
    phase: ResponseDeliveryPhase,
    request_authority: DispatchLifetime,
    session_authority: Option<DispatchLifetime>,
}

/// Shared, non-droppable ownership transfer for one authenticated response.
///
/// The worker must acknowledge a complete bounded socket write. Every other
/// terminal path marks delivery abandoned and revokes both the request
/// authority and the exact session authority published by the dispatcher.
#[derive(Clone)]
struct ResponseDelivery {
    state: Arc<(Mutex<ResponseDeliveryState>, Condvar)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResponseDeliveryOutcome {
    Delivered,
    Abandoned,
}

impl ResponseDelivery {
    fn new(request_authority: DispatchLifetime) -> Self {
        Self {
            state: Arc::new((
                Mutex::new(ResponseDeliveryState {
                    phase: ResponseDeliveryPhase::Pending,
                    request_authority,
                    session_authority: None,
                }),
                Condvar::new(),
            )),
        }
    }

    /// Publishes the exact active session authority before the dispatcher lets
    /// the worker own final response delivery. If the worker already abandoned
    /// delivery, the authority is synchronously revoked and publication fails.
    fn publish_session_authority(&self, authority: Option<&DispatchLifetime>) -> bool {
        let mut state = self.lock();
        let publication_rejected = state.phase != ResponseDeliveryPhase::Pending
            || authority.is_some_and(DispatchLifetime::is_cancelled);
        if publication_rejected {
            Self::abandon_locked(&mut state);
            if let Some(authority) = authority {
                authority.revoke();
            }
            self.state.1.notify_all();
            return false;
        }
        state.session_authority = authority.cloned();
        true
    }

    fn abandon(&self) {
        let mut state = self.lock();
        if state.phase == ResponseDeliveryPhase::Pending {
            Self::abandon_locked(&mut state);
            self.state.1.notify_all();
        }
    }

    fn acknowledge(&self) -> bool {
        let mut state = self.lock();
        match state.phase {
            ResponseDeliveryPhase::Delivered => return true,
            ResponseDeliveryPhase::Abandoned => return false,
            ResponseDeliveryPhase::Pending => {}
        }
        if state.request_authority.is_cancelled()
            || state
                .session_authority
                .as_ref()
                .is_some_and(DispatchLifetime::is_cancelled)
        {
            Self::abandon_locked(&mut state);
            self.state.1.notify_all();
            return false;
        }
        state.phase = ResponseDeliveryPhase::Delivered;
        self.state.1.notify_all();
        true
    }

    fn wait_for_terminal(
        &self,
        deadline: Instant,
        shutdown: &AtomicBool,
    ) -> ResponseDeliveryOutcome {
        let mut state = self.lock();
        loop {
            match state.phase {
                ResponseDeliveryPhase::Delivered => return ResponseDeliveryOutcome::Delivered,
                ResponseDeliveryPhase::Abandoned => return ResponseDeliveryOutcome::Abandoned,
                ResponseDeliveryPhase::Pending => {}
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                Self::abandon_locked(&mut state);
                self.state.1.notify_all();
                return ResponseDeliveryOutcome::Abandoned;
            };
            if shutdown.load(Ordering::Acquire) {
                Self::abandon_locked(&mut state);
                self.state.1.notify_all();
                return ResponseDeliveryOutcome::Abandoned;
            }
            let waited = self
                .state
                .1
                .wait_timeout(state, remaining.min(DISPATCHER_SHUTDOWN_POLL))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = waited.0;
        }
    }

    fn abandon_locked(state: &mut ResponseDeliveryState) {
        state.phase = ResponseDeliveryPhase::Abandoned;
        state.request_authority.revoke();
        if let Some(authority) = &state.session_authority {
            authority.revoke();
        }
    }

    fn lock(&self) -> MutexGuard<'_, ResponseDeliveryState> {
        self.state
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

struct ResponseDeliveryGuard {
    delivery: ResponseDelivery,
    armed: bool,
}

impl ResponseDeliveryGuard {
    fn new(delivery: ResponseDelivery) -> Self {
        Self {
            delivery,
            armed: true,
        }
    }

    fn acknowledge(mut self) {
        let _ = self.delivery.acknowledge();
        self.armed = false;
    }
}

impl Drop for ResponseDeliveryGuard {
    fn drop(&mut self) {
        if self.armed {
            self.delivery.abandon();
        }
    }
}

#[derive(Debug)]
struct UnexpectedRequestBody;

impl warp::reject::Reject for UnexpectedRequestBody {}

#[derive(Clone, Debug, PartialEq)]
/// Representation of whether we managed to successfully send a `DeleteSession` message
/// and read the response during session teardown.
pub enum SessionTeardownKind {
    /// A `DeleteSession` message has been sent and the response handled.
    Deleted,
    /// No `DeleteSession` message has been sent, or the response was not received.
    NotDeleted,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub id: String,
}

impl Session {
    fn new(id: String) -> Session {
        Session { id }
    }
}

pub trait WebDriverHandler<U: WebDriverExtensionRoute = VoidWebDriverExtensionRoute>: Send {
    /// Handles one already parsed and session-validated command.
    ///
    /// # Errors
    ///
    /// Returns the command's protocol error without mutating dispatcher state.
    fn handle_command(
        &mut self,
        session: &Option<Session>,
        msg: WebDriverMessage<U>,
    ) -> WebDriverResult<WebDriverResponse>;

    /// Handles one authenticated command under the exact HTTP admission
    /// lifetime. Legacy embedders retain [`Self::handle_command`] unchanged.
    ///
    /// # Errors
    ///
    /// Returns the command's protocol error without mutating dispatcher state.
    fn handle_command_with_lifetime(
        &mut self,
        session: &Option<Session>,
        msg: WebDriverMessage<U>,
        _lifetime: &DispatchLifetime,
    ) -> WebDriverResult<WebDriverResponse> {
        self.handle_command(session, msg)
    }

    fn teardown_session(&mut self, kind: SessionTeardownKind);
}

#[derive(Debug)]
struct Dispatcher<T: WebDriverHandler<U>, U: WebDriverExtensionRoute> {
    handler: T,
    session: Option<Session>,
    session_authority: Option<DispatchLifetime>,
    handler_session_open: bool,
    extension_type: PhantomData<U>,
}

impl<T: WebDriverHandler<U>, U: WebDriverExtensionRoute> Dispatcher<T, U> {
    fn new(handler: T) -> Dispatcher<T, U> {
        Dispatcher {
            handler,
            session: None,
            session_authority: None,
            handler_session_open: false,
            extension_type: PhantomData,
        }
    }

    fn run(&mut self, msg_chan: &Receiver<DispatchMessage<U>>, shutdown: &AtomicBool) {
        loop {
            if self
                .session_authority
                .as_ref()
                .is_some_and(DispatchLifetime::is_cancelled)
            {
                self.teardown_session(SessionTeardownKind::NotDeleted);
            }
            if shutdown.load(Ordering::Acquire) {
                Self::cancel_queued(msg_chan);
                self.teardown_session(SessionTeardownKind::NotDeleted);
                break;
            }
            match msg_chan.recv_timeout(DISPATCHER_SHUTDOWN_POLL) {
                Ok(DispatchMessage::HandleWebDriver(request)) => {
                    self.handle_dispatch(*request, shutdown);
                }
                Ok(DispatchMessage::Quit) => {
                    debug!("Quit signal received, tearing down session");
                    shutdown.store(true, Ordering::Release);
                    self.teardown_session(SessionTeardownKind::NotDeleted);
                    break;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.teardown_session(SessionTeardownKind::NotDeleted);
                    break;
                }
            }
        }
    }

    fn cancel_queued(msg_chan: &Receiver<DispatchMessage<U>>) {
        while let Ok(message) = msg_chan.try_recv() {
            if let DispatchMessage::HandleWebDriver(request) = message {
                let DispatchRequest {
                    response,
                    lifetime,
                    delivery,
                    ..
                } = *request;
                if let Some(lifetime) = lifetime {
                    lifetime.revoke();
                }
                if let Some(delivery) = delivery {
                    delivery.abandon();
                }
                let _ = response.send(DispatchResponse {
                    response: Err(WebDriverError::new(
                        ErrorStatus::UnknownError,
                        "WebDriver server is shutting down",
                    )),
                });
            }
        }
    }

    fn handle_dispatch(&mut self, request: DispatchRequest<U>, shutdown: &AtomicBool) {
        let DispatchRequest {
            message,
            response: response_send,
            lifetime,
            delivery,
        } = request;
        if lifetime
            .as_ref()
            .is_some_and(DispatchLifetime::cancel_if_expired)
        {
            if let Some(delivery) = &delivery {
                delivery.abandon();
            }
            self.teardown_session(SessionTeardownKind::NotDeleted);
            send_dispatch_error(&response_send, command_deadline_error());
            return;
        }
        if delivery.as_ref().is_some_and(|delivery| {
            !delivery.publish_session_authority(self.session_authority.as_ref())
        }) {
            self.teardown_session(SessionTeardownKind::NotDeleted);
            send_dispatch_error(
                &response_send,
                WebDriverError::new(
                    ErrorStatus::UnknownError,
                    "WebDriver response delivery was abandoned",
                ),
            );
            return;
        }

        let is_new_session = matches!(&message.command, WebDriverCommand::NewSession(_));
        let handled = catch_unwind(AssertUnwindSafe(|| {
            self.invoke_handler(message, lifetime.as_ref(), is_new_session)
        }));
        let response = match handled {
            Ok(response) => response,
            Err(payload) => {
                self.resume_after_dispatch_panic(delivery.as_ref(), lifetime.as_ref(), payload)
            }
        };

        if is_new_session
            && !matches!(&response, Ok(WebDriverResponse::NewSession(_)))
            && !matches!(&response, Err(error) if error.delete_session)
        {
            self.handler_session_open = false;
        }

        if let Some(lifetime) = &lifetime
            && !lifetime.try_complete()
        {
            if let Some(delivery) = &delivery {
                delivery.abandon();
            }
            lifetime.revoke();
            self.teardown_session(SessionTeardownKind::NotDeleted);
            send_dispatch_error(&response_send, command_deadline_error());
            return;
        }

        let applied = catch_unwind(AssertUnwindSafe(|| {
            self.apply_response_effects(&response, lifetime.as_ref());
        }));
        if let Err(payload) = applied {
            self.resume_after_dispatch_panic(delivery.as_ref(), lifetime.as_ref(), payload);
        }

        let session_authority = self.session_authority.clone();
        if delivery
            .as_ref()
            .is_some_and(|delivery| !delivery.publish_session_authority(session_authority.as_ref()))
        {
            if let Some(lifetime) = &lifetime {
                lifetime.revoke();
            }
            if let Some(authority) = &session_authority {
                authority.revoke();
            }
            self.teardown_session(SessionTeardownKind::NotDeleted);
            send_dispatch_error(
                &response_send,
                WebDriverError::new(
                    ErrorStatus::UnknownError,
                    "WebDriver response delivery was abandoned",
                ),
            );
            return;
        }
        if response_send.send(DispatchResponse { response }).is_err() {
            self.fail_response_send(
                delivery.as_ref(),
                lifetime.as_ref(),
                session_authority.as_ref(),
            );
            return;
        }
        self.await_terminal_delivery(
            delivery.as_ref(),
            lifetime.as_ref(),
            session_authority.as_ref(),
            shutdown,
        );
    }

    fn await_terminal_delivery(
        &mut self,
        delivery: Option<&ResponseDelivery>,
        lifetime: Option<&DispatchLifetime>,
        session_authority: Option<&DispatchLifetime>,
        shutdown: &AtomicBool,
    ) {
        let (Some(delivery), Some(lifetime)) = (delivery, lifetime) else {
            return;
        };
        if delivery.wait_for_terminal(lifetime.deadline(), shutdown)
            == ResponseDeliveryOutcome::Delivered
        {
            return;
        }
        lifetime.revoke();
        if let Some(authority) = session_authority {
            authority.revoke();
        }
        self.teardown_session(SessionTeardownKind::NotDeleted);
    }

    fn invoke_handler(
        &mut self,
        message: WebDriverMessage<U>,
        lifetime: Option<&DispatchLifetime>,
        is_new_session: bool,
    ) -> WebDriverResult<WebDriverResponse> {
        self.check_session(&message)?;
        if is_new_session {
            self.handler_session_open = true;
        }
        match lifetime {
            Some(lifetime) => {
                self.handler
                    .handle_command_with_lifetime(&self.session, message, lifetime)
            }
            None => self.handler.handle_command(&self.session, message),
        }
    }

    fn resume_after_dispatch_panic(
        &mut self,
        delivery: Option<&ResponseDelivery>,
        lifetime: Option<&DispatchLifetime>,
        payload: Box<dyn std::any::Any + Send>,
    ) -> ! {
        if let Some(delivery) = delivery {
            delivery.abandon();
        }
        if let Some(lifetime) = lifetime {
            lifetime.revoke();
        }
        self.cancel_active_authority();
        let _ = catch_unwind(AssertUnwindSafe(|| {
            self.teardown_session(SessionTeardownKind::NotDeleted);
        }));
        resume_unwind(payload);
    }

    fn fail_response_send(
        &mut self,
        delivery: Option<&ResponseDelivery>,
        lifetime: Option<&DispatchLifetime>,
        session_authority: Option<&DispatchLifetime>,
    ) {
        error!("Sending response to the main thread failed");
        if let Some(delivery) = delivery {
            delivery.abandon();
        }
        if let Some(lifetime) = lifetime {
            lifetime.revoke();
        }
        if let Some(authority) = session_authority {
            authority.revoke();
        }
        self.teardown_session(SessionTeardownKind::NotDeleted);
    }

    fn apply_response_effects(
        &mut self,
        response: &WebDriverResult<WebDriverResponse>,
        lifetime: Option<&DispatchLifetime>,
    ) {
        match response {
            Ok(WebDriverResponse::NewSession(new_session)) => {
                self.session = Some(Session::new(new_session.session_id.clone()));
                self.session_authority = lifetime.cloned();
                self.handler_session_open = true;
            }
            Ok(WebDriverResponse::CloseWindow(CloseWindowResponse(handles)))
                if handles.is_empty() =>
            {
                debug!("Last window was closed, deleting session");
                self.teardown_session(SessionTeardownKind::NotDeleted);
            }
            Ok(WebDriverResponse::DeleteSession) => {
                self.teardown_session(SessionTeardownKind::Deleted);
            }
            Err(error) if error.delete_session => {
                self.teardown_session(SessionTeardownKind::NotDeleted);
            }
            _ => {}
        }
    }

    fn cancel_active_authority(&self) {
        if let Some(authority) = &self.session_authority {
            authority.revoke();
        }
    }

    fn teardown_session(&mut self, kind: SessionTeardownKind) {
        debug!("Teardown session");
        self.cancel_active_authority();
        let session = self.session.take();
        self.session_authority = None;
        let should_teardown = self.handler_session_open || session.is_some();
        self.handler_session_open = false;
        if !should_teardown {
            return;
        }

        let mut teardown_panic = None;
        let final_kind = match (kind, session.as_ref()) {
            (SessionTeardownKind::NotDeleted, Some(session)) => {
                let delete_session = WebDriverMessage {
                    session_id: Some(session.id.clone()),
                    command: WebDriverCommand::DeleteSession,
                };
                let dispatcher_session = Some(session.clone());
                match catch_unwind(AssertUnwindSafe(|| {
                    self.handler
                        .handle_command(&dispatcher_session, delete_session)
                })) {
                    Ok(Ok(_)) => SessionTeardownKind::Deleted,
                    Ok(Err(_)) => SessionTeardownKind::NotDeleted,
                    Err(payload) => {
                        teardown_panic = Some(payload);
                        SessionTeardownKind::NotDeleted
                    }
                }
            }
            (kind, _) => kind,
        };
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| {
            self.handler.teardown_session(final_kind);
        })) {
            teardown_panic.get_or_insert(payload);
        }
        if let Some(payload) = teardown_panic {
            resume_unwind(payload);
        }
    }

    fn check_session(&self, msg: &WebDriverMessage<U>) -> WebDriverResult<()> {
        match msg.session_id {
            Some(ref msg_session_id) => match self.session {
                Some(ref existing_session) => {
                    if existing_session.id == *msg_session_id {
                        Ok(())
                    } else {
                        Err(WebDriverError::new(
                            ErrorStatus::InvalidSessionId,
                            format!("Got unexpected session id {msg_session_id}"),
                        ))
                    }
                }
                None => Err(WebDriverError::new(
                    ErrorStatus::InvalidSessionId,
                    "No WebDriver session is active",
                )),
            },
            None => {
                match self.session {
                    Some(_) => {
                        match msg.command {
                            WebDriverCommand::Status => Ok(()),
                            WebDriverCommand::NewSession(_) => Err(WebDriverError::new(
                                ErrorStatus::SessionNotCreated,
                                "Session is already started",
                            )),
                            _ => {
                                //This should be impossible
                                error!("Got a message with no session id");
                                Err(WebDriverError::new(
                                    ErrorStatus::UnknownError,
                                    "Got a command with no session?!",
                                ))
                            }
                        }
                    }
                    None => match msg.command {
                        WebDriverCommand::NewSession(_) | WebDriverCommand::Status => Ok(()),
                        _ => Err(WebDriverError::new(
                            ErrorStatus::InvalidSessionId,
                            "Tried to run a command before creating a session",
                        )),
                    },
                }
            }
        }
    }
}

impl<T: WebDriverHandler<U>, U: WebDriverExtensionRoute> Drop for Dispatcher<T, U> {
    fn drop(&mut self) {
        self.cancel_active_authority();
        if self.handler_session_open || self.session.is_some() {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                self.teardown_session(SessionTeardownKind::NotDeleted);
            }));
        }
    }
}

fn command_deadline_error() -> WebDriverError {
    WebDriverError::new(ErrorStatus::Timeout, "WebDriver command deadline exceeded")
}

fn send_dispatch_error(response: &Sender<DispatchResponse>, error: WebDriverError) {
    let _ = response.send(DispatchResponse {
        response: Err(error),
    });
}

#[derive(Clone)]
enum AdmissionPolicy {
    Legacy {
        allow_hosts: Arc<[Host]>,
        allow_origins: Arc<[Url]>,
    },
    Authenticated {
        bearer_token: Arc<BearerToken>,
        allowed_origins: Arc<[Box<str>]>,
        max_request_body_bytes: usize,
        max_dispatch_queue_depth: usize,
        request_deadline: Duration,
    },
}

impl AdmissionPolicy {
    const fn max_request_body_bytes(&self) -> usize {
        match self {
            Self::Legacy { .. } => LEGACY_MAX_REQUEST_BODY_BYTES,
            Self::Authenticated {
                max_request_body_bytes,
                ..
            } => *max_request_body_bytes,
        }
    }

    const fn max_dispatch_queue_depth(&self) -> usize {
        match self {
            Self::Legacy { .. } => LEGACY_MAX_DISPATCH_QUEUE_DEPTH,
            Self::Authenticated {
                max_dispatch_queue_depth,
                ..
            } => *max_dispatch_queue_depth,
        }
    }

    const fn request_deadline(&self) -> Duration {
        match self {
            Self::Legacy { .. } => LEGACY_REQUEST_DEADLINE,
            Self::Authenticated {
                request_deadline, ..
            } => *request_deadline,
        }
    }

    fn host_allowed(&self, server_address: &SocketAddr, host_header: &str) -> bool {
        match self {
            Self::Legacy { allow_hosts, .. } => {
                is_host_allowed(server_address, allow_hosts, host_header)
            }
            Self::Authenticated { .. } => host_header == server_address.to_string(),
        }
    }

    fn origin_allowed(&self, origin_header: &str) -> bool {
        match self {
            Self::Legacy { allow_origins, .. } => Url::parse(origin_header)
                .is_ok_and(|origin| is_origin_allowed(allow_origins, &origin)),
            Self::Authenticated {
                allowed_origins, ..
            } => allowed_origins
                .iter()
                .any(|allowed| allowed.as_ref() == origin_header),
        }
    }

    fn authorization_allowed(&self, authorization_header: Option<&str>) -> bool {
        match self {
            Self::Legacy { .. } => true,
            Self::Authenticated { bearer_token, .. } => {
                bearer_token.authorizes(authorization_header)
            }
        }
    }
}

/// Running `WebDriver` HTTP and dispatch threads.
pub struct Listener {
    server: Option<thread::JoinHandle<()>>,
    workers: Vec<thread::JoinHandle<()>>,
    dispatcher: Option<thread::JoinHandle<()>>,
    explicit_shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    shutdown: Arc<AtomicBool>,
    pub socket: SocketAddr,
}

impl Listener {
    /// Stops admission, cancels queued calls, tears down the handler session,
    /// and joins both owned threads. Repeated calls are harmless.
    ///
    /// # Errors
    ///
    /// Returns an error if either owned thread panicked.
    pub fn shutdown(&mut self) -> io::Result<()> {
        self.shutdown.store(true, Ordering::Release);
        if let Some(shutdown) = self.explicit_shutdown.take() {
            let _ = shutdown.send(());
        }
        let mut failure = None;
        if self
            .server
            .take()
            .is_some_and(|handle| handle.join().is_err())
        {
            failure = Some("WebDriver server thread panicked");
        }
        for worker in self.workers.drain(..) {
            if worker.join().is_err() {
                failure.get_or_insert("WebDriver connection worker panicked");
            }
        }
        if self
            .dispatcher
            .take()
            .is_some_and(|handle| handle.join().is_err())
        {
            failure.get_or_insert("WebDriver dispatcher thread panicked");
        }
        failure.map_or(Ok(()), |detail| Err(io::Error::other(detail)))
    }
}

impl fmt::Debug for Listener {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Listener")
            .field("socket", &self.socket)
            .field("running", &!self.shutdown.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(unix)]
struct ShutdownSignal {
    sigint: Option<tokio::signal::unix::Signal>,
    sigterm: Option<tokio::signal::unix::Signal>,
}

#[cfg(unix)]
impl ShutdownSignal {
    fn new() -> Self {
        use tokio::signal::unix::{SignalKind, signal};
        ShutdownSignal {
            sigint: signal(SignalKind::interrupt())
                .map_err(|_| warn!("Failed to register SIGINT handler"))
                .ok(),
            sigterm: signal(SignalKind::terminate())
                .map_err(|_| warn!("Failed to register SIGTERM handler"))
                .ok(),
        }
    }

    async fn recv(&mut self) {
        tokio::select! {
            _ = async { self.sigint.as_mut().unwrap().recv().await }, if self.sigint.is_some() => {},
            _ = async { self.sigterm.as_mut().unwrap().recv().await }, if self.sigterm.is_some() => {},
            () = std::future::pending::<()>(), if self.sigint.is_none() && self.sigterm.is_none() => {},
        }
    }
}

#[cfg(windows)]
struct ShutdownSignal {
    ctrl_c: Option<tokio::signal::windows::CtrlC>,
    ctrl_break: Option<tokio::signal::windows::CtrlBreak>,
}

#[cfg(windows)]
impl ShutdownSignal {
    fn new() -> Self {
        use tokio::signal::windows;
        ShutdownSignal {
            ctrl_c: windows::ctrl_c()
                .map_err(|_| warn!("Failed to register ctrl_c handler"))
                .ok(),
            ctrl_break: windows::ctrl_break()
                .map_err(|_| warn!("Failed to register ctrl_break handler"))
                .ok(),
        }
    }

    async fn recv(&mut self) {
        tokio::select! {
            _ = async { self.ctrl_c.as_mut().unwrap().recv().await }, if self.ctrl_c.is_some() => {},
            _ = async { self.ctrl_break.as_mut().unwrap().recv().await }, if self.ctrl_break.is_some() => {},
            () = std::future::pending::<()>(), if self.ctrl_c.is_none() && self.ctrl_break.is_none() => {},
        }
    }
}

/// Starts the legacy host/origin-admitted server used by existing embedders.
///
/// # Errors
///
/// Returns an I/O error for listener binding or owned-thread startup failure.
pub fn start<T, U>(
    address: SocketAddr,
    allow_hosts: Vec<Host>,
    allow_origins: Vec<Url>,
    handler: T,
    extension_routes: Vec<(Method, &'static str, U)>,
) -> ::std::io::Result<Listener>
where
    T: 'static + WebDriverHandler<U>,
    U: 'static + WebDriverExtensionRoute + Send + Sync,
{
    start_server(
        address,
        AdmissionPolicy::Legacy {
            allow_hosts: allow_hosts.into(),
            allow_origins: allow_origins.into(),
        },
        handler,
        extension_routes,
    )
}

/// Starts an authenticated, loopback-only, bounded embedded `WebDriver` server.
///
/// This path uses a strict one-request HTTP/1.1 connection worker rather than
/// the inherited Warp listener. Every accepted connection is assigned to a
/// fixed worker or immediately closed with 503; slow headers and bodies share
/// one absolute admission deadline. Transfer encoding and `Expect` are
/// rejected, responses close the connection, and rejected request bodies are
/// never read. The legacy [`start`] entry point retains its inherited server
/// behavior and is not the authenticated browser boundary.
///
/// # Errors
///
/// Returns an I/O error for binding or thread startup failure. The policy
/// constructor has already rejected a non-loopback address.
pub fn start_authenticated<T, U>(
    policy: ServerSecurityPolicy,
    handler: T,
    extension_routes: Vec<(Method, &'static str, U)>,
) -> io::Result<Listener>
where
    T: 'static + WebDriverHandler<U>,
    U: 'static + WebDriverExtensionRoute + Send + Sync,
{
    let ServerSecurityPolicy {
        bind_address,
        bearer_token,
        allowed_origins,
        max_request_body_bytes,
        max_dispatch_queue_depth,
        request_deadline,
    } = policy;
    let admission = AdmissionPolicy::Authenticated {
        bearer_token,
        allowed_origins: allowed_origins.into(),
        max_request_body_bytes,
        max_dispatch_queue_depth,
        request_deadline,
    };
    start_authenticated_server(bind_address, &admission, handler, extension_routes)
}

fn start_authenticated_server<T, U>(
    address: SocketAddr,
    admission: &AdmissionPolicy,
    handler: T,
    extension_routes: Vec<(Method, &'static str, U)>,
) -> io::Result<Listener>
where
    T: 'static + WebDriverHandler<U>,
    U: 'static + WebDriverExtensionRoute + Send + Sync,
{
    let listener = StdTcpListener::bind(address)?;
    listener.set_nonblocking(true)?;
    let socket = listener.local_addr()?;
    let (message_send, message_recv) = sync_channel(admission.max_dispatch_queue_depth());
    let shutdown = Arc::new(AtomicBool::new(false));

    let dispatcher_shutdown = Arc::clone(&shutdown);
    let dispatcher = thread::Builder::new()
        .name("webdriver dispatcher".to_owned())
        .spawn(move || {
            let mut dispatcher = Dispatcher::new(handler);
            dispatcher.run(&message_recv, &dispatcher_shutdown);
        })?;

    let mut routes = standard_routes::<U>();
    routes.extend(
        extension_routes
            .into_iter()
            .map(|(method, path, route)| (method, path, Route::Extension(route))),
    );
    let routes = Arc::new(routes);
    let worker_count = admission
        .max_dispatch_queue_depth()
        .saturating_add(1)
        .min(AUTHENTICATED_MAX_CONNECTION_WORKERS);
    let mut connection_senders = Vec::with_capacity(worker_count);
    let mut workers: Vec<thread::JoinHandle<()>> = Vec::with_capacity(worker_count);
    for worker_index in 0..worker_count {
        let (connection_send, connection_recv) =
            sync_channel(AUTHENTICATED_CONNECTION_QUEUE_PER_WORKER);
        connection_senders.push(connection_send);
        let worker_admission: AdmissionPolicy = admission.clone();
        let worker_routes = Arc::clone(&routes);
        let worker_messages = message_send.clone();
        let worker_shutdown = Arc::clone(&shutdown);
        let worker = match thread::Builder::new()
            .name(format!("webdriver connection {worker_index}"))
            .spawn(move || {
                authenticated_connection_worker(
                    socket,
                    &worker_admission,
                    &worker_routes,
                    &worker_messages,
                    &connection_recv,
                    &worker_shutdown,
                );
            }) {
            Ok(worker) => worker,
            Err(error) => {
                shutdown.store(true, Ordering::Release);
                drop(connection_senders);
                for worker in workers {
                    let _ = worker.join();
                }
                drop(message_send);
                let _ = dispatcher.join();
                return Err(error);
            }
        };
        workers.push(worker);
    }

    let server_shutdown = Arc::clone(&shutdown);
    let server = match thread::Builder::new()
        .name("webdriver server".to_owned())
        .spawn(move || {
            authenticated_accept_loop(&listener, &connection_senders, &server_shutdown);
            server_shutdown.store(true, Ordering::Release);
        }) {
        Ok(server) => server,
        Err(error) => {
            shutdown.store(true, Ordering::Release);
            for worker in workers {
                let _ = worker.join();
            }
            drop(message_send);
            let _ = dispatcher.join();
            return Err(error);
        }
    };
    drop(message_send);

    Ok(Listener {
        server: Some(server),
        workers,
        dispatcher: Some(dispatcher),
        explicit_shutdown: None,
        shutdown,
        socket,
    })
}

fn start_server<T, U>(
    address: SocketAddr,
    admission: AdmissionPolicy,
    handler: T,
    extension_routes: Vec<(Method, &'static str, U)>,
) -> io::Result<Listener>
where
    T: 'static + WebDriverHandler<U>,
    U: 'static + WebDriverExtensionRoute + Send + Sync,
{
    let listener = StdTcpListener::bind(address)?;
    listener.set_nonblocking(true)?;
    let addr = listener.local_addr()?;
    let (msg_send, msg_recv) = sync_channel(admission.max_dispatch_queue_depth());
    let shutdown = Arc::new(AtomicBool::new(false));
    let (explicit_shutdown_send, explicit_shutdown_recv) = tokio::sync::oneshot::channel();

    let builder = thread::Builder::new().name("webdriver server".to_string());
    let server_shutdown = Arc::clone(&shutdown);
    let server_handle = builder.spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .expect("WebDriver current-thread runtime construction failed");
        let listener = runtime
            .block_on(async { TcpListener::from_std(listener) })
            .expect("validated WebDriver listener conversion failed");
        let wroutes = build_warp_routes(addr, &admission, &extension_routes, &msg_send);
        let fut = warp::serve(wroutes).incoming(listener).run();
        runtime.block_on(async move {
            let mut shutdown_signal = ShutdownSignal::new();
            tokio::select! {
                () = fut => {}
                () = shutdown_signal.recv() => {}
                _ = explicit_shutdown_recv => {}
            }
        });
        server_shutdown.store(true, Ordering::Release);
        let _ = msg_send.try_send(DispatchMessage::Quit);
    })?;

    let builder = thread::Builder::new().name("webdriver dispatcher".to_string());
    let dispatcher_shutdown = Arc::clone(&shutdown);
    let dispatcher_handle = builder.spawn(move || {
        let mut dispatcher = Dispatcher::new(handler);
        dispatcher.run(&msg_recv, &dispatcher_shutdown);
    })?;

    Ok(Listener {
        server: Some(server_handle),
        workers: Vec::new(),
        dispatcher: Some(dispatcher_handle),
        explicit_shutdown: Some(explicit_shutdown_send),
        shutdown,
        socket: addr,
    })
}

struct RawHttpResponse {
    status: StatusCode,
    body: String,
}

struct AuthenticatedDispatchResponse {
    response: RawHttpResponse,
    delivery: Option<ResponseDeliveryGuard>,
}

struct AuthenticatedHeader {
    bytes: SecretBytes<AUTHENTICATED_MAX_HEADER_BYTES>,
    len: usize,
}

impl AuthenticatedHeader {
    fn as_slice(&self) -> &[u8] {
        &self.bytes.as_slice()[..self.len]
    }
}

#[derive(Clone, Copy)]
enum AdmissionReadError {
    Timeout,
    HeaderTooLarge,
    Malformed,
    Closed,
    Shutdown,
    Io,
}

struct AuthenticatedRequestHead<'a> {
    method: Method,
    path: &'a str,
    host: Option<&'a str>,
    authorization: Option<&'a str>,
    origin: Option<&'a str>,
    content_type: Option<&'a str>,
    content_length: Option<usize>,
    transfer_encoding: Option<&'a str>,
    expect: Option<&'a str>,
}

enum AuthenticatedRoute<U: WebDriverExtensionRoute> {
    Head,
    Command(Route<U>, Parameters),
}

fn authenticated_accept_loop(
    listener: &StdTcpListener,
    connection_senders: &[SyncSender<TcpStream>],
    shutdown: &AtomicBool,
) {
    let mut next_worker = 0;
    while !shutdown.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, peer)) => {
                if !peer.ip().is_loopback() {
                    drop(stream);
                    continue;
                }
                let mut pending = Some(stream);
                let mut disconnected = 0;
                for offset in 0..connection_senders.len() {
                    let index = (next_worker + offset) % connection_senders.len();
                    let stream = pending.take().expect("pending connection is retained");
                    match connection_senders[index].try_send(stream) {
                        Ok(()) => {
                            next_worker = (index + 1) % connection_senders.len();
                            break;
                        }
                        Err(TrySendError::Full(stream)) => pending = Some(stream),
                        Err(TrySendError::Disconnected(stream)) => {
                            disconnected += 1;
                            pending = Some(stream);
                        }
                    }
                }
                if disconnected == connection_senders.len() {
                    shutdown.store(true, Ordering::Release);
                }
                if let Some(mut stream) = pending {
                    let response = webdriver_error_parts(
                        WebDriverError::new(
                            ErrorStatus::UnknownError,
                            "WebDriver connection admission is full",
                        ),
                        Some(StatusCode::SERVICE_UNAVAILABLE),
                    );
                    let _ = stream.set_write_timeout(Some(AUTHENTICATED_IO_POLL));
                    let _ = write_raw_response(&mut stream, &response);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(AUTHENTICATED_ACCEPT_POLL);
            }
            Err(_) => {
                shutdown.store(true, Ordering::Release);
                break;
            }
        }
    }
}

fn authenticated_connection_worker<U: 'static + WebDriverExtensionRoute + Send + Sync>(
    server_address: SocketAddr,
    admission: &AdmissionPolicy,
    routes: &[(Method, &'static str, Route<U>)],
    message_send: &SyncSender<DispatchMessage<U>>,
    connection_recv: &Receiver<TcpStream>,
    shutdown: &AtomicBool,
) {
    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        match connection_recv.recv_timeout(DISPATCHER_SHUTDOWN_POLL) {
            Ok(mut stream) => handle_authenticated_connection(
                &mut stream,
                server_address,
                admission,
                routes,
                message_send,
                shutdown,
            ),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    while let Ok(stream) = connection_recv.try_recv() {
        drop(stream);
    }
}

fn handle_authenticated_connection<U: 'static + WebDriverExtensionRoute + Send + Sync>(
    stream: &mut TcpStream,
    server_address: SocketAddr,
    admission: &AdmissionPolicy,
    routes: &[(Method, &'static str, Route<U>)],
    message_send: &SyncSender<DispatchMessage<U>>,
    shutdown: &AtomicBool,
) {
    let io_poll = admission.request_deadline().min(AUTHENTICATED_IO_POLL);
    if stream.set_read_timeout(Some(io_poll)).is_err()
        || stream.set_write_timeout(Some(io_poll)).is_err()
    {
        return;
    }
    let deadline = Instant::now()
        .checked_add(admission.request_deadline())
        .unwrap_or_else(Instant::now);
    let header = match read_authenticated_header(stream, deadline, shutdown) {
        Ok(header) => header,
        Err(AdmissionReadError::Closed | AdmissionReadError::Shutdown) => return,
        Err(error) => {
            let response = admission_read_error_response(error);
            let _ = write_raw_response(stream, &response);
            return;
        }
    };
    let Ok(request) = parse_authenticated_header(header.as_slice()) else {
        let response = webdriver_error_parts(
            WebDriverError::new(ErrorStatus::UnknownError, "Malformed WebDriver HTTP header"),
            Some(StatusCode::BAD_REQUEST),
        );
        let _ = write_raw_response(stream, &response);
        return;
    };
    let admission_error = authenticate_request_head(server_address, admission, &request);
    if let Some(response) = admission_error {
        let _ = write_raw_response(stream, &response);
        return;
    }
    let route = match match_authenticated_route(routes, &request.method, request.path) {
        Ok(route) => route,
        Err(response) => {
            let _ = write_raw_response(stream, &response);
            return;
        }
    };
    let body_length = match authenticated_body_length(admission, &request) {
        Ok(body_length) => body_length,
        Err(response) => {
            let _ = write_raw_response(stream, &response);
            return;
        }
    };
    if let AuthenticatedRoute::Head = route {
        let _ = write_raw_response(
            stream,
            &RawHttpResponse {
                status: StatusCode::OK,
                body: String::new(),
            },
        );
        return;
    }
    let method = request.method.clone();
    drop(request);
    drop(header);
    let body = match read_authenticated_body(stream, body_length, deadline, shutdown) {
        Ok(body) => body,
        Err(AdmissionReadError::Closed | AdmissionReadError::Shutdown) => return,
        Err(error) => {
            let response = admission_read_error_response(error);
            let _ = write_raw_response(stream, &response);
            return;
        }
    };
    let AuthenticatedRoute::Command(route, parameters) = route else {
        unreachable!("HEAD was returned before body admission")
    };
    let response = match parse_authenticated_command(route, &parameters, &method, &body) {
        Ok(message) => {
            dispatch_authenticated_request(message, message_send, stream, deadline, shutdown)
        }
        Err(error) => authenticated_dispatch_error(error),
    };
    let AuthenticatedDispatchResponse { response, delivery } = response;
    let write_result = write_raw_response_until(stream, &response, deadline, shutdown);
    if let Some(delivery) = delivery
        && write_result.is_ok()
    {
        delivery.acknowledge();
    }
}

fn read_authenticated_header(
    stream: &mut TcpStream,
    deadline: Instant,
    shutdown: &AtomicBool,
) -> Result<AuthenticatedHeader, AdmissionReadError> {
    let mut header = AuthenticatedHeader {
        bytes: SecretBytes::zeroed(),
        len: 0,
    };
    loop {
        if shutdown.load(Ordering::Acquire) {
            return Err(AdmissionReadError::Shutdown);
        }
        if Instant::now() >= deadline {
            return Err(AdmissionReadError::Timeout);
        }
        if header.len == AUTHENTICATED_MAX_HEADER_BYTES {
            return Err(AdmissionReadError::HeaderTooLarge);
        }
        match stream.read(&mut header.bytes.as_mut_slice()[header.len..=header.len]) {
            Ok(0) => return Err(AdmissionReadError::Closed),
            Ok(_) => {
                header.len += 1;
                if header.as_slice().ends_with(b"\r\n\r\n") {
                    return Ok(header);
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return Err(AdmissionReadError::Io),
        }
    }
}

fn read_authenticated_body(
    stream: &mut TcpStream,
    length: usize,
    deadline: Instant,
    shutdown: &AtomicBool,
) -> Result<Vec<u8>, AdmissionReadError> {
    let mut body = vec![0_u8; length];
    let mut received = 0;
    while received < length {
        if shutdown.load(Ordering::Acquire) {
            return Err(AdmissionReadError::Shutdown);
        }
        if Instant::now() >= deadline {
            return Err(AdmissionReadError::Timeout);
        }
        match stream.read(&mut body[received..]) {
            Ok(0) => return Err(AdmissionReadError::Malformed),
            Ok(count) => received += count,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return Err(AdmissionReadError::Io),
        }
    }
    Ok(body)
}

fn parse_authenticated_header(header: &[u8]) -> Result<AuthenticatedRequestHead<'_>, ()> {
    let header = std::str::from_utf8(header).map_err(|_| ())?;
    let header = header.strip_suffix("\r\n\r\n").ok_or(())?;
    let mut lines = header.split("\r\n");
    let mut request_parts = lines.next().ok_or(())?.split(' ');
    let method = request_parts.next().ok_or(())?;
    let path = request_parts.next().ok_or(())?;
    let version = request_parts.next().ok_or(())?;
    if request_parts.next().is_some()
        || version != "HTTP/1.1"
        || !path.starts_with('/')
        || path.contains(['?', '#'])
    {
        return Err(());
    }
    let method = Method::from_bytes(method.as_bytes()).map_err(|_| ())?;
    let mut request = AuthenticatedRequestHead {
        method,
        path,
        host: None,
        authorization: None,
        origin: None,
        content_type: None,
        content_length: None,
        transfer_encoding: None,
        expect: None,
    };
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(())?;
        if name.is_empty()
            || name.trim() != name
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(());
        }
        let value = value.trim_matches([' ', '\t']);
        if value
            .bytes()
            .any(|byte| byte.is_ascii_control() && byte != b'\t')
        {
            return Err(());
        }
        if name.eq_ignore_ascii_case("host") {
            insert_unique_header(&mut request.host, value)?;
        } else if name.eq_ignore_ascii_case("authorization") {
            insert_unique_header(&mut request.authorization, value)?;
        } else if name.eq_ignore_ascii_case("origin") {
            insert_unique_header(&mut request.origin, value)?;
        } else if name.eq_ignore_ascii_case("content-type") {
            insert_unique_header(&mut request.content_type, value)?;
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            insert_unique_header(&mut request.transfer_encoding, value)?;
        } else if name.eq_ignore_ascii_case("expect") {
            insert_unique_header(&mut request.expect, value)?;
        } else if name.eq_ignore_ascii_case("content-length") {
            if request.content_length.is_some() {
                return Err(());
            }
            request.content_length = Some(value.parse().map_err(|_| ())?);
        }
    }
    Ok(request)
}

fn insert_unique_header<'a>(slot: &mut Option<&'a str>, value: &'a str) -> Result<(), ()> {
    if slot.replace(value).is_some() {
        Err(())
    } else {
        Ok(())
    }
}

fn authenticate_request_head(
    server_address: SocketAddr,
    admission: &AdmissionPolicy,
    request: &AuthenticatedRequestHead<'_>,
) -> Option<RawHttpResponse> {
    if !admission.authorization_allowed(request.authorization) {
        return Some(webdriver_error_parts(
            WebDriverError::new(
                ErrorStatus::UnknownError,
                "Missing or invalid WebDriver authorization",
            ),
            Some(StatusCode::UNAUTHORIZED),
        ));
    }
    let Some(host) = request.host else {
        return Some(webdriver_error_parts(
            WebDriverError::new(ErrorStatus::UnknownError, "Missing Host header"),
            None,
        ));
    };
    if !admission.host_allowed(&server_address, host) {
        return Some(webdriver_error_parts(
            WebDriverError::new(ErrorStatus::UnknownError, "Invalid Host header"),
            None,
        ));
    }
    if request
        .origin
        .is_some_and(|origin| !admission.origin_allowed(origin))
    {
        return Some(webdriver_error_parts(
            WebDriverError::new(ErrorStatus::UnknownError, "Invalid Origin header"),
            None,
        ));
    }
    None
}

fn match_authenticated_route<U: WebDriverExtensionRoute>(
    routes: &[(Method, &'static str, Route<U>)],
    method: &Method,
    path: &str,
) -> Result<AuthenticatedRoute<U>, RawHttpResponse> {
    let mut path_exists = false;
    for (route_method, pattern, route) in routes {
        let Some(parameters) = match_route_parameters(pattern, path) else {
            continue;
        };
        path_exists = true;
        if method == Method::HEAD {
            return Ok(AuthenticatedRoute::Head);
        }
        if method == route_method {
            return Ok(AuthenticatedRoute::Command(route.clone(), parameters));
        }
    }
    if path_exists {
        Err(webdriver_error_parts(
            WebDriverError::new(ErrorStatus::UnknownMethod, "Unsupported WebDriver method"),
            None,
        ))
    } else {
        Err(webdriver_error_parts(
            WebDriverError::new(ErrorStatus::UnknownCommand, "Unknown WebDriver command"),
            None,
        ))
    }
}

fn match_route_parameters(pattern: &str, path: &str) -> Option<Parameters> {
    let pattern = pattern.strip_prefix('/')?;
    let path = path.strip_prefix('/')?;
    let pattern_parts: Vec<_> = pattern.split('/').collect();
    let path_parts: Vec<_> = path.split('/').collect();
    if pattern_parts.len() != path_parts.len() {
        return None;
    }
    let mut parameters = Parameters::new();
    for (pattern, value) in pattern_parts.into_iter().zip(path_parts) {
        if let Some(name) = pattern
            .strip_prefix('{')
            .and_then(|part| part.strip_suffix('}'))
        {
            if value.is_empty() {
                return None;
            }
            parameters.insert(name.to_owned(), value.to_owned());
        } else if pattern != value {
            return None;
        }
    }
    Some(parameters)
}

fn authenticated_body_length(
    admission: &AdmissionPolicy,
    request: &AuthenticatedRequestHead<'_>,
) -> Result<usize, RawHttpResponse> {
    if request.transfer_encoding.is_some() || request.expect.is_some() {
        return Err(request_body_error());
    }
    let admits_body = matches!(request.method, Method::POST | Method::PUT);
    let length = if admits_body {
        request.content_length.ok_or_else(request_body_error)?
    } else {
        request.content_length.unwrap_or(0)
    };
    if length > admission.max_request_body_bytes() || (!admits_body && length != 0) {
        return Err(request_body_error());
    }
    if request.method == Method::POST {
        let content_type = request
            .content_type
            .map(|value| value.split_once(';').map_or(value, |(kind, _)| kind))
            .map(str::trim);
        if content_type.is_some_and(|kind| {
            kind.eq_ignore_ascii_case("application/x-www-form-urlencoded")
                || kind.eq_ignore_ascii_case("multipart/form-data")
                || kind.eq_ignore_ascii_case("text/plain")
        }) {
            return Err(webdriver_error_parts(
                WebDriverError::new(ErrorStatus::UnknownError, "Invalid Content-Type"),
                None,
            ));
        }
    }
    Ok(length)
}

fn request_body_error() -> RawHttpResponse {
    webdriver_error_parts(
        WebDriverError::new(
            ErrorStatus::InvalidArgument,
            "WebDriver request body is missing a safe length or exceeds its hard limit",
        ),
        Some(StatusCode::PAYLOAD_TOO_LARGE),
    )
}

fn admission_read_error_response(error: AdmissionReadError) -> RawHttpResponse {
    match error {
        AdmissionReadError::Timeout => webdriver_error_parts(
            WebDriverError::new(
                ErrorStatus::Timeout,
                "WebDriver connection admission deadline exceeded",
            ),
            Some(StatusCode::REQUEST_TIMEOUT),
        ),
        AdmissionReadError::HeaderTooLarge => webdriver_error_parts(
            WebDriverError::new(
                ErrorStatus::InvalidArgument,
                "WebDriver HTTP header exceeds its hard limit",
            ),
            Some(StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE),
        ),
        AdmissionReadError::Malformed | AdmissionReadError::Io => webdriver_error_parts(
            WebDriverError::new(
                ErrorStatus::UnknownError,
                "Malformed WebDriver HTTP request",
            ),
            Some(StatusCode::BAD_REQUEST),
        ),
        AdmissionReadError::Closed | AdmissionReadError::Shutdown => webdriver_error_parts(
            WebDriverError::new(ErrorStatus::UnknownError, "WebDriver connection closed"),
            Some(StatusCode::BAD_REQUEST),
        ),
    }
}

fn parse_authenticated_command<U: WebDriverExtensionRoute>(
    route: Route<U>,
    parameters: &Parameters,
    method: &Method,
    body: &[u8],
) -> WebDriverResult<WebDriverMessage<U>> {
    let body = std::str::from_utf8(body).map_err(|_| {
        WebDriverError::new(
            ErrorStatus::InvalidArgument,
            "Request body was not valid UTF-8",
        )
    })?;
    WebDriverMessage::from_http(route, parameters, body, method == Method::POST)
}

fn dispatch_authenticated_request<U: 'static + WebDriverExtensionRoute + Send + Sync>(
    message: WebDriverMessage<U>,
    channel: &SyncSender<DispatchMessage<U>>,
    stream: &mut TcpStream,
    deadline: Instant,
    shutdown: &AtomicBool,
) -> AuthenticatedDispatchResponse {
    let lifetime = DispatchLifetime::new(deadline);
    let delivery = ResponseDelivery::new(lifetime.clone());
    let delivery_guard = ResponseDeliveryGuard::new(delivery.clone());
    let (response_send, response_recv) = std::sync::mpsc::channel();
    match channel.try_send(DispatchMessage::HandleWebDriver(Box::new(
        DispatchRequest {
            message,
            response: response_send,
            lifetime: Some(lifetime.clone()),
            delivery: Some(delivery.clone()),
        },
    ))) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            delivery.abandon();
            return AuthenticatedDispatchResponse {
                response: webdriver_error_parts(
                    WebDriverError::new(
                        ErrorStatus::UnknownError,
                        "WebDriver dispatch queue is full",
                    ),
                    Some(StatusCode::SERVICE_UNAVAILABLE),
                ),
                delivery: None,
            };
        }
        Err(TrySendError::Disconnected(_)) => {
            delivery.abandon();
            return authenticated_dispatch_error(WebDriverError::new(
                ErrorStatus::UnknownError,
                "WebDriver dispatcher is unavailable",
            ));
        }
    }
    if stream.set_nonblocking(true).is_err() {
        delivery.abandon();
        return authenticated_dispatch_error(WebDriverError::new(
            ErrorStatus::UnknownError,
            "WebDriver connection state could not be observed",
        ));
    }
    let waited =
        wait_authenticated_dispatch(&response_recv, &lifetime, &delivery, stream, shutdown);
    match waited {
        Ok(DispatchResponse { response }) => {
            let response = match response {
                Ok(response) => webdriver_success_parts(response),
                Err(error) => webdriver_error_parts(error, None),
            };
            if Instant::now() >= deadline {
                delivery.abandon();
                return authenticated_dispatch_error(command_deadline_error());
            }
            AuthenticatedDispatchResponse {
                response,
                delivery: Some(delivery_guard),
            }
        }
        Err(DispatchWaitError::Timeout) => authenticated_dispatch_error(command_deadline_error()),
        Err(DispatchWaitError::PeerClosed) => authenticated_dispatch_error(WebDriverError::new(
            ErrorStatus::UnknownError,
            "WebDriver client disconnected before its response",
        )),
        Err(DispatchWaitError::Shutdown) => authenticated_dispatch_error(WebDriverError::new(
            ErrorStatus::UnknownError,
            "WebDriver server shut down before its response",
        )),
        Err(DispatchWaitError::Disconnected) => authenticated_dispatch_error(WebDriverError::new(
            ErrorStatus::UnknownError,
            "WebDriver response channel closed",
        )),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DispatchWaitError {
    Timeout,
    PeerClosed,
    Shutdown,
    Disconnected,
}

fn wait_authenticated_dispatch(
    response: &Receiver<DispatchResponse>,
    lifetime: &DispatchLifetime,
    delivery: &ResponseDelivery,
    stream: &TcpStream,
    shutdown: &AtomicBool,
) -> Result<DispatchResponse, DispatchWaitError> {
    // Reserve bounded terminal-response time *inside* the one absolute request
    // deadline. This is not a fresh command budget: once the reserve begins,
    // abandonment synchronously revokes the same request/session authorities.
    let Some(initial_remaining) = lifetime.deadline().checked_duration_since(Instant::now()) else {
        delivery.abandon();
        return Err(DispatchWaitError::Timeout);
    };
    let response_write_reserve = AUTHENTICATED_RESPONSE_WRITE_RESERVE.min(initial_remaining / 2);
    let dispatch_wait_deadline = lifetime
        .deadline()
        .checked_sub(response_write_reserve)
        .unwrap_or_else(|| lifetime.deadline());
    loop {
        if shutdown.load(Ordering::Acquire) {
            delivery.abandon();
            return Err(DispatchWaitError::Shutdown);
        }
        let Some(remaining) = dispatch_wait_deadline.checked_duration_since(Instant::now()) else {
            delivery.abandon();
            return Err(DispatchWaitError::Timeout);
        };
        match response.recv_timeout(remaining.min(AUTHENTICATED_IO_POLL)) {
            Ok(response) => return Ok(response),
            Err(RecvTimeoutError::Disconnected) => {
                delivery.abandon();
                return Err(DispatchWaitError::Disconnected);
            }
            Err(RecvTimeoutError::Timeout) => {}
        }

        let mut byte = [0_u8; 1];
        match stream.peek(&mut byte) {
            Ok(0) => {
                delivery.abandon();
                return Err(DispatchWaitError::PeerClosed);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => {
                delivery.abandon();
                return Err(DispatchWaitError::PeerClosed);
            }
        }
    }
}

fn authenticated_dispatch_error(error: WebDriverError) -> AuthenticatedDispatchResponse {
    AuthenticatedDispatchResponse {
        response: webdriver_error_parts(error, None),
        delivery: None,
    }
}

fn write_raw_response(stream: &mut TcpStream, response: &RawHttpResponse) -> io::Result<()> {
    let reason = response
        .status
        .canonical_reason()
        .unwrap_or("WebDriver Response");
    let header = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nCache-Control: no-cache\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        response.status.as_u16(),
        response.body.len(),
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(response.body.as_bytes())?;
    stream.flush()?;
    stream.shutdown(std::net::Shutdown::Write)
}

fn write_raw_response_until(
    stream: &mut TcpStream,
    response: &RawHttpResponse,
    deadline: Instant,
    shutdown: &AtomicBool,
) -> io::Result<()> {
    stream.set_nonblocking(true)?;
    let reason = response
        .status
        .canonical_reason()
        .unwrap_or("WebDriver Response");
    let header = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nCache-Control: no-cache\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        response.status.as_u16(),
        response.body.len(),
    );
    write_all_until(stream, header.as_bytes(), deadline, shutdown)?;
    write_all_until(stream, response.body.as_bytes(), deadline, shutdown)?;
    stream.shutdown(std::net::Shutdown::Write)
}

fn write_all_until(
    stream: &mut TcpStream,
    mut bytes: &[u8],
    deadline: Instant,
    shutdown: &AtomicBool,
) -> io::Result<()> {
    while !bytes.is_empty() {
        if shutdown.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "WebDriver server shut down during response delivery",
            ));
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "WebDriver response delivery exceeded its absolute deadline",
            ));
        };
        match stream.write(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "WebDriver response socket accepted zero bytes",
                ));
            }
            Ok(written) => bytes = &bytes[written..],
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                thread::sleep(remaining.min(AUTHENTICATED_ACCEPT_POLL));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn build_warp_routes<U: 'static + WebDriverExtensionRoute + Send + Sync>(
    address: SocketAddr,
    admission: &AdmissionPolicy,
    ext_routes: &[(Method, &'static str, U)],
    chan: &SyncSender<DispatchMessage<U>>,
) -> impl Filter<Extract = (impl Reply,), Error = Infallible> + Clone + 'static {
    let mut std_routes = standard_routes::<U>();

    let (method, path, res) = std_routes.pop().unwrap();
    trace!("Build standard route for {path}");
    let mut wroutes = build_route(address, admission.clone(), &method, path, res, chan.clone());

    for (method, path, res) in std_routes {
        trace!("Build standard route for {path}");
        wroutes = wroutes
            .or(build_route(
                address,
                admission.clone(),
                &method,
                path,
                res.clone(),
                chan.clone(),
            ))
            .unify()
            .boxed();
    }

    for (method, path, res) in ext_routes {
        trace!("Build vendor route for {path}");
        wroutes = wroutes
            .or(build_route(
                address,
                admission.clone(),
                method,
                path,
                Route::Extension(res.clone()),
                chan.clone(),
            ))
            .unify()
            .boxed();
    }

    wroutes.recover(handle_rejection)
}

fn is_host_allowed(server_address: &SocketAddr, allow_hosts: &[Host], host_header: &str) -> bool {
    // Validate that the Host header value has a hostname in allow_hosts and
    // the port matches the server configuration
    let Ok(header_host_url) = Url::parse(&format!("http://{host_header}")) else {
        return false;
    };

    let host = match header_host_url.host() {
        Some(host) => host.to_owned(),
        None => {
            // This shouldn't be possible since http URL always have a
            // host, but conservatively return false here, which will cause
            // an error response
            return false;
        }
    };
    let Some(port) = header_host_url.port_or_known_default() else {
        // This shouldn't be possible since an HTTP URL always has a default
        // port, but conservatively reject it here.
        return false;
    };

    let host_matches = match host {
        Host::Domain(_) => allow_hosts.contains(&host),
        Host::Ipv4(_) | Host::Ipv6(_) => true,
    };
    let port_matches = server_address.port() == port;
    host_matches && port_matches
}

fn is_origin_allowed(allow_origins: &[Url], origin_url: &Url) -> bool {
    // Validate that the Origin header value is in allow_origins
    allow_origins.contains(origin_url)
}

fn build_route<U: 'static + WebDriverExtensionRoute + Send + Sync>(
    server_address: SocketAddr,
    admission: AdmissionPolicy,
    method: &Method,
    path: &'static str,
    route: Route<U>,
    chan: SyncSender<DispatchMessage<U>>,
) -> warp::filters::BoxedFilter<(warp::reply::Response,)> {
    let admits_body = matches!(method.as_str(), "POST" | "PUT");
    let mut subroute = match method.as_str() {
        "GET" => warp::get().boxed(),
        "POST" => warp::post().boxed(),
        "DELETE" => warp::delete().boxed(),
        "OPTIONS" => warp::options().boxed(),
        "PUT" => warp::put().boxed(),
        _ => panic!("Unsupported method"),
    }
    .or(warp::head())
    .unify()
    .map(Parameters::new)
    .boxed();

    for part in path.split('/') {
        if part.is_empty() {
            continue;
        }
        if part.starts_with('{') {
            assert!(part.ends_with('}'));
            subroute = subroute
                .and(warp::path::param())
                .map(move |mut params: Parameters, param: String| {
                    let name = &part[1..part.len() - 1];
                    params.insert(name.to_owned(), param);
                    params
                })
                .boxed();
        } else {
            subroute = subroute.and(warp::path(part)).boxed();
        }
    }

    let body_limit = u64::try_from(admission.max_request_body_bytes())
        .expect("WebDriver request-body hard limit fits u64");
    let body = bounded_body_filter(admits_body, body_limit);
    subroute
        .and(warp::path::end())
        .and(warp::path::full())
        .and(warp::method())
        .and(warp::header::optional::<String>("origin"))
        .and(warp::header::optional::<String>("host"))
        .and(warp::header::optional::<String>("authorization"))
        .and(warp::header::optional::<String>("content-type"))
        .and(body)
        .and_then(
            move |params,
                  full_path: warp::path::FullPath,
                  method,
                  origin_header: Option<String>,
                  host_header: Option<String>,
                  authorization_header: Option<String>,
                  content_type_header: Option<String>,
                  body: Bytes| {
                let admission = admission.clone();
                let route = route.clone();
                let chan = chan.clone();
                async move {
                    Ok::<_, Infallible>(
                        handle_http_request(
                            server_address,
                            admission,
                            route,
                            chan,
                            params,
                            full_path,
                            method,
                            origin_header,
                            host_header,
                            authorization_header,
                            content_type_header,
                            body,
                        )
                        .await,
                    )
                }
            },
        )
        .boxed()
}

fn bounded_body_filter(admits_body: bool, body_limit: u64) -> warp::filters::BoxedFilter<(Bytes,)> {
    if admits_body {
        warp::body::content_length_limit(body_limit)
            .and(warp::body::bytes())
            .boxed()
    } else {
        warp::header::optional::<u64>("content-length")
            .and(warp::header::optional::<String>("transfer-encoding"))
            .and_then(
                |content_length: Option<u64>, transfer_encoding: Option<String>| async move {
                    if content_length.unwrap_or(0) == 0 && transfer_encoding.is_none() {
                        Ok(Bytes::new())
                    } else {
                        Err(warp::reject::custom(UnexpectedRequestBody))
                    }
                },
            )
            .boxed()
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn handle_http_request<U: 'static + WebDriverExtensionRoute + Send + Sync>(
    server_address: SocketAddr,
    admission: AdmissionPolicy,
    route: Route<U>,
    chan: SyncSender<DispatchMessage<U>>,
    params: Parameters,
    full_path: warp::path::FullPath,
    method: Method,
    origin_header: Option<String>,
    host_header: Option<String>,
    authorization_header: Option<String>,
    content_type_header: Option<String>,
    body: Bytes,
) -> warp::reply::Response {
    if !admission.authorization_allowed(authorization_header.as_deref()) {
        warn!("Rejected WebDriver request with missing or invalid bearer authorization");
        return webdriver_error_response(
            WebDriverError::new(
                ErrorStatus::UnknownError,
                "Missing or invalid WebDriver authorization",
            ),
            Some(StatusCode::UNAUTHORIZED),
        );
    }

    let Some(host) = host_header.as_deref() else {
        warn!("Rejected WebDriver request with missing Host header");
        return webdriver_error_response(
            WebDriverError::new(ErrorStatus::UnknownError, "Missing Host header"),
            None,
        );
    };
    if !admission.host_allowed(&server_address, host) {
        warn!("Rejected WebDriver request with an invalid Host header");
        return webdriver_error_response(
            WebDriverError::new(ErrorStatus::UnknownError, "Invalid Host header"),
            None,
        );
    }

    if let Some(origin) = origin_header.as_deref()
        && !admission.origin_allowed(origin)
    {
        warn!("Rejected WebDriver request with an invalid Origin header");
        return webdriver_error_response(
            WebDriverError::new(ErrorStatus::UnknownError, "Invalid Origin header"),
            None,
        );
    }

    if method == Method::HEAD {
        return json_response(StatusCode::OK, String::new());
    }

    if method == Method::POST {
        let content_type = content_type_header
            .as_deref()
            .map(|value| value.split_once(';').map_or(value, |(kind, _)| kind))
            .map(str::trim)
            .map(str::to_ascii_lowercase);
        if matches!(
            content_type.as_deref(),
            Some("application/x-www-form-urlencoded" | "multipart/form-data" | "text/plain")
        ) {
            warn!("Rejected WebDriver POST with a CORS-safelisted Content-Type");
            return webdriver_error_response(
                WebDriverError::new(ErrorStatus::UnknownError, "Invalid Content-Type"),
                None,
            );
        }
    }

    let Ok(body) = std::str::from_utf8(body.as_ref()) else {
        return webdriver_error_response(
            WebDriverError::new(
                ErrorStatus::InvalidArgument,
                "Request body was not valid UTF-8",
            ),
            None,
        );
    };

    trace!("WebDriver request {} {}", method, full_path.as_str());
    let message = match WebDriverMessage::from_http(route, &params, body, method == Method::POST) {
        Ok(message) => message,
        Err(error) => return webdriver_error_response(error, None),
    };

    let (response_send, response_recv) = std::sync::mpsc::channel();
    match chan.try_send(DispatchMessage::HandleWebDriver(Box::new(
        DispatchRequest {
            message,
            response: response_send,
            lifetime: None,
            delivery: None,
        },
    ))) {
        Ok(()) => {}
        Err(std::sync::mpsc::TrySendError::Full(_)) => {
            return webdriver_error_response(
                WebDriverError::new(
                    ErrorStatus::UnknownError,
                    "WebDriver dispatch queue is full",
                ),
                Some(StatusCode::SERVICE_UNAVAILABLE),
            );
        }
        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
            return webdriver_error_response(
                WebDriverError::new(
                    ErrorStatus::UnknownError,
                    "WebDriver dispatcher is unavailable",
                ),
                None,
            );
        }
    }

    let deadline = admission.request_deadline();
    let response = tokio::task::spawn_blocking(move || response_recv.recv_timeout(deadline)).await;
    match response {
        Ok(Ok(DispatchResponse {
            response: Ok(response),
            ..
        })) => webdriver_success_response(response),
        Ok(Ok(DispatchResponse {
            response: Err(error),
            ..
        })) => webdriver_error_response(error, None),
        Ok(Err(RecvTimeoutError::Timeout)) => webdriver_error_response(
            WebDriverError::new(ErrorStatus::Timeout, "WebDriver command deadline exceeded"),
            None,
        ),
        Ok(Err(RecvTimeoutError::Disconnected)) => webdriver_error_response(
            WebDriverError::new(
                ErrorStatus::UnknownError,
                "WebDriver response channel closed",
            ),
            None,
        ),
        Err(_) => webdriver_error_response(
            WebDriverError::new(
                ErrorStatus::UnknownError,
                "WebDriver response waiter failed",
            ),
            None,
        ),
    }
}

fn webdriver_success_response(response: WebDriverResponse) -> warp::reply::Response {
    let response = webdriver_success_parts(response);
    json_response(response.status, response.body)
}

fn webdriver_success_parts(response: WebDriverResponse) -> RawHttpResponse {
    let serialized = serde_json::to_string(&response);
    drop(response);
    match serialized {
        Ok(body) => RawHttpResponse {
            status: StatusCode::OK,
            body,
        },
        Err(_) => webdriver_error_parts(
            WebDriverError::new(
                ErrorStatus::UnknownError,
                "WebDriver response serialization failed",
            ),
            None,
        ),
    }
}

fn webdriver_error_response(
    error: WebDriverError,
    status_override: Option<StatusCode>,
) -> warp::reply::Response {
    let response = webdriver_error_parts(error, status_override);
    json_response(response.status, response.body)
}

fn webdriver_error_parts(
    error: WebDriverError,
    status_override: Option<StatusCode>,
) -> RawHttpResponse {
    let status = status_override.unwrap_or_else(|| error.http_status());
    let serialized = serde_json::to_string(&error);
    drop(error);
    let body = serialized.unwrap_or_else(|_| {
        r#"{"value":{"error":"unknown error","message":"WebDriver error serialization failed","stacktrace":""}}"#
            .to_owned()
    });
    RawHttpResponse { status, body }
}

fn json_response(status: StatusCode, body: String) -> warp::reply::Response {
    warp::reply::with_header(
        warp::reply::with_header(
            warp::reply::with_status(body, status),
            http::header::CONTENT_TYPE,
            "application/json; charset=utf-8",
        ),
        http::header::CACHE_CONTROL,
        "no-cache",
    )
    .into_response()
}

async fn handle_rejection(rejection: Rejection) -> Result<warp::reply::Response, Infallible> {
    let response = if rejection.is_not_found() {
        webdriver_error_response(
            WebDriverError::new(ErrorStatus::UnknownCommand, "Unknown WebDriver command"),
            None,
        )
    } else if rejection.find::<warp::reject::PayloadTooLarge>().is_some()
        || rejection.find::<warp::reject::LengthRequired>().is_some()
        || rejection.find::<UnexpectedRequestBody>().is_some()
    {
        webdriver_error_response(
            WebDriverError::new(
                ErrorStatus::InvalidArgument,
                "WebDriver request body is missing a safe length or exceeds its hard limit",
            ),
            Some(StatusCode::PAYLOAD_TOO_LARGE),
        )
    } else if rejection.find::<warp::reject::MethodNotAllowed>().is_some() {
        webdriver_error_response(
            WebDriverError::new(ErrorStatus::UnknownMethod, "Unsupported WebDriver method"),
            None,
        )
    } else {
        webdriver_error_response(
            WebDriverError::new(ErrorStatus::UnknownError, "Rejected WebDriver request"),
            None,
        )
    };
    Ok(response)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::str::FromStr;
    use std::sync::mpsc;

    use serde_json::json;

    struct DeliveryGateHandler {
        new_sessions: usize,
        session_commands: Arc<std::sync::atomic::AtomicUsize>,
        teardowns: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl WebDriverHandler<VoidWebDriverExtensionRoute> for DeliveryGateHandler {
        fn handle_command(
            &mut self,
            _: &Option<Session>,
            message: WebDriverMessage<VoidWebDriverExtensionRoute>,
        ) -> WebDriverResult<WebDriverResponse> {
            match message.command {
                WebDriverCommand::NewSession(_) => {
                    self.new_sessions += 1;
                    let session = if self.new_sessions == 1 {
                        "pending-terminal-delivery"
                    } else {
                        "fresh-after-abandonment"
                    };
                    Ok(WebDriverResponse::NewSession(
                        crate::response::NewSessionResponse::new(session.to_owned(), json!({})),
                    ))
                }
                WebDriverCommand::GetCurrentUrl => {
                    self.session_commands.fetch_add(1, Ordering::SeqCst);
                    Ok(WebDriverResponse::Generic(crate::response::ValueResponse(
                        json!("http://must-not-run.invalid/"),
                    )))
                }
                WebDriverCommand::DeleteSession => Ok(WebDriverResponse::DeleteSession),
                _ => Err(WebDriverError::new(
                    ErrorStatus::UnsupportedOperation,
                    "delivery-gate test command is unsupported",
                )),
            }
        }

        fn teardown_session(&mut self, _: SessionTeardownKind) {
            self.teardowns.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn test_new_session_message() -> WebDriverMessage<VoidWebDriverExtensionRoute> {
        WebDriverMessage {
            session_id: None,
            command: WebDriverCommand::NewSession(
                serde_json::from_value(json!({"capabilities": {}})).unwrap(),
            ),
        }
    }

    #[test]
    fn cancellation_cannot_overtake_an_admitted_owner_transition() {
        let lifetime = DispatchLifetime::new(Instant::now() + Duration::from_secs(1));
        let effects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (entered_send, entered_recv) = mpsc::channel();
        let (release_send, release_recv) = mpsc::channel();
        let effect_lifetime = lifetime.clone();
        let effect_count = Arc::clone(&effects);
        let effect = thread::spawn(move || {
            effect_lifetime.run_if_active(|| {
                entered_send.send(()).unwrap();
                release_recv.recv_timeout(Duration::from_secs(1)).unwrap();
                effect_count.fetch_add(1, Ordering::SeqCst);
            })
        });
        entered_recv.recv_timeout(Duration::from_secs(1)).unwrap();

        let cancel_lifetime = lifetime.clone();
        let (cancelled_send, cancelled_recv) = mpsc::channel();
        let cancel = thread::spawn(move || {
            cancelled_send.send(cancel_lifetime.cancel()).unwrap();
        });
        assert!(
            cancelled_recv
                .recv_timeout(Duration::from_millis(20))
                .is_err()
        );
        release_send.send(()).unwrap();
        assert_eq!(effect.join().unwrap(), Some(()));
        assert!(cancelled_recv.recv_timeout(Duration::from_secs(1)).unwrap());
        cancel.join().unwrap();
        assert!(lifetime.is_cancelled());
        assert!(
            lifetime
                .run_if_active(|| effects.fetch_add(1, Ordering::SeqCst))
                .is_none()
        );
        assert_eq!(effects.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn exact_session_revocation_and_owner_mutation_have_one_winner() {
        let request = DispatchLifetime::new(Instant::now() + Duration::from_secs(1));
        let authority = DispatchLifetime::new(Instant::now() + Duration::from_secs(1));
        assert!(authority.try_complete());
        let effects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (entered_send, entered_recv) = mpsc::channel();
        let (release_send, release_recv) = mpsc::channel();
        let effect_request = request.clone();
        let effect_authority = authority.clone();
        let effect_count = Arc::clone(&effects);
        let effect = thread::spawn(move || {
            effect_request.run_if_active_with_authority(&effect_authority, || {
                entered_send.send(()).unwrap();
                release_recv.recv_timeout(Duration::from_secs(1)).unwrap();
                effect_count.fetch_add(1, Ordering::SeqCst);
            })
        });
        entered_recv.recv_timeout(Duration::from_secs(1)).unwrap();

        let revoked_authority = authority.clone();
        let (revoked_send, revoked_recv) = mpsc::channel();
        let revoke = thread::spawn(move || {
            revoked_authority.revoke();
            revoked_send.send(()).unwrap();
        });
        assert!(
            revoked_recv
                .recv_timeout(Duration::from_millis(20))
                .is_err()
        );
        release_send.send(()).unwrap();
        assert_eq!(effect.join().unwrap(), Some(()));
        revoked_recv.recv_timeout(Duration::from_secs(1)).unwrap();
        revoke.join().unwrap();
        assert!(authority.is_cancelled());
        assert!(
            request
                .run_if_active_with_authority(&authority, || {
                    effects.fetch_add(1, Ordering::SeqCst);
                })
                .is_none()
        );
        assert_eq!(effects.load(Ordering::SeqCst), 1);

        let teardown = DispatchLifetime::new(Instant::now() + Duration::from_secs(1));
        assert_eq!(
            teardown.run_if_active_with_revoked_authority(&authority, || 7_u8),
            Some(7)
        );
    }

    #[test]
    fn pending_terminal_delivery_gates_queued_session_command_and_recovers() {
        let session_commands = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let teardowns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (send, receive) = sync_channel(4);
        let shutdown = Arc::new(AtomicBool::new(false));
        let dispatcher_shutdown = Arc::clone(&shutdown);
        let handler_commands = Arc::clone(&session_commands);
        let handler_teardowns = Arc::clone(&teardowns);
        let dispatcher = thread::spawn(move || {
            Dispatcher::new(DeliveryGateHandler {
                new_sessions: 0,
                session_commands: handler_commands,
                teardowns: handler_teardowns,
            })
            .run(&receive, &dispatcher_shutdown);
        });

        let first_lifetime = DispatchLifetime::new(Instant::now() + Duration::from_secs(2));
        let first_delivery = ResponseDelivery::new(first_lifetime.clone());
        let (first_response, first_receive) = mpsc::channel();
        send.send(DispatchMessage::HandleWebDriver(Box::new(
            DispatchRequest {
                message: test_new_session_message(),
                response: first_response,
                lifetime: Some(first_lifetime.clone()),
                delivery: Some(first_delivery.clone()),
            },
        )))
        .unwrap();
        assert!(matches!(
            first_receive
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .response,
            Ok(WebDriverResponse::NewSession(_))
        ));
        assert!(!first_lifetime.is_cancelled());

        let second_lifetime = DispatchLifetime::new(Instant::now() + Duration::from_secs(2));
        let (second_response, second_receive) = mpsc::channel();
        send.send(DispatchMessage::HandleWebDriver(Box::new(
            DispatchRequest {
                message: WebDriverMessage {
                    session_id: Some("pending-terminal-delivery".to_owned()),
                    command: WebDriverCommand::GetCurrentUrl,
                },
                response: second_response,
                lifetime: Some(second_lifetime),
                delivery: None,
            },
        )))
        .unwrap();
        assert!(
            second_receive
                .recv_timeout(Duration::from_millis(30))
                .is_err(),
            "the queued session command escaped a pending delivery gate"
        );
        assert_eq!(session_commands.load(Ordering::SeqCst), 0);

        first_delivery.abandon();
        let rejected = second_receive.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            rejected.response,
            Err(WebDriverError {
                error: ErrorStatus::InvalidSessionId,
                ..
            })
        ));
        assert!(first_lifetime.is_cancelled());
        assert_eq!(session_commands.load(Ordering::SeqCst), 0);
        assert_eq!(teardowns.load(Ordering::SeqCst), 1);

        let recovery_lifetime = DispatchLifetime::new(Instant::now() + Duration::from_secs(2));
        let (recovery_response, recovery_receive) = mpsc::channel();
        send.send(DispatchMessage::HandleWebDriver(Box::new(
            DispatchRequest {
                message: test_new_session_message(),
                response: recovery_response,
                lifetime: Some(recovery_lifetime),
                delivery: None,
            },
        )))
        .unwrap();
        let recovery = recovery_receive
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(matches!(
            recovery.response,
            Ok(WebDriverResponse::NewSession(response))
                if response.session_id == "fresh-after-abandonment"
        ));

        send.send(DispatchMessage::Quit).unwrap();
        dispatcher.join().unwrap();
    }

    #[test]
    fn completed_response_delivery_requires_ack_and_revokes_late_publication() {
        let request = DispatchLifetime::new(Instant::now() + Duration::from_secs(1));
        let session = DispatchLifetime::new(Instant::now() + Duration::from_secs(1));
        assert!(request.try_complete());
        assert!(session.try_complete());
        let delivery = ResponseDelivery::new(request.clone());
        let guard = ResponseDeliveryGuard::new(delivery.clone());
        assert!(delivery.publish_session_authority(Some(&session)));
        drop(guard);
        assert!(request.is_cancelled());
        assert!(session.is_cancelled());

        let late_request = DispatchLifetime::new(Instant::now() + Duration::from_secs(1));
        let late_session = DispatchLifetime::new(Instant::now() + Duration::from_secs(1));
        assert!(late_request.try_complete());
        assert!(late_session.try_complete());
        let late_delivery = ResponseDelivery::new(late_request.clone());
        drop(ResponseDeliveryGuard::new(late_delivery.clone()));
        assert!(!late_delivery.publish_session_authority(Some(&late_session)));
        assert!(late_request.is_cancelled());
        assert!(late_session.is_cancelled());

        let delivered_request = DispatchLifetime::new(Instant::now() + Duration::from_secs(1));
        let delivered_session = DispatchLifetime::new(Instant::now() + Duration::from_secs(1));
        assert!(delivered_request.try_complete());
        assert!(delivered_session.try_complete());
        let delivered = ResponseDelivery::new(delivered_request.clone());
        assert!(delivered.publish_session_authority(Some(&delivered_session)));
        ResponseDeliveryGuard::new(delivered).acknowledge();
        assert!(!delivered_request.is_cancelled());
        assert!(!delivered_session.is_cancelled());

        let rejected_request = DispatchLifetime::new(Instant::now() + Duration::from_secs(1));
        let rejected_session = DispatchLifetime::new(Instant::now() + Duration::from_secs(1));
        rejected_session.revoke();
        let rejected = ResponseDelivery::new(rejected_request.clone());
        assert!(!rejected.publish_session_authority(Some(&rejected_session)));
        assert!(rejected_request.is_cancelled());
        assert!(rejected_session.is_cancelled());
    }

    #[test]
    fn secret_storage_zeroizes_on_normal_drop_parse_failure_and_unwind() {
        fn observed_drop<const N: usize>(operation: impl FnOnce(&mut SecretBytes<N>)) -> Vec<u8> {
            let observed = Arc::new(Mutex::new(Vec::new()));
            let mut secret = SecretBytes::with_drop_observer(Arc::clone(&observed));
            operation(&mut secret);
            drop(secret);
            observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        let explicit = observed_drop::<32>(|secret| {
            secret.as_mut_slice().fill(0xa5);
            secret.zeroize();
            assert!(secret.as_slice().iter().all(|byte| *byte == 0));
            secret.as_mut_slice().fill(0x5a);
        });
        assert_eq!(explicit, vec![0; 32]);

        let parse_observed = Arc::new(Mutex::new(Vec::new()));
        let mut header = AuthenticatedHeader {
            bytes: SecretBytes::with_drop_observer(Arc::clone(&parse_observed)),
            len: 5,
        };
        header.bytes.as_mut_slice()[..5].copy_from_slice(b"bad\r\n");
        assert!(parse_authenticated_header(header.as_slice()).is_err());
        drop(header);
        assert_eq!(
            *parse_observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![0; AUTHENTICATED_MAX_HEADER_BYTES]
        );

        let unwind_observed = Arc::new(Mutex::new(Vec::new()));
        let observer = Arc::clone(&unwind_observed);
        let result = catch_unwind(AssertUnwindSafe(move || {
            let mut secret = SecretBytes::<64>::with_drop_observer(observer);
            secret.as_mut_slice().fill(0xff);
            panic!("injected secret-owner unwind");
        }));
        assert!(result.is_err());
        assert_eq!(
            *unwind_observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![0; 64]
        );
    }

    #[test]
    fn test_host_allowed() {
        let addr_80 = SocketAddr::new(IpAddr::from_str("127.0.0.1").unwrap(), 80);
        let addr_8000 = SocketAddr::new(IpAddr::from_str("127.0.0.1").unwrap(), 8000);
        let addr_v6_80 = SocketAddr::new(IpAddr::from_str("::1").unwrap(), 80);
        let addr_v6_8000 = SocketAddr::new(IpAddr::from_str("::1").unwrap(), 8000);

        // We match the host ip address to the server, so we can only use hosts that actually resolve
        let localhost_host = Host::Domain("localhost".to_string());
        let test_host = Host::Domain("example.test".to_string());
        let subdomain_localhost_host = Host::Domain("subdomain.localhost".to_string());

        assert!(is_host_allowed(
            &addr_80,
            std::slice::from_ref(&localhost_host),
            "localhost:80"
        ));
        assert!(is_host_allowed(
            &addr_80,
            std::slice::from_ref(&test_host),
            "example.test:80"
        ));
        assert!(is_host_allowed(
            &addr_80,
            &[test_host.clone(), localhost_host.clone()],
            "example.test"
        ));
        assert!(is_host_allowed(
            &addr_80,
            std::slice::from_ref(&subdomain_localhost_host),
            "subdomain.localhost"
        ));

        // ip address cases
        assert!(is_host_allowed(&addr_80, &[], "127.0.0.1:80"));
        assert!(is_host_allowed(&addr_v6_80, &[], "127.0.0.1"));
        assert!(is_host_allowed(&addr_80, &[], "[::1]"));
        assert!(is_host_allowed(&addr_8000, &[], "127.0.0.1:8000"));
        assert!(is_host_allowed(
            &addr_80,
            std::slice::from_ref(&subdomain_localhost_host),
            "[::1]"
        ));
        assert!(is_host_allowed(
            &addr_v6_8000,
            std::slice::from_ref(&subdomain_localhost_host),
            "[::1]:8000"
        ));

        // Mismatch cases

        assert!(!is_host_allowed(&addr_80, &[test_host], "localhost"));

        assert!(!is_host_allowed(&addr_80, &[], "localhost:80"));

        // Port mismatch cases

        assert!(!is_host_allowed(
            &addr_80,
            std::slice::from_ref(&localhost_host),
            "localhost:8000"
        ));
        assert!(!is_host_allowed(
            &addr_8000,
            std::slice::from_ref(&localhost_host),
            "localhost"
        ));
        assert!(!is_host_allowed(
            &addr_v6_8000,
            std::slice::from_ref(&localhost_host),
            "[::1]"
        ));
    }

    #[test]
    fn test_origin_allowed() {
        assert!(is_origin_allowed(
            &[Url::parse("http://localhost").unwrap()],
            &Url::parse("http://localhost").unwrap()
        ));
        assert!(is_origin_allowed(
            &[Url::parse("http://localhost").unwrap()],
            &Url::parse("http://localhost:80").unwrap()
        ));
        assert!(is_origin_allowed(
            &[
                Url::parse("https://test.example").unwrap(),
                Url::parse("http://localhost").unwrap()
            ],
            &Url::parse("http://localhost").unwrap()
        ));
        assert!(is_origin_allowed(
            &[
                Url::parse("https://test.example").unwrap(),
                Url::parse("http://localhost").unwrap()
            ],
            &Url::parse("https://test.example:443").unwrap()
        ));
        // Mismatch cases
        assert!(!is_origin_allowed(
            &[],
            &Url::parse("http://localhost").unwrap()
        ));
        assert!(!is_origin_allowed(
            &[Url::parse("http://localhost").unwrap()],
            &Url::parse("http://localhost:8000").unwrap()
        ));
        assert!(!is_origin_allowed(
            &[Url::parse("https://localhost").unwrap()],
            &Url::parse("http://localhost").unwrap()
        ));
        assert!(!is_origin_allowed(
            &[Url::parse("https://example.test").unwrap()],
            &Url::parse("http://subdomain.example.test").unwrap()
        ));
    }
}
