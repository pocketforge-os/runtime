//! https URLs, redirect resolution and the fetch host scope (tsp-ziac.4.3.5 design item 4).
//!
//! Scope = (host, port) pairs. The first hop is each `[[fetch.files]].url`; redirect hops
//! may additionally reach the platform-owned [`REDIRECT_TABLE`] targets of a declared host
//! on 443. Apps cannot widen the table: a CDN move needs a system update, hence the
//! `source_moved` class of `redirect_refused`. The scope is DECLARED and checked here,
//! before every connect; it is not kernel-enforced (see pf-app-fetch@.service).

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

/// Declared host -> hosts its redirects may reach (port 443). Reviewed platform code.
pub const REDIRECT_TABLE: &[(&str, &[&str])] = &[(
    "github.com",
    &[
        "release-assets.githubusercontent.com",
        "objects.githubusercontent.com",
    ],
)];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Url {
    pub host: String,
    pub port: u16,
    /// Path plus query, always starting with `/`; never a fragment.
    pub target: String,
}

impl Url {
    pub fn as_string(&self) -> String {
        if self.port == 443 {
            format!("https://{}{}", self.host, self.target)
        } else {
            format!("https://{}:{}{}", self.host, self.port, self.target)
        }
    }
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.parse::<IpAddr>().is_err()
        && !host.ends_with('.')
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
        // WHATWG: a numeric last label makes the host an IPv4 literal (e.g. 0x7f.1, 2130706433).
        && !host.rsplit('.').next().is_some_and(|last| {
            last.bytes().all(|b| b.is_ascii_digit())
                || last
                    .strip_prefix("0x")
                    .is_some_and(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))
        })
}

fn valid_target(target: &str) -> bool {
    target.starts_with('/') && target.bytes().all(|b| (0x21..0x7f).contains(&b))
}

/// Parse an absolute `https://host[:port]/path?query` URL. No userinfo, no IP literal, an
/// ASCII DNS name (IDNs must be given in their `xn--` form).
pub fn parse_https(raw: &str) -> Result<Url, String> {
    let rest = raw
        .strip_prefix("https://")
        .ok_or_else(|| format!("not an https URL: {raw}"))?;
    let rest = rest.split('#').next().unwrap_or_default();
    let split = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, target) = rest.split_at(split);
    if authority.contains('@') {
        return Err(format!("userinfo in URL: {raw}"));
    }
    if authority.starts_with('[') {
        return Err(format!("IP literal in URL: {raw}"));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => {
            let port: u16 = port
                .parse()
                .ok()
                .filter(|p| *p != 0)
                .ok_or_else(|| format!("bad port in URL: {raw}"))?;
            (host, port)
        }
        None => (authority, 443),
    };
    let host = host.to_ascii_lowercase();
    if !valid_host(&host) {
        return Err(format!("bad host in URL: {raw}"));
    }
    let target = if target.is_empty() {
        "/".to_string()
    } else if target.starts_with('?') {
        format!("/{target}")
    } else {
        target.to_string()
    };
    if !valid_target(&target) {
        return Err(format!("bad path in URL: {raw}"));
    }
    Ok(Url {
        host,
        port,
        target: normalize_target(&target),
    })
}

fn normalize_target(target: &str) -> String {
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (target, None),
    };
    let mut out: Vec<&str> = Vec::new();
    let segments: Vec<&str> = path.split('/').skip(1).collect();
    let last = segments.len().saturating_sub(1);
    for (index, segment) in segments.iter().enumerate() {
        match *segment {
            "." => {
                if index == last {
                    out.push("");
                }
            }
            ".." => {
                out.pop();
                if index == last {
                    out.push("");
                }
            }
            other => out.push(other),
        }
    }
    let mut result = format!("/{}", out.join("/"));
    if let Some(query) = query {
        result.push('?');
        result.push_str(query);
    }
    result
}

