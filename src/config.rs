// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Deployment registry (networks -> tokens -> pools) + accounts, loaded from
//! config.json — networks, endpoints, and the app's few settings.
//!
//! Some schema fields (verified-pool metadata, explorer labels) mirror the JSON
//! and aren't all read yet — they back the dormant Verified-pool feature.
#![allow(dead_code)]

use alloy::primitives::Address;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Pool {
    pub label: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub pool_id: String,
    #[serde(default)]
    pub address: String,
    pub currency0: String,
    pub currency1: String,
    pub fee: u32,
    #[serde(default)]
    pub tick_spacing: u32,
    #[serde(default)]
    pub state_view: String,
    #[serde(default)]
    pub owned: bool, // true only for pools we actually created/own
}

fn default_kind() -> String {
    "v4".to_string()
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Token {
    pub name: String,
    pub symbol: String,
    pub address: String,
    #[serde(default)]
    pub pools: Vec<Pool>,
}

/// A real-world v4 pool we don't own but want to trade/test against. Only its
/// key parameters are needed — the pool id is computed from them.
#[derive(Debug, Deserialize, Clone)]
pub struct PublicPool {
    pub label: String,
    pub token: String, // currency1 (ETH is currency0)
    #[serde(default)]
    pub sym: String,
    pub fee: u32,
    pub tick_spacing: u32,
}

/// Which family of chain a registry entry describes. Drives the whole app path
/// after the start-screen picker: EVM uses alloy + Uniswap, Solana uses the
/// pump.fun engine. Defaults to `Evm` so every existing registry entry keeps
/// working untouched.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ChainKind {
    #[default]
    Evm,
    Solana,
}

impl ChainKind {
    pub fn is_solana(&self) -> bool {
        *self == ChainKind::Solana
    }
    /// Short tag for the picker / header.
    pub fn tag(&self) -> &'static str {
        match self {
            ChainKind::Evm => "evm",
            ChainKind::Solana => "sol",
        }
    }
}

/// `"url"`, `["url", …]`, or absent — all become a `Vec`. One endpoint is a
/// string, several are a list, and no second field name is needed to grow
/// from one to many. Old configs (string) and new ones (list) both parse.
fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Option::<OneOrMany>::deserialize(d)? {
        None => Vec::new(),
        Some(OneOrMany::One(s)) => vec![s],
        Some(OneOrMany::Many(v)) => v,
    })
}

#[derive(Default, Debug, Deserialize, Clone)]
pub struct Network {
    pub name: String,
    /// EVM chain id. `null` or absent for Solana, which has no equivalent —
    /// `deserialize_with` accepts both so an explicit null is not an error.
    #[serde(default, deserialize_with = "null_as_zero")]
    pub chain_id: u64,
    /// Chain family — `"evm"` (default) or `"solana"`.
    #[serde(default)]
    pub kind: ChainKind,
    /// RPC endpoint(s): one URL as a string, or several as a list, first is
    /// primary. Requests rotate across all of them with failover, and the
    /// transport steers each request to an endpoint that can answer it — so
    /// one field covers what used to be `rpc` + `rpcs` + `discovery_rpc`.
    #[serde(default, deserialize_with = "one_or_many")]
    pub rpc: Vec<String>,
    /// LEGACY — extra RPC endpoints from older configs. Still read, merged
    /// into the pool by `rpc_pool()`. New configs put a list in `rpc`.
    #[serde(default)]
    pub rpcs: Vec<String>,
    /// WebSocket endpoint(s): a string or a list, same union as `rpc`. The
    /// launch feed rotates across them on a drop or error. Providers often
    /// host WS on a DIFFERENT domain (e.g. `wss://ws.us.fluxrpc.com` for
    /// `https://us.fluxrpc.com`), so it can't be derived from `rpc` alone.
    #[serde(default, deserialize_with = "one_or_many")]
    pub ws: Vec<String>,
    /// LEGACY — the old separate discovery endpoint. Still read, merged into
    /// the pool by `rpc_pool()`; the balanced transport now decides per
    /// request which endpoint serves a log-heavy scan, so the distinction
    /// no longer needs to live in the config.
    #[serde(default)]
    pub discovery_rpc: Option<String>,
    /// Hand-curated "verified" pools (e.g. Robinhood stock tokens). Resolved ONCE
    /// and pinned by pool_id — the bot only prices these, never mines them. Edit
    /// by hand to add/remove. Everything else is live bot discovery.
    #[serde(default)]
    pub verified_pools: Vec<VerifiedPool>,
    #[serde(default)]
    pub tokens: Vec<Token>,
    #[serde(default)]
    pub public_pools: Vec<PublicPool>,
}

