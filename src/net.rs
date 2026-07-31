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

#[cfg(test)]
mod tests {
    use super::*;

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
