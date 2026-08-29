// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! URLs the app is willing to fetch.
//!
//! Token metadata URIs are written on chain by whoever launched the coin, and
//! both chains have a path that follows one to read socials. An unguarded
//! fetch turns "look at this token" into "make my machine request an address
//! of the attacker's choosing" — `localhost`, the LAN, a validator's own RPC
//! on 8899, a cloud metadata endpoint. One guard, used by both sides, because
//! the Solana path shipped it first and the EVM path went without.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// The host and port a URL addresses, credentials dropped, host lowercased.
///
/// Split out of `public_host` because the guard now needs the host as a name
/// it can resolve, not only as a string it can pattern-match.
fn host_port(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    // An IPv6 literal wears brackets, and the colons inside them are not the
    // port separator. The old one-line split treated them as one and reduced
    // `[::1]` to an empty host — refused, but by accident, and it refused
    // every public IPv6 literal the same way.
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        let (h, after) = inner.split_once(']')?;
        (h, after.strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return None;
    }
    let port = match port {
        Some(p) => p.parse().ok()?,
        None if scheme.eq_ignore_ascii_case("http") => 80,
        None => 443,
    };
    Some((host.to_ascii_lowercase(), port))
}

/// True when an address is one the wider internet could have answered with —
/// not loopback, not private, link-local, or otherwise only-reachable-here.
///
/// The v6 arm used to check loopback and unspecified alone. That was survivable
/// while only IP literals in a URL reached it; it is not, now that a resolver's
/// answers do. `fc00::/7` and `fe80::/10` are the v6 spellings of the v4 ranges
/// just above, and `::ffff:10.0.0.1` is the v4 ranges themselves wearing a v6
/// address.
pub fn public_ip(ip: IpAddr) -> bool {
    fn public_v4(v4: Ipv4Addr) -> bool {
        !(v4.is_loopback()
            || v4.is_private()
            || v4.is_link_local()
            || v4.is_broadcast()
            || v4.is_unspecified()
            || v4.octets()[0] == 0
            // Carrier-grade NAT and the cloud metadata neighbourhood.
            || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1])))
    }
    fn public_v6(v6: Ipv6Addr) -> bool {
        if v6.is_loopback() || v6.is_unspecified() {
            return false;
        }
        // `::ffff:a.b.c.d` and the deprecated `::a.b.c.d` are v4 in a v6 coat.
        if let Some(v4) = v6.to_ipv4() {
            return public_v4(v4);
        }
        let head = v6.segments()[0];
        !((head & 0xfe00) == 0xfc00 || (head & 0xffc0) == 0xfe80)
    }
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => public_v6(v6),
    }
}

/// True when a URL's host is a public name — not loopback, not private or
/// link-local, not a bare `.local`.
///
/// This reads the string and nothing else, so it settles IP literals and only
/// IP literals. A NAME that is not obviously local passes here and is decided
/// later, by `guarded_client`, which resolves it. Keep both: this one is cheap,
/// synchronous, and is what decides which candidate URLs are worth queueing.
pub fn public_host(url: &str) -> bool {
    let Some((host, _)) = host_port(url) else { return false };
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return false;
    }
    match host.parse::<IpAddr>() {
        Ok(ip) => public_ip(ip),
        // A name. Nothing in the string can settle it — see `guarded_client`.
        Err(_) => true,
    }
}

/// Every address a name answered with, judged together.
///
/// All of them, not any of them: a name that answers with one public address
/// and one loopback address is not half safe, it is a rebinding attempt with
/// the answer already in it. An empty answer is refused too — there is nothing
/// there to have approved.
fn all_public(addrs: &[SocketAddr]) -> bool {
    !addrs.is_empty() && addrs.iter().all(|a| public_ip(a.ip()))
}

