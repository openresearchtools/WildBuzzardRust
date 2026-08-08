// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A small, bounded HTTP/1.1 transport for Wild Buzzard's first vertical slice.
//!
//! The only connectable targets in this crate are numeric loopback addresses.
//! Higher-level Fetch behavior, including CORS and redirect semantics, belongs
//! to the web-platform layer rather than this transport.

#![forbid(unsafe_code)]

mod client;
mod error;
mod message;
mod target;

pub use client::{Body, ClientConfig, HttpClient, Response};
pub use error::{Error, LimitKind, Operation, Result};
pub use message::{
    BodyFraming, ConnectionDisposition, HeaderName, HeaderValue, Headers, HttpVersion, Method,
    RedirectPolicy, Request, ResponseHead, StatusCode,
};
pub use target::{LoopbackTarget, Origin, RequestTarget};
pub use wild_buzzard_runtime::{CancellationSource, CancellationToken};
