// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    cmp,
    io::{self, Read, Write},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

use crate::{
    BodyFraming, CancellationToken, Error, HeaderName, HeaderValue, Headers, HttpVersion,
    LimitKind, Method, Operation, RedirectPolicy, Request, ResponseHead, Result, StatusCode,
    message::{is_token_byte, trim_optional_whitespace},
};

const IO_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MIN_SOCKET_TIMEOUT: Duration = Duration::from_micros(1);
const REQUEST_LINE_SUFFIX_AND_HOST_PREFIX: &[u8] = b" HTTP/1.1\r\nHost: ";
const CONNECTION_LINE: &[u8] = b"\r\nConnection: close\r\n";
const CONTENT_LENGTH_PREFIX: &[u8] = b"Content-Length: ";
const FIELD_SEPARATOR: &[u8] = b": ";
const CRLF: &[u8] = b"\r\n";

/// Resource limits and timeout policy for [`HttpClient`].
#[derive(Clone, Debug)]
pub struct ClientConfig {
    max_header_bytes: usize,
    max_header_count: usize,
    max_body_bytes: usize,
    max_request_head_bytes: usize,
    max_request_header_count: usize,
    max_request_body_bytes: usize,
    max_chunk_line_bytes: usize,
    max_informational_responses: usize,
    connect_timeout: Duration,
    read_timeout: Duration,
    write_timeout: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            max_header_bytes: 64 * 1024,
            max_header_count: 256,
            max_body_bytes: 8 * 1024 * 1024,
            max_request_head_bytes: 64 * 1024,
            max_request_header_count: 128,
            max_request_body_bytes: 1024 * 1024,
            max_chunk_line_bytes: 8 * 1024,
            max_informational_responses: 8,
            connect_timeout: Duration::from_secs(2),
            read_timeout: Duration::from_secs(5),
            write_timeout: Duration::from_secs(5),
        }
    }
}

impl ClientConfig {
    /// Sets the aggregate status/header/trailer byte limit.
    #[must_use]
    pub const fn with_max_header_bytes(mut self, limit: usize) -> Self {
        self.max_header_bytes = limit;
        self
    }

    /// Sets the aggregate response header and trailer field count limit.
    #[must_use]
    pub const fn with_max_header_count(mut self, limit: usize) -> Self {
        self.max_header_count = limit;
        self
    }

    /// Sets the maximum decoded response-body size.
    #[must_use]
    pub const fn with_max_body_bytes(mut self, limit: usize) -> Self {
        self.max_body_bytes = limit;
        self
    }

    /// Sets the maximum fully serialized outgoing request-head size.
    #[must_use]
    pub const fn with_max_request_head_bytes(mut self, limit: usize) -> Self {
        self.max_request_head_bytes = limit;
        self
    }

    /// Sets the maximum number of caller-supplied request header fields.
    #[must_use]
    pub const fn with_max_request_header_count(mut self, limit: usize) -> Self {
        self.max_request_header_count = limit;
        self
    }

    /// Sets the maximum outgoing request-body size.
    #[must_use]
    pub const fn with_max_request_body_bytes(mut self, limit: usize) -> Self {
        self.max_request_body_bytes = limit;
        self
    }

    /// Sets the maximum chunk-size line length, excluding CRLF.
    #[must_use]
    pub const fn with_max_chunk_line_bytes(mut self, limit: usize) -> Self {
        self.max_chunk_line_bytes = limit;
        self
    }

    /// Sets the maximum number of 1xx responses before the final response.
    #[must_use]
    pub const fn with_max_informational_responses(mut self, limit: usize) -> Self {
        self.max_informational_responses = limit;
        self
    }

    /// Sets the TCP connection-attempt timeout.
    #[must_use]
    pub const fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Sets the maximum inactive duration for each socket read.
    #[must_use]
    pub const fn with_read_timeout(mut self, timeout: Duration) -> Self {
        self.read_timeout = timeout;
        self
    }

    /// Sets the maximum inactive duration for each socket write.
    #[must_use]
    pub const fn with_write_timeout(mut self, timeout: Duration) -> Self {
        self.write_timeout = timeout;
        self
    }

    /// Returns the aggregate response metadata byte limit.
    #[must_use]
    pub const fn max_header_bytes(&self) -> usize {
        self.max_header_bytes
    }

    /// Returns the aggregate response field count limit.
    #[must_use]
    pub const fn max_header_count(&self) -> usize {
        self.max_header_count
    }

    /// Returns the decoded response-body byte limit.
    #[must_use]
    pub const fn max_body_bytes(&self) -> usize {
        self.max_body_bytes
    }

    /// Returns the fully serialized outgoing request-head byte limit.
    #[must_use]
    pub const fn max_request_head_bytes(&self) -> usize {
        self.max_request_head_bytes
    }

    /// Returns the caller-supplied request header field-count limit.
    #[must_use]
    pub const fn max_request_header_count(&self) -> usize {
        self.max_request_header_count
    }
}

/// A synchronous, cancellation-aware loopback HTTP/1.1 client.
#[derive(Clone, Debug)]
pub struct HttpClient {
    config: ClientConfig,
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new(ClientConfig::default())
    }
}

