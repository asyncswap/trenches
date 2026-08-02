// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! The keyboard shortcuts, from the one file that lists them.
//!
//! `keys.json` is the source. This reads it AT COMPILE TIME, so a malformed
//! file is a build failure rather than an empty help screen someone discovers
//! in production, and the binary carries no runtime dependency on a path.
//!
//! What this replaces: three hand-maintained lists that had drifted apart —
//! one in the EVM dashboard, one in the Solana dashboard, and prose in the
//! docs. `M` existed only on Solana, `m` only on EVM, and `s` was described
//! two different ways. None of that was carelessness; three lists cannot be
//! held in step by intending to.

use serde::Deserialize;

#[derive(Deserialize)]
pub struct Key {
    pub key: String,
    pub desc: String,
    pub section: String,
    pub chains: Vec<String>,
}

#[derive(Deserialize)]
struct File {
    keys: Vec<Key>,
}

/// Which dashboard is asking.
#[derive(Clone, Copy, PartialEq)]
pub enum Chain {
    Evm,
    Sol,
}

impl Chain {
    fn tag(self) -> &'static str {
        match self {
            Chain::Evm => "evm",
            Chain::Sol => "sol",
        }
    }
}

fn all() -> &'static [Key] {
    static ALL: std::sync::OnceLock<Vec<Key>> = std::sync::OnceLock::new();
    ALL.get_or_init(|| {
        // Parsed once. A panic here means keys.json is malformed, which is a
        // programming error in this repo and not something to paper over with
        // an empty list — an app that silently shows no shortcuts looks like an
        // app that has none.
        let raw = include_str!("../keys.json");
        serde_json::from_str::<File>(raw).expect("keys.json is malformed").keys
    })
}

/// Every shortcut for one chain, grouped, in the order `keys.json` lists them.
///
/// Returns the shape the help widget wants: `("SECTION", "")` for a heading and
/// `("", "key|desc")` for a row. That shape belongs to the widget, so building
/// it here keeps both dashboards from formatting the same data two ways.
pub fn help_rows(chain: Chain) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut section = "";
    for k in all().iter().filter(|k| k.chains.iter().any(|c| c == chain.tag())) {
        if k.section != section {
            section = &k.section;
            out.push((section.to_string(), String::new()));
        }
        out.push((String::new(), format!("{}|{}", k.key, k.desc)));
    }
    out
}

/// The shortcuts as markdown, for the docs page.
pub fn markdown() -> String {
    let mut out = String::from("# KEYS\n\nEvery shortcut, by section. `EVM` and `SOL` mark which\ndashboard has it.\n");
    let mut section = "";
    for k in all() {
        if k.section != section {
            section = &k.section;
            out.push_str(&format!("\n## {section}\n\n| key | does | where |\n| --- | --- | --- |\n"));
        }
        let where_ = match (k.chains.iter().any(|c| c == "evm"), k.chains.iter().any(|c| c == "sol")) {
            (true, true) => "both",
            (true, false) => "EVM",
            _ => "SOL",
        };
        out.push_str(&format!("| `{}` | {} | {} |\n", k.key, k.desc, where_));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_parses_and_is_not_empty() {
        assert!(all().len() > 20, "keys.json should list every shortcut");
    }

    #[test]
    fn every_key_names_at_least_one_chain() {
        for k in all() {
            assert!(!k.chains.is_empty(), "`{}` belongs to no dashboard", k.key);
            for c in &k.chains {
                assert!(c == "evm" || c == "sol", "`{}` has unknown chain {c}", k.key);
            }
        }
    }

    /// The same key doing two different things on one chain is a collision, and
    /// the person who finds it should be whoever added the second one — not a
    /// user who pressed it expecting the first.
    #[test]
    fn no_key_is_claimed_twice_on_the_same_chain() {
        for chain in ["evm", "sol"] {
            let mut seen: std::collections::HashMap<&str, &str> = Default::default();
            for k in all().iter().filter(|k| k.chains.iter().any(|c| c == chain)) {
                if let Some(prev) = seen.insert(&k.key, &k.desc) {
                    panic!("on {chain}, `{}` is both \"{prev}\" and \"{}\"", k.key, k.desc);
                }
            }
        }
    }

    #[test]
    fn both_dashboards_get_rows_and_headings() {
        for chain in [Chain::Evm, Chain::Sol] {
            let rows = help_rows(chain);
            assert!(rows.iter().any(|(s, _)| !s.is_empty()), "has section headings");
            assert!(rows.iter().any(|(_, r)| r.starts_with("b|")), "has the buy key");
        }
    }

    /// docs/keys.md is generated from this file. If it has drifted, the docs
    /// are describing a build that no longer exists — which is the failure this
    /// whole module was written to make impossible.
    #[test]
    fn the_docs_page_matches_the_source() {
        let on_disk = include_str!("../docs/keys.md");
        assert_eq!(
            on_disk.trim(),
            markdown().trim(),
            "docs/keys.md is stale — regenerate it with `cargo run -- --dump-keys > docs/keys.md`"
        );
    }
}
