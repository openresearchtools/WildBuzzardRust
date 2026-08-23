//! Authenticated `WebDriver` Classic ingress for the real browser owner thread.

use std::fs::File;
use std::io::{self, Read};
use std::num::NonZeroU64;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{
    Receiver, Sender, SyncSender, TryRecvError, TrySendError, channel, sync_channel,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use webdriver::capabilities::{BrowserCapabilities, Capabilities, CapabilitiesMatching};
use webdriver::command::{NewSessionParameters, WebDriverCommand, WebDriverMessage};
use webdriver::error::{ErrorStatus, WebDriverError, WebDriverResult};
use webdriver::httpapi::VoidWebDriverExtensionRoute;
use webdriver::response::{NewSessionResponse, ValueResponse, WebDriverResponse};
use webdriver::server::{
    DispatchLifetime, Listener, ServerSecurityPolicy, Session, SessionTeardownKind,
    WebDriverHandler,
};
use wild_buzzard_engine::{MAX_NAVIGATION_URL_BYTES, NavigationId};
use wild_buzzard_linux::{
    BrowserFrameCapture, BrowserFrameRequest, BrowserPageSnapshot, LinuxAccelerationClass,
    LinuxResetProtection, LinuxWakeHandle, LinuxWakeStatus,
};
use wild_buzzard_platform::{PixelFormat, SurfaceRole};
use wild_buzzard_ui::{
    BrowserCommand, BrowserCommandOutcome, BrowserSession, BrowserTabId, BrowserWindowId,
    EnginePort, NavigationPhase, SessionError, SessionLifecycle,
};

use super::{
    NativePresentationCommitMarker, NativePresentationCommitOutcome, PresentationCommitIdentity,
};

pub const AUTOMATION_PROTOCOL_ID: u16 = 9;
pub const AUTOMATION_COMMAND_KIND: u16 = 1;
pub const AUTOMATION_RESULT_KIND: u16 = 2;

const MAX_OWNER_COMMANDS_PER_WAKE: usize = 8;
const MAX_OWNER_QUEUE_DEPTH: usize = 16;
const MAX_SESSION_ID_BYTES: usize = 64;
const SESSION_ID_BYTES: usize = 16;
const MIN_COMMAND_DEADLINE: Duration = Duration::from_millis(10);
const OWNER_RESPONSE_POLL: Duration = Duration::from_millis(20);
#[cfg_attr(not(test), allow(dead_code))]
const MAX_SCREENSHOT_PIXELS: u64 = 16 * 1024 * 1024;
#[cfg_attr(not(test), allow(dead_code))]
const MAX_SCREENSHOT_PNG_BYTES: usize = 68 * 1024 * 1024;
#[cfg_attr(not(test), allow(dead_code))]
const MAX_SCREENSHOT_BASE64_BYTES: usize = 92 * 1024 * 1024;

/// Opt-in embedded `WebDriver` configuration. Merely compiling the feature does
/// not open a listener; a caller must pass this value to the explicit runner.
pub struct BrowserWebDriverConfig {
    policy: ServerSecurityPolicy,
    command_deadline: Duration,
}

impl BrowserWebDriverConfig {
    /// Binds owner-thread command cancellation to the server's bounded request
    /// deadline.
    ///
    /// # Errors
    ///
    /// Rejects deadlines too short for deterministic owner-thread admission.
    pub fn new(policy: ServerSecurityPolicy) -> io::Result<Self> {
        if policy.bind_address().port() == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "browser WebDriver requires an explicit nonzero loopback port",
            ));
        }
        let command_deadline = policy.request_deadline();
        if command_deadline < MIN_COMMAND_DEADLINE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "WebDriver command deadline is shorter than 10 milliseconds",
            ));
        }
        Ok(Self {
            policy,
            command_deadline,
        })
    }
}

impl std::fmt::Debug for BrowserWebDriverConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrowserWebDriverConfig")
            .field("policy", &self.policy)
            .field("command_deadline", &self.command_deadline)
            .finish()
    }
}

pub(crate) fn start(
    config: BrowserWebDriverConfig,
    wake: LinuxWakeHandle,
) -> io::Result<(Listener, AutomationOwner)> {
    let (command_send, command_recv) = sync_channel(MAX_OWNER_QUEUE_DEPTH);
    let ingress = AutomationIngress {
        command_send,
        wake: Arc::new(wake),
        command_deadline: config.command_deadline,
        next_request_id: Some(1),
        next_generation: Some(1),
        active_generation: None,
        active_session_id: None,
        active_authority: None,
        closed: false,
    };
    let owner = AutomationOwner::new(command_recv);
    let listener = webdriver::server::start_authenticated::<_, VoidWebDriverExtensionRoute>(
        config.policy,
        ingress,
        Vec::new(),
    )?;
    Ok((listener, owner))
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct AutomationRequestId(NonZeroU64);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct AutomationSessionGeneration(NonZeroU64);

#[derive(Debug)]
enum UnsupportedCommand {
    Title,
    Other,
}

#[derive(Debug)]
enum AutomationCommand {
    Status,
    NewSession {
        parameters: NewSessionParameters,
        page_load_timeout_ms: u64,
    },
    DeleteSession,
    Navigate(Box<str>),
    GetCurrentUrl,
    TakeScreenshot,
    Unsupported(UnsupportedCommand),
    Revoke,
}

#[derive(Debug)]
struct AutomationCommandMessage {
    protocol: u16,
    kind: u16,
    request: AutomationRequestId,
    generation: AutomationSessionGeneration,
    session_id: Option<Box<str>>,
    command: AutomationCommand,
}

struct QueuedCommand {
    message: AutomationCommandMessage,
    lifetime: DispatchLifetime,
    response: Sender<AutomationResultMessage>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AutomationFailureKind {
    InvalidArgument,
    InvalidSession,
    SessionNotCreated,
    NoSuchWindow,
    Timeout,
    UnableToCaptureScreen,
    UnsupportedOperation,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AutomationFailure {
    kind: AutomationFailureKind,
    detail: &'static str,
}

impl AutomationFailure {
    const fn new(kind: AutomationFailureKind, detail: &'static str) -> Self {
        Self { kind, detail }
    }

    const fn invalid_session(detail: &'static str) -> Self {
        Self::new(AutomationFailureKind::InvalidSession, detail)
    }

    const fn unknown(detail: &'static str) -> Self {
        Self::new(AutomationFailureKind::Unknown, detail)
    }
}

#[derive(Debug)]
enum AutomationValue {
    Status {
        ready: bool,
        message: Box<str>,
    },
    NewSession {
        session_id: Box<str>,
        capabilities: Value,
    },
    DeleteSession,
    NavigationComplete,
    CurrentUrl(Box<str>),
    #[cfg_attr(not(test), allow(dead_code))]
    Screenshot(Box<str>),
}

#[derive(Debug)]
struct AutomationResultMessage {
    protocol: u16,
    kind: u16,
    request: AutomationRequestId,
    generation: AutomationSessionGeneration,
    result: Result<AutomationValue, AutomationFailure>,
}

trait AutomationWake: Send + Sync {
    fn wake(&self) -> LinuxWakeStatus;
}

impl AutomationWake for LinuxWakeHandle {
    fn wake(&self) -> LinuxWakeStatus {
        LinuxWakeHandle::wake(self)
    }
}

struct AutomationIngress {
    command_send: SyncSender<QueuedCommand>,
    wake: Arc<dyn AutomationWake>,
    command_deadline: Duration,
    next_request_id: Option<u64>,
    next_generation: Option<u64>,
    active_generation: Option<AutomationSessionGeneration>,
    active_session_id: Option<Box<str>>,
    active_authority: Option<DispatchLifetime>,
    closed: bool,
}

impl AutomationIngress {
    fn allocate_request(&mut self) -> WebDriverResult<AutomationRequestId> {
        let value = self.next_request_id.ok_or_else(|| {
            WebDriverError::new(
                ErrorStatus::UnknownError,
                "WebDriver request identity exhausted",
            )
        })?;
        let request = AutomationRequestId(
            NonZeroU64::new(value).expect("WebDriver request identity starts nonzero"),
        );
        self.next_request_id = value.checked_add(1);
        Ok(request)
    }

    fn allocate_generation(&mut self) -> WebDriverResult<AutomationSessionGeneration> {
        let value = self.next_generation.ok_or_else(|| {
            WebDriverError::new(
                ErrorStatus::SessionNotCreated,
                "WebDriver session generation exhausted",
            )
        })?;
        let generation = AutomationSessionGeneration(
            NonZeroU64::new(value).expect("WebDriver session generation starts nonzero"),
        );
        self.next_generation = value.checked_add(1);
        Ok(generation)
    }

    fn validate_dispatcher_session(&self, session: Option<&Session>) -> WebDriverResult<()> {
        let dispatcher = session.map(|session| session.id.as_str());
        if dispatcher == self.active_session_id.as_deref() {
            Ok(())
        } else {
            Err(WebDriverError::new(
                ErrorStatus::InvalidSessionId,
                "WebDriver dispatcher and browser session authority disagree",
            ))
        }
    }

    fn take_and_revoke_active_authority(
        &mut self,
    ) -> Option<(AutomationSessionGeneration, Box<str>)> {
        let generation = self.active_generation.take();
        let session_id = self.active_session_id.take();
        let authority = self.active_authority.take();
        if let Some(authority) = &authority {
            authority.revoke();
        }
        match (generation, session_id, authority) {
            (Some(generation), Some(session_id), Some(_)) => Some((generation, session_id)),
            _ => None,
        }
    }

    fn send_revoke(&mut self) {
        let Some((generation, session_id)) = self.take_and_revoke_active_authority() else {
            self.closed = true;
            let _ = self.wake.wake();
            return;
        };
        let Ok(request) = self.allocate_request() else {
            self.closed = true;
            let _ = self.wake.wake();
            return;
        };
        let (response, response_recv) = channel();
        drop(response_recv);
        let queued = QueuedCommand {
            message: AutomationCommandMessage {
                protocol: AUTOMATION_PROTOCOL_ID,
                kind: AUTOMATION_COMMAND_KIND,
                request,
                generation,
                session_id: Some(session_id),
                command: AutomationCommand::Revoke,
            },
            lifetime: DispatchLifetime::new(
                Instant::now()
                    .checked_add(self.command_deadline)
                    .unwrap_or_else(Instant::now),
            ),
            response,
        };
        if matches!(
            self.command_send.try_send(queued),
            Err(TrySendError::Disconnected(_))
        ) {
            self.closed = true;
        }
        if matches!(self.wake.wake(), LinuxWakeStatus::Closed) {
            self.closed = true;
        }
    }

    fn webdriver_response(value: AutomationValue) -> WebDriverResponse {
        match value {
            AutomationValue::Status { ready, message } => {
                WebDriverResponse::Generic(ValueResponse(json!({
                    "ready": ready,
                    "message": message,
                })))
            }
            AutomationValue::NewSession {
                session_id,
                capabilities,
            } => WebDriverResponse::NewSession(NewSessionResponse::new(
                session_id.into(),
                capabilities,
            )),
            AutomationValue::DeleteSession => WebDriverResponse::DeleteSession,
            AutomationValue::NavigationComplete => WebDriverResponse::Void,
            AutomationValue::CurrentUrl(url) => {
                WebDriverResponse::Generic(ValueResponse(Value::String(url.into())))
            }
            AutomationValue::Screenshot(png) => {
                WebDriverResponse::Generic(ValueResponse(Value::String(png.into())))
            }
        }
    }

    fn webdriver_error(failure: AutomationFailure) -> WebDriverError {
        let status = match failure.kind {
            AutomationFailureKind::InvalidArgument => ErrorStatus::InvalidArgument,
            AutomationFailureKind::InvalidSession => ErrorStatus::InvalidSessionId,
            AutomationFailureKind::SessionNotCreated => ErrorStatus::SessionNotCreated,
            AutomationFailureKind::NoSuchWindow => ErrorStatus::NoSuchWindow,
            AutomationFailureKind::Timeout => ErrorStatus::Timeout,
            AutomationFailureKind::UnableToCaptureScreen => ErrorStatus::UnableToCaptureScreen,
            AutomationFailureKind::UnsupportedOperation => ErrorStatus::UnsupportedOperation,
            AutomationFailureKind::Unknown => ErrorStatus::UnknownError,
        };
        WebDriverError::new(status, failure.detail)
    }
}

impl AutomationIngress {
    #[allow(clippy::too_many_lines)]
    fn handle_with_lifetime(
        &mut self,
        session: Option<&Session>,
        message: WebDriverMessage<VoidWebDriverExtensionRoute>,
        lifetime: &DispatchLifetime,
    ) -> WebDriverResult<WebDriverResponse> {
        if self.closed {
            return Err(WebDriverError::new(
                ErrorStatus::UnknownError,
                "WebDriver automation ingress is closed",
            ));
        }
        if lifetime.cancel_if_expired() {
            return Err(WebDriverError::new(
                ErrorStatus::Timeout,
                "browser automation command inherited an expired HTTP deadline",
            ));
        }
        self.validate_dispatcher_session(session)?;
        let request = self.allocate_request()?;
        let is_new_session = matches!(&message.command, WebDriverCommand::NewSession(_));
        let is_delete_session = matches!(&message.command, WebDriverCommand::DeleteSession);
        let generation = if is_new_session {
            self.allocate_generation()?
        } else {
            match self.active_generation {
                Some(generation) => generation,
                None => self.allocate_generation()?,
            }
        };
        let session_id = message.session_id.map(String::into_boxed_str);
        if session_id
            .as_ref()
            .is_some_and(|value| value.len() > MAX_SESSION_ID_BYTES)
        {
            return Err(WebDriverError::new(
                ErrorStatus::InvalidSessionId,
                "WebDriver session ID exceeds its hard bound",
            ));
        }
        if self.active_session_id.is_some()
            && session_id.as_deref() != self.active_session_id.as_deref()
            && !matches!(&message.command, WebDriverCommand::Status)
        {
            return Err(WebDriverError::new(
                ErrorStatus::InvalidSessionId,
                "WebDriver session ID does not own browser automation",
            ));
        }
        let command = match message.command {
            WebDriverCommand::Status => AutomationCommand::Status,
            WebDriverCommand::NewSession(parameters) => AutomationCommand::NewSession {
                parameters,
                page_load_timeout_ms: u64::try_from(self.command_deadline.as_millis())
                    .expect("WebDriver's 120-second hard deadline fits u64 milliseconds"),
            },
            WebDriverCommand::DeleteSession => AutomationCommand::DeleteSession,
            WebDriverCommand::Get(parameters) => {
                if parameters.url.len() > MAX_NAVIGATION_URL_BYTES {
                    return Err(WebDriverError::new(
                        ErrorStatus::InvalidArgument,
                        "navigation URL exceeds the browser hard bound",
                    ));
                }
                AutomationCommand::Navigate(parameters.url.into_boxed_str())
            }
            WebDriverCommand::GetCurrentUrl => AutomationCommand::GetCurrentUrl,
            WebDriverCommand::GetTitle => AutomationCommand::Unsupported(UnsupportedCommand::Title),
            WebDriverCommand::TakeScreenshot => AutomationCommand::TakeScreenshot,
            _ => AutomationCommand::Unsupported(UnsupportedCommand::Other),
        };
        let (response, response_recv) = channel();
        let queued = QueuedCommand {
            message: AutomationCommandMessage {
                protocol: AUTOMATION_PROTOCOL_ID,
                kind: AUTOMATION_COMMAND_KIND,
                request,
                generation,
                session_id,
                command,
            },
            lifetime: lifetime.clone(),
            response,
        };
        match self.command_send.try_send(queued) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                return Err(WebDriverError::new(
                    ErrorStatus::UnknownError,
                    "browser automation owner queue is full",
                ));
            }
            Err(TrySendError::Disconnected(_)) => {
                let _ = self.take_and_revoke_active_authority();
                self.closed = true;
                return Err(WebDriverError::new(
                    ErrorStatus::UnknownError,
                    "browser automation owner is unavailable",
                ));
            }
        }
        if matches!(self.wake.wake(), LinuxWakeStatus::Closed) {
            let _ = lifetime.cancel();
            self.closed = true;
            return Err(WebDriverError::new(
                ErrorStatus::UnknownError,
                "browser event loop is closed",
            ));
        }
        let result = loop {
            if lifetime.cancel_if_expired() {
                let _ = self.wake.wake();
                return Err(WebDriverError::new(
                    ErrorStatus::Timeout,
                    "browser automation command deadline exceeded",
                ));
            }
            let Some(remaining) = lifetime.remaining() else {
                let _ = self.wake.wake();
                return Err(WebDriverError::new(
                    ErrorStatus::Timeout,
                    "browser automation command was cancelled",
                ));
            };
            match response_recv.recv_timeout(remaining.min(OWNER_RESPONSE_POLL)) {
                Ok(result) => break result,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = lifetime.cancel();
                    let _ = self.wake.wake();
                    return Err(WebDriverError::new(
                        ErrorStatus::UnknownError,
                        "browser automation owner response channel closed",
                    ));
                }
            }
        };
        if result.protocol != AUTOMATION_PROTOCOL_ID
            || result.kind != AUTOMATION_RESULT_KIND
            || result.request != request
            || result.generation != generation
        {
            let _ = lifetime.cancel();
            let _ = self.wake.wake();
            return Err(WebDriverError::new(
                ErrorStatus::UnknownError,
                "browser automation result correlation failed",
            ));
        }
        match result.result {
            Ok(value) => {
                if let AutomationValue::NewSession { session_id, .. } = &value {
                    if lifetime
                        .run_if_active(|| {
                            self.active_generation = Some(generation);
                            self.active_session_id = Some(session_id.clone());
                            self.active_authority = Some(lifetime.clone());
                        })
                        .is_none()
                    {
                        let _ = self.wake.wake();
                        return Err(WebDriverError::new(
                            ErrorStatus::Timeout,
                            "automation session expired before ingress authority publication",
                        ));
                    }
                } else if is_delete_session {
                    if let Some(authority) = self.active_authority.take() {
                        authority.revoke();
                    }
                    self.active_generation = None;
                    self.active_session_id = None;
                }
                Ok(Self::webdriver_response(value))
            }
            Err(failure) => Err(Self::webdriver_error(failure)),
        }
    }
}