impl HttpClient {
    /// Creates a client with explicit limits and timeouts.
    #[must_use]
    pub const fn new(config: ClientConfig) -> Self {
        Self { config }
    }

    /// Returns this client's immutable policy.
    #[must_use]
    pub const fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// Sends one request and parses the final response head.
    ///
    /// The returned body remains streaming and bounded. This method never
    /// follows redirects, performs DNS, or connects beyond numeric loopback.
    ///
    /// # Errors
    ///
    /// Returns a structured validation, limit, cancellation, timeout, I/O, or
    /// HTTP framing error. Untrusted peer bytes never cause a panic.
    pub fn execute(&self, request: &Request) -> Result<Response> {
        if request.body().len() > self.config.max_request_body_bytes {
            return Err(Error::LimitExceeded {
                kind: LimitKind::RequestBodyBytes,
                limit: self.config.max_request_body_bytes,
            });
        }
        let request_head = serialize_request_head(request, &self.config)?;

        check_control(
            request.cancellation(),
            request.deadline(),
            Operation::Connect,
        )?;
        let mut stream = connect_interruptible(
            request.target().origin().socket_addr(),
            self.config.connect_timeout,
            request.cancellation(),
            request.deadline(),
        )?;

        write_interruptible(
            &mut stream,
            &request_head,
            self.config.write_timeout,
            request.cancellation(),
            request.deadline(),
        )?;
        write_interruptible(
            &mut stream,
            request.body(),
            self.config.write_timeout,
            request.cancellation(),
            request.deadline(),
        )?;

        let reader = WireReader::new(
            stream,
            self.config.read_timeout,
            request.cancellation().clone(),
            request.deadline(),
        );
        let ParsedResponse {
            head,
            reader,
            header_bytes,
            header_count,
        } = parse_response_head(reader, request.method(), &self.config)?;

        if request.redirect_policy() == RedirectPolicy::Reject && head.status().is_redirect() {
            return Err(Error::RedirectRejected(head.status().as_u16()));
        }

        let body = Body::new(
            reader,
            head.body_framing(),
            &self.config,
            header_bytes,
            header_count,
        );
        Ok(Response { head, body })
    }
}

/// A parsed response head and its streaming body.
#[derive(Debug)]
pub struct Response {
    head: ResponseHead,
    body: Body,
}

impl Response {
    /// Returns the final response metadata.
    #[must_use]
    pub const fn head(&self) -> &ResponseHead {
        &self.head
    }

    /// Returns the streaming response body.
    #[must_use]
    pub const fn body(&self) -> &Body {
        &self.body
    }

    /// Returns the mutable streaming response body.
    #[must_use]
    pub const fn body_mut(&mut self) -> &mut Body {
        &mut self.body
    }

    /// Splits the response into independently owned head and body values.
    #[must_use]
    pub fn into_parts(self) -> (ResponseHead, Body) {
        (self.head, self.body)
    }

    /// Reads the bounded body to completion.
    ///
    /// # Errors
    ///
    /// Returns a cancellation, timeout, I/O, limit, or body-framing error.
    pub fn read_body_to_end(mut self) -> Result<Vec<u8>> {
        self.body.read_to_end()
    }
}

/// A bounded streaming response body.
#[derive(Debug)]
pub struct Body {
    reader: Option<WireReader>,
    state: BodyState,
    terminal_error: Option<Error>,
    decoded_bytes: usize,
    max_body_bytes: usize,
    max_header_bytes: usize,
    max_header_count: usize,
    header_bytes: usize,
    header_count: usize,
    max_chunk_line_bytes: usize,
    trailers: Headers,
}

#[derive(Clone, Copy, Debug)]
enum BodyState {
    Empty,
    ContentLength {
        remaining: u64,
    },
    Chunked {
        chunk_remaining: u64,
        needs_data_crlf: bool,
    },
    ConnectionClose,
}

impl Body {
    fn new(
        reader: WireReader,
        framing: BodyFraming,
        config: &ClientConfig,
        header_bytes: usize,
        header_count: usize,
    ) -> Self {
        let (reader, state) = match framing {
            BodyFraming::None | BodyFraming::ContentLength(0) => (None, BodyState::Empty),
            BodyFraming::ContentLength(remaining) => {
                (Some(reader), BodyState::ContentLength { remaining })
            }
            BodyFraming::Chunked => (
                Some(reader),
                BodyState::Chunked {
                    chunk_remaining: 0,
                    needs_data_crlf: false,
                },
            ),
            BodyFraming::ConnectionClose => (Some(reader), BodyState::ConnectionClose),
        };
        Self {
            reader,
            state,
            terminal_error: None,
            decoded_bytes: 0,
            max_body_bytes: config.max_body_bytes,
            max_header_bytes: config.max_header_bytes,
            max_header_count: config.max_header_count,
            header_bytes,
            header_count,
            max_chunk_line_bytes: config.max_chunk_line_bytes,
            trailers: Headers::new(),
        }
    }