/// An HTTP client that will only connect to where `url` resolved when we
/// checked it.
///
/// `public_host` reads a string, and a name is not an address. Token metadata
/// URIs are written by whoever launched the coin, so `evil.example` can carry
/// an A record for `127.0.0.1`, `10.0.0.5` or `169.254.169.254`, and a guard
/// that never resolves anything waves it straight through — the metadata-URI
/// SSRF again, with a hostname where the IP literal used to be. Requiring
/// HTTPS does not help: a DNS-01 challenge issues a publicly trusted
/// certificate for a domain whose records point anywhere at all, because it
/// proves control of the records and never asks whether the address answers
/// from the internet.
///
/// So resolve it here, judge every address the name gave back, and pin the
/// survivors onto the client. The pinning is the half that closes rebinding:
/// left to itself `reqwest` resolves the name again when it connects, and the
/// second answer does not have to be the one that was approved.
pub async fn guarded_client(url: &str, timeout: std::time::Duration) -> Option<reqwest::Client> {
    if !public_host(url) {
        return None;
    }
    let (host, port) = host_port(url)?;
    // An IP literal was already judged, in full, by `public_host`. There is no
    // name here to resolve and nothing a resolver could change underneath us.
    if host.parse::<IpAddr>().is_ok() {
        return reqwest::Client::builder().timeout(timeout).build().ok();
    }
    let addrs: Vec<SocketAddr> = match tokio::net::lookup_host((host.as_str(), port)).await {
        Ok(a) => a.collect(),
        Err(e) => {
            crate::trace(&format!("net: {host} did not resolve: {e}"));
            return None;
        }
    };
    if !all_public(&addrs) {
        // Deliberately says nothing about which address: the answer is the
        // attacker's, and echoing it back only confirms what they aimed at.
        crate::trace(&format!("net: {host} resolves to an address this machine keeps to itself; refused"));
        return None;
    }
    reqwest::Client::builder()
        .timeout(timeout)
        .resolve_to_addrs(&host, &addrs)
        .build()
        .ok()
}

/// A metadata URI turned into a URL worth fetching, or `None`.
///
/// `ipfs://` travels over a public gateway; anything else must already be
/// HTTPS to a public host. Plaintext HTTP is refused for the same reason a
/// wallet refuses it: the answer steers what the screen says about a coin.
pub fn metadata_url(uri: &str) -> Option<String> {
    let uri = uri.trim();
    if uri.is_empty() {
        return None;
    }
    let url = match uri.strip_prefix("ipfs://") {
        Some(cid) => format!("https://ipfs.io/ipfs/{}", cid.trim_start_matches('/')),
        None if uri.starts_with("https://") => uri.to_string(),
        None => return None,
    };
    public_host(&url).then_some(url)
}

/// The public gateways an IPFS document is worth asking for at once.
///
/// One gateway is one queue: ipfs.io alone regularly takes seconds to answer
/// for a CID it has not cached, and a coin that lives for minutes cannot spend
/// them waiting for its own picture. Asking several and taking the first
/// answer costs a few extra requests and turns the slowest gateway's day into
/// somebody else's problem.
///
/// cf-ipfs.com was here and is gone: Cloudflare retired it, so every fetch
/// spent a connection failing to reach it. A gateway that never answers is not
/// redundancy, it is a request nobody gets anything for.
pub const IPFS_GATEWAYS: [&str; 2] = ["https://ipfs.io", "https://dweb.link"];

/// The CID and path of an IPFS document, from either form it arrives in:
/// `ipfs://<cid>/...` or `https://<any-gateway>/ipfs/<cid>/...`.
pub fn ipfs_path(uri: &str) -> Option<String> {
    let uri = uri.trim();
    if let Some(rest) = uri.strip_prefix("ipfs://") {
        return Some(rest.trim_start_matches('/').to_string());
    }
    let (_, rest) = uri.split_once("://")?;
    let (_, after) = rest.split_once("/ipfs/")?;
    (!after.is_empty()).then(|| after.to_string())
}

/// Every URL worth racing for one document, best-effort first.
pub fn fetch_urls(uri: &str) -> Vec<String> {
    if let Some(path) = ipfs_path(uri) {
        let mut urls: Vec<String> = IPFS_GATEWAYS.iter().map(|g| format!("{g}/ipfs/{path}")).collect();
        // A document that arrived as a gateway URL names the gateway that
        // pinned it, and that one answers first — Flap's, for instance, is
        // where every Flap CID lives. Race it with the public ones rather
        // than throw the hint away; a private gateway that refuses a foreign
        // CID just loses the race.
        if uri.starts_with("https://") && !urls.iter().any(|u| u == uri) && public_host(uri) {
            urls.insert(0, uri.to_string());
        }
        urls
    } else {
        metadata_url(uri).into_iter().collect()
    }
}

