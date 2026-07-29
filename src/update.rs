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

/// The one repository this trusts for releases.
const REPO: &str = "asyncswap/trenches";

/// The public half of the release signing key, pinned into the binary.
///
/// A checksum proves a download arrived intact. It cannot prove who published
/// it: SHA256SUMS lives in the same release as the files it describes, so
/// anything able to replace one can replace both. A signature is what makes the
/// release answer for itself, and pinning the key here is what makes it mean
/// something — fetching the key alongside the signature would prove only that
/// they came from the same place.
///
/// This is the public half only. It is not a secret and it is meant to be read:
/// the whole point is that it travels with the binary rather than being fetched
/// beside the thing it is checking.
///
/// Verification is mandatory while this is set. An unsigned release, or one
/// signed by any other key, is refused — after a key exists, "unsigned" and
/// "signed by someone else" deserve the same answer. Key id f823386447eb3da2.
const MINISIGN_PUBKEY: &str = "RWT4IzhkR+s9ogBA7iYghVU2KONHSS2FPvRtGHzoRr6/dESB8D5U4lUU";

/// What the check knows so far.
///
/// Three states, not two. "No newer version" and "never got an answer" are
/// different facts, and collapsing them let the app tell someone on 0.1.2 that
/// they were up to date while 0.1.3 sat published — the check had 404'd and
/// nothing said so.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Check {
    /// Still in flight, or not started.
    Pending,
    /// Asked and did not get an answer.
    Failed,
    /// The newest published tag, and when it was published.
    Known(String, u64),
}

/// How long a release has to have been public before this offers it.
///
/// A bad release — broken, or worse, one published by someone who should not
/// have been able to — reaches nobody during this window. It is the difference
/// between yanking a release and chasing it. Anyone who wants it sooner can say
/// so; the point is that the default does not rush.
pub const SOAK: u64 = 24 * 60 * 60;

/// Seconds since the epoch, from an ISO-8601 stamp like `2026-07-29T05:31:43Z`.
///
/// GitHub emits exactly this shape and always in UTC, so this parses that shape
/// and nothing else — a half-written date parser is worse than one that admits
/// it does not recognise the input.
fn parse_iso8601(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || !s.ends_with('Z') {
        return None;
    }
    let n = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    let (h, mi, sec) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
    let days = crate::ledger::days_from_civil(y as i32, mo as u32, d as u32);
    u64::try_from(days * 86_400 + h * 3600 + mi * 60 + sec).ok()
}

static LATEST: RwLock<Check> = RwLock::new(Check::Pending);

/// What to tell someone who asks about updates.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Status {
    /// The check has not answered yet.
    Checking,
    /// The check failed. We do not know.
    Unknown,
    /// Checked, and this is the newest there is.
    Latest,
    /// Checked, and there is a newer one.
    Update(String),
    /// A newer one exists, but it is too fresh to offer yet.
    Soaking { tag: String, ready_in: u64 },
}

/// The current state of the update check.
pub fn status() -> Status {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match LATEST.read().map(|s| s.clone()) {
        Ok(Check::Known(v, at)) if is_newer(&v, current()) => {
            let age = now.saturating_sub(at);
            // Someone can opt out of the wait deliberately. An env var rather
            // than a flag, so the same choice covers the key, the flag and the
            // launch banner without three ways to say it.
            let forced = std::env::var_os("TRENCHES_UPDATE_NOW").is_some();
            if forced || at == 0 || age >= SOAK {
                Status::Update(v)
            } else {
                Status::Soaking { tag: v, ready_in: SOAK - age }
            }
        }
        Ok(Check::Known(..)) => Status::Latest,
        Ok(Check::Failed) => Status::Unknown,
        _ => Status::Checking,
    }
}

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
    match status() {
        Status::Update(v) => Some(v),
        _ => None,
    }
}

/// Start the check. Returns immediately.
///
/// A detached task on the runtime the app already has, rather than a thread
/// with a blocking client: the same reqwest is in use everywhere else here, and
/// this is not worth a second HTTP stack.
pub fn spawn_check() {
    tokio::spawn(async {
        let result = match fetch_latest_tag().await {
            Some((tag, at)) => Check::Known(tag, at),
            // Recorded as a failure rather than left pending: an answer that
            // never comes must not read as "nothing newer".
            None => Check::Failed,
        };
        if let Ok(mut w) = LATEST.write() {
            *w = result;
        }
    });
}