    /// Reads the next decoded body bytes into `output`.
    ///
    /// A zero return value means the body and any trailers are complete unless
    /// `output` is empty. As with [`Read`], an empty output slice returns zero
    /// without establishing end-of-body.
    ///
    /// Protocol, resource-limit, premature-EOF, and non-timeout I/O failures
    /// permanently poison the body. Every later non-empty read returns the
    /// same error without consuming or exposing more peer bytes. Cancellation
    /// and timeout errors are control-flow failures rather than parser poison:
    /// an inactivity timeout may be retried, while cancellation continues to
    /// follow the request's one-way token.
    ///
    /// # Errors
    ///
    /// Returns a cancellation, timeout, I/O, limit, or body-framing error.
    pub fn read_chunk(&mut self, output: &mut [u8]) -> Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if let Some(error) = &self.terminal_error {
            return Err(error.clone());
        }

        let result = self.read_chunk_unlatched(output);
        if let Err(error) = &result
            && is_terminal_body_error(error)
        {
            self.terminal_error = Some(error.clone());
            self.reader = None;
            self.state = BodyState::Empty;
            self.trailers = Headers::new();
        }
        result
    }

    fn read_chunk_unlatched(&mut self, output: &mut [u8]) -> Result<usize> {
        if let Some(reader) = &self.reader {
            check_control(&reader.cancellation, reader.deadline, Operation::ReadBody)?;
        }
        match self.state {
            BodyState::Empty => Ok(0),
            BodyState::ContentLength { remaining } => self.read_content_length(output, remaining),
            BodyState::ConnectionClose => self.read_connection_close(output),
            BodyState::Chunked {
                chunk_remaining,
                needs_data_crlf,
            } => self.read_chunked(output, chunk_remaining, needs_data_crlf),
        }
    }

    fn read_content_length(&mut self, output: &mut [u8], remaining: u64) -> Result<usize> {
        let permitted = bounded_read_len(
            output.len(),
            remaining,
            self.decoded_bytes,
            self.max_body_bytes,
        )?;
        let count = reader_mut(&mut self.reader)?
            .read_some(&mut output[..permitted], Operation::ReadBody)?;
        if count == 0 {
            return Err(Error::PrematureEof);
        }
        let count_u64 = u64::try_from(count).map_err(|_| body_limit_error(self.max_body_bytes))?;
        let remaining = remaining - count_u64;
        self.decoded_bytes += count;
        if remaining == 0 {
            self.state = BodyState::Empty;
            self.reader = None;
        } else {
            self.state = BodyState::ContentLength { remaining };
        }
        Ok(count)
    }

    fn read_connection_close(&mut self, output: &mut [u8]) -> Result<usize> {
        if self.decoded_bytes == self.max_body_bytes {
            let mut probe = [0_u8; 1];
            let count = reader_mut(&mut self.reader)?.read_some(&mut probe, Operation::ReadBody)?;
            if count == 0 {
                self.state = BodyState::Empty;
                self.reader = None;
                return Ok(0);
            }
            return Err(body_limit_error(self.max_body_bytes));
        }
        let permitted = cmp::min(output.len(), self.max_body_bytes - self.decoded_bytes);
        let count = reader_mut(&mut self.reader)?
            .read_some(&mut output[..permitted], Operation::ReadBody)?;
        if count == 0 {
            self.state = BodyState::Empty;
            self.reader = None;
            return Ok(0);
        }
        self.decoded_bytes += count;
        Ok(count)
    }

    fn read_chunked(
        &mut self,
        output: &mut [u8],
        mut chunk_remaining: u64,
        mut needs_data_crlf: bool,
    ) -> Result<usize> {
        loop {
            if chunk_remaining > 0 {
                let permitted = bounded_read_len(
                    output.len(),
                    chunk_remaining,
                    self.decoded_bytes,
                    self.max_body_bytes,
                )?;
                let count = reader_mut(&mut self.reader)?
                    .read_some(&mut output[..permitted], Operation::ReadBody)?;
                if count == 0 {
                    return Err(Error::PrematureEof);
                }
                let count_u64 =
                    u64::try_from(count).map_err(|_| body_limit_error(self.max_body_bytes))?;
                chunk_remaining -= count_u64;
                self.decoded_bytes += count;
                self.state = BodyState::Chunked {
                    chunk_remaining,
                    needs_data_crlf: chunk_remaining == 0,
                };
                return Ok(count);
            }
            if needs_data_crlf {
                let mut ending = [0_u8; 2];
                reader_mut(&mut self.reader)?.read_exact_wire(&mut ending, Operation::ReadBody)?;
                if ending != *b"\r\n" {
                    return Err(Error::InvalidLineEnding);
                }
                needs_data_crlf = false;
                self.state = BodyState::Chunked {
                    chunk_remaining: 0,
                    needs_data_crlf,
                };
            }

            let (line, _) = reader_mut(&mut self.reader)?.read_crlf_line(
                self.max_chunk_line_bytes,
                self.max_chunk_line_bytes,
                LimitKind::ChunkLineBytes,
                Operation::ReadBody,
            )?;
            let size = parse_chunk_size(&line)?;
            if size == 0 {
                read_trailers(
                    reader_mut(&mut self.reader)?,
                    &mut self.trailers,
                    &mut self.header_bytes,
                    &mut self.header_count,
                    self.max_header_bytes,
                    self.max_header_count,
                )?;
                self.state = BodyState::Empty;
                self.reader = None;
                return Ok(0);
            }
            let available = self.max_body_bytes - self.decoded_bytes;
            if size > u64::try_from(available).unwrap_or(u64::MAX) {
                return Err(body_limit_error(self.max_body_bytes));
            }
            chunk_remaining = size;
            self.state = BodyState::Chunked {
                chunk_remaining,
                needs_data_crlf: false,
            };
        }
    }

    /// Reads this body to completion without exceeding its configured bound.
    ///
    /// # Errors
    ///
    /// Returns a cancellation, timeout, I/O, limit, or body-framing error.
    pub fn read_to_end(&mut self) -> Result<Vec<u8>> {
        let mut bytes = Vec::with_capacity(cmp::min(self.max_body_bytes, 16 * 1024));
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            let count = self.read_chunk(&mut buffer)?;
            if count == 0 {
                return Ok(bytes);
            }
            bytes.extend_from_slice(&buffer[..count]);
        }
    }

    /// Returns trailers parsed after a chunked body has completed.
    #[must_use]
    pub const fn trailers(&self) -> &Headers {
        &self.trailers
    }

    /// Returns the number of decoded bytes delivered so far.
    #[must_use]
    pub const fn decoded_bytes(&self) -> usize {
        self.decoded_bytes
    }
}

