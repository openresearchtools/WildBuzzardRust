// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::net::{IpAddr, SocketAddr};

use url::{Host, Url};

use crate::{Error, Result};

pub(crate) const MAX_GENERAL_URL_BYTES: usize = 2 * 1024 * 1024;

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

/// An explicitly requested cleartext or TLS-protected web transport.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebScheme {
    /// Cleartext HTTP without an implicit upgrade or insecure fallback.
    Http,
    /// HTTP over an authenticated TLS connection.
    Https,
}

impl WebScheme {
    /// Returns the normalized URL scheme.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }

    const fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https => 443,
        }
    }
}

/// A normalized host from a general-web URL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WebHost {
    /// A WHATWG-normalized ASCII domain name.
    Domain(String),
    /// A numeric IPv4 or IPv6 address.
    Ip(IpAddr),
}

impl WebHost {
    /// Returns the normalized host text without IPv6 authority brackets.
    #[must_use]
    pub fn as_str(&self) -> String {
        match self {
            Self::Domain(domain) => domain.clone(),
            Self::Ip(address) => address.to_string(),
        }
    }

    pub(crate) fn ip_addr(&self) -> Option<IpAddr> {
        match self {
            Self::Domain(_) => None,
            Self::Ip(address) => Some(*address),
        }
    }

    pub(crate) fn domain(&self) -> Option<&str> {
        match self {
            Self::Domain(domain) => Some(domain),
            Self::Ip(_) => None,
        }
    }
}

/// A normalized general-web origin.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebOrigin {
    scheme: WebScheme,
    host: WebHost,
    port: u16,
    authority: String,
}

impl WebOrigin {
    /// Returns the explicit transport scheme.
    #[must_use]
    pub const fn scheme(&self) -> WebScheme {
        self.scheme
    }

    /// Returns the WHATWG-normalized host.
    #[must_use]
    pub const fn host(&self) -> &WebHost {
        &self.host
    }

    /// Returns the effective TCP port.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    pub(crate) fn authority(&self) -> &str {
        &self.authority
    }
}

/// A validated target for the explicit general-web transport capability.
///
/// Unlike [`LoopbackTarget`], this type can name DNS hosts and non-loopback
/// numeric addresses. Constructing it grants no socket or resolver authority;
/// only [`crate::GeneralWebClient`] consumes it.
#[derive(Clone, Debug)]
pub struct GeneralWebTarget {
    url: Url,
    origin: WebOrigin,
    request_target: RequestTarget,
}

impl GeneralWebTarget {
    /// Parses a bounded, credential-free and fragment-free HTTP(S) URL.
    ///
    /// # Errors
    ///
    /// Returns a structured URL, scheme, credential, fragment, port, or
    /// resource-limit failure.
    pub fn parse(input: &str) -> Result<Self> {
        if input.len() > MAX_GENERAL_URL_BYTES {
            return Err(Error::LimitExceeded {
                kind: crate::LimitKind::UrlBytes,
                limit: MAX_GENERAL_URL_BYTES,
            });
        }
        let url = Url::parse(input).map_err(|error| Error::InvalidUrl(error.to_string()))?;
        Self::from_url(url)
    }

    /// Validates a previously parsed WHATWG URL for general-web transport.
    ///
    /// # Errors
    ///
    /// Returns a structured scheme, credential, fragment, port, or
    /// resource-limit failure.
    pub fn from_url(url: Url) -> Result<Self> {
        if url.as_str().len() > MAX_GENERAL_URL_BYTES {
            return Err(Error::LimitExceeded {
                kind: crate::LimitKind::UrlBytes,
                limit: MAX_GENERAL_URL_BYTES,
            });
        }
        let scheme = match url.scheme() {
            "http" => WebScheme::Http,
            "https" => WebScheme::Https,
            other => return Err(Error::UnsupportedScheme(other.to_owned())),
        };
        if !url.username().is_empty() || url.password().is_some() {
            return Err(Error::CredentialsNotAllowed);
        }
        if url.fragment().is_some() {
            return Err(Error::FragmentNotAllowed);
        }

        let host = match url.host() {
            Some(Host::Domain(domain)) => WebHost::Domain(domain.to_owned()),
            Some(Host::Ipv4(address)) => WebHost::Ip(IpAddr::V4(address)),
            Some(Host::Ipv6(address)) => WebHost::Ip(IpAddr::V6(address)),
            None => return Err(Error::MissingHost),
        };
        let port = url.port_or_known_default().ok_or(Error::MissingPort)?;
        if port == 0 {
            return Err(Error::MissingPort);
        }

        let mut serialized_target = url.path().to_owned();
        if let Some(query) = url.query() {
            serialized_target.push('?');
            serialized_target.push_str(query);
        }
        let request_target = RequestTarget::new(serialized_target)?;
        let authority = serialize_web_authority(&host, port, scheme);

        Ok(Self {
            url,
            origin: WebOrigin {
                scheme,
                host,
                port,
                authority,
            },
            request_target,
        })
    }

    /// Returns the parsed WHATWG URL.
    #[must_use]
    pub const fn url(&self) -> &Url {
        &self.url
    }

    /// Returns the validated general-web origin.
    #[must_use]
    pub const fn origin(&self) -> &WebOrigin {
        &self.origin
    }

    /// Returns the validated HTTP origin-form request target.
    #[must_use]
    pub const fn request_target(&self) -> &RequestTarget {
        &self.request_target
    }
}

fn serialize_web_authority(host: &WebHost, port: u16, scheme: WebScheme) -> String {
    let include_port = port != scheme.default_port();
    match (host, include_port) {
        (WebHost::Domain(host), false) => host.clone(),
        (WebHost::Domain(host), true) => format!("{host}:{port}"),
        (WebHost::Ip(IpAddr::V4(host)), false) => host.to_string(),
        (WebHost::Ip(IpAddr::V4(host)), true) => format!("{host}:{port}"),
        (WebHost::Ip(IpAddr::V6(host)), false) => format!("[{host}]"),
        (WebHost::Ip(IpAddr::V6(host)), true) => format!("[{host}]:{port}"),
    }
}