/// Resolve a `Location` value against the current URL (RFC 3986 §5.2, https only).
pub fn resolve(base: &Url, location: &str) -> Result<Url, String> {
    let location = location.trim();
    if location.is_empty() {
        return Err("empty Location".into());
    }
    let scheme_end = location.find(':');
    let first_delim = location.find(['/', '?', '#']).unwrap_or(location.len());
    if let Some(colon) = scheme_end {
        if colon < first_delim {
            return parse_https(location);
        }
    }
    if let Some(rest) = location.strip_prefix("//") {
        return parse_https(&format!("https://{rest}"));
    }
    let location = location.split('#').next().unwrap_or_default();
    let target = if location.starts_with('/') {
        location.to_string()
    } else if location.starts_with('?') {
        let path = base.target.split('?').next().unwrap_or("/");
        format!("{path}{location}")
    } else {
        let path = base.target.split('?').next().unwrap_or("/");
        let dir = &path[..=path.rfind('/').unwrap_or(0)];
        format!("{dir}{location}")
    };
    if target.is_empty() {
        return Ok(base.clone());
    }
    if !valid_target(&target) {
        return Err(format!("bad Location path: {location}"));
    }
    Ok(Url {
        host: base.host.clone(),
        port: base.port,
        target: normalize_target(&target),
    })
}

#[derive(Clone, Debug, Default)]
pub struct Scope {
    allowed: BTreeSet<(String, u16)>,
}

impl Scope {
    /// The declared first hops plus `table`'s redirect targets of each declared host.
    pub fn new(first_hops: &[Url], table: &BTreeMap<String, Vec<String>>) -> Scope {
        let mut allowed = BTreeSet::new();
        for url in first_hops {
            allowed.insert((url.host.clone(), url.port));
            for target in table.get(&url.host).into_iter().flatten() {
                allowed.insert((target.clone(), 443));
            }
        }
        Scope { allowed }
    }

    pub fn allows(&self, url: &Url) -> bool {
        self.allowed.contains(&(url.host.clone(), url.port))
    }

    pub fn ports(&self) -> BTreeSet<u16> {
        self.allowed.iter().map(|(_, port)| *port).collect()
    }
}

pub fn platform_table() -> BTreeMap<String, Vec<String>> {
    REDIRECT_TABLE
        .iter()
        .map(|(from, to)| {
            (
                (*from).to_string(),
                to.iter().map(|t| (*t).to_string()).collect(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(raw: &str) -> Url {
        parse_https(raw).unwrap()
    }

    #[test]
    fn parses_and_refuses() {
        let url = u("https://GitHub.com/a/b?x=1#frag");
        assert_eq!((url.host.as_str(), url.port), ("github.com", 443));
        assert_eq!(url.target, "/a/b?x=1");
        assert_eq!(u("https://h.example:8443").target, "/");
        for bad in [
            "http://h.example/",
            "https://u:p@h.example/",
            "https://127.0.0.1/",
            "https://[::1]/",
            "https://0x7f.1/",
            "https://h.example:0/",
            "https://h.example:99999/",
            "https:///x",
            "https://bücher.example/",
            "https://h.example/a b",
            "ftp://h.example/",
        ] {
            assert!(parse_https(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn resolves_relative_locations() {
        let base = u("https://h.example/a/b/c?q");
        assert_eq!(
            resolve(&base, "/x").unwrap().as_string(),
            "https://h.example/x"
        );
        assert_eq!(
            resolve(&base, "d").unwrap().as_string(),
            "https://h.example/a/b/d"
        );
        assert_eq!(
            resolve(&base, "../d").unwrap().as_string(),
            "https://h.example/a/d"
        );
        assert_eq!(
            resolve(&base, "?z").unwrap().as_string(),
            "https://h.example/a/b/c?z"
        );
        assert_eq!(
            resolve(&base, "//o.example/p").unwrap().as_string(),
            "https://o.example/p"
        );
        assert!(resolve(&base, "http://h.example/x").is_err());
        assert!(resolve(&base, "javascript:alert(1)").is_err());
        assert!(resolve(&base, "").is_err());
    }

    #[test]
    fn github_release_redirect_is_in_scope_and_others_are_not() {
        let first = u("https://github.com/o/r/releases/download/t/a.tar.gz");
        let scope = Scope::new(&[first], &platform_table());
        assert!(scope.allows(&u("https://release-assets.githubusercontent.com/x")));
        assert!(scope.allows(&u("https://objects.githubusercontent.com/x")));
        assert!(scope.allows(&u("https://github.com/elsewhere")));
        assert!(!scope.allows(&u("https://evil.example/x")));
        assert!(!scope.allows(&u("https://objects.githubusercontent.com:444/x")));
        assert_eq!(scope.ports(), BTreeSet::from([443]));
    }
}