const fn is_terminal_body_error(error: &Error) -> bool {
    !matches!(error, Error::Cancelled | Error::Timeout(_))
}

impl Read for Body {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.read_chunk(output).map_err(io::Error::other)
    }
}

fn reader_mut(reader: &mut Option<WireReader>) -> Result<&mut WireReader> {
    reader.as_mut().ok_or(Error::PrematureEof)
}

fn bounded_read_len(
    output_len: usize,
    wire_remaining: u64,
    decoded_bytes: usize,
    max_body_bytes: usize,
) -> Result<usize> {
    let available = max_body_bytes
        .checked_sub(decoded_bytes)
        .ok_or_else(|| body_limit_error(max_body_bytes))?;
    if available == 0 {
        return Err(body_limit_error(max_body_bytes));
    }
    let wire_limit = usize::try_from(wire_remaining).unwrap_or(usize::MAX);
    Ok(cmp::min(output_len, cmp::min(wire_limit, available)))
}

const fn body_limit_error(limit: usize) -> Error {
    Error::LimitExceeded {
        kind: LimitKind::BodyBytes,
        limit,
    }
}

#[derive(Debug)]
struct ParsedResponse {
    head: ResponseHead,
    reader: WireReader,
    header_bytes: usize,
    header_count: usize,
}

fn parse_response_head(
    mut reader: WireReader,
    method: &Method,
    config: &ClientConfig,
) -> Result<ParsedResponse> {
    let mut header_bytes = 0;
    let mut header_count = 0;
    let mut informational_responses = 0_usize;

    loop {
        let (version, status, reason_phrase, headers) =
            parse_one_response_head(&mut reader, &mut header_bytes, &mut header_count, config)?;
        validate_content_coding(&headers)?;
        let content_length = parse_content_length(&headers)?;
        let transfer_encoding = parse_transfer_encoding(&headers)?;
        if content_length.is_some() && transfer_encoding {
            return Err(Error::AmbiguousBodyFraming);
        }

        if status.as_u16() == 101 {
            return Err(Error::ProtocolSwitchUnsupported);
        }
        if status.is_informational() {
            if content_length.is_some() || transfer_encoding {
                return Err(Error::MalformedHeader);
            }
            informational_responses =
                informational_responses
                    .checked_add(1)
                    .ok_or(Error::LimitExceeded {
                        kind: LimitKind::InformationalResponses,
                        limit: config.max_informational_responses,
                    })?;
            if informational_responses > config.max_informational_responses {
                return Err(Error::LimitExceeded {
                    kind: LimitKind::InformationalResponses,
                    limit: config.max_informational_responses,
                });
            }
            continue;
        }
        if status.as_u16() == 204 && (content_length.is_some() || transfer_encoding) {
            return Err(Error::MalformedHeader);
        }

        let framing = determine_body_framing(
            method,
            status,
            content_length,
            transfer_encoding,
            config.max_body_bytes,
        )?;
        let head = ResponseHead::new(
            version,
            status,
            reason_phrase,
            headers,
            framing,
            informational_responses,
        );
        return Ok(ParsedResponse {
            head,
            reader,
            header_bytes,
            header_count,
        });
    }
}

