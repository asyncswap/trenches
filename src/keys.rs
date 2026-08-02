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
    let mine: Vec<&Key> =
        all().iter().filter(|k| k.chains.iter().any(|c| c == chain.tag())).collect();
    let mut out = Vec::new();
    let mut section = "";
    let mut i = 0;
    while i < mine.len() {
        let k = mine[i];
        if k.section != section {
            section = &k.section;
            out.push((section.to_string(), String::new()));
        }
        // Two keys that are one control share a line.
        //
        // The DATA stays one key per entry — that is what makes `]` findable
        // and the collision check possible. But a help screen is scanned, not
        // queried, and "decrease buy size" directly above "increase buy size"
        // is two lines saying one thing. The pairing is derived rather than
        // declared: nothing extra to keep in step, and a pair that stops being
        // adjacent stops being grouped, which is correct.
        if let Some(next) = mine.get(i + 1) {
            if let (Some(a), Some(b)) =
                (k.desc.strip_prefix("decrease "), next.desc.strip_prefix("increase "))
            {
                if a == b && k.section == next.section && k.chains == next.chains {
                    out.push((String::new(), format!("{}  {}|{a} −/+", k.key, next.key)));
                    i += 2;
                    continue;
                }
            }
        }
        out.push((String::new(), format!("{}|{}", k.key, k.desc)));
        i += 1;
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

    /// The help screen pairs what the data keeps apart. `[` and `]` are two
    /// entries, so `]` is findable and checkable, and one line on screen.
    #[test]
    fn a_decrease_increase_pair_shares_one_help_row() {
        let rows = help_rows(Chain::Evm);
        assert!(
            rows.iter().any(|(_, r)| r == "[  ]|buy size −/+"),
            "the pair should render as one row: {rows:?}"
        );
        assert!(
            !rows.iter().any(|(_, r)| r.starts_with("[|")),
            "and not also as its halves"
        );
    }

    /// Only genuine pairs. Two unrelated keys that happen to sit together must
    /// not be welded into one line.
    #[test]
    fn unrelated_neighbours_keep_their_own_rows() {
        let rows = help_rows(Chain::Evm);
        assert!(rows.iter().any(|(_, r)| r == "b|buy"));
        assert!(rows.iter().any(|(_, r)| r == "s|sell"));
    }

    #[test]
    fn both_dashboards_get_rows_and_headings() {
        for chain in [Chain::Evm, Chain::Sol] {
            let rows = help_rows(chain);
            assert!(rows.iter().any(|(s, _)| !s.is_empty()), "has section headings");
            assert!(rows.iter().any(|(_, r)| r.starts_with("b|")), "has the buy key");
        }
    }

    /// The marketing site is a separate repository, so it cannot `include_str!`
    /// this file — it keeps a copy at `trenches.sh/src/keys.json`. A copy that
    /// can go stale quietly is the failure this module exists to remove, so
    /// when that checkout is next to ours, the copy is checked.
    ///
    /// Skipped when the sibling is absent: CI and anyone who cloned only this
    /// repo should not fail a test over a directory they have no reason to
    /// have. The check runs where the change is made, which is here.
    #[test]
    fn the_website_copy_matches_the_source() {
        let site = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../trenches.sh/src/keys.json");
        let Ok(theirs) = std::fs::read_to_string(&site) else { return };
        let ours = include_str!("../keys.json");
        assert_eq!(
            theirs.trim(),
            ours.trim(),
            "the website's keys.json is stale — refresh it with `cp keys.json ../trenches.sh/src/keys.json`"
        );
    }

    /// docs/keys.md is generated from this file, for people reading the
    /// repository rather than running the app. The Shortcuts page in the app no
    /// longer embeds it — that renders `markdown()` directly, so it cannot be
    /// stale — but a checked-in file that quietly stops matching is still a
    /// reader being told something untrue.
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