impl WebDriverHandler<VoidWebDriverExtensionRoute> for AutomationIngress {
    fn handle_command(
        &mut self,
        session: &Option<Session>,
        message: WebDriverMessage<VoidWebDriverExtensionRoute>,
    ) -> WebDriverResult<WebDriverResponse> {
        let deadline = Instant::now()
            .checked_add(self.command_deadline)
            .unwrap_or_else(Instant::now);
        self.handle_with_lifetime(session.as_ref(), message, &DispatchLifetime::new(deadline))
    }

    fn handle_command_with_lifetime(
        &mut self,
        session: &Option<Session>,
        message: WebDriverMessage<VoidWebDriverExtensionRoute>,
        lifetime: &DispatchLifetime,
    ) -> WebDriverResult<WebDriverResponse> {
        self.handle_with_lifetime(session.as_ref(), message, lifetime)
    }

    fn teardown_session(&mut self, kind: SessionTeardownKind) {
        if kind == SessionTeardownKind::NotDeleted {
            self.send_revoke();
        }
    }
}

struct ActiveSession {
    generation: AutomationSessionGeneration,
    session_id: Box<str>,
    authority: DispatchLifetime,
}

struct PendingNavigation {
    request: AutomationRequestId,
    generation: AutomationSessionGeneration,
    tab: BrowserTabId,
    navigation: NavigationId,
    lifetime: DispatchLifetime,
    response: Sender<AutomationResultMessage>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScreenshotPageIdentity {
    Blank,
    Scene {
        navigation: u64,
        revision: u64,
        document: u64,
        document_revision: u64,
        pipeline_source: u32,
        pipeline: u32,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ScreenshotFrameIdentity {
    surface_namespace: u64,
    surface_slot: u32,
    surface_generation: u32,
    surface_revision: u64,
    width: u32,
    height: u32,
    scale_bits: u64,
    format: PixelFormat,
    role: SurfaceRole,
    acceleration: LinuxAccelerationClass,
    reset_protection: LinuxResetProtection,
    page: ScreenshotPageIdentity,
    chrome_revision: u64,
    epoch: u32,
    sequence: u64,
}

impl From<BrowserFrameRequest> for ScreenshotFrameIdentity {
    fn from(request: BrowserFrameRequest) -> Self {
        let surface = request.surface();
        let descriptor = surface.descriptor();
        let page = match request.page() {
            BrowserPageSnapshot::Blank => ScreenshotPageIdentity::Blank,
            BrowserPageSnapshot::Scene(page) => ScreenshotPageIdentity::Scene {
                navigation: page.navigation().get(),
                revision: page.revision().get(),
                document: page.document_version().document_id().get(),
                document_revision: page.document_version().revision(),
                pipeline_source: page.pipeline().source(),
                pipeline: page.pipeline().pipeline(),
            },
        };
        Self {
            surface_namespace: descriptor.id.namespace().get(),
            surface_slot: descriptor.id.slot(),
            surface_generation: descriptor.id.generation(),
            surface_revision: surface.revision().get(),
            width: descriptor.size.width,
            height: descriptor.size.height,
            scale_bits: descriptor.scale.get().to_bits(),
            format: descriptor.format,
            role: descriptor.role,
            acceleration: surface.capabilities().acceleration(),
            reset_protection: surface.capabilities().reset_protection(),
            page,
            chrome_revision: request.chrome_revision().get(),
            epoch: request.epoch(),
            sequence: request.sequence(),
        }
    }
}

struct PendingScreenshot {
    request: AutomationRequestId,
    generation: AutomationSessionGeneration,
    lifetime: DispatchLifetime,
    response: Sender<AutomationResultMessage>,
    #[cfg_attr(not(test), allow(dead_code))]
    claimed_frame: Option<ScreenshotFrameIdentity>,
}

/// Opaque one-shot authority for the owner loop's next exact captured frame.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct AutomationScreenshotCaptureRequest {
    request: AutomationRequestId,
    generation: AutomationSessionGeneration,
    frame: ScreenshotFrameIdentity,
}

#[cfg_attr(not(test), allow(dead_code))]
trait ScreenshotCaptureSource {
    fn frame_identity(&self) -> ScreenshotFrameIdentity;
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    fn row(&self, row: u32) -> Option<&[u8]>;
}

impl ScreenshotCaptureSource for BrowserFrameCapture {
    fn frame_identity(&self) -> ScreenshotFrameIdentity {
        self.receipt().request().into()
    }

    fn width(&self) -> u32 {
        self.content_rect().width()
    }

    fn height(&self) -> u32 {
        self.content_rect().height()
    }

    fn row(&self, row: u32) -> Option<&[u8]> {
        let content = self.content_rect();
        if row >= content.height() {
            return None;
        }
        let y = usize::try_from(content.y().checked_add(row)?).ok()?;
        let x = usize::try_from(content.x()).ok()?;
        let row_bytes = usize::try_from(content.width()).ok()?.checked_mul(4)?;
        let start = y
            .checked_mul(self.stride())?
            .checked_add(x.checked_mul(4)?)?;
        self.pixels().get(start..start.checked_add(row_bytes)?)
    }
}

pub(crate) enum AutomationPresentationAdmission {
    Unrestricted,
    Authorized(AutomationPresentationPermit),
    Rejected,
}

pub(crate) struct AutomationPresentationPermit {
    identity: PresentationCommitIdentity,
    request_authority: DispatchLifetime,
    session_authority: DispatchLifetime,
    outcome: Mutex<NativePresentationCommitOutcome>,
}

impl AutomationPresentationPermit {
    pub(crate) fn commit<R>(
        &self,
        identity: PresentationCommitIdentity,
        operation: impl FnOnce(&mut NativePresentationCommitMarker<'_>) -> R,
    ) -> Option<R> {
        let mut outcome = self
            .outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if identity != self.identity {
            if *outcome == NativePresentationCommitOutcome::Pending {
                *outcome = NativePresentationCommitOutcome::Cancelled;
            }
            return None;
        }
        if *outcome != NativePresentationCommitOutcome::Pending {
            return None;
        }
        let result =
            self.request_authority
                .run_if_active_with_authority(&self.session_authority, || {
                    operation(&mut NativePresentationCommitMarker {
                        outcome: &mut outcome,
                    })
                });
        if result.is_none() {
            if *outcome == NativePresentationCommitOutcome::Pending {
                *outcome = NativePresentationCommitOutcome::Cancelled;
            }
        } else if matches!(
            *outcome,
            NativePresentationCommitOutcome::Pending
                | NativePresentationCommitOutcome::SubmissionInProgress
        ) {
            *outcome = NativePresentationCommitOutcome::NotCommitted;
        }
        result
    }

    #[cfg(test)]
    fn outcome(&self) -> NativePresentationCommitOutcome {
        *self
            .outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

pub(crate) struct AutomationDrainOutcome {
    pub navigation_started: bool,
    #[cfg_attr(not(test), allow(dead_code))]
    pub screenshot_requested: bool,
    pub more_may_remain: bool,
}

pub(crate) struct AutomationOwner {
    command_recv: Receiver<QueuedCommand>,
    active: Option<ActiveSession>,
    pending_navigation: Option<PendingNavigation>,
    pending_screenshot: Option<PendingScreenshot>,
    highest_request: Option<AutomationRequestId>,
    highest_generation: Option<AutomationSessionGeneration>,
    stopped: bool,
}

impl AutomationOwner {
    fn new(command_recv: Receiver<QueuedCommand>) -> Self {
        Self {
            command_recv,
            active: None,
            pending_navigation: None,
            pending_screenshot: None,
            highest_request: None,
            highest_generation: None,
            stopped: false,
        }
    }

    pub(crate) fn drain<E: EnginePort>(
        &mut self,
        session: &mut BrowserSession<E>,
        window: BrowserWindowId,
    ) -> AutomationDrainOutcome {
        self.revoke_cancelled_authority(session);
        let screenshot_was_pending = self.pending_screenshot.is_some();
        let mut processed = 0;
        let mut navigation_started = false;
        while processed < MAX_OWNER_COMMANDS_PER_WAKE {
            let queued = match self.command_recv.try_recv() {
                Ok(queued) => queued,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            };
            processed += 1;
            navigation_started |= self.handle_queued(session, window, queued);
        }
        self.observe_session(session);
        AutomationDrainOutcome {
            navigation_started,
            screenshot_requested: !screenshot_was_pending && self.pending_screenshot.is_some(),
            more_may_remain: processed == MAX_OWNER_COMMANDS_PER_WAKE,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle_queued<E: EnginePort>(
        &mut self,
        session: &mut BrowserSession<E>,
        window: BrowserWindowId,
        queued: QueuedCommand,
    ) -> bool {
        let request = queued.message.request;
        let generation = queued.message.generation;
        let response = |result| AutomationResultMessage {
            protocol: AUTOMATION_PROTOCOL_ID,
            kind: AUTOMATION_RESULT_KIND,
            request,
            generation,
            result,
        };
        if self.stopped {
            let _ = queued
                .response
                .send(response(Err(AutomationFailure::unknown(
                    "browser automation owner has stopped",
                ))));
            return false;
        }
        if queued.message.protocol != AUTOMATION_PROTOCOL_ID
            || queued.message.kind != AUTOMATION_COMMAND_KIND
            || self
                .highest_request
                .is_some_and(|highest| request <= highest)
        {
            let _ = queued
                .response
                .send(response(Err(AutomationFailure::unknown(
                    "stale or invalid automation command identity",
                ))));
            return false;
        }
        if queued
            .lifetime
            .run_if_active(|| self.highest_request = Some(request))
            .is_none()
        {
            let _ = queued.response.send(response(Err(AutomationFailure::new(
                AutomationFailureKind::Timeout,
                "automation command expired before owner-thread admission",
            ))));
            return false;
        }

        let command = queued.message.command;
        let result = match command {
            AutomationCommand::Status => queued
                .lifetime
                .run_if_active(|| {
                    self.validate_status_generation(generation)?;
                    let ready = matches!(session.lifecycle(), SessionLifecycle::Running)
                        && self.active.is_none();
                    let message = if ready {
                        "Wild Buzzard is ready for one WebDriver Classic session"
                    } else {
                        "Wild Buzzard already has an automation session"
                    };
                    Ok(AutomationValue::Status {
                        ready,
                        message: message.into(),
                    })
                })
                .unwrap_or_else(|| {
                    Err(AutomationFailure::new(
                        AutomationFailureKind::Timeout,
                        "automation status expired before owner observation",
                    ))
                }),
            AutomationCommand::NewSession {
                parameters,
                page_load_timeout_ms,
            } => self.create_session(
                generation,
                &parameters,
                page_load_timeout_ms,
                &queued.lifetime,
            ),
            AutomationCommand::DeleteSession => {
                match self
                    .validated_active_authority(generation, queued.message.session_id.as_deref())
                {
                    Ok(authority) => {
                        match queued
                            .lifetime
                            .run_if_active_with_authority(&authority, || {
                                let pending = self.take_pending_navigation_after_stop(session);
                                let screenshot = self.pending_screenshot.take();
                                (pending, screenshot, self.active.take())
                            }) {
                            Some((pending, screenshot, active)) => {
                                if let Some(pending) = pending {
                                    self.send_pending_result(
                                        &pending,
                                        Err(AutomationFailure::invalid_session(
                                            "automation session was explicitly deleted",
                                        )),
                                    );
                                }
                                if let Some(screenshot) = screenshot {
                                    self.send_screenshot_result(
                                        &screenshot,
                                        Err(AutomationFailure::invalid_session(
                                            "automation session was explicitly deleted",
                                        )),
                                    );
                                }
                                if let Some(active) = active {
                                    active.authority.revoke();
                                }
                                Ok(AutomationValue::DeleteSession)
                            }
                            None => Err(AutomationFailure::new(
                                AutomationFailureKind::Timeout,
                                "automation delete expired before owner mutation",
                            )),
                        }
                    }
                    Err(failure) => Err(failure),
                }
            }
            AutomationCommand::Navigate(url) => {
                match self
                    .validated_active_authority(generation, queued.message.session_id.as_deref())
                {
                    Err(failure) => Err(failure),
                    Ok(_) if self.pending_navigation.is_some() => Err(AutomationFailure::unknown(
                        "another automation navigation is pending",
                    )),
                    Ok(authority) => {
                        match queued
                            .lifetime
                            .run_if_active_with_authority(&authority, || {
                                Self::start_navigation(session, window, &url).map(
                                    |(tab, navigation)| {
                                        self.pending_navigation = Some(PendingNavigation {
                                            request,
                                            generation,
                                            tab,
                                            navigation,
                                            lifetime: queued.lifetime.clone(),
                                            response: queued.response.clone(),
                                        });
                                    },
                                )
                            }) {
                            Some(Ok(())) => return true,
                            Some(Err(failure)) => Err(failure),
                            None => Err(AutomationFailure::new(
                                AutomationFailureKind::Timeout,
                                "automation navigation expired before browser dispatch",
                            )),
                        }
                    }
                }
            }
            AutomationCommand::GetCurrentUrl => self
                .validated_active_authority(generation, queued.message.session_id.as_deref())
                .and_then(|authority| {
                    queued
                        .lifetime
                        .run_if_active_with_authority(&authority, || {
                            Self::current_url(session, window).map(AutomationValue::CurrentUrl)
                        })
                        .unwrap_or_else(|| {
                            Err(AutomationFailure::new(
                                AutomationFailureKind::Timeout,
                                "current URL authority expired before owner observation",
                            ))
                        })
                }),
            AutomationCommand::TakeScreenshot => {
                match self
                    .validated_active_authority(generation, queued.message.session_id.as_deref())
                {
                    Err(failure) => Err(failure),
                    Ok(_) if self.pending_screenshot.is_some() => Err(AutomationFailure::new(
                        AutomationFailureKind::UnableToCaptureScreen,
                        "another compositor screenshot is pending",
                    )),
                    Ok(authority) => {
                        match queued
                            .lifetime
                            .run_if_active_with_authority(&authority, || {
                                session.window_snapshot(window).map_err(map_session_error)?;
                                self.pending_screenshot = Some(PendingScreenshot {
                                    request,
                                    generation,
                                    lifetime: queued.lifetime.clone(),
                                    response: queued.response.clone(),
                                    claimed_frame: None,
                                });
                                Ok(())
                            }) {
                            Some(Ok(())) => return false,
                            Some(Err(failure)) => Err(failure),
                            None => Err(AutomationFailure::new(
                                AutomationFailureKind::Timeout,
                                "screenshot expired before owner admission",
                            )),
                        }
                    }
                }
            }
            AutomationCommand::Unsupported(kind) => {
                let _kind_name = match kind {
                    UnsupportedCommand::Title => "document title",
                    UnsupportedCommand::Other => "requested command",
                };
                self.validated_active_authority(generation, queued.message.session_id.as_deref())
                    .and_then(|authority| {
                        queued
                            .lifetime
                            .run_if_active_with_authority(&authority, || {
                                Err(AutomationFailure::new(
                                    AutomationFailureKind::UnsupportedOperation,
                                    "WebDriver command is not implemented by this bounded slice",
                                ))
                            })
                            .unwrap_or_else(|| {
                                Err(AutomationFailure::new(
                                    AutomationFailureKind::Timeout,
                                    "unsupported command expired before owner rejection",
                                ))
                            })
                    })
            }
            AutomationCommand::Revoke => {
                match self.exact_active_authority(generation, queued.message.session_id.as_deref())
                {
                    Ok(authority) => match queued.lifetime.run_if_active_with_revoked_authority(
                        &authority,
                        || {
                            let pending = self.take_pending_navigation_after_stop(session);
                            let screenshot = self.pending_screenshot.take();
                            (pending, screenshot, self.active.take())
                        },
                    ) {
                        Some((pending, screenshot, active)) => {
                            if let Some(pending) = pending {
                                self.send_pending_result(
                                    &pending,
                                    Err(AutomationFailure::invalid_session(
                                        "automation ingress revoked its session authority",
                                    )),
                                );
                            }
                            if let Some(screenshot) = screenshot {
                                self.send_screenshot_result(
                                    &screenshot,
                                    Err(AutomationFailure::invalid_session(
                                        "automation ingress revoked its session authority",
                                    )),
                                );
                            }
                            if let Some(active) = active {
                                active.authority.revoke();
                            }
                            Ok(AutomationValue::DeleteSession)
                        }
                        None => Err(AutomationFailure::invalid_session(
                            "automation revocation lost its exact authority arbitration",
                        )),
                    },
                    Err(failure) => Err(failure),
                }
            }
        };
        if let Ok(AutomationValue::NewSession {
            session_id,
            capabilities,
        }) = result
        {
            let sent = queued
                .response
                .send(response(Ok(AutomationValue::NewSession {
                    session_id,
                    capabilities,
                })));
            if sent.is_err() {
                queued.lifetime.revoke();
                if let Some(active) = &self.active {
                    active.authority.revoke();
                }
                self.active = None;
            }
        } else {
            let sent = queued.response.send(response(result));
            if sent.is_err() {
                queued.lifetime.revoke();
                if self
                    .active
                    .as_ref()
                    .is_some_and(|active| active.generation == generation)
                {
                    if let Some(active) = &self.active {
                        active.authority.revoke();
                    }
                    self.cancel_pending_navigation(
                        session,
                        AutomationFailure::invalid_session(
                            "automation result receiver disconnected",
                        ),
                    );
                    self.cancel_pending_screenshot(AutomationFailure::invalid_session(
                        "automation result receiver disconnected",
                    ));
                    self.active = None;
                }
            }
        }
        false
    }

    fn validate_status_generation(
        &mut self,
        generation: AutomationSessionGeneration,
    ) -> Result<(), AutomationFailure> {
        if let Some(active) = &self.active {
            if active.generation == generation {
                return Ok(());
            }
            return Err(AutomationFailure::invalid_session(
                "status named a foreign automation generation",
            ));
        }
        if self
            .highest_generation
            .is_some_and(|highest| generation <= highest)
        {
            return Err(AutomationFailure::invalid_session(
                "status reused a retired automation generation",
            ));
        }
        self.highest_generation = Some(generation);
        Ok(())
    }

    fn validated_active_authority(
        &self,
        generation: AutomationSessionGeneration,
        session_id: Option<&str>,
    ) -> Result<DispatchLifetime, AutomationFailure> {
        let active = self
            .active
            .as_ref()
            .ok_or_else(|| AutomationFailure::invalid_session("no automation session is active"))?;
        if active.generation != generation || Some(active.session_id.as_ref()) != session_id {
            return Err(AutomationFailure::invalid_session(
                "automation request named a stale session authority",
            ));
        }
        if active.authority.is_cancelled() {
            return Err(AutomationFailure::invalid_session(
                "automation requester cancelled its session authority",
            ));
        }
        Ok(active.authority.clone())
    }

    fn exact_active_authority(
        &self,
        generation: AutomationSessionGeneration,
        session_id: Option<&str>,
    ) -> Result<DispatchLifetime, AutomationFailure> {
        let active = self
            .active
            .as_ref()
            .ok_or_else(|| AutomationFailure::invalid_session("no automation session is active"))?;
        if active.generation != generation || Some(active.session_id.as_ref()) != session_id {
            return Err(AutomationFailure::invalid_session(
                "automation request named a stale session authority",
            ));
        }
        Ok(active.authority.clone())
    }

    fn create_session(
        &mut self,
        generation: AutomationSessionGeneration,
        parameters: &NewSessionParameters,
        page_load_timeout_ms: u64,
        authority: &DispatchLifetime,
    ) -> Result<AutomationValue, AutomationFailure> {
        if self.active.is_some() {
            return Err(AutomationFailure::new(
                AutomationFailureKind::SessionNotCreated,
                "one automation session already owns the browser",
            ));
        }
        if self
            .highest_generation
            .is_some_and(|highest| generation <= highest)
        {
            return Err(AutomationFailure::new(
                AutomationFailureKind::SessionNotCreated,
                "automation session generation was already retired",
            ));
        }
        let mut matcher = WildBuzzardCapabilities;
        let selected = parameters
            .match_browser(&mut matcher)
            .map_err(|_| {
                AutomationFailure::new(
                    AutomationFailureKind::InvalidArgument,
                    "requested WebDriver capabilities are invalid",
                )
            })?
            .ok_or_else(|| {
                AutomationFailure::new(
                    AutomationFailureKind::SessionNotCreated,
                    "requested WebDriver capabilities are unsupported",
                )
            })?;
        let capabilities = resolved_capabilities(&selected, page_load_timeout_ms)?;
        let session_id = random_session_id().map_err(|_| {
            AutomationFailure::new(
                AutomationFailureKind::SessionNotCreated,
                "secure WebDriver session identity generation failed",
            )
        })?;
        let active_authority = authority.clone();
        authority
            .run_if_active(|| {
                self.highest_generation = Some(generation);
                self.active = Some(ActiveSession {
                    generation,
                    session_id: session_id.clone(),
                    authority: active_authority,
                });
            })
            .ok_or_else(|| {
                AutomationFailure::new(
                    AutomationFailureKind::Timeout,
                    "automation session expired before authority publication",
                )
            })?;
        Ok(AutomationValue::NewSession {
            session_id,
            capabilities,
        })
    }

    fn start_navigation<E: EnginePort>(
        session: &mut BrowserSession<E>,
        window: BrowserWindowId,
        url: &str,
    ) -> Result<(BrowserTabId, NavigationId), AutomationFailure> {
        let tab = session
            .window_snapshot(window)
            .map_err(map_session_error)?
            .active;
        match session.dispatch(BrowserCommand::Navigate {
            tab,
            address: url.into(),
        }) {
            Ok(BrowserCommandOutcome::NavigationQueued {
                tab: queued_tab,
                navigation,
            }) if queued_tab == tab => Ok((tab, navigation)),
            Ok(_) => Err(AutomationFailure::unknown(
                "browser returned a non-navigation command outcome",
            )),
            Err(error) => Err(map_session_error(error)),
        }
    }

    fn current_url<E: EnginePort>(
        session: &BrowserSession<E>,
        window: BrowserWindowId,
    ) -> Result<Box<str>, AutomationFailure> {
        let tab = session
            .window_snapshot(window)
            .map_err(map_session_error)?
            .active;
        if let Some(url) = session.committed_url(tab).map_err(map_session_error)? {
            return Ok(url);
        }
        let snapshot = session.tab_snapshot(tab).map_err(map_session_error)?;
        if snapshot.history_len == 0 {
            Ok("about:blank".into())
        } else {
            Err(AutomationFailure::unknown(
                "current history entry has not committed",
            ))
        }
    }

    /// Claims the next exact browser-frame request for the sole pending
    /// screenshot. A normal frame remains zero-readback unless this method
    /// returns a one-shot request.
    // The shell owner-loop call site is the explicit handoff for this lane.
    #[allow(dead_code)]
    pub(crate) fn claim_screenshot_capture(
        &mut self,
        frame: BrowserFrameRequest,
    ) -> Option<AutomationScreenshotCaptureRequest> {
        self.claim_screenshot_frame(frame.into())
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn claim_screenshot_frame(
        &mut self,
        frame: ScreenshotFrameIdentity,
    ) -> Option<AutomationScreenshotCaptureRequest> {
        let pending = self.pending_screenshot.as_ref()?;
        if pending.lifetime.cancel_if_expired() {
            self.cancel_pending_screenshot(AutomationFailure::new(
                AutomationFailureKind::Timeout,
                "screenshot expired before compositor capture",
            ));
            return None;
        }
        let active = self.active.as_ref()?;
        if active.generation != pending.generation || active.authority.is_cancelled() {
            self.cancel_pending_screenshot(AutomationFailure::invalid_session(
                "screenshot lost its active session authority",
            ));
            return None;
        }
        let request = pending.request;
        let generation = pending.generation;
        let request_authority = pending.lifetime.clone();
        let session_authority = active.authority.clone();
        request_authority
            .run_if_active_with_authority(&session_authority, || {
                let pending = self.pending_screenshot.as_mut()?;
                if pending.request != request
                    || pending.generation != generation
                    || pending.claimed_frame.is_some()
                {
                    return None;
                }
                pending.claimed_frame = Some(frame);
                Some(AutomationScreenshotCaptureRequest {
                    request,
                    generation,
                    frame,
                })
            })
            .flatten()
    }

    /// Completes one exact compositor-authored capture. Foreign, stale, and
    /// replayed request/receipt combinations are rejected without consuming a
    /// different live screenshot request.
    // The shell owner-loop call site is the explicit handoff for this lane.
    #[allow(dead_code)]
    #[allow(clippy::needless_pass_by_value)]
    pub(crate) fn complete_screenshot_capture(
        &mut self,
        request: AutomationScreenshotCaptureRequest,
        capture: BrowserFrameCapture,
    ) -> bool {
        self.complete_screenshot_source(request, &capture, ScreenshotEncodingLimits::PRODUCT)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn complete_screenshot_source(
        &mut self,
        request: AutomationScreenshotCaptureRequest,
        capture: &impl ScreenshotCaptureSource,
        limits: ScreenshotEncodingLimits,
    ) -> bool {
        if capture.frame_identity() != request.frame {
            return false;
        }
        let Some(pending) = self.pending_screenshot.as_ref() else {
            return false;
        };
        if pending.request != request.request
            || pending.generation != request.generation
            || pending.claimed_frame != Some(request.frame)
        {
            return false;
        }
        if pending.lifetime.cancel_if_expired() {
            self.cancel_pending_screenshot(AutomationFailure::new(
                AutomationFailureKind::Timeout,
                "screenshot expired during compositor capture",
            ));
            return false;
        }
        let Some(active) = self
            .active
            .as_ref()
            .filter(|active| active.generation == request.generation)
        else {
            self.cancel_pending_screenshot(AutomationFailure::invalid_session(
                "screenshot completion named a retired session",
            ));
            return false;
        };
        if active.authority.is_cancelled() {
            self.cancel_pending_screenshot(AutomationFailure::invalid_session(
                "screenshot completion lost session authority",
            ));
            return false;
        }
        let encoded = encode_screenshot_png_base64(capture, limits);
        if self
            .pending_screenshot
            .as_ref()
            .is_some_and(|pending| pending.lifetime.cancel_if_expired())
        {
            self.cancel_pending_screenshot(AutomationFailure::new(
                AutomationFailureKind::Timeout,
                "screenshot expired during PNG encoding",
            ));
            return false;
        }
        let Some(active) = self
            .active
            .as_ref()
            .filter(|active| active.generation == request.generation)
        else {
            self.cancel_pending_screenshot(AutomationFailure::invalid_session(
                "screenshot session retired during PNG encoding",
            ));
            return false;
        };
        let request_authority = self
            .pending_screenshot
            .as_ref()
            .expect("exact pending screenshot was rechecked")
            .lifetime
            .clone();
        let session_authority = active.authority.clone();
        let completed = request_authority
            .run_if_active_with_authority(&session_authority, || {
                let still_exact = self.pending_screenshot.as_ref().is_some_and(|pending| {
                    pending.request == request.request
                        && pending.generation == request.generation
                        && pending.claimed_frame == Some(request.frame)
                });
                still_exact
                    .then(|| self.pending_screenshot.take())
                    .flatten()
            })
            .flatten();
        let Some(pending) = completed else {
            return false;
        };
        match encoded {
            Ok(png) => self.send_screenshot_result(&pending, Ok(AutomationValue::Screenshot(png))),
            Err(()) => self.send_screenshot_result(
                &pending,
                Err(AutomationFailure::new(
                    AutomationFailureKind::UnableToCaptureScreen,
                    "compositor screenshot exceeded encoding bounds or was malformed",
                )),
            ),
        }
        true
    }

    /// Resolves an exact claimed screenshot after native capture submission
    /// fails. A foreign or replayed claim cannot cancel another request.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn fail_screenshot_capture(
        &mut self,
        request: AutomationScreenshotCaptureRequest,
    ) -> bool {
        let Some(pending) = self.pending_screenshot.as_ref() else {
            return false;
        };
        if pending.request != request.request
            || pending.generation != request.generation
            || pending.claimed_frame != Some(request.frame)
        {
            return false;
        }
        if pending.lifetime.cancel_if_expired() {
            self.cancel_pending_screenshot(AutomationFailure::new(
                AutomationFailureKind::Timeout,
                "screenshot expired while native capture failed",
            ));
            return true;
        }
        self.cancel_pending_screenshot(AutomationFailure::new(
            AutomationFailureKind::UnableToCaptureScreen,
            "native compositor could not capture the requested frame",
        ));
        true
    }

    pub(crate) fn presentation_admission<E: EnginePort>(
        &self,
        session: &BrowserSession<E>,
        identity: PresentationCommitIdentity,
    ) -> AutomationPresentationAdmission {
        let Some(pending) = self.pending_navigation.as_ref() else {
            return AutomationPresentationAdmission::Unrestricted;
        };
        if pending.tab != identity.tab || pending.navigation != identity.navigation {
            return AutomationPresentationAdmission::Unrestricted;
        }
        let Some(active) = self
            .active
            .as_ref()
            .filter(|active| active.generation == pending.generation)
        else {
            return AutomationPresentationAdmission::Rejected;
        };
        let candidate_is_exact = session.tab_snapshot(identity.tab).is_ok_and(|snapshot| {
            snapshot.latest_navigation == Some(identity.navigation)
                && snapshot.live_navigation == Some(identity.navigation)
                && snapshot.latest_navigation_phase == Some(NavigationPhase::Ready)
                && snapshot.engine_frame_version == Some(identity.document)
                && snapshot.frame == Some(identity.lease)
        }) && session
            .presentation_candidate_labels(identity.tab)
            .is_ok_and(|labels| {
                labels
                    == Some((
                        identity.navigation,
                        identity.document,
                        identity.lease,
                        identity.scene_revision,
                    ))
            })
            && session
                .committed_url(identity.tab)
                .is_ok_and(|url| url.is_some());
        if !candidate_is_exact {
            return AutomationPresentationAdmission::Rejected;
        }
        AutomationPresentationAdmission::Authorized(AutomationPresentationPermit {
            identity,
            request_authority: pending.lifetime.clone(),
            session_authority: active.authority.clone(),
            outcome: Mutex::new(NativePresentationCommitOutcome::Pending),
        })
    }

    pub(crate) fn observe_session<E: EnginePort>(&mut self, session: &mut BrowserSession<E>) {
        self.revoke_cancelled_authority(session);
        if self
            .pending_screenshot
            .as_ref()
            .is_some_and(|pending| pending.lifetime.cancel_if_expired())
        {
            self.cancel_pending_screenshot(AutomationFailure::new(
                AutomationFailureKind::Timeout,
                "screenshot exceeded its owner-thread deadline",
            ));
        }
        let Some(pending) = self.pending_navigation.as_ref() else {
            return;
        };
        if pending.lifetime.cancel_if_expired() {
            self.cancel_pending_navigation(
                session,
                AutomationFailure::new(
                    AutomationFailureKind::Timeout,
                    "automation navigation exceeded its owner-thread deadline",
                ),
            );
            return;
        }
        let snapshot = match session.tab_snapshot(pending.tab) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.finish_pending(Err(map_session_error(error)));
                return;
            }
        };
        if snapshot.latest_navigation != Some(pending.navigation) {
            self.cancel_pending_navigation(
                session,
                AutomationFailure::unknown(
                    "automation navigation was superseded before composition",
                ),
            );
            return;
        }
        match snapshot.latest_navigation_phase {
            Some(NavigationPhase::Cancelled) => self.finish_pending(Err(AutomationFailure::new(
                AutomationFailureKind::Timeout,
                "automation navigation was cancelled",
            ))),
            Some(NavigationPhase::Failed) => self.finish_pending(Err(AutomationFailure::unknown(
                "automation navigation failed before composition",
            ))),
            _ => {}
        }
    }

    pub(crate) fn observe_composition<E: EnginePort>(
        &mut self,
        session: &BrowserSession<E>,
        identity: PresentationCommitIdentity,
    ) {
        let Some(pending) = self.pending_navigation.as_ref() else {
            return;
        };
        if pending.tab != identity.tab || pending.navigation != identity.navigation {
            return;
        }
        let snapshot = match session.tab_snapshot(identity.tab) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.finish_pending(Err(map_session_error(error)));
                return;
            }
        };
        if snapshot.latest_navigation != Some(pending.navigation)
            || snapshot.live_navigation != Some(pending.navigation)
            || snapshot.latest_navigation_phase != Some(NavigationPhase::Ready)
            || snapshot.engine_frame_version != Some(identity.document)
        {
            return;
        }
        if session.committed_url(pending.tab).ok().flatten().is_none() {
            self.finish_pending(Err(AutomationFailure::unknown(
                "composed navigation lacks a committed URL",
            )));
            return;
        }
        let Some(active) = self
            .active
            .as_ref()
            .filter(|active| active.generation == pending.generation)
        else {
            return;
        };
        let pending_request = pending.request;
        let pending_generation = pending.generation;
        let request_authority = pending.lifetime.clone();
        let session_authority = active.authority.clone();
        let completed = request_authority
            .run_if_active_with_authority(&session_authority, || {
                let still_exact = self.pending_navigation.as_ref().is_some_and(|candidate| {
                    candidate.request == pending_request
                        && candidate.generation == pending_generation
                        && candidate.tab == identity.tab
                        && candidate.navigation == identity.navigation
                }) && self
                    .active
                    .as_ref()
                    .is_some_and(|candidate| candidate.generation == pending_generation);
                still_exact
                    .then(|| self.pending_navigation.take())
                    .flatten()
            })
            .flatten();
        if let Some(pending) = completed {
            self.send_pending_result(&pending, Ok(AutomationValue::NavigationComplete));
        }
    }

    fn revoke_cancelled_authority<E: EnginePort>(&mut self, session: &mut BrowserSession<E>) {
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.authority.is_cancelled())
        {
            self.cancel_pending_navigation(
                session,
                AutomationFailure::invalid_session(
                    "automation requester cancelled its session authority",
                ),
            );
            self.cancel_pending_screenshot(AutomationFailure::invalid_session(
                "automation requester cancelled its session authority",
            ));
            self.active = None;
        }
    }

    fn cancel_pending_navigation<E: EnginePort>(
        &mut self,
        session: &mut BrowserSession<E>,
        failure: AutomationFailure,
    ) {
        let pending = self.take_pending_navigation_after_stop(session);
        if let Some(pending) = pending {
            self.send_pending_result(&pending, Err(failure));
        }
    }

    fn take_pending_navigation_after_stop<E: EnginePort>(
        &mut self,
        session: &mut BrowserSession<E>,
    ) -> Option<PendingNavigation> {
        if let Some(pending) = self.pending_navigation.as_ref()
            && session.tab_snapshot(pending.tab).is_ok_and(|snapshot| {
                snapshot.latest_navigation == Some(pending.navigation) && snapshot.loading
            })
        {
            let _ = session.dispatch(BrowserCommand::Stop { tab: pending.tab });
        }
        self.pending_navigation.take()
    }

    fn finish_pending(&mut self, result: Result<AutomationValue, AutomationFailure>) {
        let Some(pending) = self.pending_navigation.take() else {
            return;
        };
        self.send_pending_result(&pending, result);
    }

    fn send_pending_result(
        &mut self,
        pending: &PendingNavigation,
        result: Result<AutomationValue, AutomationFailure>,
    ) {
        let generation = pending.generation;
        if pending
            .response
            .send(AutomationResultMessage {
                protocol: AUTOMATION_PROTOCOL_ID,
                kind: AUTOMATION_RESULT_KIND,
                request: pending.request,
                generation,
                result,
            })
            .is_err()
        {
            pending.lifetime.revoke();
            if self
                .active
                .as_ref()
                .is_some_and(|active| active.generation == generation)
            {
                if let Some(active) = &self.active {
                    active.authority.revoke();
                }
                self.active = None;
            }
        }
    }

    fn cancel_pending_screenshot(&mut self, failure: AutomationFailure) {
        let Some(pending) = self.pending_screenshot.take() else {
            return;
        };
        self.send_screenshot_result(&pending, Err(failure));
    }

    fn send_screenshot_result(
        &mut self,
        pending: &PendingScreenshot,
        result: Result<AutomationValue, AutomationFailure>,
    ) {
        let generation = pending.generation;
        if pending
            .response
            .send(AutomationResultMessage {
                protocol: AUTOMATION_PROTOCOL_ID,
                kind: AUTOMATION_RESULT_KIND,
                request: pending.request,
                generation,
                result,
            })
            .is_err()
        {
            pending.lifetime.revoke();
            if self
                .active
                .as_ref()
                .is_some_and(|active| active.generation == generation)
            {
                if let Some(active) = &self.active {
                    active.authority.revoke();
                }
                self.active = None;
            }
        }
    }

    pub(crate) fn shutdown<E: EnginePort>(&mut self, session: &mut BrowserSession<E>) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.cancel_pending_navigation(
            session,
            AutomationFailure::unknown("browser shutdown cancelled automation"),
        );
        self.cancel_pending_screenshot(AutomationFailure::unknown(
            "browser shutdown cancelled automation",
        ));
        if let Some(active) = &self.active {
            active.authority.revoke();
        }
        self.active = None;
        while let Ok(queued) = self.command_recv.try_recv() {
            queued.lifetime.revoke();
            let _ = queued.response.send(AutomationResultMessage {
                protocol: AUTOMATION_PROTOCOL_ID,
                kind: AUTOMATION_RESULT_KIND,
                request: queued.message.request,
                generation: queued.message.generation,
                result: Err(AutomationFailure::unknown(
                    "browser shutdown cancelled queued automation",
                )),
            });
        }
    }

    pub(crate) const fn has_pending_navigation(&self) -> bool {
        self.pending_navigation.is_some()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) const fn has_pending_screenshot(&self) -> bool {
        self.pending_screenshot.is_some()
    }
}

#[derive(Clone, Copy)]
#[cfg_attr(not(test), allow(dead_code))]
struct ScreenshotEncodingLimits {
    pixels: u64,
    png_bytes: usize,
    base64_bytes: usize,
}

#[cfg_attr(not(test), allow(dead_code))]
impl ScreenshotEncodingLimits {
    const PRODUCT: Self = Self {
        pixels: MAX_SCREENSHOT_PIXELS,
        png_bytes: MAX_SCREENSHOT_PNG_BYTES,
        base64_bytes: MAX_SCREENSHOT_BASE64_BYTES,
    };
}

#[cfg_attr(not(test), allow(dead_code))]
fn encode_screenshot_png_base64(
    capture: &impl ScreenshotCaptureSource,
    limits: ScreenshotEncodingLimits,
) -> Result<Box<str>, ()> {
    let width = capture.width();
    let height = capture.height();
    if width == 0 || height == 0 {
        return Err(());
    }
    let pixels = u64::from(width).checked_mul(u64::from(height)).ok_or(())?;
    if pixels > limits.pixels {
        return Err(());
    }
    let row_bytes = usize::try_from(width)
        .map_err(|_| ())?
        .checked_mul(4)
        .ok_or(())?;
    let scanline_bytes = row_bytes.checked_add(1).ok_or(())?;
    let filtered_bytes = scanline_bytes
        .checked_mul(usize::try_from(height).map_err(|_| ())?)
        .ok_or(())?;
    if filtered_bytes > limits.png_bytes {
        return Err(());
    }
    let mut raw = Vec::new();
    raw.try_reserve_exact(filtered_bytes).map_err(|_| ())?;
    for y in 0..height {
        let row = capture.row(y).ok_or(())?;
        if row.len() != row_bytes {
            return Err(());
        }
        raw.push(0);
        for bgra in row.chunks_exact(4) {
            raw.extend_from_slice(&[bgra[2], bgra[1], bgra[0], bgra[3]]);
        }
    }
    if raw.len() != filtered_bytes {
        return Err(());
    }

    let block_count = filtered_bytes.checked_add(65_534).ok_or(())? / 65_535;
    let zlib_bytes = filtered_bytes
        .checked_add(block_count.checked_mul(5).ok_or(())?)
        .and_then(|bytes| bytes.checked_add(6))
        .ok_or(())?;
    if zlib_bytes > limits.png_bytes {
        return Err(());
    }
    let mut zlib = Vec::new();
    zlib.try_reserve_exact(zlib_bytes).map_err(|_| ())?;
    zlib.extend_from_slice(&[0x78, 0x01]);
    let last_block = block_count.checked_sub(1).ok_or(())?;
    for (index, block) in raw.chunks(65_535).enumerate() {
        zlib.push(u8::from(index == last_block));
        let length = u16::try_from(block.len()).map_err(|_| ())?;
        zlib.extend_from_slice(&length.to_le_bytes());
        zlib.extend_from_slice(&(!length).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());
    if zlib.len() != zlib_bytes {
        return Err(());
    }

    let mut png = Vec::new();
    png.try_reserve_exact(limits.png_bytes.min(zlib_bytes.saturating_add(57)))
        .map_err(|_| ())?;
    push_bounded(&mut png, b"\x89PNG\r\n\x1a\n", limits.png_bytes)?;
    let mut ihdr = [0_u8; 13];
    ihdr[..4].copy_from_slice(&width.to_be_bytes());
    ihdr[4..8].copy_from_slice(&height.to_be_bytes());
    ihdr[8] = 8;
    ihdr[9] = 6;
    append_png_chunk(&mut png, *b"IHDR", &ihdr, limits.png_bytes)?;
    append_png_chunk(&mut png, *b"IDAT", &zlib, limits.png_bytes)?;
    append_png_chunk(&mut png, *b"IEND", &[], limits.png_bytes)?;
    encode_base64(&png, limits.base64_bytes).map(String::into_boxed_str)
}

#[cfg_attr(not(test), allow(dead_code))]
fn push_bounded(output: &mut Vec<u8>, bytes: &[u8], maximum: usize) -> Result<(), ()> {
    let final_len = output.len().checked_add(bytes.len()).ok_or(())?;
    if final_len > maximum {
        return Err(());
    }
    output.try_reserve(bytes.len()).map_err(|_| ())?;
    output.extend_from_slice(bytes);
    Ok(())
}

#[cfg_attr(not(test), allow(dead_code))]
fn append_png_chunk(
    png: &mut Vec<u8>,
    kind: [u8; 4],
    data: &[u8],
    maximum: usize,
) -> Result<(), ()> {
    let length = u32::try_from(data.len()).map_err(|_| ())?;
    push_bounded(png, &length.to_be_bytes(), maximum)?;
    push_bounded(png, &kind, maximum)?;
    push_bounded(png, data, maximum)?;
    let crc = !crc32_state(crc32_state(u32::MAX, &kind), data);
    push_bounded(png, &crc.to_be_bytes(), maximum)
}

#[cfg_attr(not(test), allow(dead_code))]
fn crc32(bytes: &[u8]) -> u32 {
    !crc32_state(u32::MAX, bytes)
}

#[cfg_attr(not(test), allow(dead_code))]
fn crc32_state(mut crc: u32, bytes: &[u8]) -> u32 {
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    crc
}

#[cfg_attr(not(test), allow(dead_code))]
fn adler32(bytes: &[u8]) -> u32 {
    const MODULUS: u32 = 65_521;
    let mut first = 1_u32;
    let mut second = 0_u32;
    for &byte in bytes {
        first = (first + u32::from(byte)) % MODULUS;
        second = (second + first) % MODULUS;
    }
    (second << 16) | first
}

#[cfg_attr(not(test), allow(dead_code))]
fn encode_base64(bytes: &[u8], maximum: usize) -> Result<String, ()> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let encoded_len = bytes
        .len()
        .checked_add(2)
        .ok_or(())?
        .checked_div(3)
        .ok_or(())?
        .checked_mul(4)
        .ok_or(())?;
    if encoded_len > maximum {
        return Err(());
    }
    let mut encoded = Vec::new();
    encoded.try_reserve_exact(encoded_len).map_err(|_| ())?;
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(ALPHABET[usize::from(first >> 2)]);
        encoded.push(ALPHABET[usize::from(((first & 0x03) << 4) | (second >> 4))]);
        if chunk.len() > 1 {
            encoded.push(ALPHABET[usize::from(((second & 0x0f) << 2) | (third >> 6))]);
        } else {
            encoded.push(b'=');
        }
        if chunk.len() > 2 {
            encoded.push(ALPHABET[usize::from(third & 0x3f)]);
        } else {
            encoded.push(b'=');
        }
    }
    if encoded.len() != encoded_len {
        return Err(());
    }
    String::from_utf8(encoded).map_err(|_| ())
}

fn map_session_error(error: SessionError) -> AutomationFailure {
    match error {
        SessionError::UnknownWindow(_) | SessionError::UnknownTab(_) => AutomationFailure::new(
            AutomationFailureKind::NoSuchWindow,
            "automation browsing context is no longer live",
        ),
        SessionError::Address(_) | SessionError::NavigationRequest(_) => AutomationFailure::new(
            AutomationFailureKind::InvalidArgument,
            "automation navigation URL was rejected",
        ),
        _ => AutomationFailure::unknown("browser session rejected automation command"),
    }
}

struct WildBuzzardCapabilities;

impl BrowserCapabilities for WildBuzzardCapabilities {
    fn init(&mut self, _: &Capabilities) {}

