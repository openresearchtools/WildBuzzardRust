// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bounded HTTP/1.1 transports for Wild Buzzard.
//!
//! [`HttpClient`] retains the original numeric-loopback-only capability.
//! [`GeneralWebClient`] is a separate capability for bounded DNS and
//! authenticated HTTPS. Higher-level Fetch behavior, including CORS and
//! redirect semantics, belongs to the web-platform layer rather than either
//! transport.

#![forbid(unsafe_code)]

mod client;
mod error;
mod general;
mod message;
mod target;

pub use client::{Body, ClientConfig, HttpClient, Response};
pub use error::{
    CertificateFailure, DnsFailure, Error, LimitKind, Operation, Result, TlsFailure,
    TrustStoreFailure,
};
pub use general::{
    AlpnOutcome, CommittedResponseAuthority, ConnectionSecurity, GeneralWebClient,
    GeneralWebConfig, GeneralWebExecutionError, GeneralWebNetworkAccess, GeneralWebPolicyError,
    GeneralWebRequest, GeneralWebResponse, GeneralWebTransportFailure, IpAddressSpace,
    LocalNetworkAccessPermissions, LocalNetworkPermission, LocalNetworkTarget, TlsVersion,
    TrustStore, classify_ip_address_space, is_restricted_web_port,
};
pub use message::{
    BodyFraming, ConnectionDisposition, HeaderName, HeaderValue, Headers, HttpVersion, Method,
    RedirectPolicy, Request, ResponseHead, StatusCode,
};
pub use target::{
    GeneralWebTarget, LoopbackTarget, Origin, RequestTarget, WebHost, WebOrigin, WebScheme,
};
pub use wild_buzzard_runtime::{CancellationSource, CancellationToken};
