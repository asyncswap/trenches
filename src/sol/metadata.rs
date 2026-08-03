// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Token names and symbols from Metaplex metadata.
//!
//! Coins found in the trenches carry their name inline in the `create`
//! instruction, so no lookup is needed. A coin added by pasting a mint has no
//! such event to read — especially a graduated one, whose launch may be weeks
//! back — and showed up nameless. This fills that gap.
//!
//! Two places to look, and the modern one comes FIRST: pump coins are now
//! **Token-2022** mints that carry a `TokenMetadata` extension inside the mint
//! account itself. Only older SPL-Token coins keep a separate Metaplex account,
//! so a Metaplex-only lookup returns nothing for most current coins.
//!
//! ⚠️ Names and symbols are attacker-controlled on-chain strings. They are
//! sanitised through `clean_text` before ever reaching the UI.

use solana_pubkey::Pubkey;

use super::rpc::Rpc;

/// The Metaplex Token Metadata program (fixed address).
pub const METADATA_PROGRAM: Pubkey =
    solana_pubkey::pubkey!("metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s");

/// `["metadata", metadata_program, mint]` — the coin's metadata account.
pub fn metadata_pda(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"metadata", METADATA_PROGRAM.as_ref(), mint.as_ref()],
        &METADATA_PROGRAM,
    )
    .0
}

/// A coin's display identity.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenMeta {
    pub name: String,
    pub symbol: String,
    /// The metadata URI from the on-chain account — where the off-chain JSON
    /// (image, description, socials) lives. pump.fun writes it at launch.
    pub uri: Option<String>,
    /// `("X" | "TG" | "Web", url)` — read from the URI's JSON, best-effort.
    pub socials: Vec<(&'static str, String)>,
}

/// Read a borsh string (u32 LE length + bytes) at `off`, returning it and the
/// offset just past it.
///
/// Metaplex pads these to fixed capacities (32 / 10 / 200) with NUL bytes, so
/// the declared length includes padding that must be trimmed — otherwise every
/// symbol renders with a tail of invisible characters.
fn borsh_string(data: &[u8], off: usize) -> Option<(String, usize)> {
    let len = u32::from_le_bytes(data.get(off..off + 4)?.try_into().ok()?) as usize;
    let start = off + 4;
    let end = start.checked_add(len)?;
    // Guard a hostile length field pointing past the buffer.
    if len > 512 || data.len() < end {
        return None;
    }
    let raw = String::from_utf8_lossy(&data[start..end]);
    Some((raw.trim_end_matches('\0').to_string(), end))
}

/// Decode name and symbol out of a Metaplex `Metadata` account.
///
/// Layout: `key(1) | update_authority(32) | mint(32) | name | symbol | uri…`
pub fn decode(data: &[u8]) -> Option<TokenMeta> {
    // key == 4 is `MetadataV1`; anything else is not this account type.
    if data.first()? != &4u8 {
        return None;
    }
    const DATA_START: usize = 1 + 32 + 32;
    let (name, off) = borsh_string(data, DATA_START)?;
    let (symbol, off) = borsh_string(data, off)?;
    // The uri is best-effort: a coin without one is still a coin.
    let uri = borsh_string(data, off).map(|(u, _)| u.trim().to_string()).filter(|u| !u.is_empty());
    Some(TokenMeta {
        name: super::discover::clean_text(&name, 32),
        symbol: super::discover::clean_text(&symbol, 12),
        uri,
        socials: Vec::new(),
    })
}

/// TLV extension type for `TokenMetadata` in a Token-2022 mint.
const EXT_TOKEN_METADATA: u16 = 19;
/// Where the extension TLV list starts: 165-byte padded base account, then a
/// one-byte account type tag.
const TLV_START: usize = 166;

/// Decode name and symbol from a Token-2022 mint's `TokenMetadata` extension.
///
/// The mint is a fixed 82-byte base padded to 165, a type tag, then TLV
/// entries of `type(u16) | length(u16) | value`. The metadata value is
/// `update_authority(32) | mint(32) | name | symbol | uri | additional…`.
pub fn decode_token2022(data: &[u8]) -> Option<TokenMeta> {
    if data.len() <= TLV_START {
        return None; // A plain mint with no extensions.
    }
    let mut off = TLV_START;
    while off + 4 <= data.len() {
        let ty = u16::from_le_bytes(data.get(off..off + 2)?.try_into().ok()?);
        let len = u16::from_le_bytes(data.get(off + 2..off + 4)?.try_into().ok()?) as usize;
        let start = off + 4;
        let end = start.checked_add(len)?;
        if end > data.len() {
            return None; // Truncated or hostile TLV — stop rather than guess.
        }
        if ty == EXT_TOKEN_METADATA {
            let val = &data[start..end];
            // Skip update_authority + mint.
            let (name, o) = borsh_string(val, 64)?;
            let (symbol, o) = borsh_string(val, o)?;
            let uri =
                borsh_string(val, o).map(|(u, _)| u.trim().to_string()).filter(|u| !u.is_empty());
            return Some(TokenMeta {
                name: super::discover::clean_text(&name, 32),
                symbol: super::discover::clean_text(&symbol, 12),
                uri,
                socials: Vec::new(),
            });
        }
        off = end;
    }
    None
}

