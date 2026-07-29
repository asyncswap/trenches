// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Is there a newer release than the one running?
//!
//! Checked once at launch, detached, against the public releases API. Three rules govern everything here:
//!
//! 1. **It cannot delay startup.** The check runs detached and the app never
//!    waits on it. A dead network, a captive portal or a rate limit costs
//!    nothing but a missing banner.
//! 2. **It cannot fail loudly.** Every error path ends in "no answer". Nobody
//!    should see a stack trace because GitHub was slow.
//! 3. **It never acts on its own.** The check reports; installing happens only
//!    when someone presses `U` and confirms. Nothing downloads or runs in the
//!    background — a trading binary that quietly rewrites itself is a
//!    supply-chain problem wearing a convenience costume.

use std::sync::RwLock;

/// The newest published version, once known. `None` until the check answers,
/// and stays `None` if it never does.
static LATEST: RwLock<Option<String>> = RwLock::new(None);

/// The version this binary was built as, without the build stamp.
pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The version as anyone should quote it: `0.1.0-commit-1b2b33`.
///
/// A tag says which release; the commit says which *build*, which is what
/// matters when a release is rebuilt or when someone is running a binary handed
/// to them. One string, so `--version`, the log header and the user agent
/// cannot drift apart. The comparison in `is_newer` stops at the first `-`, so
/// carrying the stamp here does not make every build look like an upgrade.
pub fn full() -> String {
    format!("{}-commit-{}", current(), env!("TRENCHES_COMMIT"))
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
        .user_agent(format!("trenches/{}", full()))
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

/// What the footer says about this build.
///
/// Either "there is a newer one, press U" or "you are on the latest". Saying
/// nothing in the second case leaves you unable to tell a current build from
/// one whose check never answered, which is the question the line exists for.
pub fn footer_label() -> String {
    match available() {
        Some(v) => format!("{}  →  {v}", full()),
        None => format!("{} (latest)", full()),
    }
}

/// Run the published installer. Returns what to tell the user.
///
/// This shells out to the same one-liner the docs give you rather than
/// downloading and swapping the binary itself, and that is the point: the
/// installer verifies the release's SHA256SUMS before it writes anything. A
/// bespoke update path inside a program that holds keys would be a second,
/// less-examined way to put a new binary on the machine.
///
/// It replaces the file on disk. The running process keeps its own inode on
/// Unix, so nothing changes underneath you — which is why this reports that a
/// restart is needed rather than pretending to have done it.
pub fn install_latest() -> Result<String, String> {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg("curl -fsSL https://trenches.sh/install | sh")
        .output()
        .map_err(|e| format!("Could not run the installer: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let why = err.trim().lines().last().unwrap_or("no reason given").to_string();
        return Err(format!("The installer failed. {why}"));
    }
    // The installer prints where it put things; the last line is the useful one.
    let tail = stdout.trim().lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
    Ok(format!("Updated. Restart Trenches to run it. {}", tail.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_stamp_rides_along_without_confusing_the_comparison() {
        let v = full();
        assert!(v.starts_with(current()), "{v} does not start with its version");
        assert!(v.contains("-commit-"), "{v} carries no build stamp");
        // And a build carrying a stamp is not newer than the same version.
        assert!(!is_newer(&v, current()));
        assert!(!is_newer(current(), &v));
    }

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
