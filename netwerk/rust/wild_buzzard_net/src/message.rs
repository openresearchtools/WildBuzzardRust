// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::time::Instant;

use crate::{CancellationSource, CancellationToken, Error, LoopbackTarget, Result};

/// A validated HTTP method token.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Method(String);

impl Method {
    /// Validates an HTTP method using the RFC token grammar.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidMethod`] for an empty value or a non-token byte.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty() || !value.bytes().all(is_token_byte) {
            return Err(Error::InvalidMethod);
        }
        Ok(Self(value))
    }

    /// Constructs the `GET` method.
    #[must_use]
    pub fn get() -> Self {
        Self("GET".to_owned())
    }

    /// Constructs the `HEAD` method.
    #[must_use]
    pub fn head() -> Self {
        Self("HEAD".to_owned())
    }

    /// Returns the serialized method.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn is_head(&self) -> bool {
        self.0 == "HEAD"
    }
}

/// A validated, normalized HTTP header field name.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct HeaderName(String);

impl HeaderName {
    /// Validates a field name and normalizes ASCII letters to lowercase.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidHeaderName`] for an empty name or non-token byte.
    pub fn new(value: &str) -> Result<Self> {
        if value.is_empty() || !value.bytes().all(is_token_byte) {
            return Err(Error::InvalidHeaderName);
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    /// Returns the normalized lowercase field name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn is(&self, expected: &str) -> bool {
        self.0 == expected
    }
}

/// A validated HTTP field value represented as raw bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeaderValue(Vec<u8>);

impl HeaderValue {
    /// Validates bytes using the HTTP field-value byte constraints.
    ///
    /// Horizontal tab, space, visible ASCII, and `obs-text` bytes are
    /// accepted. CR, LF, NUL, DEL, and other controls are rejected.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidHeaderValue`] for a prohibited control byte.
    pub fn from_bytes(value: impl Into<Vec<u8>>) -> Result<Self> {
        let value = value.into();
        if value.iter().copied().any(is_prohibited_field_value_byte) {
            return Err(Error::InvalidHeaderValue);
        }
        Ok(Self(value))
    }

    /// Validates a UTF-8 string as an HTTP field value.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidHeaderValue`] for a prohibited control byte.
    pub fn from_text(value: &str) -> Result<Self> {
        Self::from_bytes(value.as_bytes())
    }

    /// Returns the field value bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Returns the field value as UTF-8 when possible.
    #[must_use]
    pub fn to_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }
}

/// An ordered collection of validated HTTP header fields.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Headers {
    fields: Vec<(HeaderName, HeaderValue)>,
}

impl Headers {
    /// Creates an empty field collection.
    #[must_use]
    pub const fn new() -> Self {
        Self { fields: Vec::new() }
    }

    /// Appends a field without merging it with existing values.
    pub fn append(&mut self, name: HeaderName, value: HeaderValue) {
        self.fields.push((name, value));
    }

    /// Returns the first field value with this case-insensitive name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&HeaderValue> {
        self.fields
            .iter()
            .find(|(candidate, _)| candidate.as_str().eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
    }

    /// Iterates over every value with this case-insensitive name.
    pub fn values<'headers, 'name>(
        &'headers self,
        name: &'name str,
    ) -> impl Iterator<Item = &'headers HeaderValue> + 'name
    where
        'headers: 'name,
    {
        self.fields.iter().filter_map(move |(candidate, value)| {
            candidate
                .as_str()
                .eq_ignore_ascii_case(name)
                .then_some(value)
        })
    }

    /// Iterates over all fields in wire order.
    #[must_use]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&HeaderName, &HeaderValue)> {
        self.fields.iter().map(|(name, value)| (name, value))
    }

    /// Returns the number of field lines.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.fields.len()
    }

    /// Returns whether there are no field lines.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

/// Redirect handling exposed explicitly at the transport boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RedirectPolicy {
    /// Return a redirect response unchanged for higher-level Fetch processing.
    Manual,
    /// Fail when the final response has a redirect status.
    Reject,
}

/// An owned, bounded HTTP request contract.
#[derive(Clone, Debug)]
pub struct Request {
    method: Method,
    target: LoopbackTarget,
    headers: Headers,
    body: Vec<u8>,
    redirect_policy: RedirectPolicy,
    cancellation: CancellationToken,
    deadline: Option<Instant>,
}

impl Request {
    /// Creates a request with an explicit redirect policy.
    #[must_use]
    pub fn new(method: Method, target: LoopbackTarget, redirect_policy: RedirectPolicy) -> Self {
        Self {
            method,
            target,
            headers: Headers::new(),
            body: Vec::new(),
            redirect_policy,
            cancellation: CancellationSource::new().token(),
            deadline: None,
        }
    }

    /// Creates a bodyless `GET` request with an explicit redirect policy.
    #[must_use]
    pub fn get(target: LoopbackTarget, redirect_policy: RedirectPolicy) -> Self {
        Self::new(Method::get(), target, redirect_policy)
    }

    /// Adds a validated caller-owned request header.
    ///
    /// `Host`, framing, connection, and content-coding fields remain owned by
    /// the transport to prevent request smuggling and unsupported encodings.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ReservedRequestHeader`] for a transport-owned field.
    pub fn append_header(&mut self, name: HeaderName, value: HeaderValue) -> Result<()> {
        if is_reserved_request_header(&name) {
            return Err(Error::ReservedRequestHeader(name.as_str().to_owned()));
        }
        self.headers.append(name, value);
        Ok(())
    }

