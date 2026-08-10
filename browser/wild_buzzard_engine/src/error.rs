use std::fmt;

use wild_buzzard_dom::DomError;
use wild_buzzard_headless::HeadlessError;
use wild_buzzard_html::ParserStateError;
use wild_buzzard_layout::LayoutError;
use wild_buzzard_net::Error as NetworkError;
use wild_buzzard_renderer::SceneBuildError;
use wild_buzzard_stylo_adapter::StyleAdapterError;
use wild_buzzard_text::TextError;

/// Observable processing stage associated with cancellation or a deadline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PipelineStage {
    /// Target validation, connection, HTTP parsing, or body streaming.
    Fetch,
    /// UTF-8 validation and HTML tokenization/tree construction.
    Parse,
    /// Immutable DOM snapshot publication.
    Snapshot,
    /// Stylo parsing, selector matching, cascade, and computed values.
    Style,
    /// Block/inline layout using shaped text metrics.
    Layout,
    /// Layout validation and `WebRender` display-list compilation.
    SceneCompilation,
    /// Font selection, Unicode analysis, and glyph shaping.
    TextShaping,
    /// One composed `WebRender` submission and RGBA8 readback.
    ComposedRender,
}

/// Stable reason a redirect target could not be admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RedirectLocationFailure {
    /// The redirect response omitted `Location`.
    Missing,
    /// More than one `Location` field made the target ambiguous.
    Multiple,
    /// The field was not valid UTF-8 and cannot enter the WHATWG URL parser.
    NonUtf8,
    /// The field did not resolve to a valid WHATWG URL.
    Invalid,
    /// The resolved authority contained a username or password.
    CredentialsNotAllowed,
    /// The resolved target was not HTTP or HTTPS.
    UnsupportedScheme,
    /// The normalized final URL exceeded the browser navigation bound.
    UrlTooLong,
}

impl fmt::Display for PipelineStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Fetch => "fetch",
            Self::Parse => "HTML parse",
            Self::Snapshot => "DOM snapshot",
            Self::Style => "Stylo style preparation",
            Self::Layout => "layout",
            Self::SceneCompilation => "scene compilation",
            Self::TextShaping => "text shaping",
            Self::ComposedRender => "composed rendering",
        };
        formatter.write_str(name)
    }
}

/// Structured failure from one static-page pipeline operation.
#[derive(Debug)]
pub enum PipelineError {
    /// A configuration field is invalid before any external work begins.
    InvalidConfiguration {
        /// Stable field name.
        field: &'static str,
        /// Why the value cannot be accepted.
        detail: &'static str,
    },
    /// The caller cancelled at the named safe checkpoint.
    Cancelled {
        /// Last pipeline stage reached.
        stage: PipelineStage,
    },
    /// The operation deadline elapsed at the named safe checkpoint.
    DeadlineExceeded {
        /// Last pipeline stage reached.
        stage: PipelineStage,
    },
    /// An absolute deadline could not be represented by the monotonic clock.
    DeadlineOverflow,
    /// The selected HTTP transport rejected or failed the request.
    Network(NetworkError),
    /// A redirect target was missing, ambiguous, malformed, or prohibited.
    RedirectLocation(RedirectLocationFailure),
    /// A 3xx status has semantics outside the admitted top-level GET set.
    UnsupportedRedirectStatus {
        /// Unsupported status returned by the server.
        status: u16,
    },
    /// The normalized redirect chain repeated a URL.
    RedirectLoop,
    /// The redirect chain exceeded its hard ordinary-redirect bound.
    TooManyRedirects {
        /// Maximum redirects admitted before the final response.
        maximum: u8,
    },
    /// Transport security evidence contradicted the exact requested scheme.
    TransportSecurityMismatch,
    /// A non-success HTTP response was returned.
    HttpStatus(u16),
    /// The bounded parser currently accepts only UTF-8 document bytes.
    NonUtf8Html,
    /// HTML tokenization or tree construction failed.
    Html(ParserStateError),
    /// Immutable DOM snapshot validation failed.
    Dom(DomError),
    /// Imported Stylo or its immutable adapter rejected the document.
    Style(StyleAdapterError),
    /// Layout rejected the exact DOM/style publication.
    Layout(LayoutError),
    /// Rust font selection or shaping failed.
    Text(TextError),
    /// The renderer scene contract rejected layout output.
    Scene(SceneBuildError),
    /// Linux EGL, `WebRender`, readback, or shutdown failed.
    Headless(HeadlessError),
    /// A bounded evidence counter overflowed.
    EvidenceOverflow,
    /// No additional valid `WebRender` epoch can be allocated.
    EpochExhausted,
    /// No additional presentation-scene revision can be allocated.
    PresentationRevisionExhausted,
}