    fn browser_name(&mut self, _: &Capabilities) -> WebDriverResult<Option<String>> {
        Ok(Some("wild buzzard".to_owned()))
    }

    fn browser_version(&mut self, _: &Capabilities) -> WebDriverResult<Option<String>> {
        Ok(Some(env!("CARGO_PKG_VERSION").to_owned()))
    }

    fn compare_browser_version(
        &mut self,
        version: &str,
        comparison: &str,
    ) -> WebDriverResult<bool> {
        Ok(version == comparison)
    }

    fn platform_name(&mut self, _: &Capabilities) -> WebDriverResult<Option<String>> {
        Ok(Some("linux".to_owned()))
    }

    fn accept_insecure_certs(&mut self, _: &Capabilities) -> WebDriverResult<bool> {
        Ok(false)
    }

    fn set_window_rect(&mut self, _: &Capabilities) -> WebDriverResult<bool> {
        Ok(false)
    }

    fn strict_file_interactability(&mut self, _: &Capabilities) -> WebDriverResult<bool> {
        Ok(false)
    }

    fn web_socket_url(&mut self, _: &Capabilities) -> WebDriverResult<bool> {
        Ok(false)
    }

    fn webauthn_virtual_authenticators(&mut self, _: &Capabilities) -> WebDriverResult<bool> {
        Ok(false)
    }