/// A bare IPFS CID — a `Qm…` v0 or a `baf…` v1 — as an `ipfs://` URI, so
/// the fetch path recognises it. Flap's launch event carries the CID alone.
/// Anything already a URI passes through untouched.
pub fn ipfs_uri(s: &str) -> String {
    let s = s.trim();
    let bare = !s.contains("://")
        && !s.contains('/')
        && ((s.starts_with("Qm") && s.len() == 46) || (s.starts_with("baf") && s.len() >= 50));
    if bare {
        format!("ipfs://{s}")
    } else {
        s.to_string()
    }
}

/// Strip secrets out of any string that might quote a URL.
///
/// `reqwest`'s error `Display` embeds the whole request URL, and the transport
/// wrapper carries it through verbatim — so one unreachable endpoint puts its
/// API key into the status line, the events panel, the session log and the
/// trace file. The keys live in the path (`/v2/<KEY>`), the query
/// (`?api-key=<KEY>`) and occasionally the userinfo, so only the scheme and
/// host survive: enough to say WHICH endpoint failed, nothing that authorises
/// anyone to use it.
///
/// This matters more than a tidy log: docs/privacy.md promises keys are
/// stripped, and the issue template asks people to paste these files in public.
pub fn redact(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    loop {
        let Some(rel) = s[i..].find("://") else {
            out.push_str(&s[i..]);
            return out;
        };
        let sep = i + rel;
        // Walk back over the scheme name.
        let mut start = sep;
        while start > i {
            let c = b[start - 1];
            if c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.') {
                start -= 1;
            } else {
                break;
            }
        }
        if start == sep {
            // "://" with no scheme in front of it — not a URL.
            out.push_str(&s[i..sep + 3]);
            i = sep + 3;
            continue;
        }
        out.push_str(&s[i..sep + 3]); // everything before, plus "scheme://"
        let rest = &s[sep + 3..];
        // The URL runs to the first character that cannot be in one. `)` counts:
        // reqwest renders "error sending request for url (https://…)".
        let end = rest
            .find(|c: char| c.is_whitespace() || matches!(c, ')' | '(' | '"' | '\'' | '<' | '>' | '`' | '|' | '\\'))
            .unwrap_or(rest.len());
        let url = &rest[..end];
        let auth_end = url.find(['/', '?', '#']).unwrap_or(url.len());
        // Drop any userinfo; keep the host (and its port).
        let authority = &url[..auth_end];
        out.push_str(authority.rsplit('@').next().unwrap_or(authority));
        if auth_end < url.len() {
            out.push_str("/…");
        }
        i = sep + 3 + end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_in_an_error_keeps_its_host_and_loses_its_secrets() {
        // The exact shape reqwest produces, with a key in the path.
        let e = "error sending request for url (https://base-mainnet.g.alchemy.com/v2/SECRET_KEY_123)";
        let r = redact(e);
        assert!(!r.contains("SECRET_KEY_123"), "the key survived: {r}");
        assert!(r.contains("base-mainnet.g.alchemy.com"), "the host should survive: {r}");

        // A key in the query, and one in the userinfo.
        for (raw, secret) in [
            ("https://rpc.example.com/?api-key=SHHH", "SHHH"),
            ("https://mainnet.helius-rpc.com/?key=SHHH", "SHHH"),
            ("https://user:SHHH@rpc.example.com/v1", "SHHH"),
        ] {
            let r = redact(raw);
            assert!(!r.contains(secret), "{raw} leaked through as {r}");
        }
    }

    #[test]
    fn redaction_leaves_ordinary_text_alone() {
        for plain in ["no url here", "ratio 3:1 and a (paren)", "", "://"] {
            assert_eq!(redact(plain), plain, "{plain:?} should pass through");
        }
        // Several URLs in one line all get handled, and surrounding text stays.
        let r = redact("a https://h1.example/v2/K1 then https://h2.example/v2/K2 end");
        assert!(!r.contains("K1") && !r.contains("K2"), "{r}");
        assert!(r.starts_with("a ") && r.ends_with(" end"), "{r}");
        assert!(r.contains("h1.example") && r.contains("h2.example"), "{r}");
    }

    #[test]
    fn a_metadata_uri_cannot_point_at_the_machine_itself() {
        // Whoever launched the coin wrote this string. It must never be able
        // to aim a fetch at something only this machine can reach.
        for bad in [
            "https://localhost/x.json",
            "https://127.0.0.1/x.json",
            "https://[::1]/x.json",
            "https://10.0.0.5/x.json",
            "https://192.168.1.7:8899/x.json",
            "https://169.254.169.254/latest/meta-data",
            "https://nas.local/x.json",
            "https://user@127.0.0.1/x.json",
            "https://100.64.0.1/x.json",
            "https://0.0.0.0/x.json",
        ] {
            assert!(!public_host(bad), "{bad} should be refused");
            assert!(metadata_url(bad).is_none(), "{bad} should not be fetched");
        }
        for ok in ["https://ipfs.io/ipfs/Qm123", "https://example.com/meta.json"] {
            assert!(public_host(ok), "{ok} should be allowed");
            assert!(metadata_url(ok).is_some(), "{ok} should be fetchable");
        }
    }

    /// The finding this guard was rebuilt for: the string check never resolved
    /// anything, so any name whose records point somewhere private walked past
    /// it. The decision now happens on the ADDRESSES a name answers with, and
    /// this is that decision, tested without a resolver in the way.
    #[test]
    fn a_name_is_judged_by_every_address_it_answers_with() {
        let addr = |s: &str| SocketAddr::new(s.parse::<IpAddr>().unwrap(), 443);

        // What an attacker's A record would hand back.
        for private in ["127.0.0.1", "10.0.0.5", "192.168.1.7", "169.254.169.254", "100.64.0.1", "0.0.0.0"] {
            assert!(!all_public(&[addr(private)]), "{private} must not be connected to");
        }
        // v6, which the old check let through with only loopback covered.
        for private in ["::1", "fc00::1", "fd12:3456::1", "fe80::1", "::ffff:127.0.0.1", "::ffff:10.0.0.5"] {
            assert!(!all_public(&[addr(private)]), "{private} must not be connected to");
        }
        for public in ["93.184.216.34", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(all_public(&[addr(public)]), "{public} should be reachable");
        }

        // One good answer does not launder a bad one. This is the shape of a
        // rebinding record, and "all" is what refuses it.
        assert!(!all_public(&[addr("93.184.216.34"), addr("127.0.0.1")]));
        // Nothing to approve is not the same as approved.
        assert!(!all_public(&[]));
    }

    /// The host has to come out of the URL correctly before it can be resolved
    /// — a mis-parse is a guard aimed at the wrong name.
    #[test]
    fn the_host_and_port_survive_the_url() {
        assert_eq!(host_port("https://example.com/a.json"), Some(("example.com".into(), 443)));
        assert_eq!(host_port("http://example.com/a.json"), Some(("example.com".into(), 80)));
        assert_eq!(host_port("https://Example.COM:8443/x"), Some(("example.com".into(), 8443)));
        // Credentials are not the host, and were the shape of an older trick.
        assert_eq!(host_port("https://user:pw@example.com/x"), Some(("example.com".into(), 443)));
        // Brackets: the colons inside them are the address, not a port.
        assert_eq!(host_port("https://[2606:4700::1111]/x"), Some(("2606:4700::1111".into(), 443)));
        assert_eq!(host_port("https://[::1]:8899/x"), Some(("::1".into(), 8899)));
        assert_eq!(host_port("https:///nohost"), None);
        assert_eq!(host_port("not a url"), None);
    }

    /// A public IPv6 literal is a public host. It was refused before only
    /// because the bracket parse collapsed it to nothing.
    #[test]
    fn public_ipv6_literals_are_allowed_and_private_ones_are_not() {
        assert!(public_host("https://[2606:4700:4700::1111]/meta.json"));
        for bad in ["https://[::1]/x", "https://[fc00::1]/x", "https://[fe80::1]/x", "https://[::ffff:169.254.169.254]/x"] {
            assert!(!public_host(bad), "{bad} should be refused");
            assert!(metadata_url(bad).is_none(), "{bad} should not be fetched");
        }
    }

    /// The guard is asynchronous now, and the string-refusable cases must still
    /// be refused without ever reaching a resolver or a socket.
    #[tokio::test]
    async fn a_guarded_client_refuses_what_the_string_already_settles() {
        let t = std::time::Duration::from_millis(500);
        for bad in [
            "https://127.0.0.1/x.json",
            "https://10.0.0.5/x.json",
            "https://169.254.169.254/latest/meta-data",
            "https://localhost/x.json",
            "https://nas.local/x.json",
            "https://[::1]/x.json",
        ] {
            assert!(guarded_client(bad, t).await.is_none(), "{bad} should get no client");
        }
    }

    /// The reported proof of concept, run against the real resolver.
    ///
    /// `localtest.me` is a public domain whose A record is 127.0.0.1 — exactly
    /// the thing the report stood up with an `/etc/hosts` entry, and exactly
    /// what an attacker puts in a coin's metadata URI. It has a valid public
    /// name, no IP literal anywhere in the URL, and it could hold a trusted
    /// certificate. `public_host` says yes to the string, and the client is
    /// still refused, because the name is resolved before anything connects.
    ///
    /// Ignored by default: it is the one test here that needs DNS.
    /// Run: cargo test --features solana ssrf_live -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn ssrf_live_a_public_name_pointing_home_is_refused() {
        let url = "https://localtest.me/meta.json";
        assert!(public_host(url), "the string alone cannot tell — that is the bug");
        assert!(metadata_url(url).is_some(), "and it passes the URI filter too");
        assert!(
            guarded_client(url, std::time::Duration::from_millis(2_000)).await.is_none(),
            "resolving it must be what refuses it"
        );
    }

    #[test]
    fn plaintext_and_exotic_schemes_are_refused() {
        // http:// is refused even to a public host: the response decides what
        // the panel says about a coin, so it has to be authenticated.
        for bad in [
            "http://example.com/meta.json",
            "file:///etc/passwd",
            "gopher://example.com/",
            "data:application/json,{}",
            "",
            "   ",
        ] {
            assert!(metadata_url(bad).is_none(), "{bad:?} should not be fetched");
        }
    }

    #[test]
    fn ipfs_uris_travel_over_the_public_gateway() {
        assert_eq!(
            metadata_url("ipfs://QmHash/meta.json").as_deref(),
            Some("https://ipfs.io/ipfs/QmHash/meta.json")
        );
        // A leading slash in the CID must not produce a double slash that
        // resolves somewhere else on the gateway.
        assert_eq!(
            metadata_url("ipfs:///QmHash").as_deref(),
            Some("https://ipfs.io/ipfs/QmHash")
        );
    }
}

