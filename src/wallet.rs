// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Signing: unlock a keystore by password, or derive an account from a registry
//! mnemonic — never a raw private key input.
//!
//! Keystores are listed and created directly with alloy, NOT by shelling out to
//! `cast`. `cast wallet list` only reads `~/.foundry/keystores`, which is a
//! directory read; going through the CLI would mean requiring Foundry to be
//! installed, parsing human-facing output, and finding somewhere safe to put a
//! password on a command line. Reading and writing the files ourselves avoids
//! all three — the format is the standard Web3 Secret Storage one, so anything
//! created here works with `cast` and vice versa.

use alloy::signers::local::{
    coins_bip39::English, LocalSigner, MnemonicBuilder, PrivateKeySigner,
};

/// Where keystores live. Shared with Foundry deliberately, so a wallet made in
/// either tool shows up in the other.
/// Where NEW keystores are written: beside the config.
///
/// The app derives, encrypts and writes these itself and never shells out to
/// `cast`, so writing into Foundry's directory was a convention we borrowed
/// rather than one we depend on — and it put files under a tool's name for
/// users who do not have that tool. Encrypted keys next to the config is also
/// the right shape for backup: a keystore is meant to survive being copied.
pub fn keystore_dir() -> eyre::Result<std::path::PathBuf> {
    Ok(crate::config::config_dir().join("keystores"))
}

/// Foundry's directory, still READ so a wallet made with `cast` shows up here
/// and one made here can be used there. Never written to.
pub fn foundry_keystore_dir() -> Option<std::path::PathBuf> {
    crate::config::home_dir().map(|h| h.join(".foundry/keystores"))
}

/// A keystore file on disk.
pub struct KeystoreEntry {
    pub name: String,
    pub path: std::path::PathBuf,
}

/// The file a listed keystore actually lives in.
///
/// Never rebuild a keystore path from its name: the list spans two directories,
/// so `keystore_dir().join(name)` points at the wrong one for anything found in
/// Foundry's — and a missing file surfaces as "wrong password", which sends the
/// user hunting for a fault in the one thing that was correct.
pub fn keystore_path(id: &str) -> Option<std::path::PathBuf> {
    let p = std::path::PathBuf::from(id);
    if p.is_file() {
        return Some(p);
    }
    // A bare name still resolves, for anything that stored one before paths
    // became the identifier.
    list_keystores().into_iter().find(|k| k.name == id).map(|k| k.path)
}

/// Every keystore in the directory, sorted by name.
///
/// Equivalent to `cast wallet list`, without needing Foundry installed. An
/// unreadable or missing directory is an empty list, not an error: a first-run
/// user has no keystores yet and that is not a failure.
pub fn list_keystores() -> Vec<KeystoreEntry> {
    // Ours first, then Foundry's. Order matters on a name collision: a wallet
    // this app wrote wins, and the Foundry one is skipped rather than shown
    // twice under the same name with different keys behind it.
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(d) = keystore_dir() {
        dirs.push(d);
    }
    if let Some(d) = foundry_keystore_dir() {
        dirs.push(d);
    }

    let mut out: Vec<KeystoreEntry> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.filter_map(|e| e.ok()).filter(|e| e.path().is_file()) {
            let name = e.file_name().to_string_lossy().to_string();
            // Editor leftovers and dotfiles are not wallets.
            if name.starts_with('.') || name.ends_with('~') {
                continue;
            }
            // Deduped on the PATH, and only to survive the same directory being
            // listed twice. Two directories may each hold a `robin` and those
            // are two different keys — both belong in the list.
            let path = e.path();
            if seen.insert(path.clone()) {
                out.push(KeystoreEntry { name, path });
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Create a new random keystore and return its address.
///
/// The key is generated here and encrypted before it touches the disk — it is
/// never printed, logged, or passed to another process.
pub fn create_keystore(name: &str, password: &str) -> eyre::Result<alloy::primitives::Address> {
    let (dir, name) = prepare_slot(name, password)?;
    let mut rng = rand::thread_rng();
    let (signer, _) = LocalSigner::new_keystore(&dir, &mut rng, password, Some(&name))?;
    Ok(signer.address())
}

/// Import an existing private key into an encrypted keystore.
///
/// Takes the key as text because that is how a user has it, and encrypts it
/// immediately. The plaintext is never written anywhere.
pub fn import_private_key(
    name: &str,
    private_key: &str,
    password: &str,
) -> eyre::Result<alloy::primitives::Address> {
    let signer: PrivateKeySigner = private_key
        .trim()
        .trim_start_matches("0x")
        .parse()
        .map_err(|_| eyre::eyre!("that is not a valid private key"))?;
    store_signer(name, signer, password)
}

/// Import an account derived from a seed phrase into an encrypted keystore.
///
/// The phrase is used once to derive the key and is not kept — which is the
/// whole point: after this the wallet is an encrypted file, not a phrase living
/// in a config file.
pub fn import_mnemonic(
    name: &str,
    phrase: &str,
    index: u32,
    password: &str,
) -> eyre::Result<alloy::primitives::Address> {
    let signer = MnemonicBuilder::<English>::default()
        .phrase(phrase.trim().to_string())
        .index(index)?
        .build()
        .map_err(|_| eyre::eyre!("that seed phrase is not valid"))?;
    store_signer(name, signer, password)
}

/// Encrypt a signer into the keystore directory under `name`.
fn store_signer(
    name: &str,
    signer: PrivateKeySigner,
    password: &str,
) -> eyre::Result<alloy::primitives::Address> {
    let (dir, name) = prepare_slot(name, password)?;
    let mut rng = rand::thread_rng();
    let key = signer.credential().to_bytes();
    let addr = signer.address();
    LocalSigner::encrypt_keystore(&dir, &mut rng, key, password, Some(&name))?;
    Ok(addr)
}

/// Shared checks before writing a keystore: valid name, non-empty password, and
/// nothing already there to destroy.
fn prepare_slot(name: &str, password: &str) -> eyre::Result<(std::path::PathBuf, String)> {
    let name = name.trim().to_string();
    if name.is_empty() {
        eyre::bail!("give the wallet a name");
    }
    // A name is used as a filename; a path separator would write outside the
    // keystore directory.
    if name.contains('/') || name.contains('\\') || name.starts_with('.') {
        eyre::bail!("use a plain name, without slashes or a leading dot");
    }
    if password.is_empty() {
        eyre::bail!("a keystore needs a password");
    }
    let dir = keystore_dir()?;
    std::fs::create_dir_all(&dir)?;
    if dir.join(&name).exists() {
        // Silently overwriting would destroy a key with no way back.
        eyre::bail!("a wallet named '{name}' already exists");
    }
    Ok((dir, name))
}



#[cfg(test)]
mod keystore_tests {
    use super::*;

    #[test]
    fn a_name_cannot_escape_the_keystore_directory() {
        // The name becomes a filename, so a separator would write elsewhere on
        // disk — and a keystore written somewhere unexpected is a lost key.
        for bad in ["../evil", "a/b", ".hidden", "   "] {
            assert!(prepare_slot(bad, "pw").is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn a_keystore_always_needs_a_password() {
        assert!(prepare_slot("ok-name", "").is_err());
    }

    #[test]
    fn bad_secrets_are_rejected_before_anything_is_written() {
        assert!(import_private_key("x", "not-a-key", "pw").is_err());
        assert!(import_mnemonic("x", "not a real seed phrase at all", 0, "pw").is_err());
    }

    #[test]
    fn a_missing_keystore_directory_lists_as_empty() {
        // A first run has no keystores; that is not an error state.
        let _ = list_keystores();
    }
}