fn parse_one_response_head(
    reader: &mut WireReader,
    header_bytes: &mut usize,
    header_count: &mut usize,
    config: &ClientConfig,
) -> Result<(HttpVersion, StatusCode, Vec<u8>, Headers)> {
    let status_line = read_metadata_line(
        reader,
        header_bytes,
        config.max_header_bytes,
        Operation::ReadHead,
    )?;
    let (version, status, reason_phrase) = parse_status_line(&status_line)?;
    let mut headers = Headers::new();

    loop {
        let line = read_metadata_line(
            reader,
            header_bytes,
            config.max_header_bytes,
            Operation::ReadHead,
        )?;
        if line.is_empty() {
            break;
        }
        *header_count = header_count.checked_add(1).ok_or(Error::LimitExceeded {
            kind: LimitKind::HeaderCount,
            limit: config.max_header_count,
        })?;
        if *header_count > config.max_header_count {
            return Err(Error::LimitExceeded {
                kind: LimitKind::HeaderCount,
                limit: config.max_header_count,
            });
        }
        let (name, value) = parse_header_line(&line)?;
        headers.append(name, value);
    }
    Ok((version, status, reason_phrase, headers))
}

fn read_metadata_line(
    reader: &mut WireReader,
    consumed: &mut usize,
    limit: usize,
    operation: Operation,
) -> Result<Vec<u8>> {
    let remaining = limit.checked_sub(*consumed).ok_or(Error::LimitExceeded {
        kind: LimitKind::HeaderBytes,
        limit,
    })?;
    if remaining < 2 {
        return Err(Error::LimitExceeded {
            kind: LimitKind::HeaderBytes,
            limit,
        });
    }
    let (line, wire_bytes) =
        reader.read_crlf_line(remaining - 2, limit, LimitKind::HeaderBytes, operation)?;
    *consumed = consumed
        .checked_add(wire_bytes)
        .ok_or(Error::LimitExceeded {
            kind: LimitKind::HeaderBytes,
            limit,
        })?;
    Ok(line)
}

fn parse_status_line(line: &[u8]) -> Result<(HttpVersion, StatusCode, Vec<u8>)> {
    if line.len() < 13 {
        return Err(Error::MalformedStatusLine);
    }
    let version = match line.get(..9) {
        Some(b"HTTP/1.0 ") => HttpVersion::Http10,
        Some(b"HTTP/1.1 ") => HttpVersion::Http11,
        _ => return Err(Error::MalformedStatusLine),
    };
    let status_bytes = line.get(9..12).ok_or(Error::MalformedStatusLine)?;
    if !status_bytes.iter().all(u8::is_ascii_digit) || line.get(12) != Some(&b' ') {
        return Err(Error::MalformedStatusLine);
    }
    let numeric = u16::from(status_bytes[0] - b'0') * 100
        + u16::from(status_bytes[1] - b'0') * 10
        + u16::from(status_bytes[2] - b'0');
    let status = StatusCode::from_wire(numeric)?;
    let reason_phrase = line[13..].to_vec();
    HeaderValue::from_bytes(reason_phrase.clone()).map_err(|_| Error::MalformedStatusLine)?;
    Ok((version, status, reason_phrase))
}

fn parse_header_line(line: &[u8]) -> Result<(HeaderName, HeaderValue)> {
    if matches!(line.first(), Some(b' ' | b'\t')) {
        return Err(Error::ObsoleteLineFolding);
    }
    let colon = line
        .iter()
        .position(|byte| *byte == b':')
        .ok_or(Error::MalformedHeader)?;
    let name_bytes = &line[..colon];
    if name_bytes.is_empty() || !name_bytes.iter().copied().all(is_token_byte) {
        return Err(Error::MalformedHeader);
    }
    let name_string = std::str::from_utf8(name_bytes).map_err(|_| Error::MalformedHeader)?;
    let name = HeaderName::new(name_string).map_err(|_| Error::MalformedHeader)?;
    let value = trim_optional_whitespace(&line[colon + 1..]);
    let value = HeaderValue::from_bytes(value).map_err(|_| Error::MalformedHeader)?;
    Ok((name, value))
}

fn parse_content_length(headers: &Headers) -> Result<Option<u64>> {
    let mut parsed = None;
    for value in headers.values("content-length") {
        for member in value.as_bytes().split(|byte| *byte == b',') {
            let member = trim_optional_whitespace(member);
            if member.is_empty() || !member.iter().all(u8::is_ascii_digit) {
                return Err(Error::InvalidContentLength);
            }
            let mut number = 0_u64;
            for digit in member {
                number = number
                    .checked_mul(10)
                    .and_then(|value| value.checked_add(u64::from(*digit - b'0')))
                    .ok_or(Error::InvalidContentLength)?;
            }
            match parsed {
                Some(previous) if previous != number => {
                    return Err(Error::ConflictingContentLength);
                }
                Some(_) => {}
                None => parsed = Some(number),
            }
        }
    }
    Ok(parsed)
}

fn parse_transfer_encoding(headers: &Headers) -> Result<bool> {
    let mut codings = Vec::new();
    for value in headers.values("transfer-encoding") {
        codings.extend(
            value
                .as_bytes()
                .split(|byte| *byte == b',')
                .map(trim_optional_whitespace),
        );
    }
    if codings.is_empty() {
        return Ok(false);
    }
    if codings.len() == 1 && codings[0].eq_ignore_ascii_case(b"chunked") {
        return Ok(true);
    }
    Err(Error::UnsupportedTransferCoding(
        String::from_utf8_lossy(codings.concat().as_slice()).into_owned(),
    ))
}