    fn webauthn_extension_uvm(&mut self, _: &Capabilities) -> WebDriverResult<bool> {
        Ok(false)
    }

    fn webauthn_extension_prf(&mut self, _: &Capabilities) -> WebDriverResult<bool> {
        Ok(false)
    }

    fn webauthn_extension_large_blob(&mut self, _: &Capabilities) -> WebDriverResult<bool> {
        Ok(false)
    }

    fn webauthn_extension_cred_blob(&mut self, _: &Capabilities) -> WebDriverResult<bool> {
        Ok(false)
    }

    fn accept_proxy(
        &mut self,
        proxy_settings: &Map<String, Value>,
        _: &Capabilities,
    ) -> WebDriverResult<bool> {
        Ok(proxy_settings.is_empty()
            || proxy_settings.get("proxyType").and_then(Value::as_str) == Some("direct"))
    }

    fn validate_custom(&mut self, _: &str, _: &Value) -> WebDriverResult<()> {
        Ok(())
    }

    fn accept_custom(&mut self, _: &str, _: &Value, _: &Capabilities) -> WebDriverResult<bool> {
        Ok(false)
    }
}

fn resolved_capabilities(
    selected: &Capabilities,
    page_load_timeout_ms: u64,
) -> Result<Value, AutomationFailure> {
    if selected
        .get("pageLoadStrategy")
        .and_then(Value::as_str)
        .is_some_and(|strategy| strategy != "normal")
    {
        return Err(AutomationFailure::new(
            AutomationFailureKind::SessionNotCreated,
            "only normal page-load strategy is supported",
        ));
    }
    if selected.contains_key("timeouts") {
        return Err(AutomationFailure::new(
            AutomationFailureKind::SessionNotCreated,
            "custom WebDriver timeouts are not supported by this slice",
        ));
    }
    let proxy = selected.get("proxy").cloned().unwrap_or_else(|| json!({}));
    Ok(json!({
        "browserName": "wild buzzard",
        "browserVersion": env!("CARGO_PKG_VERSION"),
        "platformName": "linux",
        "acceptInsecureCerts": false,
        "pageLoadStrategy": "normal",
        "proxy": proxy,
        "setWindowRect": false,
        "timeouts": {
            "implicit": 0,
            "pageLoad": page_load_timeout_ms,
            "script": 30_000,
        },
        "strictFileInteractability": false,
        "unhandledPromptBehavior": "dismiss and notify",
        "userAgent": "WildBuzzard/0.1",
    }))
}

fn random_session_id() -> io::Result<Box<str>> {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let mut random = [0_u8; SESSION_ID_BYTES];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    random[6] = (random[6] & 0x0f) | 0x40;
    random[8] = (random[8] & 0x3f) | 0x80;
    let mut encoded = [0_u8; SESSION_ID_BYTES * 2];
    for (index, byte) in random.into_iter().enumerate() {
        encoded[index * 2] = HEX[usize::from(byte >> 4)];
        encoded[index * 2 + 1] = HEX[usize::from(byte & 0x0f)];
    }
    let value = String::from_utf8(encoded.to_vec())
        .map_err(|_| io::Error::other("hex session identity was not UTF-8"))?;
    Ok(value.into_boxed_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, VecDeque};
    use std::rc::Rc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::thread;

    use wild_buzzard_engine::{
        NavigationCommitMetadata, NavigationConnectionSecurity, NavigationGeneration,
        NavigationRequest, TopLevelContextId,
    };
    use wild_buzzard_ui::{
        EngineDocumentVersion, EngineFrameDescriptor, EngineFrameLease, EngineMutationResultLease,
        EnginePortError, EnginePortEvent, EnginePortEventKind, EnginePortExecutorShutdown,
        EnginePortFrameLeaseId, EnginePortMutationLeaseId, EnginePortSequence,
        EnginePortShutdownStatus, EnginePortStopReason, SessionLimits,
    };

    struct FakeState {
        navigation: Option<NavigationId>,
        events: VecDeque<EnginePortEvent>,
        next_sequence: u64,
        commitment: BTreeMap<NavigationId, NavigationCommitMetadata>,
        frames: BTreeMap<EnginePortFrameLeaseId, EngineFrameLease>,
        cancellations: Vec<NavigationId>,
    }

    impl Default for FakeState {
        fn default() -> Self {
            Self {
                navigation: None,
                events: VecDeque::new(),
                next_sequence: 1,
                commitment: BTreeMap::new(),
                frames: BTreeMap::new(),
                cancellations: Vec::new(),
            }
        }
    }

    #[derive(Clone)]
    struct FakeHandle(Rc<RefCell<FakeState>>);

    impl FakeHandle {
        fn navigation(&self) -> NavigationId {
            self.0.borrow().navigation.unwrap()
        }

        fn cancellations(&self) -> Vec<NavigationId> {
            self.0.borrow().cancellations.clone()
        }

        fn push(&self, kind: EnginePortEventKind) {
            let mut state = self.0.borrow_mut();
            let sequence = EnginePortSequence::new(state.next_sequence).unwrap();
            state.next_sequence += 1;
            state.events.push_back(EnginePortEvent::new(sequence, kind));
        }

        fn complete(&self, navigation: NavigationId, final_url: &str) {
            self.0.borrow_mut().commitment.insert(
                navigation,
                NavigationCommitMetadata::new(
                    final_url,
                    1,
                    NavigationConnectionSecurity::Cleartext,
                    false,
                )
                .unwrap(),
            );
            self.push(EnginePortEventKind::NavigationStarted { navigation });
            self.push(EnginePortEventKind::NavigationCommitted {
                navigation,
                http_status: 200,
            });
            let lease = EnginePortFrameLeaseId::new(1).unwrap();
            let document = EngineDocumentVersion::new(1, 0);
            let frame = EngineFrameLease::from_owned_rgba8(
                navigation,
                lease,
                1,
                1,
                vec![0, 0, 0, 255],
                Some(document),
            )
            .unwrap();
            self.0.borrow_mut().frames.insert(lease, frame);
            self.push(EnginePortEventKind::FrameReady {
                navigation,
                lease,
                descriptor: EngineFrameDescriptor::rgba8(1, 1, 4).unwrap(),
                document_version: Some(document),
            });
        }
    }

    struct FakePort(Rc<RefCell<FakeState>>);

    impl FakePort {
        fn pair() -> (Self, FakeHandle) {
            let state = Rc::new(RefCell::new(FakeState::default()));
            (Self(Rc::clone(&state)), FakeHandle(state))
        }
    }

    impl EnginePort for FakePort {
        fn navigate(
            &mut self,
            context: TopLevelContextId,
            _: NavigationRequest,
        ) -> Result<NavigationId, EnginePortError> {
            let navigation = NavigationId::new(context, NavigationGeneration::INITIAL);
            self.0.borrow_mut().navigation = Some(navigation);
            Ok(navigation)
        }

        fn cancel_navigation(&mut self, navigation: NavigationId) -> Result<(), EnginePortError> {
            self.0.borrow_mut().cancellations.push(navigation);
            Ok(())
        }

        fn close_context(&mut self, _: NavigationId) -> Result<(), EnginePortError> {
            Ok(())
        }

        fn poll_event(&mut self) -> Result<Option<EnginePortEvent>, EnginePortError> {
            Ok(self.0.borrow_mut().events.pop_front())
        }

        fn take_navigation_commit(
            &mut self,
            navigation: NavigationId,
        ) -> Result<Option<NavigationCommitMetadata>, EnginePortError> {
            Ok(self.0.borrow_mut().commitment.remove(&navigation))
        }

        fn take_frame(
            &mut self,
            navigation: NavigationId,
            lease: EnginePortFrameLeaseId,
        ) -> Result<EngineFrameLease, EnginePortError> {
            let frame = self
                .0
                .borrow_mut()
                .frames
                .remove(&lease)
                .ok_or(EnginePortError::ContractViolation("missing test frame"))?;
            if frame.navigation() != navigation {
                return Err(EnginePortError::ContractViolation(
                    "test frame navigation mismatch",
                ));
            }
            Ok(frame)
        }

        fn take_mutation_result(
            &mut self,
            _: NavigationId,
            _: EnginePortMutationLeaseId,
        ) -> Result<EngineMutationResultLease, EnginePortError> {
            Err(EnginePortError::ContractViolation(
                "test port has no mutation results",
            ))
        }

        fn shutdown(&mut self) -> EnginePortShutdownStatus {
            EnginePortShutdownStatus::new(
                EnginePortStopReason::Requested,
                EnginePortExecutorShutdown::Clean,
            )
        }
    }

    fn session() -> (BrowserSession<FakePort>, FakeHandle) {
        let (port, handle) = FakePort::pair();
        let limits = SessionLimits::new(1, 4, 4, 4, 16, 16_384, 16_384, 4_096, 32).unwrap();
        (
            BrowserSession::new_with_navigation_mode(
                port,
                limits,
                wild_buzzard_ui::BrowserNavigationMode::GeneralWeb,
            )
            .unwrap(),
            handle,
        )
    }

    fn presentation_identity(
        tab: BrowserTabId,
        navigation: NavigationId,
    ) -> PresentationCommitIdentity {
        PresentationCommitIdentity {
            tab,
            navigation,
            document: EngineDocumentVersion::new(1, 0),
            lease: EnginePortFrameLeaseId::new(1).unwrap(),
            scene_revision: 1,
        }
    }

    fn queued(
        request: u64,
        generation: u64,
        session_id: Option<&str>,
        command: AutomationCommand,
    ) -> (QueuedCommand, Receiver<AutomationResultMessage>) {
        queued_with_lifetime(
            request,
            generation,
            session_id,
            command,
            DispatchLifetime::new(Instant::now() + Duration::from_secs(1)),
        )
    }

    fn queued_with_lifetime(
        request: u64,
        generation: u64,
        session_id: Option<&str>,
        command: AutomationCommand,
        lifetime: DispatchLifetime,
    ) -> (QueuedCommand, Receiver<AutomationResultMessage>) {
        let (response, receive) = channel();
        (
            QueuedCommand {
                message: AutomationCommandMessage {
                    protocol: AUTOMATION_PROTOCOL_ID,
                    kind: AUTOMATION_COMMAND_KIND,
                    request: AutomationRequestId(NonZeroU64::new(request).unwrap()),
                    generation: AutomationSessionGeneration(NonZeroU64::new(generation).unwrap()),
                    session_id: session_id.map(Into::into),
                    command,
                },
                lifetime,
                response,
            },
            receive,
        )
    }

    #[derive(Default)]
    struct TestWake {
        wakes: AtomicUsize,
        closed: AtomicBool,
    }

    impl AutomationWake for TestWake {
        fn wake(&self) -> LinuxWakeStatus {
            self.wakes.fetch_add(1, Ordering::AcqRel);
            if self.closed.load(Ordering::Acquire) {
                LinuxWakeStatus::Closed
            } else {
                LinuxWakeStatus::Queued
            }
        }
    }

    fn test_ingress(
        command_send: SyncSender<QueuedCommand>,
        wake: Arc<TestWake>,
    ) -> AutomationIngress {
        AutomationIngress {
            command_send,
            wake,
            command_deadline: Duration::from_secs(1),
            next_request_id: Some(2),
            next_generation: Some(2),
            active_generation: None,
            active_session_id: None,
            active_authority: None,
            closed: false,
        }
    }

    fn install_active_authority(
        ingress: &mut AutomationIngress,
        owner: &mut AutomationOwner,
        generation: AutomationSessionGeneration,
        session_id: &str,
    ) -> DispatchLifetime {
        let authority = DispatchLifetime::new(Instant::now() + Duration::from_mins(1));
        ingress.active_generation = Some(generation);
        ingress.active_session_id = Some(session_id.into());
        ingress.active_authority = Some(authority.clone());
        owner.highest_generation = Some(generation);
        owner.active = Some(ActiveSession {
            generation,
            session_id: session_id.into(),
            authority: authority.clone(),
        });
        authority
    }

    fn new_session_parameters() -> NewSessionParameters {
        serde_json::from_value(json!({"capabilities": {}})).unwrap()
    }

    fn new_session_command() -> AutomationCommand {
        AutomationCommand::NewSession {
            parameters: new_session_parameters(),
            page_load_timeout_ms: 30_000,
        }
    }

    fn test_screenshot_frame(sequence: u64) -> ScreenshotFrameIdentity {
        ScreenshotFrameIdentity {
            surface_namespace: 6_006,
            surface_slot: 3,
            surface_generation: 2,
            surface_revision: 4,
            width: 1_024,
            height: 768,
            scale_bits: 1.0_f64.to_bits(),
            format: PixelFormat::Rgba8Srgb,
            role: SurfaceRole::Window,
            acceleration: LinuxAccelerationClass::Unverified,
            reset_protection: LinuxResetProtection::LoseContextOnReset,
            page: ScreenshotPageIdentity::Blank,
            chrome_revision: 9,
            epoch: 11,
            sequence,
        }
    }

    struct FakeScreenshotCapture {
        frame: ScreenshotFrameIdentity,
        width: u32,
        height: u32,
        stride: usize,
        pixels: Vec<u8>,
    }

    impl FakeScreenshotCapture {
        fn new(frame: ScreenshotFrameIdentity, width: u32, height: u32, pixels: Vec<u8>) -> Self {
            Self {
                frame,
                width,
                height,
                stride: usize::try_from(width).unwrap() * 4,
                pixels,
            }
        }
    }

    impl ScreenshotCaptureSource for FakeScreenshotCapture {
        fn frame_identity(&self) -> ScreenshotFrameIdentity {
            self.frame
        }

        fn width(&self) -> u32 {
            self.width
        }

        fn height(&self) -> u32 {
            self.height
        }

        fn row(&self, row: u32) -> Option<&[u8]> {
            let start = usize::try_from(row).ok()?.checked_mul(self.stride)?;
            self.pixels.get(start..start.checked_add(self.stride)?)
        }
    }

    fn create_test_session(
        send: &SyncSender<QueuedCommand>,
        owner: &mut AutomationOwner,
        session: &mut BrowserSession<FakePort>,
        window: BrowserWindowId,
        request: u64,
        generation: u64,
    ) -> Box<str> {
        let (create, result) = queued(request, generation, None, new_session_command());
        send.send(create).unwrap();
        owner.drain(session, window);
        match result.recv().unwrap().result {
            Ok(AutomationValue::NewSession { session_id, .. }) => session_id,
            other => panic!("unexpected new-session result: {other:?}"),
        }
    }

    fn decode_base64(encoded: &str) -> Vec<u8> {
        fn value(byte: u8) -> Option<u8> {
            match byte {
                b'A'..=b'Z' => Some(byte - b'A'),
                b'a'..=b'z' => Some(byte - b'a' + 26),
                b'0'..=b'9' => Some(byte - b'0' + 52),
                b'+' => Some(62),
                b'/' => Some(63),
                _ => None,
            }
        }

        assert_eq!(encoded.len() % 4, 0);
        let mut decoded = Vec::new();
        for quartet in encoded.as_bytes().chunks_exact(4) {
            let first = value(quartet[0]).unwrap();
            let second = value(quartet[1]).unwrap();
            decoded.push((first << 2) | (second >> 4));
            if quartet[2] != b'=' {
                let third = value(quartet[2]).unwrap();
                decoded.push((second << 4) | (third >> 2));
                if quartet[3] != b'=' {
                    let fourth = value(quartet[3]).unwrap();
                    decoded.push((third << 6) | fourth);
                }
            }
        }
        decoded
    }

    fn parse_png(png: &[u8]) -> (u32, u32, Vec<u8>) {
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let mut cursor = 8;
        let mut dimensions = None;
        let mut idat = Vec::new();
        let mut saw_end = false;
        while cursor < png.len() {
            let length = u32::from_be_bytes(png[cursor..cursor + 4].try_into().unwrap()) as usize;
            cursor += 4;
            let kind: [u8; 4] = png[cursor..cursor + 4].try_into().unwrap();
            cursor += 4;
            let data = &png[cursor..cursor + length];
            cursor += length;
            let expected_crc = u32::from_be_bytes(png[cursor..cursor + 4].try_into().unwrap());
            cursor += 4;
            let mut crc_input = kind.to_vec();
            crc_input.extend_from_slice(data);
            assert_eq!(crc32(&crc_input), expected_crc);
            match &kind {
                b"IHDR" => {
                    assert_eq!(data.len(), 13);
                    assert_eq!(&data[8..], &[8, 6, 0, 0, 0]);
                    dimensions = Some((
                        u32::from_be_bytes(data[..4].try_into().unwrap()),
                        u32::from_be_bytes(data[4..8].try_into().unwrap()),
                    ));
                }
                b"IDAT" => idat.extend_from_slice(data),
                b"IEND" => {
                    assert!(data.is_empty());
                    saw_end = true;
                }
                _ => panic!("unexpected PNG chunk"),
            }
        }
        assert!(saw_end);
        assert_eq!(&idat[..2], &[0x78, 0x01]);
        let checksum_at = idat.len() - 4;
        let mut compressed_cursor = 2;
        let mut raw = Vec::new();
        loop {
            let header = idat[compressed_cursor];
            compressed_cursor += 1;
            assert!(header <= 1);
            let length = usize::from(u16::from_le_bytes(
                idat[compressed_cursor..compressed_cursor + 2]
                    .try_into()
                    .unwrap(),
            ));
            compressed_cursor += 2;
            let inverse = u16::from_le_bytes(
                idat[compressed_cursor..compressed_cursor + 2]
                    .try_into()
                    .unwrap(),
            );
            compressed_cursor += 2;
            assert_eq!(inverse, !u16::try_from(length).unwrap());
            raw.extend_from_slice(&idat[compressed_cursor..compressed_cursor + length]);
            compressed_cursor += length;
            if header == 1 {
                break;
            }
        }
        assert_eq!(compressed_cursor, checksum_at);
        assert_eq!(
            adler32(&raw),
            u32::from_be_bytes(idat[checksum_at..].try_into().unwrap())
        );
        let (width, height) = dimensions.unwrap();
        (width, height, raw)
    }

    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    struct PresentedEffects {
        receipts: u64,
        compositions: u64,
        identity_updates: u64,
        pointer_updates: u64,
        surface_updates: u64,
        later_mutations: u64,
    }

    fn test_presentation_identity() -> PresentationCommitIdentity {
        let navigation = NavigationId::new(
            TopLevelContextId::new(1).unwrap(),
            NavigationGeneration::INITIAL,
        );
        presentation_identity(BrowserTabId::new(1).unwrap(), navigation)
    }

    fn presentation_permit(
        identity: PresentationCommitIdentity,
        request: DispatchLifetime,
        session: DispatchLifetime,
    ) -> Arc<AutomationPresentationPermit> {
        Arc::new(AutomationPresentationPermit {
            identity,
            request_authority: request,
            session_authority: session,
            outcome: Mutex::new(NativePresentationCommitOutcome::Pending),
        })
    }

    fn record_presented_effects(
        marker: &mut NativePresentationCommitMarker<'_>,
        effects: &Mutex<PresentedEffects>,
        later_mutations: u64,
    ) {
        assert!(marker.mark_native_committed());
        marker
            .commit_shell_state(|| {
                *effects.lock().unwrap() = PresentedEffects {
                    receipts: 1,
                    compositions: 1,
                    identity_updates: 1,
                    pointer_updates: 1,
                    surface_updates: 1,
                    later_mutations,
                };
            })
            .unwrap();
    }

    #[test]
    fn random_session_ids_are_uuid_shaped_and_distinct() {
        let first = random_session_id().unwrap();
        let second = random_session_id().unwrap();
        assert_eq!(first.len(), 32);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);
        assert_eq!(&first[12..13], "4");
        assert!(matches!(&first[16..17], "8" | "9" | "a" | "b"));
    }

