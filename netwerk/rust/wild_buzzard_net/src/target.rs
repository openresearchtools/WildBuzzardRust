// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::net::{IpAddr, SocketAddr};

use url::{Host, Url};

use crate::{Error, Result};

/// An HTTP origin whose host is a numeric loopback IP address.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Origin {
    host: IpAddr,
    port: u16,
    authority: String,
}

impl Origin {
    /// Returns the numeric host address.
    #[must_use]
    pub const fn host(&self) -> IpAddr {
        self.host
    }

    /// Returns the effective TCP port.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Returns the socket address used without any DNS resolution.
    #[must_use]
    pub const fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.host, self.port)
    }

    pub(crate) fn authority(&self) -> &str {
        &self.authority
    }
}

/// A validated HTTP origin-form request target such as `/path?query`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestTarget(String);

impl RequestTarget {
    /// Validates an already serialized origin-form request target.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequestTarget`] unless the value is an ASCII,
    /// control-free origin-form target beginning with `/` and without a fragment.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if !value.starts_with('/')
            || value.contains('#')
            || !value.is_ascii()
            || value.bytes().any(|byte| byte <= 0x20 || byte == 0x7f)
        {
            return Err(Error::InvalidRequestTarget);
        }
        Ok(Self(value))
    }

    /// Returns the serialized origin-form target.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A target proven to use cleartext HTTP and a numeric loopback address.
///
/// Domain names, including `localhost`, are intentionally rejected so this
/// wave never invokes DNS and cannot silently expand beyond loopback.
#[derive(Clone, Debug)]
pub struct LoopbackTarget {
    url: Url,
    origin: Origin,
    request_target: RequestTarget,
}

impl LoopbackTarget {
    /// Parses and validates a loopback-only HTTP URL with WHATWG URL rules.
    ///
    /// # Errors
    ///
    /// Returns a structured URL or target-policy error when the input is not a
    /// credential-free, fragment-free `http` URL using a numeric loopback IP.
    pub fn parse(input: &str) -> Result<Self> {
        let url = Url::parse(input).map_err(|error| Error::InvalidUrl(error.to_string()))?;
        Self::from_url(url)
    }

    /// Validates a previously parsed WHATWG URL.
    ///
    /// # Errors
    ///
    /// Returns a target-policy error when the URL is not credential-free,
    /// fragment-free cleartext HTTP on a numeric loopback IP.
    pub fn from_url(url: Url) -> Result<Self> {
        if url.scheme() != "http" {
            return Err(Error::UnsupportedScheme(url.scheme().to_owned()));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(Error::CredentialsNotAllowed);
        }
        if url.fragment().is_some() {
            return Err(Error::FragmentNotAllowed);
        }

        let host = match url.host() {
            Some(Host::Ipv4(address)) if address.is_loopback() => IpAddr::V4(address),
            Some(Host::Ipv6(address)) if address.is_loopback() => IpAddr::V6(address),
            Some(Host::Domain(_) | Host::Ipv4(_) | Host::Ipv6(_)) | None => {
                return Err(Error::NonLoopbackTarget);
            }
        };
        let port = url.port_or_known_default().ok_or(Error::MissingPort)?;
        let mut serialized_target = url.path().to_owned();
        if let Some(query) = url.query() {
            serialized_target.push('?');
            serialized_target.push_str(query);
        }
        let request_target = RequestTarget::new(serialized_target)?;

        let authority = serialize_authority(host, port);
        Ok(Self {
            url,
            origin: Origin {
                host,
                port,
                authority,
            },
            request_target,
        })
    }

    /// Returns the parsed URL.
    #[must_use]
    pub const fn url(&self) -> &Url {
        &self.url
    }

    /// Returns the validated loopback origin.
    #[must_use]
    pub const fn origin(&self) -> &Origin {
        &self.origin
    }

    /// Returns the validated HTTP origin-form request target.
    #[must_use]
    pub const fn request_target(&self) -> &RequestTarget {
        &self.request_target
    }
}

fn serialize_authority(host: IpAddr, port: u16) -> String {
    match host {
        IpAddr::V4(host) if port == 80 => host.to_string(),
        IpAddr::V4(host) => format!("{host}:{port}"),
        IpAddr::V6(host) if port == 80 => format!("[{host}]"),
        IpAddr::V6(host) => format!("[{host}]:{port}"),
    }
}