#[cfg(test)]
mod ipfs_tests {
    use super::*;

    /// Both forms a metadata file hands out, and the gateway URL we may have
    /// already rewritten it into — all the same document.
    #[test]
    fn a_cid_is_found_in_either_form() {
        assert_eq!(ipfs_path("ipfs://QmAbc/img.png").as_deref(), Some("QmAbc/img.png"));
        assert_eq!(ipfs_path("ipfs:///QmAbc").as_deref(), Some("QmAbc"));
        assert_eq!(ipfs_path("https://ipfs.io/ipfs/QmAbc").as_deref(), Some("QmAbc"));
        assert_eq!(ipfs_path("https://pump.mypinata.cloud/ipfs/QmAbc").as_deref(), Some("QmAbc"));
        // Not IPFS at all.
        assert_eq!(ipfs_path("https://example.com/pic.png"), None);
    }

    /// One document, every gateway — that is the whole point.
    #[test]
    fn an_ipfs_uri_races_every_gateway() {
        let urls = fetch_urls("ipfs://QmAbc/img.png");
        assert_eq!(urls.len(), IPFS_GATEWAYS.len());
        assert!(urls.iter().all(|u| u.ends_with("/ipfs/QmAbc/img.png")));
        assert!(urls.iter().any(|u| u.starts_with("https://ipfs.io")));
    }

    /// A plain URL is itself and nothing else — no gateways invented for it.
    #[test]
    fn an_ordinary_url_is_fetched_once() {
        assert_eq!(fetch_urls("https://example.com/pic.png"), vec!["https://example.com/pic.png"]);
        // http:// and private hosts stay refused.
        assert!(fetch_urls("http://example.com/pic.png").is_empty());
        assert!(fetch_urls("https://127.0.0.1/pic.png").is_empty());
    }
}