/// Fetch a coin's name and symbol.
///
/// Tries the Token-2022 in-mint extension first (what current pump coins use),
/// then the legacy Metaplex account. `None` when neither exists — never an
/// error the caller has to handle, since a nameless coin is still tradeable.
pub async fn token_meta(rpc: &Rpc, mint: &Pubkey) -> Option<TokenMeta> {
    let mut meta = on_chain_meta(rpc, mint).await?;
    meta.socials = fetch_socials(meta.uri.as_deref()).await;
    Some(meta)
}

/// The on-chain half only — no socials, so no HTTP.
async fn on_chain_meta(rpc: &Rpc, mint: &Pubkey) -> Option<TokenMeta> {
    let mut meta = None;
    if let Ok(Some((data, _))) = rpc.account(mint).await {
        meta = decode_token2022(&data);
    }
    if meta.is_none() {
        let (data, _owner) = rpc.account(&metadata_pda(mint)).await.ok()??;
        meta = decode(&data);
    }
    meta
}

/// Just what a token is called, cached for the life of the process.
///
/// `token_meta` fetches the creator's metadata JSON over HTTP to find socials.
/// That is right for the pool panel, which shows them, and pure cost for a list
/// that only prints a symbol — it was a three-second timeout per token, in
/// series, so a wallet holding ten of them spent half a minute waiting for
/// links nobody asked to see.
///
/// Cached without expiry on purpose: a mint's symbol is fixed at creation and
/// cannot change, so the only wrong answer this can give is one it never had.
pub async fn token_symbol(rpc: &Rpc, mint: &Pubkey) -> Option<String> {
    static SYMBOLS: std::sync::Mutex<Option<std::collections::HashMap<Pubkey, Option<String>>>> =
        std::sync::Mutex::new(None);
    if let Ok(g) = SYMBOLS.lock() {
        if let Some(hit) = g.as_ref().and_then(|m| m.get(mint)) {
            return hit.clone();
        }
    }
    let sym = on_chain_meta(rpc, mint)
        .await
        .map(|m| m.symbol.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Ok(mut g) = SYMBOLS.lock() {
        g.get_or_insert_with(Default::default).insert(*mint, sym.clone());
    }
    sym
}

/// The socials, from the URI's off-chain JSON. The POINTER is on chain; the
/// contents are one HTTP fetch away — pump.fun writes twitter/telegram/website
/// keys when the creator fills them in. Best-effort with a short timeout:
/// a coin whose metadata host is down is still a coin.
async fn fetch_socials(uri: Option<&str>) -> Vec<(&'static str, String)> {
    let Some(uri) = uri else { return Vec::new() };
    // The URI is attacker-controlled — whoever launched the coin wrote it.
    let Some(url) = crate::net::metadata_url(uri) else { return Vec::new() };
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(3_000))
        .build()
    else {
        return Vec::new();
    };
    let Ok(resp) = client.get(&url).send().await else { return Vec::new() };
    let Ok(v) = resp.json::<serde_json::Value>().await else { return Vec::new() };
    let mut out = Vec::new();
    let grab = |v: &serde_json::Value, key: &str| -> Option<String> {
        let s = v.get(key)?.as_str()?.trim();
        (!s.is_empty()).then(|| super::discover::clean_text(s, 72))
    };
    // Top level first, then the Metaplex `extensions` nest some tools write.
    let ext = v.get("extensions").cloned().unwrap_or(serde_json::Value::Null);
    if let Some(x) = grab(&v, "twitter").or_else(|| grab(&ext, "twitter")) {
        out.push(("X", x));
    }
    if let Some(t) = grab(&v, "telegram").or_else(|| grab(&ext, "telegram")) {
        out.push(("TG", t));
    }
    if let Some(w) = grab(&v, "website").or_else(|| grab(&ext, "website")) {
        out.push(("Web", w));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;


    fn encode(key: u8, name: &str, symbol: &str) -> Vec<u8> {
        let mut v = vec![key];
        v.extend_from_slice(&[0u8; 64]); // update_authority + mint
        for (s, cap) in [(name, 32usize), (symbol, 10usize)] {
            let mut padded = s.as_bytes().to_vec();
            padded.resize(cap, 0);
            v.extend_from_slice(&(cap as u32).to_le_bytes());
            v.extend_from_slice(&padded);
        }
        v
    }

    #[test]
    fn decodes_name_and_symbol() {
        let m = decode(&encode(4, "Ansem Coin", "ANSEM")).expect("should decode");
        assert_eq!(m.name, "Ansem Coin");
        // NUL padding must not survive into the UI.
        assert_eq!(m.symbol, "ANSEM");
        assert!(!m.symbol.contains('\0'));
    }

    #[test]
    fn rejects_accounts_that_are_not_metadata() {
        // Wrong discriminator: a token account, a curve, anything.
        assert!(decode(&encode(1, "x", "y")).is_none());
        assert!(decode(&[]).is_none());
        assert!(decode(&[4u8; 3]).is_none());
    }

    #[test]
    fn hostile_strings_are_sanitised() {
        // On-chain text is attacker-controlled: escapes and newlines must not
        // reach the terminal.
        let m = decode(&encode(4, "evil\u{1b}[2Jname", "a\nb")).expect("decode");
        assert!(!m.name.contains('\u{1b}'));
        assert!(!m.symbol.contains('\n'));
    }

    #[test]
    fn a_hostile_length_field_cannot_read_past_the_buffer() {
        let mut v = vec![4u8];
        v.extend_from_slice(&[0u8; 64]);
        v.extend_from_slice(&u32::MAX.to_le_bytes()); // absurd length
        assert!(decode(&v).is_none());
    }

    fn t2022(name: &str, symbol: &str) -> Vec<u8> {
        let mut v = vec![0u8; TLV_START];
        let mut val = vec![0u8; 64]; // update_authority + mint
        for s in [name, symbol, "https://uri"] {
            val.extend_from_slice(&(s.len() as u32).to_le_bytes());
            val.extend_from_slice(s.as_bytes());
        }
        v.extend_from_slice(&EXT_TOKEN_METADATA.to_le_bytes());
        v.extend_from_slice(&(val.len() as u16).to_le_bytes());
        v.extend_from_slice(&val);
        v
    }

    #[test]
    fn decodes_token2022_extension_metadata() {
        // The shape mainnet actually returns for a current pump coin.
        let m = decode_token2022(&t2022("The Black Bull", "ANSEM")).expect("decode");
        assert_eq!(m.name, "The Black Bull");
        assert_eq!(m.symbol, "ANSEM");
    }

    #[test]
    fn skips_extensions_it_does_not_understand() {
        // A MetadataPointer (18) precedes TokenMetadata on real mints; the
        // walker must step over it rather than give up or misread it.
        let mut v = vec![0u8; TLV_START];
        v.extend_from_slice(&18u16.to_le_bytes());
        v.extend_from_slice(&64u16.to_le_bytes());
        v.extend_from_slice(&[7u8; 64]);
        v.extend_from_slice(&t2022("Catecoin", "CATE")[TLV_START..]);
        let m = decode_token2022(&v).expect("decode past the pointer");
        assert_eq!(m.symbol, "CATE");
    }

    #[test]
    fn a_plain_mint_yields_nothing() {
        assert!(decode_token2022(&[0u8; 82]).is_none());
        assert!(decode_token2022(&[0u8; TLV_START]).is_none());
    }

    #[test]
    fn a_truncated_tlv_is_rejected_not_guessed() {
        let mut v = vec![0u8; TLV_START];
        v.extend_from_slice(&EXT_TOKEN_METADATA.to_le_bytes());
        v.extend_from_slice(&9999u16.to_le_bytes()); // claims more than exists
        v.extend_from_slice(&[0u8; 8]);
        assert!(decode_token2022(&v).is_none());
    }

    /// The PDA must match what explorers derive.
    ///   cargo test --features solana metadata_pda_is_stable -- --nocapture
    #[test]
    fn metadata_pda_is_stable() {
        let mint: Pubkey = "9cRCn9rGT8V2imeM2BaKs13yhMEais3ruM3rPvTGpump".parse().unwrap();
        println!("metadata pda = {}", metadata_pda(&mint));
        assert_ne!(metadata_pda(&mint), mint);
    }

    ///   cargo test --features solana live_token_meta -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_token_meta_reads_a_real_coin() {
        let reg = crate::config::Registry::load("deployments.json").expect("registry");
        let net = reg.networks.iter().find(|n| n.kind.is_solana()).expect("solana net");
        let rpc = Rpc::new_pool(net.rpc_pool());
        for m in [
            std::env::var("MINT").unwrap_or_else(|_| "Ai66LHZG9MCzg1WKdawwqduVAXpNDUuV8M3uyq5ppump".into()),
            "9cRCn9rGT8V2imeM2BaKs13yhMEais3ruM3rPvTGpump".to_string(),
        ] {
            let mint: Pubkey = m.parse().expect("valid mint");
            println!("{mint} -> {:?}", token_meta(&rpc, &mint).await);
        }
    }
}