/// A pinned, pre-resolved pool for a "verified" token (stock tokens, etc.).
#[derive(Debug, Deserialize, Clone)]
pub struct VerifiedPool {
    pub sym: String,
    pub token: String,
    pub pool_id: String,
    #[serde(default)]
    pub quote: String, // "USDG" (default) or "weth()"
    #[serde(default)]
    pub tick_spacing: i32,
    #[serde(default)]
    pub fee: u32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Account {
    pub name: String,
    pub address: String,
    #[serde(default)]
    pub keystore: Option<String>,
}

/// RugCheck integration — token risk scoring (creator rug history, LP lock,
/// mint/freeze authority, holder concentration).
///
/// Keys live in the config file rather than in source, so they stay out of git.
#[derive(Debug, Deserialize, Clone)]
pub struct RugCheck {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Base URL. Defaults to the public API.
    #[serde(default = "default_rugcheck_url")]
    pub base_url: String,
    /// Full API key. Optional — the report endpoints work unauthenticated, the
    /// key just raises rate limits.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Shielded key: safe to expose, but capped at 5 req/s per IP. Used only if
    /// no full key is set.
    #[serde(default)]
    pub shield_key: Option<String>,
    /// Normalised score (0-100) at or above which a coin is flagged. RugCheck
    /// scores risk, so HIGHER is worse.
    #[serde(default = "default_risk_threshold")]
    pub warn_score: u32,
}

/// Accept `null` where a number is expected, mapping it to 0.
///
/// `"chain_id": null` is how a Solana entry says "this does not apply", and the
/// derived impl would reject it outright. Zero is the same thing the field
/// already means when absent.
fn null_as_zero<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    Ok(Option::<u64>::deserialize(d)?.unwrap_or(0))
}

fn default_true() -> bool {
    true
}
fn default_rugcheck_url() -> String {
    "https://api.rugcheck.xyz".to_string()
}
fn default_risk_threshold() -> u32 {
    40
}