fn validate_content_coding(headers: &Headers) -> Result<()> {
    for value in headers.values("content-encoding") {
        for coding in value.as_bytes().split(|byte| *byte == b',') {
            let coding = trim_optional_whitespace(coding);
            if coding.is_empty() || !coding.eq_ignore_ascii_case(b"identity") {
                return Err(Error::UnsupportedContentCoding(
                    String::from_utf8_lossy(coding).into_owned(),
                ));
            }
        }
    }
    Ok(())
}

fn determine_body_framing(
    method: &Method,
    status: StatusCode,
    content_length: Option<u64>,
    transfer_encoding: bool,
    max_body_bytes: usize,
) -> Result<BodyFraming> {
    if method.is_head() || matches!(status.as_u16(), 204 | 205 | 304) {
        return Ok(BodyFraming::None);
    }
    if transfer_encoding {
        return Ok(BodyFraming::Chunked);
    }
    if let Some(length) = content_length {
        if length > u64::try_from(max_body_bytes).unwrap_or(u64::MAX) {
            return Err(body_limit_error(max_body_bytes));
        }
        return Ok(BodyFraming::ContentLength(length));
    }
    Ok(BodyFraming::ConnectionClose)
}

fn serialize_request_head(request: &Request, config: &ClientConfig) -> Result<Vec<u8>> {
    if request.headers().len() > config.max_request_header_count {
        return Err(Error::LimitExceeded {
            kind: LimitKind::RequestHeaderCount,
            limit: config.max_request_header_count,
        });
    }

    let authority = request.target().origin().authority();
    let limit = config.max_request_head_bytes;
    let mut length = 0_usize;
    add_request_head_length(&mut length, request.method().as_str().len(), limit)?;
    add_request_head_length(&mut length, 1, limit)?;
    add_request_head_length(
        &mut length,
        request.target().request_target().as_str().len(),
        limit,
    )?;
    add_request_head_length(
        &mut length,
        REQUEST_LINE_SUFFIX_AND_HOST_PREFIX.len(),
        limit,
    )?;
    add_request_head_length(&mut length, authority.len(), limit)?;
    add_request_head_length(&mut length, CONNECTION_LINE.len(), limit)?;
    for (name, value) in request.headers().iter() {
        add_request_head_length(&mut length, name.as_str().len(), limit)?;
        add_request_head_length(&mut length, FIELD_SEPARATOR.len(), limit)?;
        add_request_head_length(&mut length, value.as_bytes().len(), limit)?;
        add_request_head_length(&mut length, CRLF.len(), limit)?;
    }
    if !request.body().is_empty() {
        add_request_head_length(&mut length, CONTENT_LENGTH_PREFIX.len(), limit)?;
        add_request_head_length(&mut length, decimal_length(request.body().len()), limit)?;
        add_request_head_length(&mut length, CRLF.len(), limit)?;
    }
    add_request_head_length(&mut length, CRLF.len(), limit)?;

    let mut output = Vec::new();
    output
        .try_reserve_exact(length)
        .map_err(|_| request_head_limit_error(limit))?;
    output.extend_from_slice(request.method().as_str().as_bytes());
    output.push(b' ');
    output.extend_from_slice(request.target().request_target().as_str().as_bytes());
    output.extend_from_slice(REQUEST_LINE_SUFFIX_AND_HOST_PREFIX);
    output.extend_from_slice(authority.as_bytes());
    output.extend_from_slice(CONNECTION_LINE);
    for (name, value) in request.headers().iter() {
        output.extend_from_slice(name.as_str().as_bytes());
        output.extend_from_slice(FIELD_SEPARATOR);
        output.extend_from_slice(value.as_bytes());
        output.extend_from_slice(CRLF);
    }
    if !request.body().is_empty() {
        output.extend_from_slice(CONTENT_LENGTH_PREFIX);
        output.extend_from_slice(request.body().len().to_string().as_bytes());
        output.extend_from_slice(CRLF);
    }
    output.extend_from_slice(CRLF);
    Ok(output)
}

fn add_request_head_length(total: &mut usize, additional: usize, limit: usize) -> Result<()> {
    *total = total
        .checked_add(additional)
        .filter(|new_total| *new_total <= limit)
        .ok_or_else(|| request_head_limit_error(limit))?;
    Ok(())
}

const fn request_head_limit_error(limit: usize) -> Error {
    Error::LimitExceeded {
        kind: LimitKind::RequestHeadBytes,
        limit,
    }
}

const fn decimal_length(mut value: usize) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

