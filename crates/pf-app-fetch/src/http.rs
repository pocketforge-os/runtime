//! The hop loop and the bounded download (design item 4).
//!
//! ureq follows no redirects (`max_redirects(0)`) and turns no status into an error: every
//! hop's `Location` is resolved, checked for https and scope BEFORE it connects, and at
//! most [`MAX_REDIRECTS`] hops are followed with no URL repeated. The body is read raw
//! (`Accept-Encoding: identity`, no decompression feature), at most `size + 1` bytes, then
//! checked for size and SHA256.

use crate::codes::{io_failure, Code, Failure, Result};
use crate::descriptor::Entry;
use crate::scope::{resolve, Scope};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use ureq::config::Config;
use ureq::http::Uri;
use ureq::tls::{Certificate, RootCerts, TlsConfig, TlsProvider};
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

pub const MAX_REDIRECTS: usize = 5;

/// Public unicast destinations only (defence in depth; the unit's IPAddressDeny= is the
/// enforced copy of this rule).
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_unspecified()
                || v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
                || o[0] == 0
                || o[0] >= 240
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || (o[0] == 198 && (o[1] & 0xfe) == 18))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let s = v6.segments();
            !(v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
                || (s[0] == 0x2001 && s[1] == 0x0db8)
                || (s[0] == 0x0064 && s[1] == 0xff9b))
        }
    }
}

#[derive(Debug)]
struct ScopedResolver {
    overrides: BTreeMap<String, SocketAddr>,
    inner: DefaultResolver,
}

impl Resolver for ScopedResolver {
    fn resolve(
        &self,
        uri: &Uri,
        config: &Config,
        timeout: NextTimeout,
    ) -> std::result::Result<ResolvedSocketAddrs, ureq::Error> {
        let mut out = self.empty();
        if let Some(addr) = uri.host().and_then(|host| self.overrides.get(host)) {
            out.push(*addr);
            return Ok(out);
        }
        for addr in self.inner.resolve(uri, config, timeout)?.iter() {
            if is_public(addr.ip()) {
                out.push(*addr);
            }
        }
        if out.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        Ok(out)
    }
}

pub struct Client {
    agent: ureq::Agent,
    scope: Scope,
    pub gets: usize,
}

impl Client {
    pub fn new(
        roots: Vec<Certificate<'static>>,
        scope: Scope,
        overrides: BTreeMap<String, SocketAddr>,
    ) -> Client {
        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .root_certs(RootCerts::new_with_certs(&roots))
            .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .build();
        let config = ureq::Agent::config_builder()
            .tls_config(tls)
            .https_only(true)
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_resolve(Some(Duration::from_secs(15)))
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .timeout_per_read(Some(Duration::from_secs(60)))
            .user_agent(concat!("pf-app-fetch/", env!("CARGO_PKG_VERSION")))
            .build();
        let resolver = ScopedResolver {
            overrides,
            inner: DefaultResolver::default(),
        };
        Client {
            agent: ureq::Agent::with_parts(config, DefaultConnector::default(), resolver),
            scope,
            gets: 0,
        }
    }