impl Default for RugCheck {
    fn default() -> Self {
        RugCheck {
            enabled: true,
            base_url: default_rugcheck_url(),
            api_key: None,
            shield_key: None,
            warn_score: default_risk_threshold(),
        }
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Registry {
    #[serde(default)]
    pub default_account: Option<String>,
    #[serde(default)]
    pub accounts: Vec<Account>,
    #[serde(default)]
    pub networks: Vec<Network>,
    /// Token risk scoring. Absent = defaults (enabled, public API, no key).
    #[serde(default)]
    pub rugcheck: RugCheck,
    /// Keep your own address off the screen entirely. Absent = false.
    ///
    /// The wallet panel already prefers the account NAME, so the address only
    /// appears when a keystore has none. That is still an address on screen for
    /// a whole session, which is a whole session of screenshots, recordings and
    /// anyone glancing at the terminal. With this set, a nameless account reads
    /// as "account" rather than as the first and last characters of who you
    /// are.
    ///
    /// Only YOUR address. Token and pool addresses are unaffected: those are
    /// looked up and compared against an explorer, and hiding them would break
    /// the screen rather than protect anything.
    #[serde(default)]
    pub hide_address: Option<bool>,
    /// How long a Permit2 grant stays valid, IN HOURS. Absent = 24.
    ///
    /// A Permit2 grant names a spender, an amount and an expiry, and the expiry
    /// is the only part of an approval that revokes itself. Shorter is safer
    /// and costs a re-approval before the first trade of each window; longer is
    /// fewer transactions and a permission that lives longer than the trading
    /// it was for.
    ///
    /// 24 hours is the recommendation: it covers a session without leaving a
    /// grant standing across days you were not trading. Set it to 720 for a
    /// month if you would rather not think about it, or to 1 if you would
    /// rather approve every time.
    ///
    /// The unit is hours and the name does not say so — `24` is a day, `720`
    /// is a month. The clamp catches a zero or a century but cannot catch
    /// someone who meant days, so the docs and the schema both lead with the
    /// unit.
    ///
    /// Clamped when read, not here — a config file can say anything, and a
    /// zero or a century are both answers this should not simply obey.
    #[serde(default)]
    pub permit2_expiry: Option<u64>,
    /// Whether to open on the docs.
    ///
    /// Absent means "until you have been through them once" — the docs are
    /// onboarding, and onboarding that repeats forever is a splash screen you
    /// learn to dismiss without reading. Set it explicitly to keep them
    /// (`true`) or never see them again (`false`); `D` opens them from anywhere
    /// either way.
    #[serde(default)]
    pub start_on_docs: Option<bool>,
}

fn onboarded_path() -> std::path::PathBuf {
    std::path::Path::new(crate::state_dir()).join("onboarded")
}

/// Has the user been through the docs for THIS version?
///
/// Not merely "at some point". An update can move a key, rename a screen or add
/// a chain, and someone who read the docs three releases ago has read a
/// different set of docs. The version that was read is recorded, so the first
/// launch after an update opens on them once and then stops.
///
/// A marker in the CACHE, not the config: it is something that happened, not
/// something anyone decided, and the app writing to a file it tells you to edit
/// is exactly what the config/cache split exists to avoid.
/// Whether to keep the wallet's own address off the screen.
pub fn hide_address() -> bool {
    static HIDE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HIDE.get_or_init(|| {
        Registry::load(&config_path().to_string_lossy())
            .ok()
            .and_then(|r| r.hide_address)
            .unwrap_or(false)
    })
}

/// How long a Permit2 grant should live, in seconds.
///
/// Clamped to between an hour and a year. The floor stops a value that would
/// expire before the trade it was granted for could land; the ceiling stops a
/// typo becoming the year 2100 by another route, which is the defect this
/// setting exists to have fixed.
pub fn permit2_ttl_secs() -> u64 {
    // Read once. This is consulted while building an approval, which is on the
    // path to a trade — re-reading a file there to learn something that cannot
    // change without a restart would be work in the worst place to do it.
    static TTL: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *TTL.get_or_init(|| {
        let hours = Registry::load(&config_path().to_string_lossy())
            .ok()
            .and_then(|r| r.permit2_expiry)
            .unwrap_or(DEFAULT_PERMIT2_EXPIRY);
        clamp_permit2_expiry(hours) * 3_600
    })
}

/// The recommended window, and what you get by saying nothing.
pub const DEFAULT_PERMIT2_EXPIRY: u64 = 24;

/// An hour at the least, a year at the most.
fn clamp_permit2_expiry(hours: u64) -> u64 {
    hours.clamp(1, 8_760)
}

pub fn onboarded() -> bool {
    let Ok(text) = std::fs::read_to_string(onboarded_path()) else {
        return false;
    };
    // The version is the first line. An older marker has prose there instead,
    // which will not match — so an existing install sees the docs once on the
    // release that introduces this, which is the right answer anyway.
    text.lines().next().map(str::trim) == Some(env!("CARGO_PKG_VERSION"))
}

/// Remember that they have, and for which version. Failures are ignored — the
/// cost is seeing the docs once more, which is not worth failing a startup over.
pub fn mark_onboarded() {
    let _ = std::fs::create_dir_all(crate::state_dir());
    let _ = std::fs::write(
        onboarded_path(),
        format!(
            "{}\n\n\
             The version whose docs have been read. The app opens on the docs\n\
             again after an update, then stops. Delete this file to see them on\n\
             the next start, or set start_on_docs in your config to decide it\n\
             outright.\n",
            env!("CARGO_PKG_VERSION")
        ),
    );
}

#[cfg(test)]
mod endpoint_union_tests {
    use super::*;

    #[test]
    fn rpc_and_ws_accept_a_string_or_a_list() {
        let one: Network =
            serde_json::from_str(r#"{"name":"x","rpc":"https://a/rpc","ws":"wss://w"}"#).unwrap();
        assert_eq!(one.rpc, vec!["https://a/rpc"]);
        assert_eq!(one.ws, vec!["wss://w"]);

        let many: Network = serde_json::from_str(
            r#"{"name":"x","rpc":["https://a/rpc","https://b/rpc"],"ws":["wss://w1","wss://w2"]}"#,
        )
        .unwrap();
        assert_eq!(many.rpc.len(), 2);
        assert_eq!(many.ws_pool(), vec!["wss://w1", "wss://w2"]);
    }

    #[test]
    fn a_previous_versions_config_still_forms_the_same_pool() {
        // The exact shape older releases wrote: string rpc, extras in `rpcs`,
        // the Alchemy key parked in `discovery_rpc`. All of it must land in
        // one pool, primary first, nothing lost.
        let old: Network = serde_json::from_str(
            r#"{"name":"robinhood-mainnet","rpc":"https://public/rpc",
                "rpcs":["https://extra/rpc"],
                "discovery_rpc":"https://alchemy/v2/realkey"}"#,
        )
        .unwrap();
        assert_eq!(
            old.rpc_pool(),
            vec!["https://public/rpc", "https://extra/rpc", "https://alchemy/v2/realkey"]
        );
    }

    #[test]
    fn the_starter_config_parses_and_pools_cleanly() {
        let reg: Registry = serde_json::from_str(&starter_json()).unwrap();
        for net in &reg.networks {
            let pool = net.rpc_pool();
            assert!(!pool.is_empty(), "{} has no usable endpoint", net.name);
            assert!(pool.iter().all(|u| !u.contains("YOUR_")), "placeholder leaked into {}", net.name);
        }
    }
}

#[cfg(test)]
mod onboarding_tests {
    /// The rule the marker encodes, without touching the real state directory.
    fn read_docs_for(marker: Option<&str>, running: &str) -> bool {
        match marker {
            None => false,
            Some(text) => text.lines().next().map(str::trim) == Some(running),
        }
    }

    #[test]
    fn an_update_shows_the_docs_once_more() {
        // Same version: already read, stay out of the way.
        assert!(read_docs_for(Some("0.1.4\n\nsome prose"), "0.1.4"));
        // Updated since: the docs are a different set now.
        assert!(!read_docs_for(Some("0.1.3\n\nsome prose"), "0.1.4"));
        // Never read.
        assert!(!read_docs_for(None, "0.1.4"));
    }

    #[test]
    fn a_marker_from_before_this_existed_counts_as_unread() {
        // Older builds wrote prose on the first line. It cannot match a
        // version, so those installs see the docs once — which is correct, as
        // they are on a new release.
        assert!(!read_docs_for(Some("The docs have been read once, so the app…"), "0.1.4"));
    }
}

/// Where the config lives, in the order it is looked for.
///
/// 1. `$TRENCHES_CONFIG`, for anyone running several profiles.
/// 2. `./config.json`, so a checkout can carry its own settings.
/// 3. `~/.config/trenches/config.json` — the real home.
pub const CONFIG_NAME: &str = "config.json";

pub fn config_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("TRENCHES_CONFIG") {
        if !p.trim().is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    // A local `config.json` wins, so a checkout can deliberately carry its
    // own settings.
    let local = std::path::PathBuf::from(CONFIG_NAME);
    if local.exists() {
        return local;
    }
    config_dir().join(CONFIG_NAME)
}

/// Where a fresh config is WRITTEN, ignoring whatever sits in the working
/// directory. "Create my config" should put it where the docs say it lives.
pub fn init_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("TRENCHES_CONFIG") {
        if !p.trim().is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    config_dir().join(CONFIG_NAME)
}

/// The user's home directory.
///
/// `HOME` is not set on Windows — it is `USERPROFILE` there — and every path in
/// this app was reading `HOME` alone. On Windows that meant the keystore
/// directory failed to resolve at all, so the wallet screen had nothing to
/// offer and no way to say why, while the site and the installer both advertised
/// Windows support.
pub fn home_dir() -> Option<std::path::PathBuf> {
    for k in ["HOME", "USERPROFILE"] {
        if let Ok(v) = std::env::var(k) {
            if !v.trim().is_empty() {
                return Some(std::path::PathBuf::from(v));
            }
        }
    }
    // Windows also splits it across two variables when neither of the above is
    // set — an older shell, or a service account.
    match (std::env::var("HOMEDRIVE"), std::env::var("HOMEPATH")) {
        (Ok(d), Ok(p)) if !d.is_empty() && !p.is_empty() => Some(std::path::PathBuf::from(format!("{d}{p}"))),
        _ => None,
    }
}

/// `~/.config/trenches`, or the platform equivalent.
pub fn config_dir() -> std::path::PathBuf {
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        if !x.trim().is_empty() {
            return std::path::PathBuf::from(x).join("trenches");
        }
    }
    home_dir()
        .unwrap_or_else(|| ".".into())
        .join(".config")
        .join("trenches")
}

/// A registry with nothing secret in it, written on first run.
///
/// There is no place for a seed phrase in it, and that is not an omission: the
/// config type has no field for one, so a phrase written here is ignored rather
/// than honoured. Accounts are password-encrypted keystores, full stop. A config
/// file gets backed up, synced between machines, and pasted into a bug report by
/// someone trying to be helpful — none of which should be able to cost anyone
/// their funds.
///
/// The RPCs are public endpoints, so a first run works before anyone has signed
/// up for anything, and the keyed alternatives are named in `_help` rather than
/// left for the user to discover.
pub fn starter_json() -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "$schema": "https://trenches.sh/config.schema.json",
        "_help": [
            "Trenches configuration. Edit this file, then restart the app.",
            "",
            "chain_id       EVM only. Solana has no equivalent, so the field is simply absent",
            "               there — networks are identified by name.",
            "rpc            One URL, or a LIST of URLs — requests rotate across all of them,",
            "               with failover, and a rate-limited endpoint is rested while the",
            "               others carry on. The defaults are public and rate-limited; for",
            "               anything serious add your own key-bearing URL to the list.",
            "               Alchemy, Helius, QuickNode and Ankr all work.",
            "ws             Websocket, Solana only. Also one URL or a list. Often a different",
            "               host than the RPC, which is why it is not derived.",
            "accounts       Added from inside the app — press W. Nothing to write by hand.",
            "",
            "The *_example keys are placeholders showing each provider's URL shape. Copy one",
            "into the real field (rpc / ws), paste your key, and delete the example — the",
            "app ignores any field it does not recognise, so they cost nothing if you",
            "leave them. Older configs with `rpcs` / `discovery_rpc` still work: those",
            "fields are read and merged into the same pool.",
            "",
            "This file holds API keys. It is yours, it stays on this machine, and nothing here",
            "is sent anywhere except the endpoints you name. Never put a seed phrase in it: the",
            "app reads password-encrypted keystores and never needs one.",
        ],
        "start_on_docs": null,
        "accounts": [],
        "networks": [
            {
                "name": "robinhood-mainnet",
                "kind": "evm",
                "chain_id": 4663,
                // One URL or a list. Add a keyed endpoint (shape below) next
                // to the public one and requests spread across both.
                "rpc": ["https://rpc.mainnet.chain.robinhood.com/rpc"],
                "rpc_alchemy_example": "https://robinhood-mainnet.g.alchemy.com/v2/YOUR_ALCHEMY_KEY",
                "tokens": [],
                "public_pools": []
            },
            {
                "name": "base-mainnet",
                "kind": "evm",
                "chain_id": 8453,
                // Flaunch's home chain. Uniswap v3 and v4 both live here, so
                // the same trading path serves it — only the addresses differ.
                "rpc": ["https://mainnet.base.org"],
                "rpc_alchemy_example": "https://base-mainnet.g.alchemy.com/v2/YOUR_ALCHEMY_KEY",
                "tokens": [],
                "public_pools": []
            },
            {
                "name": "solana-mainnet",
                // Explicitly null, not absent: Solana has no EVM chain id, and a
                // reader should see that the question was asked and answered
                // rather than wonder whether a line went missing.
                "chain_id": null,
                "kind": "solana",
                // One URL or a list; the public endpoint works and is slow.
                // Add a keyed one (shape below) and requests spread across
                // both, resting whichever is rate limited.
                "rpc": ["https://api.mainnet-beta.solana.com"],
                "rpc_helius_example": "https://mainnet.helius-rpc.com/?api-key=YOUR_HELIUS_KEY",
                // Websocket, one URL or a list — the launch feed rotates on a
                // drop. Providers usually serve WS on a different host than
                // HTTP, which is why it is a separate field rather than derived.
                // Prefilled with the hosted endpoint so the only edit needed
                // is the key. Left as a placeholder it reads as unset — see
                // `clean_urls` — so the app polls and says so rather than
                // hammering a URL that cannot resolve.
                "ws": "wss://rpc.trenches.sh/<your-key>/robinhood-ws",
                "ws_flux_example": "wss://ws.us.fluxrpc.com?key=YOUR_FLUX_KEY",
                "tokens": [],
                "public_pools": []
            }
        ],
        "rugcheck": {
            "enabled": true,
            "base_url": "https://api.rugcheck.xyz",
            "api_key": null,
            "warn_score": 40
        }
    }))
    .unwrap_or_default()
}