    #[test]
    fn resolved_capabilities_report_exact_bounded_page_load_budget_without_bidi() {
        let capabilities = resolved_capabilities(&Capabilities::new(), 30_000).unwrap();
        let capabilities = capabilities.as_object().unwrap();
        assert!(!capabilities.contains_key("webSocketUrl"));
        assert_eq!(capabilities.get("setWindowRect"), Some(&Value::Bool(false)));
        assert_eq!(
            capabilities
                .get("timeouts")
                .and_then(Value::as_object)
                .and_then(|timeouts| timeouts.get("pageLoad")),
            Some(&json!(30_000))
        );

        let hard_limit = resolved_capabilities(&Capabilities::new(), 120_000).unwrap();
        assert_eq!(
            hard_limit
                .get("timeouts")
                .and_then(Value::as_object)
                .and_then(|timeouts| timeouts.get("pageLoad")),
            Some(&json!(120_000))
        );
    }

    #[test]
    fn png_encoder_preserves_dimensions_top_left_colors_and_checksums() {
        let capture = FakeScreenshotCapture::new(
            test_screenshot_frame(1),
            2,
            2,
            vec![
                0, 0, 255, 255, 0, 255, 0, 128, 255, 0, 0, 255, 255, 255, 255, 64,
            ],
        );
        let encoded =
            encode_screenshot_png_base64(&capture, ScreenshotEncodingLimits::PRODUCT).unwrap();
        let png = decode_base64(&encoded);
        let (width, height, raw) = parse_png(&png);
        assert_eq!((width, height), (2, 2));
        assert_eq!(
            raw,
            vec![
                0, 255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 0, 255, 255, 255, 255, 255, 64,
            ]
        );
        assert!(
            encode_screenshot_png_base64(
                &capture,
                ScreenshotEncodingLimits {
                    pixels: 3,
                    ..ScreenshotEncodingLimits::PRODUCT
                }
            )
            .is_err()
        );
        assert!(
            encode_screenshot_png_base64(
                &capture,
                ScreenshotEncodingLimits {
                    png_bytes: 32,
                    ..ScreenshotEncodingLimits::PRODUCT
                }
            )
            .is_err()
        );
        assert!(
            encode_screenshot_png_base64(
                &capture,
                ScreenshotEncodingLimits {
                    base64_bytes: 4,
                    ..ScreenshotEncodingLimits::PRODUCT
                }
            )
            .is_err()
        );
        assert_eq!(
            AutomationIngress::webdriver_error(AutomationFailure::new(
                AutomationFailureKind::UnableToCaptureScreen,
                "test capture failure",
            ))
            .error,
            ErrorStatus::UnableToCaptureScreen
        );
    }

