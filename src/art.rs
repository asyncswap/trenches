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
    // 512 KB. Coin art is a few tens of kilobytes; past this it is either not
    // artwork or not worth the wait on a launch that lives for minutes.
    const CAP: usize = 512 * 1024;
    // Every refusal says WHY, in the session log. A picture that does not
    // appear is otherwise indistinguishable from a picture that was never
    // fetched, a host that refused, or a format we decline — and those want
    // four different answers.
    let got = async {
        // ipfs:// is not fetchable; the gateway form is the same document.
        // Already-http URLs pass through, and anything else is refused.
        let Some(url) = crate::net::metadata_url(&key)
            .or_else(|| key.starts_with("https://").then(|| key.clone()))
        else {
            crate::trace(&format!("art: refused url {key}"));
            return None;
        };
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
    .await;
    if let Ok(mut g) = pngs().lock() {
        g.insert(key, got.clone());
    }
    got
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