/// The chains we develop against: a testnet and a local node.
///
/// Behind the `testnet` cargo feature, so a production binary does not merely
/// hide them — it does not contain them. A runtime flag would still leave the
/// endpoints in the shipped binary and one argument away from a picker that
/// offers a chain nobody can trade on.
///
/// Build with them:  cargo build --release --features solana,testnet
#[cfg(feature = "testnet")]
pub fn dev_networks() -> Vec<Network> {
    let mk = |name: &str, chain_id: u64, rpc: &str| Network {
        name: name.to_string(),
        chain_id,
        kind: ChainKind::Evm,
        rpc: vec![rpc.to_string()],
        ..Default::default()
    };
    vec![
        mk("robinhood-testnet", 46630, "https://rpc.testnet.chain.robinhood.com/rpc"),
        mk("anvil-local", 31337, "http://127.0.0.1:8545"),
    ]
}

/// Nothing, in a production build.
#[cfg(not(feature = "testnet"))]
pub fn dev_networks() -> Vec<Network> {
    Vec::new()
}

/// Set one field on one network, in the config file, in place.
///
/// Edits a parsed `Value` rather than serialising the `Registry` back out. The
/// config is a file a person owns and may have put things in that this app does
/// not model — comments in `_help`, fields from a newer version, keys we have
/// no struct for. Round-tripping through our own types would silently delete
/// every one of them.
///
/// Written to a temporary file and renamed, so an interrupted write cannot
/// leave a truncated config where the working one was.
pub fn set_network_field(network: &str, field: &str, value: &str) -> eyre::Result<()> {
    let path = config_path();
    let mut root: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(t) => serde_json::from_str(&t)?,
        Err(_) => serde_json::from_str(&starter_json())?,
    };

    let nets = root
        .get_mut("networks")
        .and_then(|n| n.as_array_mut())
        .ok_or_else(|| eyre::eyre!("config has no networks list"))?;
    let net = nets
        .iter_mut()
        .find(|n| n.get("name").and_then(|x| x.as_str()) == Some(network))
        .ok_or_else(|| eyre::eyre!("no network called {network} in the config"))?;

    // An empty value clears the field rather than writing "", so backing out of
    // a prompt does not leave an endpoint that resolves to nothing.
    let obj = net.as_object_mut().ok_or_else(|| eyre::eyre!("malformed network entry"))?;
    if value.trim().is_empty() {
        obj.remove(field);
    } else {
        // Several URLs separated by commas become a list; one stays a string.
        // Both parse — `rpc` and `ws` accept either shape.
        let parts: Vec<&str> = value.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        let v = if parts.len() > 1 {
            serde_json::Value::Array(
                parts.into_iter().map(|s| serde_json::Value::String(s.to_string())).collect(),
            )
        } else {
            serde_json::Value::String(value.trim().to_string())
        };
        obj.insert(field.to_string(), v);
    }

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&root)?)?;
    owner_only(&tmp);
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Make a file readable by its owner alone.
///
/// This file holds RPC API keys in plaintext, and at the default umask it is
/// created world-readable — every other account on the machine can read the
/// keys you pay for. Set on the temp file BEFORE the rename, so the published
/// path is never briefly world-readable. Best effort: a filesystem that cannot
/// represent the mode is not a reason to refuse to save.
pub fn owner_only(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// The names of the networks in the config, for a picker.
///
/// Testnets and local nodes are left out. Nobody sets an API key on a chain with
/// no money on it or on a node running on their own laptop, so listing them is
/// two rows of noise in front of the two that matter. They stay editable by hand
/// in the file.
pub fn network_names() -> Vec<String> {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| {
            v.get("networks").and_then(|n| n.as_array()).map(|a| {
                a.iter()
                    .filter_map(|n| n.get("name").and_then(|x| x.as_str()))
                    .filter(|n| {
                        let n = n.to_lowercase();
                        !n.contains("testnet") && !n.contains("anvil") && !n.contains("local")
                    })
                    .map(str::to_string)
                    .collect()
            })
        })
        .unwrap_or_default()
}