    #[test]
    fn screenshot_transaction_rejects_foreign_authority_receipt_and_replay_then_recovers() {
        let (send, receive) = sync_channel(MAX_OWNER_QUEUE_DEPTH);
        let mut owner = AutomationOwner::new(receive);
        let (mut session, _) = session();
        let window = BrowserWindowId::new(1).unwrap();
        let session_id = create_test_session(&send, &mut owner, &mut session, window, 1, 1);
        let (screenshot, result) =
            queued(2, 1, Some(&session_id), AutomationCommand::TakeScreenshot);
        send.send(screenshot).unwrap();
        let drain = owner.drain(&mut session, window);
        assert!(drain.screenshot_requested);
        assert!(owner.has_pending_screenshot());
        assert!(result.try_recv().is_err());

        let frame = test_screenshot_frame(10);
        let request = owner.claim_screenshot_frame(frame).unwrap();
        assert!(owner.claim_screenshot_frame(frame).is_none());
        let capture = FakeScreenshotCapture::new(frame, 2, 1, vec![0, 0, 255, 255, 0, 255, 0, 255]);

        let wrong_generation = AutomationScreenshotCaptureRequest {
            generation: AutomationSessionGeneration(NonZeroU64::new(2).unwrap()),
            ..request
        };
        assert!(!owner.complete_screenshot_source(
            wrong_generation,
            &capture,
            ScreenshotEncodingLimits::PRODUCT
        ));
        let wrong_request = AutomationScreenshotCaptureRequest {
            request: AutomationRequestId(NonZeroU64::new(99).unwrap()),
            ..request
        };
        assert!(!owner.complete_screenshot_source(
            wrong_request,
            &capture,
            ScreenshotEncodingLimits::PRODUCT
        ));
        let foreign_capture =
            FakeScreenshotCapture::new(test_screenshot_frame(11), 2, 1, vec![0; 8]);
        assert!(!owner.complete_screenshot_source(
            request,
            &foreign_capture,
            ScreenshotEncodingLimits::PRODUCT
        ));
        assert!(owner.has_pending_screenshot());
        assert!(owner.complete_screenshot_source(
            request,
            &capture,
            ScreenshotEncodingLimits::PRODUCT
        ));
        let encoded = match result.recv().unwrap().result {
            Ok(AutomationValue::Screenshot(encoded)) => encoded,
            other => panic!("unexpected screenshot result: {other:?}"),
        };
        assert_eq!(parse_png(&decode_base64(&encoded)).0, 2);
        assert!(!owner.complete_screenshot_source(
            request,
            &capture,
            ScreenshotEncodingLimits::PRODUCT
        ));
        assert!(!owner.has_pending_screenshot());

        let (recovery, recovery_result) =
            queued(3, 1, Some(&session_id), AutomationCommand::TakeScreenshot);
        send.send(recovery).unwrap();
        assert!(owner.drain(&mut session, window).screenshot_requested);
        let recovery_frame = test_screenshot_frame(12);
        let recovery_request = owner.claim_screenshot_frame(recovery_frame).unwrap();
        let recovery_capture =
            FakeScreenshotCapture::new(recovery_frame, 1, 1, vec![255, 0, 0, 255]);
        assert!(owner.complete_screenshot_source(
            recovery_request,
            &recovery_capture,
            ScreenshotEncodingLimits::PRODUCT
        ));
        assert!(matches!(
            recovery_result.recv().unwrap().result,
            Ok(AutomationValue::Screenshot(_))
        ));
        owner.shutdown(&mut session);
        let _ = session.shutdown();
    }

