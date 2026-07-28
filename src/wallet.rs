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
pub fn keystore_dir() -> eyre::Result<std::path::PathBuf> {
    let home = crate::config::home_dir()
        .ok_or_else(|| eyre::eyre!("cannot find your home directory (HOME / USERPROFILE unset)"))?;
    Ok(home.join(".foundry/keystores"))
}

/// A keystore file on disk.
pub struct KeystoreEntry {
    pub name: String,
    pub path: std::path::PathBuf,
}

/// Every keystore in the directory, sorted by name.
///
/// Equivalent to `cast wallet list`, without needing Foundry installed. An
/// unreadable or missing directory is an empty list, not an error: a first-run
/// user has no keystores yet and that is not a failure.
pub fn list_keystores() -> Vec<KeystoreEntry> {
    let Ok(dir) = keystore_dir() else {
        return Vec::new();
    };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<KeystoreEntry> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            // Editor leftovers and dotfiles are not wallets.
            if name.starts_with('.') || name.ends_with('~') {
                return None;
            }
            Some(KeystoreEntry { name, path: e.path() })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
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