impl Registry {
    pub fn load(path: &str) -> eyre::Result<Registry> {
        let bytes = std::fs::read_to_string(path)?;
        // Tighten on READ, not only on write. Setting the mode when saving does
        // nothing for a config that already exists and is never saved again —
        // which is every config written before this, holding its API keys
        // world-readable in perpetuity. Cheap, idempotent, and the one moment
        // the app is guaranteed to touch the file.
        owner_only(std::path::Path::new(path));
        Ok(serde_json::from_str(&bytes)?)
    }

    /// Load the registry, writing a starter file if there is nothing yet.
    ///
    /// Returns the config path alongside it, and whether this run created it —
    /// a first run has something to say that later runs do not.
    pub fn load_or_create() -> eyre::Result<(Registry, std::path::PathBuf, bool)> {
        let path = config_path();
        if path.exists() {
            let reg = Registry::load(&path.to_string_lossy())
                .map_err(|e| eyre::eyre!("{} could not be read: {e}", path.display()))?;
            return Ok((reg, path, false));
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, starter_json())?;
        owner_only(&path); // it will hold API keys the moment anyone edits it
        let reg = Registry::load(&path.to_string_lossy())?;
        Ok((reg, path, true))
    }

    pub fn network(&self, name_or_chain: &str) -> Option<&Network> {
        self.networks
            .iter()
            .find(|n| n.name.eq_ignore_ascii_case(name_or_chain) || n.chain_id.to_string() == name_or_chain)
    }
}

