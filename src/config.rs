// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Deployment registry (networks -> tokens -> pools) + accounts, loaded from
//! deployments.json — the same hierarchical format as the Zig engine.
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
    pub rpc: String,
    /// Extra RPC endpoints. Requests round-robin across `rpc` + these, with
    /// failover, so no single provider's rate limit caps throughput.
    #[serde(default)]
    pub rpcs: Vec<String>,
    /// WebSocket endpoint. Solana providers often host WS on a DIFFERENT domain
    /// (e.g. `wss://ws.us.fluxrpc.com` for `https://us.fluxrpc.com`), so it can't
    /// reliably be derived from `rpc` — set it explicitly when they differ.
    #[serde(default)]
    pub ws: Option<String>,
    /// Optional separate endpoint for Discovery's batched pool-metric reads, kept
    /// off the trading `rpc` so a graduation scan never starves the trade loop.
    /// Best a batch-capable provider (e.g. Alchemy). Falls back to `rpc` if unset.
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
    pub quote: String, // "USDG" (default) or "WETH"
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

/// Where the registry lives, in the order it is looked for.
///
/// 1. `$TRENCHES_CONFIG`, for anyone running several profiles.
/// 2. `~/.config/trenches/deployments.json` — the real home.
/// 3. `./deployments.json`, only if it already exists.
///
/// The cwd came first historically, which was fine when the binary was run out
/// of its own checkout and fatal the moment it was installed to `~/.local/bin`:
/// a fresh user's first `trenches` died on a missing file in whatever directory
/// their shell happened to be in.
/// What the config file is called.
pub const CONFIG_NAME: &str = "config.json";
/// What it used to be called. Read, never written.
pub const LEGACY_NAME: &str = "deployments.json";

pub fn config_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("TRENCHES_CONFIG") {
        if !p.trim().is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    // A local `config.json` still wins, so a checkout can deliberately carry its
    // own settings. A local `deployments.json` does NOT: that is the old name,
    // a copy is lying around in every working tree, and letting it outrank the
    // real config meant `--init` kept reporting a stray file in whatever folder
    // the shell happened to be in.
    let local = std::path::PathBuf::from(CONFIG_NAME);
    if local.exists() {
        return local;
    }
    let canonical = config_dir().join(CONFIG_NAME);
    if canonical.exists() {
        return canonical;
    }
    // Only when nothing current exists: an old config in the config directory is
    // still worth reading rather than starting someone from scratch.
    let legacy = config_dir().join(LEGACY_NAME);
    if legacy.exists() {
        return legacy;
    }
    canonical
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
            "rpc            The endpoint the app trades through. The defaults are public and",
            "               rate-limited; for anything serious put your own key-bearing URL here.",
            "discovery_rpc  Optional. Used for log-heavy scans (new pools, buyer charts). Public",
            "               endpoints usually refuse these, so discovery stays quiet without one.",
            "               Alchemy, Helius, QuickNode and Ankr all work.",
            "ws             Websocket, Solana only. Often a different host than the RPC.",
            "accounts       Added from inside the app — press W. Nothing to write by hand.",
            "",
            "The *_example keys are placeholders showing each provider's URL shape. Copy one",
            "over the real field (rpc / ws / discovery_rpc), paste your key, and delete the",
            "example — the app ignores any field it does not recognise, so they cost nothing",
            "if you leave them.",
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
                "rpc": "https://rpc.mainnet.chain.robinhood.com/rpc",
                // Log-heavy scans only — new pools, buyer charts. The public
                // endpoint refuses these, so discovery stays quiet until set.
                "discovery_rpc": "",
                "discovery_rpc_alchemy_example": "https://robinhood-mainnet.g.alchemy.com/v2/YOUR_ALCHEMY_KEY",
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
                // The public endpoint works and is slow. Swap the whole URL for
                // a keyed one — the placeholders below are the shape each
                // provider expects, so a key can be dropped straight in.
                "rpc": "https://api.mainnet-beta.solana.com",
                "rpc_helius_example": "https://mainnet.helius-rpc.com/?api-key=YOUR_HELIUS_KEY",
                // Websocket. Providers usually serve it on a different host than
                // HTTP, which is why it is a separate field rather than derived.
                "ws": "",
                "ws_flux_example": "wss://ws.us.fluxrpc.com?key=YOUR_FLUX_KEY",
                "discovery_rpc": "",
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
        rpc: rpc.to_string(),
        rpcs: Vec::new(),
        ws: None,
        discovery_rpc: None,
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
        obj.insert(field.to_string(), serde_json::Value::String(value.trim().to_string()));
    }

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&root)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
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
        let reg = Registry::load(&path.to_string_lossy())?;
        Ok((reg, path, true))
    }

    pub fn network(&self, name_or_chain: &str) -> Option<&Network> {
        self.networks
            .iter()
            .find(|n| n.name.eq_ignore_ascii_case(name_or_chain) || n.chain_id.to_string() == name_or_chain)
    }
}

impl Network {
    /// Every RPC endpoint for this network, primary first.
    pub fn rpc_pool(&self) -> Vec<String> {
        let mut v = vec![self.rpc.clone()];
        v.extend(self.rpcs.iter().cloned());
        v.retain(|u| !u.trim().is_empty());
        v
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