    #[test]
    fn screenshot_collision_bounds_and_native_failure_release_for_recovery() {
        let (send, receive) = sync_channel(MAX_OWNER_QUEUE_DEPTH);
        let mut owner = AutomationOwner::new(receive);
        let (mut session, _) = session();
        let window = BrowserWindowId::new(1).unwrap();
        let session_id = create_test_session(&send, &mut owner, &mut session, window, 1, 1);
        let (first, first_result) =
            queued(2, 1, Some(&session_id), AutomationCommand::TakeScreenshot);
        let (collision, collision_result) =
            queued(3, 1, Some(&session_id), AutomationCommand::TakeScreenshot);
        send.send(first).unwrap();
        send.send(collision).unwrap();
        owner.drain(&mut session, window);
        assert!(matches!(
            collision_result.recv().unwrap().result,
            Err(AutomationFailure {
                kind: AutomationFailureKind::UnableToCaptureScreen,
                ..
            })
        ));
        let frame = test_screenshot_frame(20);
        let request = owner.claim_screenshot_frame(frame).unwrap();
        let capture = FakeScreenshotCapture::new(frame, 2, 2, vec![0; 16]);
        assert!(owner.complete_screenshot_source(
            request,
            &capture,
            ScreenshotEncodingLimits {
                pixels: 3,
                png_bytes: 1_024,
                base64_bytes: 1_024,
            }
        ));
        assert!(matches!(
            first_result.recv().unwrap().result,
            Err(AutomationFailure {
                kind: AutomationFailureKind::UnableToCaptureScreen,
                ..
            })
        ));

        let (native_failure, native_failure_result) =
            queued(4, 1, Some(&session_id), AutomationCommand::TakeScreenshot);
        send.send(native_failure).unwrap();
        owner.drain(&mut session, window);
        let failed_request = owner
            .claim_screenshot_frame(test_screenshot_frame(21))
            .unwrap();
        assert!(owner.fail_screenshot_capture(failed_request));
        assert!(matches!(
            native_failure_result.recv().unwrap().result,
            Err(AutomationFailure {
                kind: AutomationFailureKind::UnableToCaptureScreen,
                ..
            })
        ));

        let (recovery, recovery_result) =
            queued(5, 1, Some(&session_id), AutomationCommand::TakeScreenshot);
        send.send(recovery).unwrap();
        owner.drain(&mut session, window);
        let recovery_frame = test_screenshot_frame(22);
        let recovery_request = owner.claim_screenshot_frame(recovery_frame).unwrap();
        let recovery_capture = FakeScreenshotCapture::new(recovery_frame, 1, 1, vec![0, 0, 0, 255]);
        assert!(owner.complete_screenshot_source(
            recovery_request,
            &recovery_capture,
            ScreenshotEncodingLimits::PRODUCT
        ));
        assert!(matches!(
            recovery_result.recv().unwrap().result,
            Ok(AutomationValue::Screenshot(_))
        ));
        owner.shutdown(&mut session);
        let _ = session.shutdown();
    }

    #[test]
    fn screenshot_timeout_revoke_delete_and_shutdown_cancel_exact_pending_requests() {
        for terminal in 0..4 {
            let (send, receive) = sync_channel(MAX_OWNER_QUEUE_DEPTH);
            let mut owner = AutomationOwner::new(receive);
            let (mut session, _) = session();
            let window = BrowserWindowId::new(1).unwrap();
            let session_id = create_test_session(&send, &mut owner, &mut session, window, 1, 1);
            let lifetime = DispatchLifetime::new(Instant::now() + Duration::from_secs(1));
            let (screenshot, result) = queued_with_lifetime(
                2,
                1,
                Some(&session_id),
                AutomationCommand::TakeScreenshot,
                lifetime.clone(),
            );
            send.send(screenshot).unwrap();
            owner.drain(&mut session, window);
            assert!(owner.has_pending_screenshot());
            match terminal {
                0 => {
                    lifetime.revoke();
                    owner.observe_session(&mut session);
                }
                1 => {
                    owner.active.as_ref().unwrap().authority.revoke();
                    owner.observe_session(&mut session);
                }
                2 => {
                    let (delete, delete_result) =
                        queued(3, 1, Some(&session_id), AutomationCommand::DeleteSession);
                    send.send(delete).unwrap();
                    owner.drain(&mut session, window);
                    assert!(matches!(
                        delete_result.recv().unwrap().result,
                        Ok(AutomationValue::DeleteSession)
                    ));
                }
                3 => owner.shutdown(&mut session),
                _ => unreachable!(),
            }
            let failure = result.recv().unwrap().result.unwrap_err();
            if terminal == 0 {
                assert_eq!(failure.kind, AutomationFailureKind::Timeout);
            } else if terminal == 3 {
                assert_eq!(failure.kind, AutomationFailureKind::Unknown);
            } else {
                assert_eq!(failure.kind, AutomationFailureKind::InvalidSession);
            }
            assert!(!owner.has_pending_screenshot());
            owner.shutdown(&mut session);
            let _ = session.shutdown();
        }
    }

    #[test]
    fn cancellation_wins_at_the_pre_submission_barrier_without_shell_effects() {
        let identity = test_presentation_identity();
        let cancelled_request = DispatchLifetime::new(Instant::now() + Duration::from_secs(2));
        let live_session = DispatchLifetime::new(Instant::now() + Duration::from_secs(2));
        let cancelled_permit =
            presentation_permit(identity, cancelled_request.clone(), live_session.clone());
        let effects = Arc::new(Mutex::new(PresentedEffects::default()));
        let (before_submit_send, before_submit_recv) = channel();
        let (attempt_submit_send, attempt_submit_recv) = channel();
        let waiting_permit = Arc::clone(&cancelled_permit);
        let cancelled_effects = Arc::clone(&effects);
        let cancelled_submit = thread::spawn(move || {
            before_submit_send.send(()).unwrap();
            attempt_submit_recv
                .recv_timeout(Duration::from_secs(1))
                .unwrap();
            waiting_permit.commit(identity, |marker| {
                assert!(marker.begin_submission());
                record_presented_effects(marker, &cancelled_effects, 1);
            })
        });
        before_submit_recv
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(cancelled_request.cancel());
        attempt_submit_send.send(()).unwrap();
        assert!(cancelled_submit.join().unwrap().is_none());
        assert_eq!(
            cancelled_permit.outcome(),
            NativePresentationCommitOutcome::Cancelled
        );
        assert_eq!(*effects.lock().unwrap(), PresentedEffects::default());

        let session_cancelled = presentation_permit(
            identity,
            DispatchLifetime::new(Instant::now() + Duration::from_secs(2)),
            live_session.clone(),
        );
        live_session.revoke();
        assert!(session_cancelled.commit(identity, |_| ()).is_none());
        assert_eq!(
            session_cancelled.outcome(),
            NativePresentationCommitOutcome::Cancelled
        );
        assert_eq!(*effects.lock().unwrap(), PresentedEffects::default());
    }

    #[test]
    fn native_submission_wins_during_the_barrier_and_seals_late_mutation() {
        let identity = test_presentation_identity();
        let effects = Arc::new(Mutex::new(PresentedEffects::default()));
        let request = DispatchLifetime::new(Instant::now() + Duration::from_secs(2));
        let session = DispatchLifetime::new(Instant::now() + Duration::from_secs(2));
        let permit = presentation_permit(identity, request.clone(), session);
        let (entered_send, entered_recv) = channel();
        let (release_send, release_recv) = channel();
        let submitted_permit = Arc::clone(&permit);
        let submitted_effects = Arc::clone(&effects);
        let submit = thread::spawn(move || {
            submitted_permit.commit(identity, |marker| {
                assert!(marker.begin_submission());
                entered_send.send(()).unwrap();
                release_recv.recv_timeout(Duration::from_secs(1)).unwrap();
                record_presented_effects(marker, &submitted_effects, 0);
                "native-commit-won"
            })
        });
        entered_recv.recv_timeout(Duration::from_secs(1)).unwrap();
        let cancel_request = request.clone();
        let (cancelled_send, cancelled_recv) = channel();
        let cancel = thread::spawn(move || {
            cancelled_send.send(cancel_request.cancel()).unwrap();
        });
        assert!(
            cancelled_recv
                .recv_timeout(Duration::from_millis(20))
                .is_err(),
            "cancellation must serialize behind an in-progress native commit"
        );
        release_send.send(()).unwrap();
        assert_eq!(submit.join().unwrap(), Some("native-commit-won"));
        assert_eq!(
            permit.outcome(),
            NativePresentationCommitOutcome::NativeCommitted
        );
        assert!(cancelled_recv.recv_timeout(Duration::from_secs(1)).unwrap());
        cancel.join().unwrap();
        let committed = *effects.lock().unwrap();
        assert_eq!(committed.receipts, 1);
        assert_eq!(committed.compositions, 1);
        assert_eq!(committed.identity_updates, 1);
        assert_eq!(committed.pointer_updates, 1);
        assert_eq!(committed.surface_updates, 1);

        let late_effects = Arc::clone(&effects);
        assert!(
            permit
                .commit(identity, |_| {
                    late_effects.lock().unwrap().later_mutations += 1;
                })
                .is_none()
        );
        assert_eq!(*effects.lock().unwrap(), committed);
        assert_eq!(
            permit.outcome(),
            NativePresentationCommitOutcome::NativeCommitted
        );
    }

