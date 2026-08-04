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

/// True when a URL's host is a public name — not loopback, not private or
/// link-local, not a bare `.local`.
pub fn public_host(url: &str) -> bool {
    let Some((_, rest)) = url.split_once("://") else { return false };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Strip credentials, then the port; keep the host.
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host).trim_matches(['[', ']']);
    if host.is_empty() {
        return false;
    }
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") || lower.ends_with(".local") {
        return false;
    }
    if let Ok(ip) = lower.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => {
                !(v4.is_loopback()
                    || v4.is_private()
                    || v4.is_link_local()
                    || v4.is_broadcast()
                    || v4.is_unspecified()
                    || v4.octets()[0] == 0
                    // Carrier-grade NAT and the cloud metadata neighbourhood.
                    || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1])))
            }
            std::net::IpAddr::V6(v6) => !(v6.is_loopback() || v6.is_unspecified()),
        };
    }
    true
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
pub const IPFS_GATEWAYS: [&str; 3] = ["https://ipfs.io", "https://dweb.link", "https://cf-ipfs.com"];

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
        return IPFS_GATEWAYS.iter().map(|g| format!("{g}/ipfs/{path}")).collect();
    }
    metadata_url(uri).into_iter().collect()
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