/// Ask the releases API for the newest tag. Every failure is "no answer".
async fn fetch_latest_tag() -> Option<(String, u64)> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        // GitHub rejects requests without one.
        .user_agent(format!("trenches/{}", full()))
        .build()
        .ok()?;
    // The list, newest first — not `/releases/latest`, which excludes
    // pre-releases and 404s while every release is a beta. The installer and
    // the website read the list for the same reason.
    let body = client
        .get("https://api.github.com/repos/asyncswap/trenches/releases?per_page=1")
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .text()
        .await
        .ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    // An array now, so take the first entry.
    let first = v.as_array().and_then(|a| a.first()).unwrap_or(&v);
    let tag = first.get("tag_name")?.as_str()?.trim();
    // Unparseable or absent becomes 0, which `status` reads as "no soak" — a
    // release whose age cannot be established is not held back on a guess.
    let at = first
        .get("published_at")
        .and_then(|v| v.as_str())
        .and_then(parse_iso8601)
        .unwrap_or(0);
    (!tag.is_empty()).then(|| (tag.to_string(), at))
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

/// Check now, and wait for the answer.
///
/// The launch check is detached because nothing is waiting on it. This one is
/// for `--update`, where someone typed a command and is watching the cursor —
/// so it blocks, and the result lands in the same state the rest of the app
/// reads.
pub async fn check_now() -> Status {
    let result = match fetch_latest_tag().await {
        Some((tag, at)) => Check::Known(tag, at),
        None => Check::Failed,
    };
    if let Ok(mut w) = LATEST.write() {
        *w = result;
    }
    status()
}

/// What the footer says about this build.
///
/// Either "there is a newer one, press U" or "you are on the latest". Saying
/// nothing in the second case leaves you unable to tell a current build from
/// one whose check never answered, which is the question the line exists for.
pub fn footer_label() -> String {
    match status() {
        // Nothing about the update here. The `[U] update to v0.1.4` on the left
        // already says there is one and what to press; repeating it as an arrow
        // on the right is the same news twice, and the corner's job is to answer
        // "what am I running" — which stays true either way.
        Status::Update(_) | Status::Checking | Status::Unknown => full(),
        Status::Latest => format!("{} (latest)", full()),
        // Not something to act on, so it reads as current with a note rather
        // than as a call to do anything.
        Status::Soaking { tag, ready_in } => {
            format!("{} (latest) · {tag} in {}", full(), short_hours(ready_in))
        }
    }
}

/// A coarse "in about 18h" — nobody is timing this to the minute.
pub fn short_hours(secs: u64) -> String {
    let h = secs.div_ceil(3600);
    if h <= 1 { "under an hour".to_string() } else { format!("{h}h") }
}

/// Check the release's signature over its checksums file.
///
/// Silent when no key is pinned — that case is reported by `signing_status` so
/// it appears where someone will read it, rather than as a warning buried in an
/// update they are already committed to.
async fn verify_signature<F, Fut>(sums: &str, base: &str, get: &F) -> Result<(), String>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>, String>>,
{
    if MINISIGN_PUBKEY.is_empty() {
        return Ok(());
    }
    let key = minisign_verify::PublicKey::from_base64(MINISIGN_PUBKEY)
        .map_err(|e| format!("The pinned signing key is malformed: {e}"))?;
    // A missing signature is a failure, not an absence. Once a key is pinned,
    // "unsigned" and "signed by someone else" deserve the same answer.
    let raw = get(format!("{base}/SHA256SUMS.minisig"))
        .await
        .map_err(|_| "This release is not signed. Refusing to install it.".to_string())?;
    let text = String::from_utf8(raw).map_err(|_| "The signature is not text.".to_string())?;
    let sig = minisign_verify::Signature::decode(&text)
        .map_err(|e| format!("The signature is malformed: {e}"))?;
    key.verify(sums.as_bytes(), &sig, false)
        .map_err(|_| "The signature does not match this release. Refusing to install it.".to_string())
}

/// Whether releases are signature-checked, for the places that tell someone
/// what they are trusting.
pub fn signing_status() -> &'static str {
    if MINISIGN_PUBKEY.is_empty() {
        "checksum only — releases are not signed yet"
    } else {
        "signed, and the signature is checked against a key built into this binary"
    }
}

/// The release asset for the machine this is running on.
///
/// Rust target triples, because that is what the release workflow names its
/// assets after.
fn asset_name() -> Option<&'static str> {
    Some(match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => "trenches-aarch64-apple-darwin.tar.gz",
        ("x86_64", "macos") => "trenches-x86_64-apple-darwin.tar.gz",
        ("x86_64", "linux") => "trenches-x86_64-unknown-linux-gnu.tar.gz",
        // Windows ships a .zip and has no tar guarantee; that one stays manual.
        _ => return None,
    })
}