/// Drop blanks, unfilled placeholders (`YOUR_…`), and duplicates — a config
/// value that was never filled in must be skipped, not dialed.
fn clean_urls(mut v: Vec<String>, scheme: &str) -> Vec<String> {
    v.retain(|u| {
        let u = u.trim();
        // An unreplaced placeholder is not an endpoint. Both spellings the
        // shipped config uses — `YOUR_KEY` and `<your-key>` — read as unset, so
        // a prefilled line waiting to be edited behaves like an empty field
        // rather than like an endpoint that refuses every connection.
        let placeholder = u.contains("YOUR_") || u.contains('<') || u.contains('>');
        !u.is_empty() && !placeholder && u.starts_with(scheme)
    });
    let mut seen = std::collections::HashSet::new();
    v.retain(|u| seen.insert(u.clone()));
    v
}

impl Network {
    /// Every RPC endpoint for this network, primary first — the `rpc` union
    /// plus the legacy `rpcs` and `discovery_rpc` fields, cleaned and deduped.
    /// One pool: the transport decides per request who answers.
    pub fn rpc_pool(&self) -> Vec<String> {
        let mut v = self.rpc.clone();
        v.extend(self.rpcs.iter().cloned());
        v.extend(self.discovery_rpc.iter().cloned());
        clean_urls(v, "http")
    }

