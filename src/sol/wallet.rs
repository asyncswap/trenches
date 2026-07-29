// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Solana key derivation from the SAME registry mnemonic the EVM side uses, so
//! one seed phrase drives both chains and nothing new has to be stored.
//!
//! Solana uses ed25519, which derives differently from EVM's secp256k1: the path
//! is SLIP-0010 with **hardened-only** indices (ed25519 has no public derivation),
//! at `m/44'/501'/<account>'/0'` — the convention Phantom/Solflare use, so a
//! derived address matches what those wallets show for the same phrase.

use coins_bip39::{English, Mnemonic};
use hmac::{Hmac, Mac};
use sha2::Sha512;
use solana_keypair::Keypair;
use solana_signer::Signer;

type HmacSha512 = Hmac<Sha512>;

/// Hardened-index marker: SLIP-0010 ed25519 permits *only* hardened children.
const HARDENED: u32 = 0x8000_0000;

/// One SLIP-0010 step: derive (key, chaincode) for a hardened child index.
fn derive_child(key: &[u8; 32], chain: &[u8; 32], index: u32) -> ([u8; 32], [u8; 32]) {
    // Data = 0x00 || key || ser32(index), per SLIP-0010 ed25519.
    let mut data = [0u8; 37];
    data[0] = 0;
    data[1..33].copy_from_slice(key);
    data[33..37].copy_from_slice(&(index | HARDENED).to_be_bytes());

    let mut mac = HmacSha512::new_from_slice(chain).expect("hmac accepts any key length");
    mac.update(&data);
    let out = mac.finalize().into_bytes();

    let mut k = [0u8; 32];
    let mut c = [0u8; 32];
    k.copy_from_slice(&out[..32]);
    c.copy_from_slice(&out[32..]);
    (k, c)
}

/// Derive the ed25519 secret for `m/44'/501'/<account>'/0'` from a BIP39 phrase.
fn derive_secret(phrase: &str, account: u32) -> eyre::Result<[u8; 32]> {
    let mnemonic = Mnemonic::<English>::new_from_phrase(phrase)
        .map_err(|e| eyre::eyre!("invalid mnemonic: {e}"))?;
    let seed = mnemonic
        .to_seed(None)
        .map_err(|e| eyre::eyre!("mnemonic -> seed failed: {e}"))?;

    // Master key: HMAC-SHA512 with the fixed ed25519 domain string.
    let mut mac = HmacSha512::new_from_slice(b"ed25519 seed").expect("fixed key is valid");
    mac.update(&seed);
    let out = mac.finalize().into_bytes();
    let mut key = [0u8; 32];
    let mut chain = [0u8; 32];
    key.copy_from_slice(&out[..32]);
    chain.copy_from_slice(&out[32..]);

    // m / 44' / 501' / account' / 0'
    for index in [44u32, 501, account, 0] {
        let (k, c) = derive_child(&key, &chain, index);
        key = k;
        chain = c;
    }
    Ok(key)
}

/// The signing keypair for a registry mnemonic + account index.
pub fn keypair_from_mnemonic(phrase: &str, account: u32) -> eyre::Result<Keypair> {
    Ok(Keypair::new_from_array(derive_secret(phrase, account)?))
}

/// Preview an address without building signing state — used by the account
/// picker, mirroring `wallet::mnemonic_address` on the EVM side.
pub fn mnemonic_address(phrase: &str, account: u32) -> eyre::Result<String> {
    Ok(keypair_from_mnemonic(phrase, account)?.pubkey().to_string())
}

// ---- encrypted keystore --------------------------------------------------
//
// Solana's own CLI stores keys as a PLAINTEXT json array — no password, no KDF.
// Rather than adopt that, we reuse the Web3 Secret Storage format the EVM side
// already uses (scrypt + AES-128-CTR + MAC): it encrypts arbitrary bytes, so a
// 32-byte ed25519 secret fits, and one password protects both chains.

/// Where Solana keystores live, alongside the EVM ones.
fn keystore_dir() -> eyre::Result<std::path::PathBuf> {
    // One directory for both chains — the picker lists them together, and a
    // second location would only be somewhere else to lose a key.
    crate::wallet::keystore_dir()
}