fn connect_interruptible(
    address: SocketAddr,
    timeout: Duration,
    cancellation: &CancellationToken,
    deadline: Option<Instant>,
) -> Result<TcpStream> {
    let started = Instant::now();
    loop {
        check_control(cancellation, deadline, Operation::Connect)?;
        let wait = next_wait(started, timeout, deadline, Operation::Connect)?;
        match TcpStream::connect_timeout(&address, wait) {
            Ok(stream) => {
                check_control(cancellation, deadline, Operation::Connect)?;
                return Ok(stream);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => return Err(Error::io(Operation::Connect, &error)),
        }
    }
}

fn write_interruptible(
    stream: &mut TcpStream,
    mut bytes: &[u8],
    timeout: Duration,
    cancellation: &CancellationToken,
    deadline: Option<Instant>,
) -> Result<()> {
    while !bytes.is_empty() {
        let started = Instant::now();
        loop {
            check_control(cancellation, deadline, Operation::WriteRequest)?;
            let wait = next_wait(started, timeout, deadline, Operation::WriteRequest)?;
            stream
                .set_write_timeout(Some(wait))
                .map_err(|error| Error::io(Operation::WriteRequest, &error))?;
            match stream.write(bytes) {
                Ok(0) => {
                    return Err(Error::Io {
                        operation: Operation::WriteRequest,
                        kind: io::ErrorKind::WriteZero,
                    });
                }
                Ok(count) => {
                    bytes = &bytes[count..];
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) => {}
                Err(error) => return Err(Error::io(Operation::WriteRequest, &error)),
            }
        }
    }
    Ok(())
}

fn check_control(
    cancellation: &CancellationToken,
    deadline: Option<Instant>,
    operation: Operation,
) -> Result<()> {
    if cancellation.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(Error::Timeout(operation));
    }
    Ok(())
}

fn next_wait(
    started: Instant,
    timeout: Duration,
    deadline: Option<Instant>,
    operation: Operation,
) -> Result<Duration> {
    let elapsed = started.elapsed();
    if elapsed >= timeout {
        return Err(Error::Timeout(operation));
    }
    let remaining = timeout
        .checked_sub(elapsed)
        .ok_or(Error::Timeout(operation))?;
    let mut wait = cmp::min(IO_POLL_INTERVAL, remaining);
    if let Some(deadline) = deadline {
        let now = Instant::now();
        if now >= deadline {
            return Err(Error::Timeout(operation));
        }
        wait = cmp::min(wait, deadline.duration_since(now));
    }
    Ok(cmp::max(wait, MIN_SOCKET_TIMEOUT))
}

#[derive(Debug)]
struct WireReader {
    stream: TcpStream,
    buffer: Vec<u8>,
    cursor: usize,
    read_timeout: Duration,
    cancellation: CancellationToken,
    deadline: Option<Instant>,
}

impl WireReader {
    fn new(
        stream: TcpStream,
        read_timeout: Duration,
        cancellation: CancellationToken,
        deadline: Option<Instant>,
    ) -> Self {
        Self {
            stream,
            buffer: Vec::new(),
            cursor: 0,
            read_timeout,
            cancellation,
            deadline,
        }
    }

    fn read_some(&mut self, output: &mut [u8], operation: Operation) -> Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        check_control(&self.cancellation, self.deadline, operation)?;
        if self.cursor < self.buffer.len() {
            let count = cmp::min(output.len(), self.buffer.len() - self.cursor);
            output[..count].copy_from_slice(&self.buffer[self.cursor..self.cursor + count]);
            self.cursor += count;
            self.compact_if_consumed();
            return Ok(count);
        }
        self.read_socket(output, operation)
    }

    fn read_exact_wire(&mut self, mut output: &mut [u8], operation: Operation) -> Result<()> {
        while !output.is_empty() {
            let count = self.read_some(output, operation)?;
            if count == 0 {
                return Err(Error::PrematureEof);
            }
            output = &mut output[count..];
        }
        Ok(())
    }

    fn read_crlf_line(
        &mut self,
        max_payload_bytes: usize,
        reported_limit: usize,
        limit_kind: LimitKind,
        operation: Operation,
    ) -> Result<(Vec<u8>, usize)> {
        loop {
            check_control(&self.cancellation, self.deadline, operation)?;
            let pending = &self.buffer[self.cursor..];
            for (offset, byte) in pending.iter().copied().enumerate() {
                if byte == b'\n' {
                    if offset == 0 || pending[offset - 1] != b'\r' {
                        return Err(Error::InvalidLineEnding);
                    }
                    let payload_length = offset - 1;
                    if payload_length > max_payload_bytes {
                        return Err(Error::LimitExceeded {
                            kind: limit_kind,
                            limit: reported_limit,
                        });
                    }
                    let payload = &pending[..payload_length];
                    if payload.contains(&b'\r') || payload.contains(&b'\n') {
                        return Err(Error::InvalidLineEnding);
                    }
                    let line = payload.to_vec();
                    let wire_bytes = offset + 1;
                    self.cursor += wire_bytes;
                    self.compact_if_consumed();
                    return Ok((line, wire_bytes));
                }
                if byte == b'\r' && offset + 1 < pending.len() && pending[offset + 1] != b'\n' {
                    return Err(Error::InvalidLineEnding);
                }
            }
            if pending.len() > max_payload_bytes.saturating_add(1) {
                return Err(Error::LimitExceeded {
                    kind: limit_kind,
                    limit: reported_limit,
                });
            }
            if self.fill(operation)? == 0 {
                return Err(Error::PrematureEof);
            }
        }
    }

    fn fill(&mut self, operation: Operation) -> Result<usize> {
        self.compact_if_consumed();
        let mut incoming = [0_u8; 8 * 1024];
        let count = self.read_socket(&mut incoming, operation)?;
        self.buffer.extend_from_slice(&incoming[..count]);
        Ok(count)
    }

    fn read_socket(&mut self, output: &mut [u8], operation: Operation) -> Result<usize> {
        let started = Instant::now();
        loop {
            check_control(&self.cancellation, self.deadline, operation)?;
            let wait = next_wait(started, self.read_timeout, self.deadline, operation)?;
            self.stream
                .set_read_timeout(Some(wait))
                .map_err(|error| Error::io(operation, &error))?;
            match self.stream.read(output) {
                Ok(count) => return Ok(count),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) => {}
                Err(error) => return Err(Error::io(operation, &error)),
            }
        }
    }

    fn compact_if_consumed(&mut self) {
        if self.cursor == self.buffer.len() {
            self.buffer.clear();
            self.cursor = 0;
        }
    }
}

