// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs
//! Is there a newer release than the one running?
//!
//! Checked once at launch, detached, against the public releases API. Three rules govern everything here:
//!
//! 1. **It cannot delay startup.** The check runs detached and the app never
//!    waits on it. A dead network, a captive portal or a rate limit costs
//!    nothing but a missing banner.
//! 2. **It cannot fail loudly.** Every error path ends in "no answer". Nobody
//!    should see a stack trace because GitHub was slow.
//! 3. **It reports, it does not act.** No self-update, no download, no
//!    execution. A trading binary that rewrites itself is a supply-chain
//!    problem wearing a convenience costume.

use std::sync::RwLock;

/// The newest published version, once known. `None` until the check answers,
/// and stays `None` if it never does.
static LATEST: RwLock<Option<String>> = RwLock::new(None);

/// The version this binary was built as.
pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A newer version than this one, if the check found one. Cheap; safe to call
/// every frame.
pub fn available() -> Option<String> {
    let latest = LATEST.read().ok()?.clone()?;
    is_newer(&latest, current()).then_some(latest)
}

/// Start the check. Returns immediately.
///
/// A detached task on the runtime the app already has, rather than a thread
/// with a blocking client: the same reqwest is in use everywhere else here, and
/// this is not worth a second HTTP stack.
pub fn spawn_check() {
    tokio::spawn(async {
        if let Some(tag) = fetch_latest_tag().await {
            if let Ok(mut w) = LATEST.write() {
                *w = Some(tag);
            }
        }
    });
}

/// Ask the releases API for the newest tag. Every failure is "no answer".
async fn fetch_latest_tag() -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        // GitHub rejects requests without one.
        .user_agent(concat!("trenches/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;
    let body = client
        .get("https://api.github.com/repos/asyncswap/trenches/releases/latest")
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .text()
        .await
        .ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let tag = v.get("tag_name")?.as_str()?.trim();
    (!tag.is_empty()).then(|| tag.to_string())
}

/// Split a version into numbers, ignoring a leading `v` and any `-beta.1` tail.
///
/// Anything unparseable becomes an empty list, which compares as older than
/// everything — a malformed tag must never announce itself as an upgrade.
fn parts(v: &str) -> Vec<u64> {
    v.trim()
        .trim_start_matches(['v', 'V'])
        .split('-')
        .next()
        .unwrap_or("")
        .split('.')
        .map(|p| p.parse().unwrap_or(0))
        .collect()
}

/// Is `candidate` a later version than `running`?
fn is_newer(candidate: &str, running: &str) -> bool {
    let (c, r) = (parts(candidate), parts(running));
    if c.is_empty() {
        return false;
    }
    // Compare position by position, treating a missing component as zero, so
    // "1.2" and "1.2.0" are the same version rather than different ones.
    for i in 0..c.len().max(r.len()) {
        let (a, b) = (c.get(i).copied().unwrap_or(0), r.get(i).copied().unwrap_or(0));
        if a != b {
            return a > b;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_higher_version_is_an_upgrade() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("v0.1.1", "0.1.0"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(is_newer("0.10.0", "0.9.0"), "components compare as numbers, not text");
    }

    #[test]
    fn the_same_or_an_older_version_is_not() {
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        // A shorter tag is the same version, not an older one.
        assert!(!is_newer("0.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1"));
    }

    #[test]
    fn a_prerelease_tail_is_ignored_rather_than_misread() {
        assert!(is_newer("v0.2.0-beta.1", "0.1.0"));
        assert!(!is_newer("v0.1.0-beta.1", "0.1.0"));
    }

    #[test]
    fn nonsense_never_announces_itself_as_an_upgrade() {
        // Whatever the API returns, it cannot talk someone into "updating" to
        // something that is not a version.
        assert!(!is_newer("", "0.1.0"));
        assert!(!is_newer("latest", "0.1.0"));
        assert!(!is_newer("not-a-version", "0.1.0"));
    }
}