    /// Every configured WebSocket endpoint, primary first, cleaned the same
    /// way as the HTTP pool.
    pub fn ws_pool(&self) -> Vec<String> {
        clean_urls(self.ws.clone(), "ws")
    }
}

impl Pool {
    pub fn is_v4(&self) -> bool {
        !self.kind.eq_ignore_ascii_case("toy")
    }
    pub fn token(&self) -> eyre::Result<Address> {
        // The non-ETH side (currency1 for an ETH pool).
        Ok(self.currency1.parse()?)
    }
}


#[cfg(test)]
mod permit2_ttl_tests {
    use super::*;

    #[test]
    fn saying_nothing_gets_you_a_day() {
        assert_eq!(clamp_permit2_expiry(DEFAULT_PERMIT2_EXPIRY) * 3_600, 86_400);
    }

    #[test]
    fn a_zero_becomes_an_hour_rather_than_an_expired_grant() {
        assert_eq!(clamp_permit2_expiry(0), 1);
    }

    #[test]
    fn an_absurd_window_is_capped_at_a_year() {
        assert_eq!(clamp_permit2_expiry(1_000_000), 8_760);
        assert_eq!(clamp_permit2_expiry(8_760), 8_760, "a year exactly is allowed");
    }

    #[test]
    fn a_month_is_honoured() {
        assert_eq!(clamp_permit2_expiry(720), 720);
    }
}