    /// GET `entry.url`, following in-scope redirects, and stream exactly the pinned bytes
    /// into `out`. Returns once size and SHA256 match the pin.
    pub fn download(
        &mut self,
        entry: &Entry,
        out: &mut std::fs::File,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let mut url = entry.url.clone();
        let mut seen = BTreeSet::new();
        let mut redirects = 0;
        let response = loop {
            if !self.scope.allows(&url) {
                return Err(Failure::new(
                    Code::RedirectRefused,
                    format!("{} is outside the fetch host scope", url.as_string()),
                ));
            }
            if !seen.insert(url.as_string()) {
                return Err(Failure::new(
                    Code::RedirectRefused,
                    format!("redirect loop at {}", url.as_string()),
                ));
            }
            if cancel.load(Ordering::SeqCst) {
                return Err(Failure::new(Code::Cancelled, "stop requested"));
            }
            self.gets += 1;
            let response = self
                .agent
                .get(url.as_string())
                .header("Accept-Encoding", "identity")
                .call()
                .map_err(|error| {
                    if cancel.load(Ordering::SeqCst) {
                        Failure::new(Code::Cancelled, "stop requested")
                    } else {
                        Failure::new(Code::Offline, format!("{}: {error}", url.as_string()))
                    }
                })?;
            let status = response.status().as_u16();
            match status {
                200 => break response,
                301 | 302 | 303 | 307 | 308 => {
                    let location = response
                        .headers()
                        .get("location")
                        .and_then(|v| v.to_str().ok())
                        .ok_or_else(|| {
                            Failure::new(
                                Code::RedirectRefused,
                                format!("{status} without a usable Location"),
                            )
                        })?;
                    let next = resolve(&url, location)
                        .map_err(|e| Failure::new(Code::RedirectRefused, e))?;
                    redirects += 1;
                    if redirects > MAX_REDIRECTS {
                        return Err(Failure::new(
                            Code::RedirectRefused,
                            format!("more than {MAX_REDIRECTS} redirects"),
                        ));
                    }
                    url = next;
                }
                300..=399 if status != 304 => {
                    return Err(Failure::new(
                        Code::RedirectRefused,
                        format!("redirect status {status} is not followed"),
                    ))
                }
                408 | 429 | 500..=599 => {
                    return Err(Failure::new(Code::HttpError, format!("HTTP {status}")))
                }
                400..=499 => return Err(Failure::new(Code::HttpRefused, format!("HTTP {status}"))),
                _ => return Err(Failure::new(Code::HttpError, format!("HTTP {status}"))),
            }
        };
        let declared = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        if let Some(length) = declared {
            if length > entry.size {
                return Err(Failure::new(
                    Code::DownloadTooLarge,
                    format!("Content-Length {length} exceeds pinned size {}", entry.size),
                ));
            }
        }
        let mut reader = response
            .into_body()
            .into_with_config()
            .limit(u64::MAX)
            .reader();
        let mut hasher = Sha256::new();
        let mut total: u64 = 0;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(Failure::new(Code::Cancelled, "stop requested"));
            }
            let want = usize::try_from((entry.size + 1 - total).min(buf.len() as u64))
                .unwrap_or(buf.len());
            let n = match reader.read(&mut buf[..want]) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    if cancel.load(Ordering::SeqCst) {
                        return Err(Failure::new(Code::Cancelled, "stop requested"));
                    }
                    return Err(Failure::new(
                        Code::DownloadTruncated,
                        format!("body ended after {total} bytes: {e}"),
                    ));
                }
            };
            total += n as u64;
            if total > entry.size {
                return Err(Failure::new(
                    Code::DownloadTooLarge,
                    format!("body exceeds pinned size {}", entry.size),
                ));
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])
                .map_err(|e| io_failure(Code::StateInvalid, "write archive", &e))?;
        }
        if let Some(length) = declared {
            if total < length {
                return Err(Failure::new(
                    Code::DownloadTruncated,
                    format!("body ended after {total} of {length} bytes"),
                ));
            }
        }
        if total != entry.size {
            return Err(Failure::new(
                Code::SizeMismatch,
                format!("body is {total} bytes, pinned size is {}", entry.size),
            ));
        }
        let digest: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if digest != entry.sha256 {
            return Err(Failure::new(
                Code::DigestMismatch,
                format!("sha256 {digest}, pinned {}", entry.sha256),
            ));
        }
        out.sync_all()
            .map_err(|e| io_failure(Code::StateInvalid, "fsync archive", &e))?;
        Ok(())
    }
}

/// The trusted roots: the image CA bundle (PEM), or the debug-build test CA (DER).
pub fn load_roots(ca_file: &std::path::Path) -> Result<Vec<Certificate<'static>>> {
    if let Some(test_ca) = crate::hooks::var("PF_APP_FETCH_TEST_CA") {
        let der = std::fs::read(&test_ca)
            .map_err(|e| Failure::new(Code::StateInvalid, format!("read test CA: {e}")))?;
        return Ok(vec![Certificate::from_der(&der).to_owned()]);
    }
    let pem = std::fs::read(ca_file).map_err(|e| {
        Failure::new(
            Code::StateInvalid,
            format!("read CA bundle {}: {e}", ca_file.display()),
        )
    })?;
    let roots: Vec<Certificate<'static>> = ureq::tls::parse_pem(&pem)
        .filter_map(|item| match item {
            Ok(ureq::tls::PemItem::Certificate(cert)) => Some(cert),
            _ => None,
        })
        .collect();
    if roots.is_empty() {
        return Err(Failure::new(
            Code::StateInvalid,
            format!("no certificates in {}", ca_file.display()),
        ));
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::is_public;

    #[test]
    fn only_public_unicast_is_public() {
        for ip in ["140.82.112.3", "2606:50c0:8000::154", "1.1.1.1"] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.1.1",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "fe80::1",
            "fd00::1",
            "::ffff:192.168.1.1",
            "ff02::1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
    }
}
