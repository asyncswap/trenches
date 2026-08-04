// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Coin artwork: fetched once, held in memory, handed to the terminal.
//!
//! Keyed by URL rather than by mint or token address, because it is the same
//! problem on both chains — a link a coin's creator wrote, pointing at bytes
//! this app did not make.

/// The coin's artwork as PNG bytes, or None.
///
/// PNG ONLY, checked by the file's own magic bytes rather than by its URL or
/// what the server claims. The terminal is handed these bytes directly (Kitty
/// `f=100`), so anything else is not a picture that fails to draw, it is a
/// malformed payload sent to someone else's decoder. A creator writes this URL
/// and its contents; the app decodes nothing itself, and adding a decoder for
/// attacker-supplied bytes to a process holding keys is not a trade worth
/// making for artwork.
///
/// Capped and cached. The cap is on what is READ, not on what is promised —
/// a content-length header is a claim, and a hostile host can send forever.
pub async fn png(url: &str) -> Option<std::sync::Arc<Vec<u8>>> {
    let key = url.to_string();
    if let Some(hit) = pngs().lock().ok().and_then(|g| g.get(&key).cloned()) {
        return hit;
    }
    // Every refusal says WHY, in the session log. A picture that does not
    // appear is otherwise indistinguishable from a picture that was never
    // fetched, a host that refused, or a format we decline — and those want
    // four different answers.
    // On disk from a previous session? Then it is not a fetch at all.
    if let Some(bytes) = from_disk(&key) {
        let arc = std::sync::Arc::new(bytes);
        if let Ok(mut g) = pngs().lock() {
            g.insert(key, Some(arc.clone()));
        }
        return Some(arc);
    }
    let urls = crate::net::fetch_urls(&key);
    if urls.is_empty() {
        crate::trace(&format!("art: refused url {key}"));
        if let Ok(mut g) = pngs().lock() {
            g.insert(key, None);
        }
        return None;
    }
    // Every gateway at once, first PNG wins.
    //
    // One gateway is one queue, and a cold CID on a busy one takes seconds a
    // coin this age does not have. The losers are dropped mid-flight when the
    // winner returns.
    let got = {
        let mut tasks: Vec<_> = urls
            .iter()
            .map(|u| Box::pin(fetch_one(u.clone())))
            .collect();
        let mut won = None;
        while !tasks.is_empty() {
            let (res, i, rest) = futures::future::select_all(tasks).await;
            if res.is_some() {
                won = res;
                break;
            }
            let _ = i;
            tasks = rest;
        }
        won
    };
    if let Some(bytes) = got.as_deref() {
        to_disk(&key, bytes);
    }
    if let Ok(mut g) = pngs().lock() {
        g.insert(key, got.clone());
    }
    got
}

/// Where one artwork is cached, named by a hash of its URL.
///
/// The URL is attacker-written, so it never becomes a path: a coin creator
/// could otherwise choose the filename this app writes. A hash is fixed-width,
/// has no separators and cannot climb out of the directory.
fn disk_path(url: &str) -> std::path::PathBuf {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in url.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    std::path::Path::new(crate::state_dir()).join(format!("art-{h:016x}.png"))
}

fn from_disk(url: &str) -> Option<Vec<u8>> {
    let bytes = std::fs::read(disk_path(url)).ok()?;
    // Checked on the way IN as well as on the way out. A file in the state
    // directory is not evidence of anything by the time it is read back.
    (bytes.starts_with(b"\x89PNG\r\n\x1a\n")).then_some(bytes)
}