/// Download the newest release and put it where this binary is running from.
///
/// Deliberately does NOT pipe a script from our website into a shell, which is
/// what this used to do. That made trenches.sh the trust anchor for a program
/// holding trading keys: anyone who could change what that URL serves could run
/// code on every machine that pressed `U`, and the installer's own checksum
/// step would not have helped — a substituted script simply omits it.
///
/// Now the bytes come from the GitHub release, the SHA256SUMS come from the
/// same release, and this verifies the hash itself before anything is written.
/// Nothing downloaded is ever executed: the only program run is the system's
/// own `tar`. The remaining trust is GitHub and the release workflow, which is
/// the same trust already placed in the source.
pub async fn install_latest() -> Result<String, String> {
    use sha2::{Digest, Sha256};

    let asset = asset_name().ok_or_else(|| {
        "No automatic update for this platform — download it from the releases page.".to_string()
    })?;
    let dest = std::env::current_exe().map_err(|e| format!("Cannot find my own path: {e}"))?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .user_agent(format!("trenches/{}", full()))
        .build()
        .map_err(|e| format!("Could not start the download: {e}"))?;

    let get = |url: String| {
        let client = client.clone();
        async move {
            let r = client
                .get(&url)
                .send()
                .await
                .map_err(|e| format!("{url}: {e}"))?
                .error_for_status()
                .map_err(|e| format!("{url}: {e}"))?;
            Ok::<Vec<u8>, String>(r.bytes().await.map_err(|e| format!("{url}: {e}"))?.to_vec())
        }
    };

    let tag = match status() {
        Status::Update(v) => v,
        _ => return Err("No newer release to install.".to_string()),
    };
    let base = format!("https://github.com/{REPO}/releases/download/{tag}");

    // The checksums first: without them the download is unverifiable, and an
    // unverifiable binary is not one to write over the running one.
    let sums = String::from_utf8(get(format!("{base}/SHA256SUMS")).await?)
        .map_err(|_| "SHA256SUMS was not text.".to_string())?;
    let want = sums
        .lines()
        .find_map(|l| {
            let (h, f) = l.split_once(char::is_whitespace)?;
            (f.trim() == asset).then(|| h.trim().to_lowercase())
        })
        .ok_or_else(|| format!("{asset} is not listed in SHA256SUMS."))?;

    // Who published it, before what it contains.
    //
    // Checked against the checksums file, because that is what the signature
    // covers and what every asset's hash is read from — one signature vouching
    // for the whole release.
    verify_signature(&sums, &base, &get).await?;

    let bytes = get(format!("{base}/{asset}")).await?;
    let got = format!("{:x}", Sha256::digest(&bytes));
    if got != want {
        return Err(format!(
            "Checksum mismatch for {asset}. Nothing was written. Please report this."
        ));
    }

    // Unpacked beside the binary it will replace, so the rename is on one
    // filesystem and therefore atomic.
    let dir = dest.parent().ok_or("Install path has no directory.")?;
    let staging = dir.join(".trenches-update");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| format!("Cannot write beside the binary: {e}"))?;
    let tgz = staging.join(asset);
    std::fs::write(&tgz, &bytes).map_err(|e| format!("Cannot write the download: {e}"))?;

    let out = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&tgz)
        .arg("-C")
        .arg(&staging)
        .output()
        .map_err(|e| format!("Could not run tar: {e}"))?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!(
            "Could not unpack {asset}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    let fresh = staging.join("trenches");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&fresh, std::fs::Permissions::from_mode(0o755));
    }
    // Rename over the running binary. On Unix the running process keeps its own
    // inode, so nothing changes underneath it — which is why this reports that
    // a restart is needed rather than pretending to have done it.
    std::fs::rename(&fresh, &dest).map_err(|e| format!("Could not replace the binary: {e}"))?;
    let _ = std::fs::remove_dir_all(&staging);

    Ok(format!("Updated to {tag}. Restart Trenches to run it."))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three states must stay three.
    ///
    /// Collapsing "did not answer" into "nothing newer" is what told someone on
    /// 0.1.2 they were on the latest while 0.1.3 was published — the check had
    /// 404'd, because `/releases/latest` omits pre-releases and every release
    /// here is one.
    #[test]
    fn not_knowing_is_not_the_same_as_being_up_to_date() {
        // Published long enough ago that the soak window is not the thing under
        // test here.
        let decide = |c: &Check| match c {
            Check::Known(v, _) if is_newer(v, "0.1.2") => Status::Update(v.clone()),
            Check::Known(..) => Status::Latest,
            Check::Failed => Status::Unknown,
            Check::Pending => Status::Checking,
        };
        assert_eq!(decide(&Check::Pending), Status::Checking);
        assert_eq!(decide(&Check::Failed), Status::Unknown);
        assert_eq!(decide(&Check::Known("v0.1.2".into(), 0)), Status::Latest);
        assert_eq!(
            decide(&Check::Known("v0.1.3".into(), 0)),
            Status::Update("v0.1.3".into())
        );
    }

    /// The endpoint has to be the one that includes pre-releases.
    #[test]
    fn github_timestamps_parse_and_anything_else_does_not() {
        // The exact shape GitHub emits.
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601("1970-01-02T00:00:00Z"), Some(86_400));
        assert_eq!(parse_iso8601("2026-07-29T05:31:43Z"), Some(1_785_303_103));
        // Anything else is refused rather than guessed at. A misparse here
        // would either hold a release back forever or wave it straight through.
        assert_eq!(parse_iso8601(""), None);
        assert_eq!(parse_iso8601("2026-07-29"), None);
        assert_eq!(parse_iso8601("2026-07-29T05:31:43+02:00"), None);
        assert_eq!(parse_iso8601("not a date at all!!"), None);
    }

    #[test]
    fn a_fresh_release_waits_and_an_aged_one_does_not() {
        let now = 1_000_000u64;
        // The decision `status` makes, without touching the global state a
        // parallel test could be writing to.
        let decide = |published: u64| {
            let age = now.saturating_sub(published);
            if published == 0 || age >= SOAK { "offer" } else { "wait" }
        };
        assert_eq!(decide(now), "wait", "a release published this second");
        assert_eq!(decide(now - SOAK + 1), "wait", "an hour short of the window");
        assert_eq!(decide(now - SOAK), "offer", "exactly at the window");
        assert_eq!(decide(now - SOAK * 3), "offer", "long past it");
        // An unknown publish date is not held back on a guess.
        assert_eq!(decide(0), "offer");
    }

    /// A pinned key must be a real one, and its absence must be admitted.
    #[test]
    fn the_signing_key_is_either_valid_or_honestly_empty() {
        if MINISIGN_PUBKEY.is_empty() {
            // Nothing to verify against, so nothing may claim otherwise.
            assert!(
                signing_status().contains("not signed"),
                "with no key pinned the app must say releases are unsigned"
            );
            return;
        }
        // Once set it has to decode, or every update refuses forever and the
        // first anyone knows is when they try to install one.
        assert!(
            minisign_verify::PublicKey::from_base64(MINISIGN_PUBKEY).is_ok(),
            "the pinned signing key does not decode"
        );
        assert!(signing_status().contains("signed"));
    }

    /// A signature from the wrong key must be refused.
    ///
    /// The pinned key is the whole guarantee, so "does it accept a valid
    /// signature" is only half the test — the half that matters is whether it
    /// rejects one it should not trust.
    #[test]
    fn a_signature_from_another_key_is_rejected() {
        let key = minisign_verify::PublicKey::from_base64(MINISIGN_PUBKEY)
            .expect("the pinned key must decode");

        // A syntactically valid minisign signature made by a DIFFERENT keypair.
        // It parses; it just does not belong to us.
        const OTHER: &str = concat!(
            "untrusted comment: signature from a different key\n",
            "RUQf6LRCGA9i559r3g7V1qNyPXmvLuMDwYCcOd9SuZ4Cq2wJKPS2ZBLC\n",
            "trusted comment: not ours\n",
            "0000000000000000000000000000000000000000000000000000000000000000",
        );
        match minisign_verify::Signature::decode(OTHER) {
            // Parsed, so the interesting case: it must not verify.
            Ok(sig) => assert!(
                key.verify(b"anything at all", &sig, false).is_err(),
                "a signature from another key was accepted"
            ),
            // Refused at the parse step is also a refusal.
            Err(_) => {}
        }
    }

    /// The update path must never execute something it downloaded.
    ///
    /// It used to pipe our own install script into a shell, which made
    /// trenches.sh the trust anchor for a program that holds trading keys: a
    /// compromised site meant code execution everywhere `U` was pressed, and
    /// the script's own checksum step would not have helped, because a
    /// substituted script just leaves it out.
    #[test]
    fn nothing_downloaded_is_ever_executed() {
        // The code, not this module — a test that scans itself matches its own
        // assertion strings and fails for saying what it forbids.
        let whole = include_str!("update.rs");
        let src = whole.split("#[cfg(test)]").next().unwrap_or(whole);
        assert!(!src.contains("| sh"), "a downloaded script is being piped to a shell");
        assert!(
            !src.contains("trenches.sh/install"),
            "the update path must not depend on the website"
        );
        // The bytes and their checksums both come from the release itself.
        assert!(src.contains("SHA256SUMS"), "the download is not verified");
        assert!(
            src.contains("Sha256::digest"),
            "the checksum must be computed here, not by something we fetched"
        );
    }

    #[test]
    fn the_check_reads_the_release_list_not_releases_latest() {
        let src = include_str!("update.rs");
        assert!(
            src.contains("releases?per_page=1"),
            "the check must read the release list"
        );
        assert!(
            !src.contains("/releases/latest\""),
            "/releases/latest omits pre-releases and 404s while every release is a beta"
        );
    }

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