fn parse_chunk_size(line: &[u8]) -> Result<u64> {
    let size_end = line
        .iter()
        .position(|byte| *byte == b';')
        .unwrap_or(line.len());
    let size_bytes = &line[..size_end];
    if size_bytes.is_empty() || !size_bytes.iter().all(u8::is_ascii_hexdigit) {
        return Err(Error::MalformedChunkSize);
    }
    let mut size = 0_u64;
    for digit in size_bytes {
        size = size
            .checked_mul(16)
            .and_then(|value| value.checked_add(u64::from(hex_value(*digit))))
            .ok_or(Error::MalformedChunkSize)?;
    }
    validate_chunk_extensions(&line[size_end..])?;
    Ok(size)
}

const fn hex_value(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => 0,
    }
}

fn validate_chunk_extensions(mut input: &[u8]) -> Result<()> {
    while !input.is_empty() {
        if input.first() != Some(&b';') {
            return Err(Error::MalformedChunkSize);
        }
        input = &input[1..];
        let name_length = input
            .iter()
            .take_while(|byte| is_token_byte(**byte))
            .count();
        if name_length == 0 {
            return Err(Error::MalformedChunkSize);
        }
        input = &input[name_length..];
        if input.first() != Some(&b'=') {
            continue;
        }
        input = &input[1..];
        if input.first() == Some(&b'"') {
            input = consume_quoted_string(input)?;
        } else {
            let value_length = input
                .iter()
                .take_while(|byte| is_token_byte(**byte))
                .count();
            if value_length == 0 {
                return Err(Error::MalformedChunkSize);
            }
            input = &input[value_length..];
        }
    }
    Ok(())
}

fn consume_quoted_string(input: &[u8]) -> Result<&[u8]> {
    let mut index = 1;
    while index < input.len() {
        match input[index] {
            b'"' => return Ok(&input[index + 1..]),
            b'\\' => {
                index += 1;
                let escaped = input.get(index).copied().ok_or(Error::MalformedChunkSize)?;
                if !is_quoted_pair_byte(escaped) {
                    return Err(Error::MalformedChunkSize);
                }
            }
            byte if is_quoted_text_byte(byte) => {}
            _ => return Err(Error::MalformedChunkSize),
        }
        index += 1;
    }
    Err(Error::MalformedChunkSize)
}

const fn is_quoted_text_byte(byte: u8) -> bool {
    byte == b'\t'
        || byte == b' '
        || byte == b'!'
        || (byte >= 0x23 && byte <= 0x5b)
        || (byte >= 0x5d && byte <= 0x7e)
        || byte >= 0x80
}

const fn is_quoted_pair_byte(byte: u8) -> bool {
    byte == b'\t' || byte == b' ' || (byte >= 0x21 && byte <= 0x7e) || byte >= 0x80
}

fn read_trailers(
    reader: &mut WireReader,
    trailers: &mut Headers,
    header_bytes: &mut usize,
    header_count: &mut usize,
    max_header_bytes: usize,
    max_header_count: usize,
) -> Result<()> {
    loop {
        let line = read_metadata_line(reader, header_bytes, max_header_bytes, Operation::ReadBody)?;
        if line.is_empty() {
            return Ok(());
        }
        *header_count = header_count.checked_add(1).ok_or(Error::LimitExceeded {
            kind: LimitKind::HeaderCount,
            limit: max_header_count,
        })?;
        if *header_count > max_header_count {
            return Err(Error::LimitExceeded {
                kind: LimitKind::HeaderCount,
                limit: max_header_count,
            });
        }
        let (name, value) = parse_header_line(&line)?;
        if is_prohibited_trailer(&name) {
            return Err(Error::ProhibitedTrailer(name.as_str().to_owned()));
        }
        trailers.append(name, value);
    }
}

fn is_prohibited_trailer(name: &HeaderName) -> bool {
    [
        "transfer-encoding",
        "content-length",
        "host",
        "trailer",
        "connection",
        "upgrade",
        "content-encoding",
    ]
    .iter()
    .any(|prohibited| name.is(prohibited))
}