fn to_disk(url: &str, bytes: &[u8]) {
    let path = disk_path(url);
    let _ = std::fs::create_dir_all(crate::state_dir());
    // Written beside and renamed, so a half-written file is never read as art.
    let tmp = path.with_extension("png.tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// One gateway. `None` for every way this can fail, each with a reason in the
/// session log — a picture that does not appear is otherwise
/// indistinguishable from one never fetched.
async fn fetch_one(url: String) -> Option<std::sync::Arc<Vec<u8>>> {
    // 512 KB. Coin art is a few tens of kilobytes; past this it is either not
    // artwork or not worth the wait on a launch that lives for minutes.
    const CAP: usize = 512 * 1024;
    async {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(4_000))
            .build()
            .ok()?;
        let mut resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                crate::trace(&format!("art: {url} did not answer: {}", crate::net::redact(&e.to_string())));
                return None;
            }
        };
        if !resp.status().is_success() {
            crate::trace(&format!("art: {url} returned {}", resp.status()));
            return None;
        }
        // Read in pieces and stop at the cap. `content-length` is a claim, and
        // a hostile host can send past it forever.
        let mut body = Vec::new();
        while let Ok(Some(chunk)) = resp.chunk().await {
            body.extend_from_slice(&chunk);
            if body.len() > CAP {
                crate::trace(&format!("art: {url} is over {CAP} bytes"));
                return None;
            }
        }
        // The magic bytes, not the extension and not the content type.
        if !body.starts_with(b"\x89PNG\r\n\x1a\n") {
            let head: Vec<String> = body.iter().take(8).map(|b| format!("{b:02x}")).collect();
            crate::trace(&format!(
                "art: {url} is not a PNG ({} bytes, starts {})",
                body.len(),
                head.join(" ")
            ));
            return None;
        }
        crate::trace(&format!("art: {url} ok, {} bytes", body.len()));
        Some(std::sync::Arc::new(body))
    }
    .await
}

type PngCache = std::collections::HashMap<String, Option<std::sync::Arc<Vec<u8>>>>;

fn pngs() -> &'static std::sync::Mutex<PngCache> {
    static PNGS: std::sync::OnceLock<std::sync::Mutex<PngCache>> = std::sync::OnceLock::new();
    PNGS.get_or_init(Default::default)
}

/// What `token_png` already fetched, without waiting for anything.
///
/// The draw loop cannot await. It asks; a background fetch answers, and the
/// next frame has the picture — which is also why a coin's art appears a moment
/// after its numbers rather than holding them up.
pub fn cached(url: &str) -> Option<std::sync::Arc<Vec<u8>>> {
    pngs().lock().ok().and_then(|g| g.get(url).cloned()).flatten()
}

/// A JSON document from an IPFS URI, from whichever gateway answers first.
///
/// The same race the artwork gets, for the file that NAMES the artwork. This
/// document is fetched first and the picture cannot start until it lands, so a
/// slow gateway here delays everything behind it — and it was the one fetch
/// still going to a single host.
pub async fn fetch_json(uri: &str, timeout_ms: u64) -> Option<serde_json::Value> {
    let urls = crate::net::fetch_urls(uri);
    if urls.is_empty() {
        return None;
    }
    let mut tasks: Vec<_> = urls.into_iter().map(|u| Box::pin(one_json(u, timeout_ms))).collect();
    while !tasks.is_empty() {
        let (res, _, rest) = futures::future::select_all(tasks).await;
        if res.is_some() {
            return res;
        }
        tasks = rest;
    }
    None
}

async fn one_json(url: String, timeout_ms: u64) -> Option<serde_json::Value> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .build()
        .ok()?;
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<serde_json::Value>().await.ok()
}

#[cfg(test)]
mod disk_tests {
    use super::*;

    /// The URL is written by whoever launched the coin. It must never reach the
    /// filesystem as a path — a creator choosing where this app writes is a
    /// creator choosing what it overwrites.
    #[test]
    fn a_hostile_url_cannot_choose_the_filename() {
        for nasty in [
            "ipfs://../../../../etc/passwd",
            "https://x/%2e%2e%2fetc%2fpasswd",
            "https://x/a\u{0000}b",
            "ipfs://" .to_string().as_str(),
        ] {
            let p = disk_path(nasty);
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            assert!(name.starts_with("art-") && name.ends_with(".png"), "{name}");
            assert!(!name.contains('/') && !name.contains(".."), "{name}");
            assert_eq!(name.len(), "art-".len() + 16 + ".png".len(), "fixed width: {name}");
        }
    }

    /// Same URL, same file — otherwise the cache never hits.
    #[test]
    fn the_same_url_maps_to_the_same_file() {
        assert_eq!(disk_path("ipfs://QmAbc/a.png"), disk_path("ipfs://QmAbc/a.png"));
        assert_ne!(disk_path("ipfs://QmAbc/a.png"), disk_path("ipfs://QmAbc/b.png"));
    }
}

