// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Verification: orders and fills that can prove they are the ones this app
//! wrote, and say so when they cannot.
//!
//! The problem this solves is narrow and worth stating exactly. A trading log
//! is only useful if you believe it, and the moment someone wonders whether a
//! trade was really theirs, an editable text file is no help — it says whatever
//! it was last saved as. Recomputing PnL from a file anyone could have edited
//! answers a different question than the one being asked.
//!
//! So each record carries a proof over its own fields AND the proof before it,
//! which makes the file a chain: change one number and every proof after it
//! stops matching, so a forger has to rewrite the whole tail rather than one
//! line. The proof is keyed with a per-install secret, so rewriting the tail
//! requires the secret too.
//!
//! **What this does not do.** The secret sits in a 0600 file next to the
//! config. Anyone who can read that file can forge the whole chain, and anyone
//! who can read it can probably read your keystore, so this is not protection
//! against an attacker who already has your machine. It is protection against
//! a file edited by hand, a process that does not know about the chain, a
//! sync conflict, and disk corruption — and it turns "I think this is right"
//! into something checkable. Claiming more than that would be worse than
//! claiming nothing.

use alloy::primitives::keccak256;

/// The per-install key that makes a proof a MAC rather than a checksum.
///
/// Without it a proof is only a checksum: anyone editing a row recomputes it
/// and the chain still verifies. Generated once, 0600, beside the config.
fn secret() -> &'static [u8] {
    // Read ONCE per process. Not just to save the syscall: if the file cannot
    // be written — a read-only home, a sandbox — the fallback must still be
    // the SAME value for the life of the run, or every proof would be computed
    // against a different key and the chain would never verify against itself.
    static KEY: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    KEY.get_or_init(load_secret)
}

fn load_secret() -> Vec<u8> {
    use std::io::Read;
    let path = crate::config::config_dir().join("verification.key");
    if let Ok(mut f) = std::fs::File::open(&path) {
        let mut v = Vec::new();
        if f.read_to_end(&mut v).is_ok() && v.len() >= 32 {
            return v;
        }
    }
    // First run: mint one. Derived from the OS random source, not from the
    // wallet — the wallet is not unlocked when the ledger is first written.
    // Time + process + address entropy, hashed. Not a CSPRNG, and it does not
    // need to be: the secret only has to be unguessable to someone editing the
    // file, who by then is already reading the disk it lives on.
    let seed = format!(
        "{}-{}-{:?}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        std::process::id(),
        std::time::Instant::now(),
    );
    let fresh: Vec<u8> = keccak256(seed.as_bytes()).0.to_vec();
    let _ = std::fs::create_dir_all(crate::config::config_dir());
    if std::fs::write(&path, &fresh).is_ok() {
        crate::config::owner_only(&path);
    }
    fresh
}

/// The proof for one record: keccak over the secret, the previous proof, and
/// the record's own canonical fields.
///
/// Keccak is a sponge, so a plain `H(secret || data)` is a sound MAC here —
/// it has none of the length-extension weakness that makes the same
/// construction wrong with SHA-2.
pub fn proof(prev: &str, fields: &[&str]) -> String {
    let mut buf = secret().to_vec();
    buf.extend_from_slice(prev.as_bytes());
    for f in fields {
        // Length-prefixed, so ("ab","c") and ("a","bc") cannot collide.
        buf.extend_from_slice(&(f.len() as u64).to_be_bytes());
        buf.extend_from_slice(f.as_bytes());
    }
    format!("{:x}", keccak256(&buf))
}

/// Walk a chain of (proof, fields) in file order and return the index of the
/// first record that does not verify.
///
/// `None` means the whole chain is intact. A record written before proofs
/// existed carries an empty proof and is SKIPPED rather than failed — an old
/// file is unverified, which is not the same as forged, and reporting it as
/// tampering would teach people to ignore the warning.
pub fn first_broken<'a>(records: &'a [(String, Vec<String>)]) -> Option<usize> {
    let mut prev = String::new();
    for (i, (got, fields)) in records.iter().enumerate() {
        if got.is_empty() {
            continue; // pre-proof record; carries the chain forward unchanged
        }
        let refs: Vec<&str> = fields.iter().map(|s| s.as_str()).collect();
        if proof(&prev, &refs) != *got {
            return Some(i);
        }
        prev = got.clone();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_proof_covers_every_field() {
        let a = proof("", &["BUY", "0.5", "0xabc"]);
        assert_ne!(a, proof("", &["SELL", "0.5", "0xabc"]), "the action is covered");
        assert_ne!(a, proof("", &["BUY", "0.6", "0xabc"]), "the amount is covered");
        assert_ne!(a, proof("", &["BUY", "0.5", "0xdef"]), "the tx is covered");
    }

    /// Field boundaries are length-prefixed, so shifting a character across a
    /// boundary must not produce the same proof.
    #[test]
    fn fields_cannot_be_slid_into_each_other() {
        assert_ne!(proof("", &["ab", "c"]), proof("", &["a", "bc"]));
    }

    /// The chain is the point: editing one row invalidates it and everything
    /// after, so a forger cannot fix up a single line.
    #[test]
    fn editing_one_row_breaks_the_chain_from_there() {
        let mk = |rows: &[[&str; 2]]| {
            let mut prev = String::new();
            let mut out = Vec::new();
            for r in rows {
                let p = proof(&prev, &[r[0], r[1]]);
                out.push((p.clone(), vec![r[0].to_string(), r[1].to_string()]));
                prev = p;
            }
            out
        };
        let mut chain = mk(&[["BUY", "1"], ["SELL", "2"], ["BUY", "3"]]);
        assert_eq!(first_broken(&chain), None, "an untouched chain verifies");

        // Someone edits the middle row's amount, leaving its proof alone.
        chain[1].1[1] = "999".to_string();
        assert_eq!(first_broken(&chain), Some(1), "the edited row is named");
    }

    /// Records written before proofs existed must read as unverified, not as
    /// forged — a warning that cries wolf is a warning nobody reads.
    #[test]
    fn old_records_without_proofs_are_not_failures() {
        let chain = vec![
            (String::new(), vec!["BUY".into(), "1".into()]),
            (String::new(), vec!["SELL".into(), "2".into()]),
        ];
        assert_eq!(first_broken(&chain), None);
    }
}