    /// Builder form of [`Self::append_header`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::ReservedRequestHeader`] for a transport-owned field.
    pub fn with_header(mut self, name: HeaderName, value: HeaderValue) -> Result<Self> {
        self.append_header(name, value)?;
        Ok(self)
    }

    /// Replaces the outgoing body.
    #[must_use]
    pub fn with_body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    /// Uses a cancellation token supplied by the caller.
    #[must_use]
    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    /// Sets an absolute deadline covering connection, head, and body reads.
    #[must_use]
    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Returns the validated method.
    #[must_use]
    pub const fn method(&self) -> &Method {
        &self.method
    }

    /// Returns the loopback-only URL, origin, and request target.
    #[must_use]
    pub const fn target(&self) -> &LoopbackTarget {
        &self.target
    }

    /// Returns caller-supplied request fields.
    #[must_use]
    pub const fn headers(&self) -> &Headers {
        &self.headers
    }

    /// Returns the outgoing body.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Returns the caller's explicit redirect policy.
    #[must_use]
    pub const fn redirect_policy(&self) -> RedirectPolicy {
        self.redirect_policy
    }

    /// Returns the cooperative cancellation token.
    #[must_use]
    pub const fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// Returns the absolute request deadline, when configured.
    #[must_use]
    pub const fn deadline(&self) -> Option<Instant> {
        self.deadline
    }
}

/// HTTP version parsed from a response status line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpVersion {
    /// HTTP/1.0.
    Http10,
    /// HTTP/1.1.
    Http11,
}

/// A validated three-digit HTTP response status code.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct StatusCode(u16);

impl StatusCode {
    pub(crate) fn from_wire(value: u16) -> Result<Self> {
        if (100..=599).contains(&value) {
            Ok(Self(value))
        } else {
            Err(Error::MalformedStatusLine)
        }
    }

    /// Returns the numeric status code.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self.0
    }

    /// Returns whether this is a 1xx informational status.
    #[must_use]
    pub const fn is_informational(self) -> bool {
        self.0 >= 100 && self.0 < 200
    }

    /// Returns whether Fetch may treat this status as a redirect.
    #[must_use]
    pub const fn is_redirect(self) -> bool {
        matches!(self.0, 301 | 302 | 303 | 307 | 308)
    }
}

/// How the final response body is delimited on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BodyFraming {
    /// The response semantics prohibit a body.
    None,
    /// A validated Content-Length determines the exact body size.
    ContentLength(u64),
    /// A strict chunked decoder determines the body and trailer boundary.
    Chunked,
    /// EOF on the connection delimits the body.
    ConnectionClose,
}

/// Whether this transport may reuse the TCP connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionDisposition {
    /// This nucleus sends `Connection: close` and never pools the socket.
    CloseAfterResponse,
}

/// Parsed metadata for a final HTTP response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponseHead {
    version: HttpVersion,
    status: StatusCode,
    reason_phrase: Vec<u8>,
    headers: Headers,
    framing: BodyFraming,
    connection: ConnectionDisposition,
    informational_responses: usize,
}

impl ResponseHead {
    pub(crate) fn new(
        version: HttpVersion,
        status: StatusCode,
        reason_phrase: Vec<u8>,
        headers: Headers,
        framing: BodyFraming,
        informational_responses: usize,
    ) -> Self {
        Self {
            version,
            status,
            reason_phrase,
            headers,
            framing,
            connection: ConnectionDisposition::CloseAfterResponse,
            informational_responses,
        }
    }

    /// Returns the parsed HTTP version.
    #[must_use]
    pub const fn version(&self) -> HttpVersion {
        self.version
    }

    /// Returns the final response status.
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        self.status
    }

    /// Returns the untrusted reason-phrase bytes.
    #[must_use]
    pub fn reason_phrase(&self) -> &[u8] {
        &self.reason_phrase
    }

    /// Returns the validated final response fields.
    #[must_use]
    pub const fn headers(&self) -> &Headers {
        &self.headers
    }

    /// Returns the selected body framing.
    #[must_use]
    pub const fn body_framing(&self) -> BodyFraming {
        self.framing
    }

    /// Returns this nucleus's connection disposition.
    #[must_use]
    pub const fn connection_disposition(&self) -> ConnectionDisposition {
        self.connection
    }

    /// Returns the number of bounded 1xx heads consumed before this response.
    #[must_use]
    pub const fn informational_response_count(&self) -> usize {
        self.informational_responses
    }
}

pub(crate) fn trim_optional_whitespace(mut value: &[u8]) -> &[u8] {
    while matches!(value.first(), Some(b' ' | b'\t')) {
        value = &value[1..];
    }
    while matches!(value.last(), Some(b' ' | b'\t')) {
        value = &value[..value.len() - 1];
    }
    value
}

pub(crate) const fn is_token_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'#'
            | b'$'
            | b'%'
            | b'&'
            | b'\''
            | b'*'
            | b'+'
            | b'-'
            | b'.'
            | b'^'
            | b'_'
            | b'`'
            | b'|'
            | b'~'
            | b'0'..=b'9'
            | b'A'..=b'Z'
            | b'a'..=b'z'
    )
}

const fn is_prohibited_field_value_byte(byte: u8) -> bool {
    (byte < 0x20 && byte != b'\t') || byte == 0x7f
}

fn is_reserved_request_header(name: &HeaderName) -> bool {
    [
        "host",
        "connection",
        "content-length",
        "transfer-encoding",
        "content-encoding",
        "trailer",
        "upgrade",
        "expect",
    ]
    .iter()
    .any(|reserved| name.is(reserved))
}