impl fmt::Display for PipelineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration { field, detail } => {
                write!(formatter, "invalid {field}: {detail}")
            }
            Self::Cancelled { stage } => write!(formatter, "cancelled during {stage}"),
            Self::DeadlineExceeded { stage } => {
                write!(formatter, "operation deadline exceeded during {stage}")
            }
            Self::DeadlineOverflow => {
                formatter.write_str("operation deadline exceeds the monotonic clock range")
            }
            Self::Network(error) => write!(formatter, "HTTP transport failed: {error}"),
            Self::RedirectLocation(failure) => {
                write!(formatter, "HTTP redirect target was rejected: {failure:?}")
            }
            Self::UnsupportedRedirectStatus { status } => {
                write!(
                    formatter,
                    "HTTP status {status} has unsupported redirect semantics"
                )
            }
            Self::RedirectLoop => formatter.write_str("HTTP redirect loop detected"),
            Self::TooManyRedirects { maximum } => {
                write!(formatter, "HTTP redirect chain exceeded {maximum} hops")
            }
            Self::TransportSecurityMismatch => formatter.write_str(
                "HTTP transport security evidence contradicted the requested URL scheme",
            ),
            Self::HttpStatus(status) => {
                write!(formatter, "HTTP returned status {status}")
            }
            Self::NonUtf8Html => formatter.write_str(
                "document bytes are not UTF-8; HTML encoding sniffing is not integrated yet",
            ),
            Self::Html(error) => write!(formatter, "HTML parsing failed: {error}"),
            Self::Dom(error) => write!(formatter, "DOM snapshot failed: {error}"),
            Self::Style(error) => write!(formatter, "Stylo preparation failed: {error}"),
            Self::Layout(error) => write!(formatter, "layout failed: {error}"),
            Self::Text(error) => write!(formatter, "text shaping failed: {error}"),
            Self::Scene(error) => write!(formatter, "scene compilation failed: {error}"),
            Self::Headless(error) => write!(formatter, "headless WebRender failed: {error}"),
            Self::EvidenceOverflow => formatter.write_str("pipeline evidence counter overflowed"),
            Self::EpochExhausted => formatter.write_str("WebRender epoch space is exhausted"),
            Self::PresentationRevisionExhausted => {
                formatter.write_str("presentation scene revision space is exhausted")
            }
        }
    }
}

impl std::error::Error for PipelineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Network(error) => Some(error),
            Self::Html(error) => Some(error),
            Self::Dom(error) => Some(error),
            Self::Style(error) => Some(error),
            Self::Layout(error) => Some(error),
            Self::Text(error) => Some(error),
            Self::Scene(error) => Some(error),
            Self::Headless(error) => Some(error),
            Self::InvalidConfiguration { .. }
            | Self::Cancelled { .. }
            | Self::DeadlineExceeded { .. }
            | Self::DeadlineOverflow
            | Self::RedirectLocation(_)
            | Self::UnsupportedRedirectStatus { .. }
            | Self::RedirectLoop
            | Self::TooManyRedirects { .. }
            | Self::TransportSecurityMismatch
            | Self::HttpStatus(_)
            | Self::NonUtf8Html
            | Self::EvidenceOverflow
            | Self::EpochExhausted
            | Self::PresentationRevisionExhausted => None,
        }
    }
}

impl From<NetworkError> for PipelineError {
    fn from(error: NetworkError) -> Self {
        Self::Network(error)
    }
}

impl From<ParserStateError> for PipelineError {
    fn from(error: ParserStateError) -> Self {
        Self::Html(error)
    }
}

impl From<DomError> for PipelineError {
    fn from(error: DomError) -> Self {
        Self::Dom(error)
    }
}

impl From<StyleAdapterError> for PipelineError {
    fn from(error: StyleAdapterError) -> Self {
        Self::Style(error)
    }
}

impl From<LayoutError> for PipelineError {
    fn from(error: LayoutError) -> Self {
        Self::Layout(error)
    }
}

impl From<TextError> for PipelineError {
    fn from(error: TextError) -> Self {
        Self::Text(error)
    }
}

impl From<SceneBuildError> for PipelineError {
    fn from(error: SceneBuildError) -> Self {
        Self::Scene(error)
    }
}

impl From<HeadlessError> for PipelineError {
    fn from(error: HeadlessError) -> Self {
        Self::Headless(error)
    }
}