    #[test]
    fn queue_full_teardown_revokes_shared_authority_and_fresh_session_recovers() {
        let (send, receive) = sync_channel(1);
        let wake = Arc::new(TestWake::default());
        let mut ingress = test_ingress(send.clone(), Arc::clone(&wake));
        let mut owner = AutomationOwner::new(receive);
        let generation = AutomationSessionGeneration(NonZeroU64::new(1).unwrap());
        let cancelled =
            install_active_authority(&mut ingress, &mut owner, generation, "active-session");
        let (filler, filler_result) = queued(
            1,
            1,
            Some("active-session"),
            AutomationCommand::GetCurrentUrl,
        );
        send.try_send(filler).unwrap();

        ingress.teardown_session(SessionTeardownKind::NotDeleted);
        assert!(cancelled.is_cancelled());
        assert!(ingress.active_generation.is_none());
        assert!(ingress.active_session_id.is_none());
        assert!(ingress.active_authority.is_none());
        assert!(!ingress.closed);
        assert_eq!(wake.wakes.load(Ordering::Acquire), 1);
        ingress.validate_dispatcher_session(None).unwrap();
        let fresh_generation = ingress.allocate_generation().unwrap();
        assert_eq!(fresh_generation.0.get(), 2);

        let (mut session, _) = session();
        let window = BrowserWindowId::new(1).unwrap();
        owner.drain(&mut session, window);
        assert!(owner.active.is_none());
        assert!(matches!(
            filler_result.recv().unwrap().result,
            Err(AutomationFailure {
                kind: AutomationFailureKind::InvalidSession,
                ..
            })
        ));

        let (fresh, fresh_result) =
            queued(3, fresh_generation.0.get(), None, new_session_command());
        send.try_send(fresh).unwrap();
        owner.drain(&mut session, window);
        assert!(matches!(
            fresh_result.recv().unwrap().result,
            Ok(AutomationValue::NewSession { .. })
        ));
        owner.shutdown(&mut session);
        let _ = session.shutdown();
    }

    #[test]
    fn disconnected_revoke_still_cancels_exact_authority_and_closes_ingress() {
        let (send, receive) = sync_channel(1);
        drop(receive);
        let wake = Arc::new(TestWake::default());
        let mut ingress = test_ingress(send, Arc::clone(&wake));
        let cancelled = DispatchLifetime::new(Instant::now() + Duration::from_mins(1));
        ingress.active_generation = Some(AutomationSessionGeneration(NonZeroU64::new(1).unwrap()));
        ingress.active_session_id = Some("disconnected-session".into());
        ingress.active_authority = Some(cancelled.clone());

        ingress.teardown_session(SessionTeardownKind::NotDeleted);
        assert!(cancelled.is_cancelled());
        assert!(ingress.closed);
        assert!(ingress.active_generation.is_none());
        assert!(ingress.active_session_id.is_none());
        assert!(ingress.active_authority.is_none());
        assert_eq!(wake.wakes.load(Ordering::Acquire), 1);
    }

    #[test]
    fn command_send_disconnected_branch_revokes_and_clears_exact_ingress_authority() {
        let (send, receive) = sync_channel(1);
        drop(receive);
        let wake = Arc::new(TestWake::default());
        let mut ingress = test_ingress(send, Arc::clone(&wake));
        let authority = DispatchLifetime::new(Instant::now() + Duration::from_mins(1));
        ingress.active_generation = Some(AutomationSessionGeneration(NonZeroU64::new(1).unwrap()));
        ingress.active_session_id = Some("active-disconnected-session".into());
        ingress.active_authority = Some(authority.clone());
        let dispatcher_session = Some(Session {
            id: "active-disconnected-session".to_owned(),
        });
        let command_lifetime = DispatchLifetime::new(Instant::now() + Duration::from_secs(1));

        let error = ingress
            .handle_command_with_lifetime(
                &dispatcher_session,
                WebDriverMessage {
                    session_id: Some("active-disconnected-session".to_owned()),
                    command: WebDriverCommand::GetCurrentUrl,
                },
                &command_lifetime,
            )
            .unwrap_err();

        assert_eq!(error.error, ErrorStatus::UnknownError);
        assert!(authority.is_cancelled());
        assert!(ingress.active_generation.is_none());
        assert!(ingress.active_session_id.is_none());
        assert!(ingress.active_authority.is_none());
        assert!(ingress.closed);
        assert_eq!(wake.wakes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn disconnected_new_session_result_retires_owner_and_next_generation_recovers() {
        let (send, receive) = sync_channel(1);
        let mut owner = AutomationOwner::new(receive);
        let (mut session, _) = session();
        let window = BrowserWindowId::new(1).unwrap();
        let (lost, lost_result) = queued(1, 1, None, new_session_command());
        drop(lost_result);
        send.try_send(lost).unwrap();
        owner.drain(&mut session, window);
        assert!(owner.active.is_none());

        let (fresh, fresh_result) = queued(2, 2, None, new_session_command());
        send.try_send(fresh).unwrap();
        owner.drain(&mut session, window);
        assert!(matches!(
            fresh_result.recv().unwrap().result,
            Ok(AutomationValue::NewSession { .. })
        ));
        owner.shutdown(&mut session);
        let _ = session.shutdown();
    }

    #[test]
    fn expired_queued_session_never_mutates_owner_and_fresh_generation_recovers() {
        let (send, receive) = sync_channel(1);
        let mut owner = AutomationOwner::new(receive);
        let (mut session, _) = session();
        let window = BrowserWindowId::new(1).unwrap();
        let expired_lifetime = DispatchLifetime::new(Instant::now());
        let (expired, expired_result) =
            queued_with_lifetime(1, 1, None, new_session_command(), expired_lifetime.clone());
        send.try_send(expired).unwrap();
        owner.drain(&mut session, window);
        assert!(expired_lifetime.is_cancelled());
        assert!(owner.active.is_none());
        assert!(matches!(
            expired_result.recv().unwrap().result,
            Err(AutomationFailure {
                kind: AutomationFailureKind::Timeout,
                ..
            })
        ));

        let (fresh, fresh_result) = queued(2, 2, None, new_session_command());
        send.try_send(fresh).unwrap();
        owner.drain(&mut session, window);
        assert!(matches!(
            fresh_result.recv().unwrap().result,
            Ok(AutomationValue::NewSession { .. })
        ));
        owner.shutdown(&mut session);
        let _ = session.shutdown();
    }

    #[test]
    fn cancelled_navigation_authorities_block_late_composition_and_allow_recovery() {
        let (send, receive) = sync_channel(MAX_OWNER_QUEUE_DEPTH);
        let mut owner = AutomationOwner::new(receive);
        let (mut session, handle) = session();
        let window = BrowserWindowId::new(1).unwrap();
        let session_authority = DispatchLifetime::new(Instant::now() + Duration::from_mins(1));
        let (new_session, new_session_result) =
            queued_with_lifetime(1, 1, None, new_session_command(), session_authority.clone());
        send.try_send(new_session).unwrap();
        owner.drain(&mut session, window);
        let session_id = match new_session_result.recv().unwrap().result {
            Ok(AutomationValue::NewSession { session_id, .. }) => session_id,
            other => panic!("unexpected new-session result: {other:?}"),
        };

        let navigation_lifetime = DispatchLifetime::new(Instant::now() + Duration::from_mins(1));
        let (navigate, navigation_result) = queued_with_lifetime(
            2,
            1,
            Some(&session_id),
            AutomationCommand::Navigate("http://cancelled.invalid/".into()),
            navigation_lifetime.clone(),
        );
        send.try_send(navigate).unwrap();
        assert!(owner.drain(&mut session, window).navigation_started);
        let navigation = handle.navigation();
        navigation_lifetime.revoke();
        session_authority.revoke();
        owner.observe_session(&mut session);
        assert!(owner.active.is_none());
        assert!(!owner.has_pending_navigation());
        assert_eq!(handle.cancellations(), vec![navigation]);
        assert!(matches!(
            navigation_result.recv().unwrap().result,
            Err(AutomationFailure {
                kind: AutomationFailureKind::InvalidSession,
                ..
            })
        ));

        handle.complete(navigation, "http://late.invalid/");
        session.pump_engine(32).unwrap();
        owner.observe_session(&mut session);
        owner.observe_composition(
            &session,
            presentation_identity(BrowserTabId::new(1).unwrap(), navigation),
        );
        assert!(!owner.has_pending_navigation());

        let (fresh, fresh_result) = queued(3, 2, None, new_session_command());
        send.try_send(fresh).unwrap();
        owner.drain(&mut session, window);
        assert!(matches!(
            fresh_result.recv().unwrap().result,
            Ok(AutomationValue::NewSession { .. })
        ));
        owner.shutdown(&mut session);
        let _ = session.shutdown();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn owner_correlates_session_navigation_composition_and_committed_url() {
        let (send, receive) = sync_channel(MAX_OWNER_QUEUE_DEPTH);
        let mut owner = AutomationOwner::new(receive);
        let (mut session, handle) = session();
        let window = BrowserWindowId::new(1).unwrap();

        let (status, status_result) = queued(1, 1, None, AutomationCommand::Status);
        send.send(status).unwrap();
        owner.drain(&mut session, window);
        assert!(matches!(
            status_result.recv().unwrap().result,
            Ok(AutomationValue::Status { ready: true, .. })
        ));

        let parameters: NewSessionParameters = serde_json::from_value(json!({
            "capabilities": {}
        }))
        .unwrap();
        let (new_session, new_session_result) = queued(
            2,
            2,
            None,
            AutomationCommand::NewSession {
                parameters,
                page_load_timeout_ms: 30_000,
            },
        );
        send.send(new_session).unwrap();
        owner.drain(&mut session, window);
        let session_id = match new_session_result.recv().unwrap().result {
            Ok(AutomationValue::NewSession {
                session_id,
                capabilities,
            }) => {
                assert!(capabilities.get("webSocketUrl").is_none());
                session_id
            }
            other => panic!("unexpected new-session result: {other:?}"),
        };

        let (navigate, navigation_result) = queued(
            3,
            2,
            Some(&session_id),
            AutomationCommand::Navigate("http://requested.invalid/".into()),
        );
        send.send(navigate).unwrap();
        let drain = owner.drain(&mut session, window);
        assert!(drain.navigation_started);
        assert!(navigation_result.try_recv().is_err());
        let navigation = handle.navigation();
        let final_url = "http://final.invalid/";
        handle.complete(navigation, final_url);
        session.pump_engine(32).unwrap();
        owner.observe_session(&mut session);
        let tab = BrowserTabId::new(1).unwrap();
        assert!(navigation_result.try_recv().is_err());
        let foreign = NavigationId::new(
            TopLevelContextId::new(99).unwrap(),
            NavigationGeneration::INITIAL,
        );
        owner.observe_composition(&session, presentation_identity(tab, foreign));
        assert!(navigation_result.try_recv().is_err());
        owner.observe_composition(&session, presentation_identity(tab, navigation));
        assert!(matches!(
            navigation_result.recv().unwrap().result,
            Ok(AutomationValue::NavigationComplete)
        ));

        session.address_mut(tab).unwrap().insert("draft").unwrap();
        let (current, current_result) =
            queued(4, 2, Some(&session_id), AutomationCommand::GetCurrentUrl);
        send.send(current).unwrap();
        owner.drain(&mut session, window);
        assert!(matches!(
            current_result.recv().unwrap().result,
            Ok(AutomationValue::CurrentUrl(url)) if url.as_ref() == final_url
        ));

        let (stale_generation, stale_generation_result) =
            queued(5, 1, Some(&session_id), AutomationCommand::GetCurrentUrl);
        send.send(stale_generation).unwrap();
        owner.drain(&mut session, window);
        assert!(matches!(
            stale_generation_result.recv().unwrap().result,
            Err(AutomationFailure {
                kind: AutomationFailureKind::InvalidSession,
                ..
            })
        ));

        for (request, kind) in [
            (6, UnsupportedCommand::Title),
            (7, UnsupportedCommand::Other),
        ] {
            let (unsupported, unsupported_result) = queued(
                request,
                2,
                Some(&session_id),
                AutomationCommand::Unsupported(kind),
            );
            send.send(unsupported).unwrap();
            owner.drain(&mut session, window);
            assert!(matches!(
                unsupported_result.recv().unwrap().result,
                Err(AutomationFailure {
                    kind: AutomationFailureKind::UnsupportedOperation,
                    ..
                })
            ));
        }

        let (stale, stale_result) =
            queued(7, 2, Some(&session_id), AutomationCommand::GetCurrentUrl);
        send.send(stale).unwrap();
        owner.drain(&mut session, window);
        assert!(matches!(
            stale_result.recv().unwrap().result,
            Err(AutomationFailure {
                kind: AutomationFailureKind::Unknown,
                ..
            })
        ));

        let (delete, delete_result) =
            queued(8, 2, Some(&session_id), AutomationCommand::DeleteSession);
        send.send(delete).unwrap();
        owner.drain(&mut session, window);
        assert!(matches!(
            delete_result.recv().unwrap().result,
            Ok(AutomationValue::DeleteSession)
        ));
        let (ready, ready_result) = queued(9, 3, None, AutomationCommand::Status);
        send.send(ready).unwrap();
        owner.drain(&mut session, window);
        assert!(matches!(
            ready_result.recv().unwrap().result,
            Ok(AutomationValue::Status { ready: true, .. })
        ));
        let _ = session.shutdown();
    }

    #[test]
    fn owner_shutdown_resolves_a_pending_navigation_without_a_receipt() {
        let (send, receive) = sync_channel(MAX_OWNER_QUEUE_DEPTH);
        let mut owner = AutomationOwner::new(receive);
        let (mut session, _) = session();
        let window = BrowserWindowId::new(1).unwrap();
        let parameters: NewSessionParameters = serde_json::from_value(json!({
            "capabilities": {}
        }))
        .unwrap();
        let (new_session, new_session_result) = queued(
            1,
            1,
            None,
            AutomationCommand::NewSession {
                parameters,
                page_load_timeout_ms: 30_000,
            },
        );
        send.send(new_session).unwrap();
        owner.drain(&mut session, window);
        let session_id = match new_session_result.recv().unwrap().result {
            Ok(AutomationValue::NewSession { session_id, .. }) => session_id,
            other => panic!("unexpected new-session result: {other:?}"),
        };
        let (navigate, navigation_result) = queued(
            2,
            1,
            Some(&session_id),
            AutomationCommand::Navigate("http://pending.invalid/".into()),
        );
        send.send(navigate).unwrap();
        owner.drain(&mut session, window);
        owner.shutdown(&mut session);
        assert!(matches!(
            navigation_result.recv().unwrap().result,
            Err(AutomationFailure {
                kind: AutomationFailureKind::Unknown,
                ..
            })
        ));
        let _ = session.shutdown();
    }
}