/// Encrypt a mnemonic-derived Solana key into a password-protected keystore.
/// Returns the public address so the caller can confirm what was saved.
///
/// This is how a Solana key stops depending on a plaintext mnemonic sitting in
/// `deployments.json`.
pub fn create_keystore(phrase: &str, account: u32, name: &str, password: &str) -> eyre::Result<String> {
    let secret = derive_secret(phrase, account)?;
    let address = Keypair::new_from_array(secret).pubkey().to_string();

    let dir = keystore_dir()?;
    std::fs::create_dir_all(&dir)?;
    let mut rng = rand::rngs::OsRng; // OS entropy for the salt/IV
    eth_keystore::encrypt_key(&dir, &mut rng, secret, password, Some(name))
        .map_err(|e| eyre::eyre!("failed to write keystore '{name}': {e}"))?;
    Ok(address)
}

/// Unlock a Solana keystore by password.
pub fn keypair_from_keystore(name: &str, password: &str) -> eyre::Result<Keypair> {
    let path = crate::wallet::keystore_path(name)
        .ok_or_else(|| eyre::eyre!("{name} is no longer on disk"))?;
    let bytes = eth_keystore::decrypt_key(&path, password)
        .map_err(|e| eyre::eyre!("failed to unlock keystore '{name}': {e}"))?;
    let secret: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| eyre::eyre!("keystore '{name}' holds {} bytes, expected 32", bytes.len()))?;
    Ok(Keypair::new_from_array(secret))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical (public, never-funded) BIP39 test mnemonic. Deriving it here
    /// lets the path be checked against Phantom/Solflare, which use the same
    /// `m/44'/501'/<n>'/0'` convention — import this phrase there and the
    /// addresses must match, otherwise our derivation is wrong.
    const TEST_PHRASE: &str =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    #[test]
    fn derivation_is_deterministic_and_account_separated() {
        let a0 = mnemonic_address(TEST_PHRASE, 0).unwrap();
        let a0_again = mnemonic_address(TEST_PHRASE, 0).unwrap();
        let a1 = mnemonic_address(TEST_PHRASE, 1).unwrap();

        assert_eq!(a0, a0_again, "same phrase+index must derive the same key");
        assert_ne!(a0, a1, "different account indices must differ");

        // Base58-encoded 32-byte ed25519 pubkeys are 32..=44 chars.
        assert!((32..=44).contains(&a0.len()), "not a plausible pubkey: {a0}");
        println!("m/44'/501'/0'/0' -> {a0}");
        println!("m/44'/501'/1'/0' -> {a1}");
    }

    /// Encrypt → decrypt must return the same signing key, and a wrong password
    /// must fail rather than yield a different (silently wrong) key.
    #[test]
    fn keystore_roundtrips_and_rejects_bad_password() {
        let dir = std::env::temp_dir().join(format!("solks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let secret = derive_secret(TEST_PHRASE, 0).unwrap();
        let expected = Keypair::new_from_array(secret).pubkey().to_string();

        let mut rng = rand::rngs::OsRng;
        let name = "sol-test-key";
        eth_keystore::encrypt_key(&dir, &mut rng, secret, "hunter2", Some(name)).unwrap();
        let path = dir.join(name);

        let back = eth_keystore::decrypt_key(&path, "hunter2").unwrap();
        let arr: [u8; 32] = back.as_slice().try_into().unwrap();
        assert_eq!(Keypair::new_from_array(arr).pubkey().to_string(), expected);

        assert!(
            eth_keystore::decrypt_key(&path, "wrong").is_err(),
            "a wrong password must fail, never silently return a different key"
        );
        // The file on disk must not contain the raw secret.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains(&hex_of(&secret)), "secret must not be stored in cleartext");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn hex_of(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Locks in the known-good address for the canonical test phrase, so a future
    /// refactor of the SLIP-0010 code can't silently change what we derive.
    #[test]
    fn matches_known_phantom_address() {
        assert_eq!(
            mnemonic_address(TEST_PHRASE, 0).unwrap(),
            "HAgk14JpMQLgt6rVgv7cBQFJWFto5Dqxi472uT3DKpqk",
        );
    }
}
