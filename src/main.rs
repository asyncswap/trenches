// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs
//! Robinhood Chain speed bot (Rust). Full feature port of the Zig engine,
//! built speed-first: concurrent reads, pre-flight gas protection, and ready
//! for a local Nitro node over ws:// or IPC (remote ~380ms -> local ~1ms).

mod config;
mod contracts;
mod discover;
mod engine;
mod ledger;
mod pnl;
mod pricing;
/// Solana / pump.fun adapter — compiled only with `--features solana`.
#[cfg(feature = "solana")]
mod sol;
mod ui;
mod update;
mod v3;
mod v4;
mod view;
mod wallet;

use std::collections::VecDeque;
use std::time::Duration;

use alloy::network::EthereumWallet;
use alloy::primitives::B256;
use alloy::providers::{Provider, ProviderBuilder};
use crossterm::{
    event::{Event, KeyCode},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{prelude::*, widgets::*};

use config::Registry;
use engine::{Bot, Side, Strategy};

/// Where the config was read from, for anything that needs to name it.
///
/// Nothing writes to it any more: discovered tokens go to the cache, so the
/// only file the app edits is one it owns. A config a user is told to edit
/// should not also be edited behind their back.
static REGISTRY_PATH: std::sync::OnceLock<String> = std::sync::OnceLock::new();


/// A selectable pool: either one we own (from the registry) or a public
/// real-world pool (added by key params, pool id computed).
#[derive(Clone)]
struct SelPool {
    label: String,
    kind: engine::PoolKind, // carries v4 pool_id or v3 pool_addr — no half-empty fields
    token: alloy::primitives::Address,
    sym: String,
    fee: u32,
    owned: bool,
    quote: engine::Quote,  // ETH or a stablecoin (type-safe, no ETH assumption)
    quote_sym: String,     // display symbol for the quote side
}

/// Symbol for a known stablecoin quote token (fallback "USD").
fn stable_symbol(addr: alloy::primitives::Address) -> String {
    match format!("{addr:#x}").as_str() {
        "0x5fc5360d0400a0fd4f2af552add042d716f1d168" => "USDG".to_string(),
        _ => "USD".to_string(),
    }
}

/// CoinGecko coin id for a stablecoin quote token, for the live USD fetch.
fn stable_cg_id(addr: alloy::primitives::Address) -> Option<&'static str> {
    match format!("{addr:#x}").as_str() {
        "0x5fc5360d0400a0fd4f2af552add042d716f1d168" => Some("global-dollar"),
        _ => None,
    }
}

/// ERC-20 decimals for a known stablecoin quote (USDG is 6-dec, not 18).
/// Fallback 18. TODO: read on-chain for arbitrary stables.
fn stable_decimals(addr: alloy::primitives::Address) -> u8 {
    match format!("{addr:#x}").as_str() {
        "0x5fc5360d0400a0fd4f2af552add042d716f1d168" => 6, // USDG
        _ => 18,
    }
}

/// Live USD value of one unit of a quote currency, from the CoinGecko feed.
/// Stables fall back to $1 only until the first fetch lands — never permanently.
fn quote_usd_of(quote: engine::Quote, eth_usd: f64, prices: &std::collections::HashMap<String, f64>) -> f64 {
    match quote {
        engine::Quote::Eth => eth_usd,
        engine::Quote::Stable { token, .. } => stable_cg_id(token)
            .and_then(|id| prices.get(id).copied())
            .unwrap_or(1.0),
    }
}

/// Push freshly-fetched USD prices into the bot: ETH and each pool's quote.
fn apply_prices(bot: &mut Bot, prices: &std::collections::HashMap<String, f64>) {
    if let Some(&e) = prices.get("ethereum") {
        if e > 0.0 {
            bot.eth_usd = e;
        }
    }
    let eu = bot.eth_usd;
    bot.pool.quote_usd = quote_usd_of(bot.pool.quote, eu, prices);
    if let Some(pb) = bot.pool_b.as_mut() {
        pb.quote_usd = quote_usd_of(pb.quote, eu, prices);
    }
}

/// CoinGecko ids to fetch for a set of pools: ETH plus every stable quote.
fn price_ids(pools: &[SelPool]) -> Vec<&'static str> {
    let mut ids = vec!["ethereum"];
    for p in pools {
        if let engine::Quote::Stable { token, .. } = p.quote {
            if let Some(id) = stable_cg_id(token) {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    ids
}

/// Remember the last pool used, per chain, so the next session starts on it.
/// The wallet unlocked last, so it can be offered first next time.
fn last_wallet_path() -> String {
    format!("{}/last-wallet.txt", state_dir())
}

fn load_last_wallet() -> Option<String> {
    std::fs::read_to_string(last_wallet_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn save_last_wallet(name: &str) {
    let _ = std::fs::create_dir_all(state_dir());
    let _ = std::fs::write(last_wallet_path(), name);
}

/// The chain used last, remembered across runs.
///
/// Stored by NAME rather than by index: the config is a file people edit, and an
/// index would silently point at a different chain the moment a line moved.
fn last_chain_path() -> String {
    format!("{}/last-chain.txt", state_dir())
}

fn load_last_chain() -> Option<String> {
    std::fs::read_to_string(last_chain_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn save_last_chain(name: &str) {
    let _ = std::fs::create_dir_all(state_dir());
    let _ = std::fs::write(last_chain_path(), name);
}

/// Append a diagnostic line to this session's trace.
///
/// Separate from the trading log: that records what was traded, this records
/// what the app BELIEVED — decimals, quote currency, token ordering. Every
/// pricing bug so far has come from one of those being wrong, and none of them
/// were visible on screen until the number was already nonsense.
pub fn trace(msg: &str) {
    use std::io::Write;
    use std::sync::{Mutex, OnceLock};
    static FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    let start = START.get_or_init(std::time::Instant::now);
    let f = FILE.get_or_init(|| {
        std::fs::create_dir_all(state_dir()).ok()?;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(format!("{}/evm-trace-{ts}.log", state_dir()))
            .ok()
            .map(Mutex::new)
    });
    if let Some(f) = f {
        if let Ok(mut f) = f.lock() {
            let _ = writeln!(f, "{:8.3}  {msg}", start.elapsed().as_secs_f64());
            let _ = f.flush();
        }
    }
}

/// Everything the pricing math depends on, in one line, whenever a pool loads.
pub fn trace_pool(where_: &str, p: &engine::PoolCfg) {
    trace(&format!(
        "pool {where_}: sym={} quote={} quote_dec={} token_dec={} kind={} token={:#x} fee={}",
        p.sym,
        p.quote_sym,
        p.quote.decimals(),
        p.token_decimals,
        p.kind.proto(),
        p.token,
        p.fee,
    ));
}

/// Directory for everything this bot writes: session logs, the daily PnL file,
/// the saved theme, the last pool. One constant so a rename cannot leave half
/// the app writing to the old place.
/// Everything this bot writes: session logs, traces, the theme, cached tokens,
/// the fill ledger, the daily PnL baseline.
///
/// Resolved once, and ABSOLUTE. It used to be the literal `".trenches"`, which
/// is relative to whatever directory the shell happened to be in — so a binary
/// on your PATH scattered a fresh, empty state directory everywhere it was run
/// from, and a PnL calendar opened from the wrong folder found no trades because
/// they were written somewhere else. Every doc already said `~/.trenches`.
///
/// A `./.trenches` that already exists still wins, so a checkout that has been
/// accumulating logs keeps them.
pub fn state_dir() -> &'static str {
    static DIR: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let local = std::path::Path::new(".trenches");
        if local.is_dir() {
            return ".trenches".to_string();
        }
        match config::home_dir() {
            Some(h) => h.join(".trenches").to_string_lossy().into_owned(),
            None => ".trenches".to_string(),
        }
    })
}

/// Read an ERC-20 symbol on-chain (fallback "TOK").
async fn read_symbol<P: Provider>(provider: &P, token: alloy::primitives::Address) -> String {
    contracts::IERC20::new(token, provider)
        .symbol()
        .call()
        .await
        .map(|s| s._0)
        .unwrap_or_else(|_| "TOK".to_string())
}

/// Auto-find a token's WETH v3 pool: scan the fee tiers and return the first with
/// live liquidity as (pool_addr, fee, weth_is_token0). None if the token has no
/// liquid v3 pool.
async fn find_v3_pool<P: Provider>(
    provider: &P,
    token: alloy::primitives::Address,
) -> Option<(alloy::primitives::Address, u32, bool)> {
    let factory = contracts::IV3Factory::new(contracts::V3_FACTORY, provider);
    for fee in [10000u32, 3000, 500, 100] {
        if let Ok(p) = factory.getPool(token, contracts::WETH, fee.try_into().unwrap()).call().await {
            let addr = p.pool;
            if addr != alloy::primitives::Address::ZERO {
                let liq = contracts::IV3Pool::new(addr, provider)
                    .liquidity()
                    .call()
                    .await
                    .map(|l| l._0)
                    .unwrap_or(0);
                if liq > 0 {
                    return Some((addr, fee, contracts::WETH < token));
                }
            }
        }
    }
    None
}

/// Candidate ETH-quoted venues for a token — the routes a buy/sell is simulated
/// across for best execution. Only ETH-quoted pools qualify (proceeds in ETH).
fn routes_for(pools: &[SelPool], token: alloy::primitives::Address) -> Vec<engine::Route> {
    pools
        .iter()
        .filter(|p| p.token == token && p.quote.is_eth())
        .map(|p| engine::Route { kind: p.kind, token: p.token, fee: p.fee, label: fee_label(p.fee) })
        .collect()
}

/// Derive the quote currency from a pool's currency0. ETH pools use ZERO/WETH;
/// anything else is a stablecoin quote (priced live, never assumed $1).
fn quote_from(currency0: &str) -> (engine::Quote, String) {
    let addr = currency0.parse::<alloy::primitives::Address>().unwrap_or(alloy::primitives::Address::ZERO);
    if addr == alloy::primitives::Address::ZERO || addr == contracts::WETH {
        (engine::Quote::Eth, "ETH".to_string())
    } else {
        (engine::Quote::Stable { token: addr, decimals: stable_decimals(addr) }, stable_symbol(addr))
    }
}

/// What the header wears: the mark and the name beside it.
///
/// Most specific first — a launchpad beats the AMM it graduates into, which
/// beats the chain underneath. With no pool selected there is no venue at all,
/// so it falls all the way back to the chain.
fn header_venue(bot: &Bot) -> ui::image::Venue {
    if bot.pool.token.is_zero() {
        ui::image::Venue::Chain
    } else if bot.pons_launch().is_some() {
        ui::image::Venue::Pons
    } else {
        ui::image::Venue::Uniswap
    }
}

/// The empty state: no pool selected. Named for the CHAIN, because that is the
/// one thing still true when nothing is chosen — and because a blank-looking
/// screen that is actually still pointed at last session's token is how you buy
/// a coin you never meant to touch.
fn blank_pool(net_label: &str) -> SelPool {
    SelPool {
        label: net_label.to_string(),
        kind: engine::PoolKind::V4 { pool_id: B256::ZERO, tick_spacing: 0 },
        token: alloy::primitives::Address::ZERO,
        sym: "—".into(),
        fee: 0,
        owned: false,
        quote: engine::Quote::Eth,
        quote_sym: "ETH".to_string(),
    }
}

/// Build the engine PoolCfg from a selectable pool. quote_usd starts at a safe
/// fallback and is overwritten by the live CoinGecko fetch before first render.
fn to_poolcfg(p: &SelPool) -> engine::PoolCfg {
    engine::PoolCfg {
        kind: p.kind,
        token: p.token,
        sym: p.sym.clone(),
        fee: p.fee,
        // Optimistic default; `refresh_token_decimals` reads the real value from
        // the ERC-20 as soon as the pool is active. 18 is right for almost every
        // memecoin, so this is only wrong for the brief moment before the read.
        token_decimals: 18,
        quote: p.quote,
        quote_sym: p.quote_sym.clone(),
        quote_usd: if p.quote.is_eth() { 1851.0 } else { 1.0 },
    }
}

/// Read the tracked token's `decimals()` and store it on the pool config.
///
/// Assuming 18 breaks any token that isn't (USDG is 6): reserves, balance and
/// supply all come out 10^(18-d) too small, which shows up as price 0.000000 and
/// a nonsense market cap. Called on every pool switch.
async fn refresh_token_decimals<P: Provider>(provider: &P, bot: &mut engine::Bot) {
    if bot.pool.token == alloy::primitives::Address::ZERO {
        return;
    }
    match contracts::IERC20::new(bot.pool.token, provider).decimals().call().await {
        Ok(d) => {
            let d = d._0;
            // Sanity-bound it: a nonsense value would corrupt every amount.
            if (1..=36).contains(&d) {
                if d != bot.pool.token_decimals {
                    bot.note(format!("{} uses {d} decimals", bot.pool.sym));
                }
                bot.pool.token_decimals = d;
            }
        }
        // Non-standard tokens may omit decimals(); 18 is the sane default.
        Err(_) => bot.pool.token_decimals = 18,
    }
}

/// v3 orientation: WETH is token0 iff its address sorts below the token's.
fn weth_is_token0(token: alloy::primitives::Address) -> bool {
    contracts::WETH < token
}

/// Menu label with honest ownership + protocol tags: "[ours]   [v4] ETH/SYM 1%".
fn pool_label(owned: bool, proto: &str, quote_sym: &str, sym: &str, fee: u32, note: &str) -> String {
    // The quote must be in the label. It used to be hardcoded "ETH/", so a
    // USDG-quoted pool was labelled ETH/AAPL — and since the last-used pool is
    // restored BY LABEL, that label then matched a different, ETH-quoted entry
    // on the next start. Every price and reserve came out scaled by 10^12.
    format!(
        "{} [{}] {}/{} {}{}",
        if owned { "[ours]  " } else { "[public]" },
        proto,
        quote_sym,
        sym,
        fee_label(fee),
        note,
    )
}

/// The same pool, written as prose for the status line.
///
/// Menu labels carry `[ours]` / `[v3]` tags because a list needs to be scanned
/// in columns. A status line is a sentence, and in this app square brackets
/// mean "press this key" — so they must never appear in one.
fn pool_sentence(label: &str) -> String {
    let mut out = label.to_string();
    for (tag, word) in [
        ("[ours]", "your"),
        ("[public]", "public"),
        ("[v3]", "Uniswap V3"),
        ("[v4]", "Uniswap V4"),
    ] {
        out = out.replace(tag, word);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn symbol_for(net: &config::Network, addr: &str) -> String {
    net.tokens
        .iter()
        .find(|t| t.address.eq_ignore_ascii_case(addr))
        .map(|t| t.symbol.clone())
        .unwrap_or_else(|| "TOK".into())
}

/// v4 pool id = keccak256(abi.encode(PoolKey)).
fn compute_pool_id(token: alloy::primitives::Address, fee: u32, tick_spacing: i32) -> B256 {
    use alloy::sol_types::SolValue;
    let key = contracts::PoolKey {
        currency0: alloy::primitives::Address::ZERO,
        currency1: token,
        fee: fee.try_into().unwrap(),
        tickSpacing: tick_spacing.try_into().unwrap(),
        hooks: alloy::primitives::Address::ZERO,
    };
    alloy::primitives::keccak256(key.abi_encode())
}

/// Pools we own (registry) tagged [ours], then public pools tagged [public].
/// Where a chain's discovered tokens live.
///
/// Cache, not configuration. These accumulate on their own — every coin added
/// by CA or picked out of the trenches lands here — and they are reconstructible
/// from the chain at any time. Config is what a person decides; this is what the
/// app found out. Keeping them together meant a file you were told to edit grew
/// hundreds of entries you never wrote, and burying an RPC URL among them.
fn token_cache_path(network: &str) -> String {
    // Filesystem-safe: a network name comes from config and can hold anything.
    let safe: String = network
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    format!("{}/tokens-{safe}.json", state_dir())
}

/// Read a chain's cached tokens. A missing or unreadable cache is empty, never
/// an error: it can always be rebuilt by finding the coins again.
fn load_token_cache(network: &str) -> Vec<config::Token> {
    std::fs::read_to_string(token_cache_path(network))
        .ok()
        .and_then(|t| serde_json::from_str::<Vec<config::Token>>(&t).ok())
        .unwrap_or_default()
}

fn collect_pools(net: &config::Network) -> Vec<SelPool> {
    let mut v = Vec::new();
    // Config first, then cache: a token someone wrote by hand should win over
    // one the app stumbled across, and `collect_pools` dedupes downstream.
    let cached = load_token_cache(&net.name);
    for t in net.tokens.iter().chain(cached.iter()) {
        for p in &t.pools {
            if !p.is_v4() {
                continue;
            }
            let tok = match p.currency1.parse::<alloy::primitives::Address>() {
                Ok(t) => t,
                Err(_) => continue,
            };
            // Build the type-safe kind: v3 needs a pool address, v4 a pool id.
            let kind = if p.kind.eq_ignore_ascii_case("v3") {
                match p.address.parse::<alloy::primitives::Address>() {
                    Ok(a) if a != alloy::primitives::Address::ZERO => {
                        engine::PoolKind::V3 { pool_addr: a, weth_is_token0: weth_is_token0(tok) }
                    }
                    _ => continue,
                }
            } else {
                match p.pool_id.parse::<B256>() {
                    Ok(id) if id != B256::ZERO => engine::PoolKind::V4 { pool_id: id, tick_spacing: p.tick_spacing as i32 },
                    _ => continue,
                }
            };
            let sym = symbol_for(net, &p.currency1);
            let note = p.label.find('(').map(|i| format!("  {}", &p.label[i..])).unwrap_or_default();
            let (quote, quote_sym) = quote_from(&p.currency0);
            v.push(SelPool {
                label: pool_label(p.owned, kind.proto(), &quote_sym, &sym, p.fee, &note),
                kind,
                token: tok,
                sym,
                fee: p.fee,
                owned: p.owned,
                quote,
                quote_sym,
            });
        }
    }
    for pp in &net.public_pools {
        if let Ok(tok) = pp.token.parse::<alloy::primitives::Address>() {
            let sym = if pp.sym.is_empty() { symbol_for(net, &pp.token) } else { pp.sym.clone() };
            v.push(SelPool {
                label: pool_label(false, "v4", "ETH", &sym, pp.fee, ""),
                kind: engine::PoolKind::V4 {
                    pool_id: compute_pool_id(tok, pp.fee, pp.tick_spacing as i32),
                    tick_spacing: pp.tick_spacing as i32,
                },
                token: tok,
                sym,
                fee: pp.fee,
                owned: false,
                quote: engine::Quote::Eth,
                quote_sym: "ETH".to_string(),
            });
        }
    }
    v
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    // Answered before anything touches the terminal or the registry: a bug
    // report needs the version, and a build too broken to reach the dashboard
    // is exactly when someone will be asked for it.
    if let Some(a) = std::env::args().nth(1) {
        match a.as_str() {
            "--version" | "-V" => {
                // Version AND commit: a rebuilt release carries the same tag,
                // and a bug report needs to name the build, not the label.
                println!("trenches {} ({})", env!("CARGO_PKG_VERSION"), env!("TRENCHES_COMMIT"));
                return Ok(());
            }
            // Create the config and state directories, then stop. The
            // installer calls this so a fresh machine has both, with the schema
            // line in place, before the app is ever opened.
            //
            // The binary does it rather than the install script, so the starter
            // config has exactly one definition. A copy in a shell script is a
            // copy that drifts the first time a field is added.
            "--init" => {
                // Always the canonical path — not whatever `config_path()`
                // resolves to from the current directory.
                let path = config::init_path();
                let created = !path.exists();
                if created {
                    if let Some(dir) = path.parent() {
                        std::fs::create_dir_all(dir)?;
                    }
                    std::fs::write(&path, config::starter_json())?;
                }
                std::fs::create_dir_all(state_dir())?;
                println!();
                if created {
                    println!("  Wrote a starter config:");
                } else {
                    println!("  Config already present:");
                }
                println!("    {}", path.display());
                println!("    {}   logs, cache, PnL history", state_dir());
                println!();
                println!("  It works as-is on public endpoints. Open the app and press `e`,");
                println!("  or edit the file — your editor will complete it from the schema.");
                println!();
                return Ok(());
            }
            "--help" | "-h" => {
                println!("trenches {} ({})", env!("CARGO_PKG_VERSION"), env!("TRENCHES_COMMIT"));
                println!();
                println!("A terminal for trading memecoins on Robinhood Chain and Solana.");
                println!();
                println!("USAGE:");
                println!("    trenches            start the app");
                println!("    trenches --init     write the config and state dirs, then exit");
                println!("    trenches --version  print the version");
                println!();
                println!("There are no other flags — everything is a keypress once you are in.");
                println!("Press ? for the shortcuts and D for the docs.");
                println!();
                println!("Config  ~/.config/trenches/     Logs  ~/.trenches/");
                println!("Issues  https://github.com/asyncswap/trenches/issues");
                return Ok(());
            }
            _ => {}
        }
    }

    // Ask whether there is a newer release, in the background. Started here, at
    // the top, so it has answered by the time anything is drawn — and detached,
    // so a slow or absent network delays nothing. It only ever reports.
    update::spawn_check();

    // Loads what is there, or writes a starter file and says so. A first run
    // used to end on a file-not-found for a path the user had never heard of.
    let (mut reg, cfg_path, created) = Registry::load_or_create()?;

    // Testnets and local nodes are ours, not a user's.
    //
    // Empty unless the binary was built with `--features testnet`, so a release
    // does not merely hide them — a chain picker whose first entry is a testnet
    // invites a first trade that goes nowhere, and an anvil node nobody is
    // running is a dead row. Never written to the config either way.
    reg.networks.extend(config::dev_networks());

    // A build without the solana feature cannot trade Solana, so it does not
    // offer it. The dispatch below still refuses politely if one slips through,
    // but a chain in the picker that answers "not compiled in" is a door that
    // opens onto a wall — better never to draw the door.
    #[cfg(not(feature = "solana"))]
    reg.networks.retain(|n| !n.kind.is_solana());
    let _ = REGISTRY_PATH.set(cfg_path.to_string_lossy().into_owned());
    if created {
        println!();
        println!("  Welcome to Trenches.");
        println!();
        println!("  Wrote a starter config to");
        println!("    {}", cfg_path.display());
        println!();
        println!("  It works as-is on public endpoints, which are rate limited. For live");
        println!("  trading put your own RPC URL in it — the file explains which fields and");
        println!("  why. Never put a seed phrase in it; accounts are keystores, added with W.");
        println!();
        println!("  Starting…");
        println!();
    }

    // One native ratatui app: selection screens, then the trading dashboard.
    enable_raw_mode()?;
    std::io::stdout().execute(EnterAlternateScreen)?;
    // Raw mode + alternate screen are global terminal state. A panic unwinds
    // past the teardown below and would leave the user with a shell that shows
    // no typing and no prompt, so restore it first and let the panic through.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = std::io::stdout().execute(LeaveAlternateScreen);
        default_hook(info);
    }));
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let res = app(&mut terminal, &reg).await;
    disable_raw_mode()?;
    std::io::stdout().execute(LeaveAlternateScreen)?;
    res
}

/// Solana entry point: pick an account (addresses shown in Solana's own format,
/// derived from the same registry mnemonics), then hand off to the pump.fun
/// dashboard. Keystore accounts are unlocked by password, exactly like EVM.
#[cfg(feature = "solana")]
async fn solana_app(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    reg: &Registry,
    net: &config::Network,
    // False on the way in, true when `W` sent us back round — same contract as
    // the EVM side.
    ask_account: bool,
) -> eyre::Result<Exit> {
    // Offer keystore accounts first, then mnemonic-derived ones. The addresses
    // shown are ed25519/base58 — not the EVM addresses for the same seed.
    // Same rule as the EVM side: keystores only. A seed phrase in a config file
    // is not an account we are willing to offer, so the wallet screen IS the
    // account picker here too.
    // Optional, exactly like the EVM side. Esc goes on WITHOUT an account and
    // the dashboard opens read-only: prices, launches and the tape are worth
    // seeing before committing a key to the machine, and `W` unlocks one at any
    // point. Skipped entirely when there is nothing to unlock — an empty list
    // you have to Esc past is a question with no answer standing in the way.
    let mut unlocked: Option<solana_keypair::Keypair> = None;
    while ask_account && !wallet::list_keystores().is_empty() {
        let Some(ks) = wallet_screen(terminal, config::ChainKind::Solana)? else {
            break;
        };
        let Some(pass) = ui::password(terminal, &format!("Password for {ks}"))? else {
            continue;
        };
        match sol::wallet::keypair_from_keystore(&ks, &pass) {
            Ok(kp) => {
                save_last_wallet(&ks);
                unlocked = Some(kp);
                break;
            }
            Err(e) => {
                ui::select(terminal, &format!("Could not unlock: {e}"), &["Back".into()])?;
            }
        }
    }
    // No account: a throwaway key builds the client. Nothing is ever signed with
    // it — the order keys are guarded on `has_account` — but it keeps ONE code
    // path rather than a second dashboard differing only in whether it can sign.
    let has_account = unlocked.is_some();
    let signer = unlocked.unwrap_or_else(solana_keypair::Keypair::new);

    let exit = sol::app::run(
        terminal,
        net.rpc_pool(),
        net.ws.as_deref(),
        signer,
        has_account,
        &reg.rugcheck,
        &view::pretty_network(&net.name),
    )
    .await?;
    if exit == Exit::ChangeAccount {
        // Straight back to the wallet list on this same chain — the same shape
        // the EVM session uses, so both chains behave identically.
        return Box::pin(solana_app(terminal, reg, net, true)).await;
    }
    Ok(exit)
}

/// Wallet manager: pick a keystore, or make one.
///
/// Lists what is actually on disk rather than what the registry claims, so a
/// wallet created in `cast` shows up here and one created here shows up there.
/// Returns the chosen keystore name.
fn wallet_screen(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    kind: config::ChainKind,
) -> eyre::Result<Option<String>> {
    loop {
        let mut found = wallet::list_keystores();
        // The one you used last sits at the top: it is overwhelmingly the one
        // you want again, and hunting for it in an alphabetical list every run
        // is friction for no reason.
        if let Some(last) = load_last_wallet() {
            if let Some(i) = found.iter().position(|k| k.name == last) {
                let k = found.remove(i);
                found.insert(0, k);
            }
        }
        let mut labels: Vec<String> = vec![
            "＋ Create new account".to_string(),
            "＋ Import private key".to_string(),
            "＋ Import seed phrase".to_string(),
        ];
        // The PATH alone, not the name beside it. The keystore's filename is the
        // last segment of the path, so printing both said everything twice —
        // and what you actually check before unlocking a key is where it lives.
        labels.extend(found.iter().enumerate().map(|(i, k)| {
            let last = i == 0 && load_last_wallet().as_deref() == Some(k.name.as_str());
            format!("{}{}", short_home(&k.path), if last { "   · last used" } else { "" })
        }));

        let chain = match kind {
            config::ChainKind::Solana => "Solana",
            config::ChainKind::Evm => "EVM",
        };
        let title = if found.is_empty() {
            format!("No wallets yet — create one ({chain})")
        } else {
            format!("Select Wallet or create one ({chain})")
        };
        let Some(i) = ui::select(terminal, &title, &labels)? else {
            return Ok(None);
        };
        const ACTIONS: usize = 3;
        if i >= ACTIONS {
            return Ok(Some(found[i - ACTIONS].name.clone()));
        }

        // Creating: name, then the secret if importing, then a password.
        let Some(name) = ui::input(terminal, "Wallet name", "e.g. robin — becomes the file name")? else {
            continue;
        };
        let secret = match i {
            1 => ui::password(terminal, "Private key (hidden)")?,
            2 => ui::password(terminal, "Seed phrase (hidden)")?,
            _ => None,
        };
        if i > 0 && secret.is_none() {
            continue;
        }
        let Some(pass) = ui::password(terminal, "Password for the new keystore")? else {
            continue;
        };
        let Some(again) = ui::password(terminal, "Password again")? else {
            continue;
        };
        if pass != again {
            ui::select(terminal, "Those passwords did not match", &["Try again".into()])?;
            continue;
        }

        // Each arm returns the address as text: the two chains format addresses
        // differently, and the Solana path used to hand back a placeholder that
        // rendered as 0x000…000.
        let made: eyre::Result<String> = match i {
            1 => match kind {
                #[cfg(feature = "solana")]
                config::ChainKind::Solana => {
                    Err(eyre::eyre!("import a Solana key from its seed phrase instead"))
                }
                _ => wallet::import_private_key(&name, secret.as_deref().unwrap_or(""), &pass)
                    .map(|a| a.to_string()),
            },
            2 => {
                // One phrase holds many accounts; taking index 0 silently is
                // how you import a wallet that is not the one you meant.
                let idx = ui::input(terminal, "Account index", "0 is the first account")?
                    .and_then(|t| t.trim().parse::<u32>().ok())
                    .unwrap_or(0);
                let phrase = secret.as_deref().unwrap_or("");
                // The chains derive DIFFERENTLY from the same phrase: Ethereum
                // is secp256k1 on m/44'/60', Solana is ed25519 SLIP-0010 on
                // m/44'/501'/n'/0'. Using the Ethereum path for a Solana import
                // yields a real key that is not the address Phantom shows.
                match kind {
                    #[cfg(feature = "solana")]
                    config::ChainKind::Solana => {
                        sol::wallet::create_keystore(phrase, idx, &name, &pass)
                    }
                    _ => wallet::import_mnemonic(&name, phrase, idx, &pass).map(|a| a.to_string()),
                }
            }
            _ => wallet::create_keystore(&name, &pass).map(|a| a.to_string()),
        };
        match made {
            Ok(addr) => {
                // Saved: no extra screen. The address is confirmed on the next
                // one, where it can be checked against the wallet you expect.
                save_last_wallet(&name);
                trace(&format!("wallet created: {name} {addr}"));
                return Ok(Some(name));
            }
            Err(e) => {
                ui::select(terminal, &format!("Could not create it: {e}"), &["Back".into()])?;
            }
        }
    }
}

/// First 6 and last 4 of an address — enough to match against an explorer
/// without eating the row.
fn short_addr(a: alloy::primitives::Address) -> String {
    let s = format!("{a:#x}");
    if s.len() <= 12 {
        return s;
    }
    format!("{}…{}", &s[..6], &s[s.len() - 4..])
}

/// `~`-shortened path, so a wallet list reads as a location rather than a wall
/// of home directory.
fn short_home(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    match config::home_dir().map(|h| h.display().to_string()) {
        Some(h) if s.starts_with(&h) => s.replacen(&h, "~", 1),
        _ => s,
    }
}

/// How a chain's dashboard ended.
///
/// The chain used to be a one-way choice made at startup, so switching between
/// the EVM and Solana sides meant killing and relaunching the binary.
#[derive(PartialEq, Clone, Copy)]
pub enum Exit {
    /// Leave the program.
    Quit,
    /// Back out to the start screen — one step further than the chain picker.
    /// Esc from the chain picker lands here; nothing else uses it.
    Docs,
    /// Return to the chain picker.
    ChangeChain,
    /// Re-run account selection on the SAME chain.
    ///
    /// The signer is baked into the provider when it is built, so switching
    /// accounts means building a new provider — which is exactly what
    /// re-entering the session does. Reusing that is far less fragile than
    /// swapping a signer underneath a live provider.
    ChangeAccount,
}

async fn app(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    reg: &Registry,
) -> eyre::Result<()> {
    // Docs first.
    //
    // The first screen used to be the chain picker, which asks a question before
    // saying what any of it is. The docs are already in the binary and already
    // explain the keys, the wallets and the config — landing there costs one
    // keypress to leave and answers most of what a first run needs.
    //
    // It is also the screen Esc falls back to. Esc on the chain picker used to
    // quit the app outright, which is not what "back" means anywhere else.
    loop {
        // The docs can make an account, because the Accounts page tells you to
        // and the app is already open. `wallet_screen` handles the whole flow —
        // create, import, name, password — and its return value is a selection
        // this caller has no use for.
        //
        // Which chain has to be asked. The two derive differently from the same
        // phrase — Ethereum on m/44'/60', Solana on m/44'/501' — so guessing
        // hands someone an address that does not match what their wallet shows,
        // with nothing on screen to explain why.
        let mut make_wallet = |t: &mut Terminal<CrosstermBackend<std::io::Stdout>>| -> eyre::Result<()> {
            let opts = vec![
                "Robinhood Chain / EVM".to_string(),
                "Solana".to_string(),
            ];
            let Some(i) = ui::select(t, "An account for which chain?", &opts)? else {
                return Ok(());
            };
            let kind = if i == 1 { config::ChainKind::Solana } else { config::ChainKind::Evm };
            wallet_screen(t, kind).map(|_| ())
        };
        // Docs on the way in — the first time, or whenever the config asks for
        // them. Skipping straight to the chain picker on a machine that has
        // already been set up is the difference between onboarding and a splash
        // screen you learn to dismiss without reading.
        let show = reg.start_on_docs.unwrap_or(!config::onboarded());
        if show {
            if !ui::start_screen(terminal, &mut make_wallet)? {
                return Ok(());
            }
            config::mark_onboarded();
        }
        // Straight back to the chain used last, if there is one.
        //
        // The picker is a question with one obvious answer for anyone past
        // their first run — you trade the same chain most days. `C` still
        // changes it, and Esc from the account list lands on the picker, so
        // nothing is unreachable; it is just no longer in the way.
        let mut resume = load_last_chain()
            .and_then(|n| reg.networks.iter().position(|x| x.name == n));

        loop {
            let exit = match resume.take() {
                Some(i) => {
                    save_last_chain(&reg.networks[i].name);
                    chain_session_on(terminal, reg, &reg.networks[i], i, false).await?
                }
                None => chain_session(terminal, reg, None).await?,
            };
            match exit {
                Exit::ChangeChain => continue,
                Exit::Docs => break,
                _ => return Ok(()),
            }
        }
    }
}

async fn chain_session(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    reg: &Registry,
    keep_net: Option<usize>,
) -> eyre::Result<Exit> {
    // 1) Chain: just the names, mainnets first. The logo beside the list says
    // which chain it is; an id and a pool count are numbers you never pick by.
    struct Pick {
        idx: usize,
        name: String,
        mainnet: bool,
    }
    let picks: Vec<Pick> = reg
        .networks
        .iter()
        .enumerate()
        .map(|(idx, n)| {
            let env = view::network_env(&n.name);
            let base = view::pretty_network(n.name.split(['-', '_']).next().unwrap_or(&n.name));
            // "Robinhood Chain" is what it is called; "Solana" already reads as
            // a chain, so only the one that needs it gets the suffix.
            let base = if base.eq_ignore_ascii_case("robinhood") {
                "Robinhood Chain".to_string()
            } else {
                base
            };
            Pick {
                idx,
                name: if env.eq_ignore_ascii_case("mainnet") { base } else { format!("{base} ({})", env.to_lowercase()) },
                mainnet: env.eq_ignore_ascii_case("mainnet"),
            }
        })
        .collect();

    // Wide enough that a chain name has room around it rather than filling its
    // box edge to edge. The mark beside it is square and sized off the row
    // count, so widening this does not stretch the logo.
    const COLS: &[u16] = &[44];
    // Mainnets first, then test and local. No divider row: the ordering already
    // groups them, and a rule just costs a line.
    let (mut rows, mut back): (Vec<ui::PickRow>, Vec<Option<usize>>) = (Vec::new(), Vec::new());
    for main in [true, false] {
        for p in picks.iter().filter(|p| p.mainnet == main) {
            rows.push(ui::PickRow::new([p.name.clone()]));
            back.push(Some(p.idx));
        }
    }
    // Keyed on the NETWORK, not the chain family: a local anvil node is EVM but
    // is not Robinhood, and showing their feather next to it is just wrong.
    let names: Vec<String> = reg.networks.iter().map(|n| n.name.clone()).collect();
    let brand = |row: usize| {
        back.get(row)
            .copied()
            .flatten()
            .and_then(|i| names.get(i))
            .map(|n| n.to_lowercase())
    };
    let _ = keep_net;
    // The chain picker is the first thing drawn, so it is where a waiting
    // update gets said. One line, no prompt to dismiss, no blocking.
    let title = match update::available() {
        Some(v) => format!("Select chain          ▲ {v} available — curl -fsSL https://trenches.sh/install | sh"),
        None => "Select chain".to_string(),
    };
    let chosen = ui::select_table(
        terminal,
        &title,
        &[],
        COLS,
        &rows,
        |row| brand(row).as_deref().and_then(ui::logo::for_network),
        |row| brand(row).as_deref().and_then(ui::image::for_network),
    )?;
    let (net_idx, net) = match chosen.and_then(|r| back.get(r).copied().flatten()) {
        Some(i) => {
            // Remembered only once it is actually chosen, so a chain you looked
            // at and backed out of is not where you land next time.
            save_last_chain(&reg.networks[i].name);
            (i, &reg.networks[i])
        }
        // Esc on the first screen means back, not quit. `q` is how you leave,
        // and it asks first.
        None => return Ok(Exit::Docs),
    };
    chain_session_on(terminal, reg, net, net_idx, false).await
}

/// The session for one already-chosen network.
async fn chain_session_on(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    reg: &Registry,
    net: &config::Network,
    _net_idx: usize,
    // False on the way in, true when `W` sent us back round. A first run should
    // reach the dashboard without being asked for a password it may not need —
    // watching costs nothing and unlocking is one keypress away.
    ask_account: bool,
) -> eyre::Result<Exit> {

    // Solana networks take an entirely separate path: different signing curve,
    // different venues, different engine. Only the UI components are shared.
    if net.kind.is_solana() {
        #[cfg(feature = "solana")]
        {
            return solana_app(terminal, reg, net, false).await;
        }
        #[cfg(not(feature = "solana"))]
        {
            ui::select(
                terminal,
                "Solana support is not compiled in",
                &["Rebuild with:  cargo build --release --features solana".to_string()],
            )?;
            return Ok(Exit::ChangeChain);
        }
    }

    // 2) Account: keystores on disk, or make one. There is no separate account
    // list — a seed phrase in a config file is not an account we are willing to
    // offer, so the only accounts are encrypted keystores.
    //
    // Optional, and skipped entirely when there is nothing to unlock. Esc goes
    // on WITHOUT an account: the dashboard is worth looking at before you commit
    // a key to it — prices, launches, the tape — and making an unlock the price
    // of entry means anyone who just wants to watch hands over a password first.
    // `W` unlocks one at any point.
    let mut unlocked: Option<(String, alloy::signers::local::PrivateKeySigner)> = None;
    while ask_account && !wallet::list_keystores().is_empty() {
        let Some(ks) = wallet_screen(terminal, config::ChainKind::Evm)? else {
            break;
        };
        let Some(pass) = ui::password(terminal, &format!("Password for {ks}"))? else {
            continue;
        };
        let Some(path) = wallet::keystore_path(&ks) else {
            ui::select(terminal, &format!("{ks} is no longer on disk"), &["Back".into()])?;
            continue;
        };
        match alloy::signers::local::LocalSigner::decrypt_keystore(&path, &pass) {
            Ok(sg) => {
                // Only after a successful unlock: a mistyped password should
                // not change which wallet comes up next time.
                save_last_wallet(&ks);
                unlocked = Some((ks, sg));
                break;
            }
            Err(_) => {
                ui::select(terminal, "Wrong password for that wallet", &["Back".into()])?;
            }
        }
    }

    // 3) Pools: ours (from the registry) + public real-world pools we added.
    let pools = collect_pools(net);
    // Start with NO pool selected, every session, deliberately.
    //
    // This used to restore the last pool used on this chain. That put a fresh
    // session one keypress away from buying whatever was open days ago — the
    // screen reads as blank-and-idle, and `b` does not care. Selecting a pool
    // is now always an explicit act.
    let pool = blank_pool(&view::pretty_network(&net.name));
    let strategy = Strategy::Manual; // default: manual — nothing preset, nothing automatic

    // Wallet-selectable assets: ETH (currency0) + every known token on this
    // network. Used by the pair picker so the user selects assets, not addresses.
    let mut assets: Vec<(alloy::primitives::Address, String)> =
        vec![(alloy::primitives::Address::ZERO, "ETH".to_string())];
    for t in &net.tokens {
        if let Ok(a) = t.address.parse::<alloy::primitives::Address>() {
            if !assets.iter().any(|(x, _)| *x == a) {
                assets.push((a, t.symbol.clone()));
            }
        }
    }

    // No account: a throwaway key builds the provider and `trader` stays zero to
    // mark it. Nothing is ever signed with it — every key that sends is guarded
    // on that zero address — but it keeps ONE provider type and one code path,
    // rather than a second generic instantiation of the whole dashboard that
    // differs only in whether it can sign.
    let no_account = unlocked.is_none();
    let (account, signer) = match unlocked {
        Some((ks, sg)) => (ks, sg),
        None => ("(no account)".to_string(), alloy::signers::local::LocalSigner::random()),
    };
    let trader = if no_account { alloy::primitives::Address::ZERO } else { signer.address() };
    let wallet = EthereumWallet::from(signer);
    // with_recommended_fillers() adds the gas / nonce / chain-id fillers.
    // Without it the WalletFiller tries to sign a tx that has no nonce/gas/fee
    // set → "missing properties [nonce, gas_limit, max_fee_per_gas]" on send.
    let provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(wallet)
        .on_builtin(&net.rpc)
        .await?;

    // Per-session log in a .bot/ folder (created if missing).
    std::fs::create_dir_all(state_dir())?;
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let log_path = format!("{}/session-{secs}.log", state_dir());
    let log = std::fs::File::create(&log_path)?;

    let mut bot = Bot {
        trader,
        net: view::pretty_network(&net.name),
        account: account.clone(),
        pool: to_poolcfg(&pool),
        strategy,
        arb_mode: false,
        pool_b: None,
        mkt_b: engine::Market::default(),
        sqrt_price: 0.0,
        tick: 0,
        r0: 0.0,
        r1: 0.0,
        eth: 0.0,
        token_bal: 0.0,
        ready: false,
        baseline_eth: None,
        daily_baseline: None,
        daily_day: 0,
        bought_qty: 0.0,
        bought_cost: 0.0,
        realized_pnl: 0.0,
        last_fill_pnl: None,
        entry_mc: 0.0,
        entry_pooled_eth: 0.0,
        entry_tx: None,
        entry_at: None,
        trades: 0,
        fails: 0,
        skips: 0,
        last_side: Side::Sell,
        pending: Vec::new(),
        positions: Vec::new(),
        pos_liq: std::collections::HashMap::new(),
        mint_liq: std::collections::HashMap::new(),
        orders: VecDeque::new(),
        log,
        logs: VecDeque::new(),
        buy_frac: 0.05,       // buy 5% of ETH balance (fine steps)
        sell_frac: 1.00,      // sell 100% of token balance by default (10% steps)
        slippage_pct: 3.0,    // matches the previous hardcoded floor
        max_price_move: 0.0,  // impact cap OFF by default — full-size swaps / instant exits ('}' to cap, '{' lower)
        lp_frac: 0.05,        // add 5% of ETH balance as LP
        nonce: None,
        token_supply: 0.0,
        eth_usd: 1871.0,    // ETH price estimate for USD market cap (adjust as needed)
        profit_guard: false, // OFF by default — don't gate on positive EV; toggle with 'g'
        guard_dup: true,     // ON by default — stop double buys; toggle with 'n'
        copy_buy_eth: 0.0,
        copy_tiers: Vec::new(),
        copy_idx: 0,
        copy_manual: false,
        min_edge_eth: 0.0,
        ref_price: 0.0,
        gas_price: 0.0,
        last_edge: 0.0,
        last_read_ms: 0.0,
        lp_permit2_done: false,
        v3_covered: false,
        routes: routes_for(&pools, pool.token),
        meta: engine::Meta::default(),
        pool_launch_block: None,
        status: "ready".into(),
    };

    let _ = log_path;
    bot.load_daily(); // restore today's PnL baseline across restarts
    bot.meta = engine::fetch_token_meta(&provider, bot.pool.token).await; // socials for the start pool
    bot.pool_launch_block = discover::fetch_launch_block(bot.pool.token).await; // pool age

    // --- trading dashboard (same terminal), with live option switching ---
    let verified = build_verified(net);
    let exit = run(terminal, &provider, &mut bot, pools, assets, net.name.clone(), net.discovery_rpc.clone(), verified).await?;
    if exit == Exit::ChangeAccount {
        // Straight back to the account list on this same chain. Recursing
        // rebuilds the provider around the new signer, which is the whole
        // reason this cannot be swapped in place.
        return Box::pin(chain_session_on(terminal, reg, net, _net_idx, true)).await;
    }
    Ok(exit)
}

/// Remember a newly added pool, so it survives a restart.
///
/// Writes to the token CACHE, not the config file. A coin added by CA is
/// something the app learned, not something the user configured — and a config
/// file that silently grows a few hundred entries is one nobody can find their
/// own RPC URL in any more. The cache is disposable: delete it and the coins
/// come back the next time they are found.
fn persist_pool(network: &str, p: &SelPool) -> eyre::Result<()> {
    use serde_json::{json, Value};
    let path = token_cache_path(network);
    let mut tokens_val: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!([]));
    if !tokens_val.is_array() {
        tokens_val = json!([]);
    }

    let token_addr = p.token.to_string();
    let (kind, pool_id, addr, tick_spacing, currency0, state_view) = match p.kind {
        engine::PoolKind::V4 { pool_id, tick_spacing } => (
            "v4", pool_id.to_string(), String::new(), tick_spacing,
            alloy::primitives::Address::ZERO.to_string(), contracts::STATE_VIEW.to_string(),
        ),
        engine::PoolKind::V3 { pool_addr, .. } => (
            "v3", String::new(), pool_addr.to_string(), 0,
            contracts::WETH.to_string(), String::new(),
        ),
    };
    let pool_obj = json!({
        "label": format!("ETH/{} {}", p.sym, fee_label(p.fee)),
        "kind": kind,
        "pool_id": pool_id,
        "address": addr,
        "currency0": currency0,
        "currency1": token_addr,
        "fee": p.fee,
        "tick_spacing": tick_spacing,
        "state_view": state_view,
        "owned": p.owned,
    });

    let tokens = tokens_val.as_array_mut().unwrap();
    let existing = tokens.iter_mut().find(|t| {
        t.get("address")
            .and_then(|a| a.as_str())
            .map(|s| s.eq_ignore_ascii_case(&token_addr))
            .unwrap_or(false)
    });
    match existing {
        Some(tok) => {
            let pls = tok
                .as_object_mut()
                .unwrap()
                .entry("pools")
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .unwrap();
            let dup = pls
                .iter()
                .any(|pl| pl.get("pool_id").and_then(|x| x.as_str()) == Some(pool_id.as_str()));
            if !dup {
                pls.push(pool_obj);
            }
        }
        None => tokens.push(json!({
            "name": p.sym, "symbol": p.sym, "address": token_addr, "pools": [pool_obj]
        })),
    }
    std::fs::create_dir_all(state_dir())?;
    std::fs::write(&path, serde_json::to_string_pretty(&tokens_val)?)?;
    Ok(())
}

/// Parse the config's hand-curated verified pools into runtime form. Bad entries
/// are skipped (so a typo in the registry never crashes startup).
fn build_verified(net: &config::Network) -> Vec<discover::VerifiedPool> {
    let usdg: alloy::primitives::Address =
        "0x5fc5360D0400a0Fd4f2af552ADD042D716F1d168".parse().unwrap();
    net.verified_pools
        .iter()
        .filter_map(|v| {
            Some(discover::VerifiedPool {
                token: v.token.parse().ok()?,
                pool_id: v.pool_id.parse().ok()?,
                quote: if v.quote.eq_ignore_ascii_case("WETH") {
                    engine::Quote::Eth
                } else {
                    engine::Quote::Stable { token: usdg, decimals: 6 }
                },
                tick_spacing: v.tick_spacing,
                fee: v.fee,
                sym: v.sym.clone(),
            })
        })
        .collect()
}


async fn run<P: Provider + Clone + Send + Sync + 'static>(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    provider: &P,
    bot: &mut Bot,
    mut pools: Vec<SelPool>,
    assets: Vec<(alloy::primitives::Address, String)>,
    network: String,
    discovery_rpc: Option<String>,
    verified: Vec<discover::VerifiedPool>,
) -> eyre::Result<Exit> {
    use futures::StreamExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    // Header logo. Any screen that takes over clears images on entry, which
    // marks this stale, so it redraws itself on return with no bookkeeping here.
    let mut chain_logo = ui::image::Placement::default();


    use std::sync::{Arc, Mutex};

    // Resolve the STARTING pool's token decimals before anything reads the
    // market — otherwise the very first render of a non-18-dec pool (e.g. a
    // 6-dec USDG pool restored from last-pool) shows price 0.000000.
    refresh_token_decimals(provider, bot).await;
    trace_pool("startup", &bot.pool);

    // Shared state written by the background poll task, read by the UI thread.
    let market = Arc::new(Mutex::new(engine::Market::default()));
    let pool_cell = Arc::new(Mutex::new(bot.pool.as_ref()));
    let block = Arc::new(AtomicU64::new(0));
    let rpc_ok = Arc::new(std::sync::atomic::AtomicBool::new(true));
    // Live trade tape (all traders' swaps on the current pool).
    let tape: Arc<Mutex<VecDeque<engine::Swap>>> = Arc::new(Mutex::new(VecDeque::new()));
    // Arb mode: second pool ref + its market snapshot.
    let pool_b_cell: Arc<Mutex<Option<engine::PoolRef>>> = Arc::new(Mutex::new(None));
    let market_b = Arc::new(Mutex::new(engine::Market::default()));

    // Background polling task — the ONLY continuous RPC. Hard 1.5s timeouts so a
    // slow/broken RPC can never freeze the UI; the render loop keeps running.
    {
        let provider = provider.clone();
        let market = market.clone();
        let pool_cell = pool_cell.clone();
        let block = block.clone();
        let rpc_ok = rpc_ok.clone();
        let tape = tape.clone();
        let pool_b_cell = pool_b_cell.clone();
        let market_b = market_b.clone();
        let trader = bot.trader;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(350));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_swap_block = 0u64; // last block scanned for the tape (pool A)
            let mut last_swap_block_b = 0u64; // pool B (arb mode) tape cursor
            let mut tape_pref = *pool_cell.lock().unwrap();
            let mut tape_b_key = alloy::primitives::Address::ZERO;
            loop {
                tick.tick().await;
                let pref = *pool_cell.lock().unwrap();
                match tokio::time::timeout(
                    Duration::from_millis(1500),
                    engine::read_market(&provider, pref, trader),
                )
                .await
                {
                    Ok(Ok(m)) => { *market.lock().unwrap() = m; rpc_ok.store(true, Ordering::Relaxed); }
                    _ => { rpc_ok.store(false, Ordering::Relaxed); }
                }
                // Arb mode: also read the second pool.
                let pref_b = *pool_b_cell.lock().unwrap();
                if let Some(pb) = pref_b {
                    if let Ok(Ok(mb)) = tokio::time::timeout(Duration::from_millis(1500), engine::read_market(&provider, pb, trader)).await {
                        *market_b.lock().unwrap() = mb;
                    }
                }
                if let Ok(Ok(b)) =
                    tokio::time::timeout(Duration::from_millis(1500), provider.get_block_number()).await
                {
                    block.store(b, Ordering::Relaxed);
                    // Pool switched? reset the tape scan window.
                    if pref.token != tape_pref.token { tape.lock().unwrap().clear(); last_swap_block = 0; tape_pref = pref; }
                    // Scan a recent window for new swaps on this pool (cap range).
                    let from = if last_swap_block == 0 { b.saturating_sub(200) } else { last_swap_block + 1 };
                    if b >= from {
                        // Only advance the scan cursor when the fetch SUCCEEDS —
                        // otherwise a timeout/error would skip those blocks' events.
                        if let Ok(Ok(sw)) = tokio::time::timeout(Duration::from_millis(1500), engine::read_swaps(&provider, pref, from, b)).await {
                            let mut t = tape.lock().unwrap();
                            for s in sw { t.push_back(s); }
                            while t.len() > 400 { t.pop_front(); }
                            drop(t);
                            last_swap_block = b;
                        }
                    }
                    // Arb mode: merge the SECOND pool's swaps into the same tape
                    // (matches gmgn's token-aggregate view across venues).
                    if let Some(pb) = pref_b {
                        if pb.token != tape_b_key { last_swap_block_b = 0; tape_b_key = pb.token; }
                        let from_b = if last_swap_block_b == 0 { b.saturating_sub(200) } else { last_swap_block_b + 1 };
                        if b >= from_b {
                            if let Ok(Ok(sw)) = tokio::time::timeout(Duration::from_millis(1500), engine::read_swaps(&provider, pb, from_b, b)).await {
                                let mut t = tape.lock().unwrap();
                                for s in sw { t.push_back(s); }
                                while t.len() > 400 { t.pop_front(); }
                                drop(t);
                                last_swap_block_b = b;
                            }
                        }
                    } else {
                        last_swap_block_b = 0;
                    }
                }
            }
        });
    }

    let mut prices: VecDeque<f64> = VecDeque::new();
    let mut sma = 0.0;
    let mut tele_ctr: u64 = 0;
    let mut view = Panel::Tape; // default to the live tape
    let mut show_help = false;
    let mut orders_scroll: usize = 0;
    let mut reader = crossterm::event::EventStream::new();
    // Live USD feed (CoinGecko) for quote currencies — fetched up front so ETH
    // and any stablecoin quotes are valued correctly before the first render.
    let feed_ids = price_ids(&pools);
    let mut feed = pricing::fetch_usd(&feed_ids).await;
    apply_prices(bot, &feed);
    let mut price_refresh: u32 = 0; // action-tick counter; refetch every ~60s

    // Render on a fast fixed cadence — never does RPC, so it stays smooth even
    // when the node is slow. Actions run on their own slower tick.
    let mut render = tokio::time::interval(Duration::from_millis(100));
    render.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut act = tokio::time::interval(Duration::from_millis(500));
    act.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // fast render — pulls the latest snapshot, no network I/O
            _ = render.tick() => {
                let m = *market.lock().unwrap();
                bot.apply_market(m);
                bot.mkt_b = *market_b.lock().unwrap();
                let blk = block.load(Ordering::Relaxed);
                sma = if prices.is_empty() { bot.price() } else { prices.iter().sum::<f64>() / prices.len() as f64 };
                bot.ref_price = sma; // reference for the profitability filter
                if !rpc_ok.load(Ordering::Relaxed) {
                    bot.status = "Cannot reach the RPC endpoint. Retrying now.".into();
                }
                let mut tape_snap: Vec<engine::Swap> = tape.lock().unwrap().iter().copied().collect();
                tape_snap.sort_by_key(|s| s.block); // merged pools -> chronological
                // Copy mode: build the deployer's buy ladder from the tape (recurring
                // external buy sizes, excluding our own trades) and resolve the
                // selected rung. Auto-snaps to the modal rung until the user steps
                // it with [ ]; then holds the manual pick.
                if bot.is_copy() {
                    let ours: std::collections::HashSet<alloy::primitives::TxHash> =
                        bot.orders.iter().filter_map(|o| o.hash).collect();
                    let buys: Vec<u128> = tape_snap.iter()
                        .filter(|s| matches!(s.action, engine::TapeAction::Buy) && s.eth_wei > 0 && !ours.contains(&s.tx))
                        .map(|s| s.eth_wei)
                        .collect();
                    bot.copy_tiers = buy_tiers(&buys);
                    if bot.copy_tiers.is_empty() {
                        bot.copy_idx = 0;
                        bot.copy_buy_eth = 0.0;
                    } else {
                        if !bot.copy_manual {
                            // Snap to the modal rung (highest count) as the blend-in default.
                            bot.copy_idx = bot.copy_tiers.iter().enumerate()
                                .max_by_key(|(_, (_, c))| *c).map(|(i, _)| i).unwrap_or(0);
                        }
                        bot.copy_idx = bot.copy_idx.min(bot.copy_tiers.len() - 1);
                        bot.copy_buy_eth = bot.copy_tiers[bot.copy_idx].0;
                    }
                }
                let mut logo_box = None;
                terminal.draw(|f| {
                    logo_box = draw(f, bot, blk, bot.last_read_ms, view, orders_scroll, &tape_snap, show_help);
                })?;
                // After the frame, so ratatui's own output cannot cover it. The
                // placement only redraws when its key changes, so adjusting a
                // value does not make it blink.
                let venue = header_venue(bot);
                let term_size = terminal.size().map(|s| (s.width, s.height)).unwrap_or((0, 0));
                if let (Some(r), Some(png)) = (logo_box, ui::image::for_venue(venue, &bot.net)) {
                    chain_logo.show(png, venue as usize, r.x, r.y, r.width, r.height, term_size);
                }
            }
            // slower action tick — auto-strategy + reap, each timeout-bounded
            _ = act.tick() => {
                if bot.ready {
                    prices.push_back(bot.price());
                    if prices.len() > 60 { prices.pop_front(); }
                    if bot.strategy != Strategy::Manual {
                        if let Some(side) = bot.signal(sma, 0.003) {
                            let _ = tokio::time::timeout(Duration::from_secs(3), bot.place(provider, side)).await;
                        }
                    }
                } else if rpc_ok.load(Ordering::Relaxed) {
                    bot.status = "This pool has no active liquidity. Press a to add liquidity.".into();
                }
                let _ = tokio::time::timeout(Duration::from_secs(3), bot.reap(provider)).await;
                tele_ctr += 1;
                if tele_ctr % 4 == 0 { bot.telemetry(block.load(Ordering::Relaxed), bot.last_read_ms); }
                // Refresh the USD feed every ~60s (120 * 500ms) so quote values track.
                price_refresh += 1;
                if price_refresh >= 120 {
                    price_refresh = 0;
                    let f = pricing::fetch_usd(&feed_ids).await;
                    if !f.is_empty() { feed = f; apply_prices(bot, &feed); }
                }
            }
            // key input — handled the instant it arrives
            ev = reader.next() => {
                if let Some(Ok(Event::Key(k))) = ev {

                    if k.kind != crossterm::event::KeyEventKind::Press { continue; }
                    // Help overlay: '?' opens it; any other key closes it.
                    if show_help {
                        show_help = false;
                        if k.code == KeyCode::Char('?') { continue; }
                    }
                    // No account: nothing can be signed, and the throwaway key
                    // that built the provider must never be asked to try.
                    if bot.trader.is_zero()
                        && matches!(k.code, KeyCode::Char('b' | 's' | 'a' | 'r' | 'x' | 'S'))
                    {
                        bot.status = "no account — press [W] to unlock one".into();
                        continue;
                    }
                    // Nothing selected means nothing to trade: the order keys
                    // have no pool to act on, and the zero address would go to
                    // the router as if it were a token. Say so instead.
                    if bot.pool.token.is_zero()
                        && matches!(k.code, KeyCode::Char('b' | 's' | 'a' | 'r' | 'x' | 'S' | 'c'))
                    {
                        bot.status = "no pool selected — press [f] to find pools".into();
                        continue;
                    }
                    match k.code {
                        // `q` is one keystroke away from every other action, so
                        // it asks first. `Q` is the deliberate escape hatch.
                        KeyCode::Char('q') => {
                            if ui::confirm(terminal, "Quit the Trenches?")? {
                                return Ok(Exit::Quit);
                            }
                        }
                        KeyCode::Char('Q') => return Ok(Exit::Quit),
                        KeyCode::Char('D') => ui::docs(terminal)?,
                        // Back to the account list on this same chain.
                        KeyCode::Char('W') => return Ok(Exit::ChangeAccount),
                        // Back to the chain picker without restarting.
                        KeyCode::Char('C') => return Ok(Exit::ChangeChain),
                        KeyCode::Char('?') => { show_help = true; }
                        // Theme picker with live preview (persists the choice).
                        KeyCode::Char('T') => {
                            match ui::widgets::theme_picker(terminal)? {
                                Some(name) => bot.status = format!("Changed theme to {name}"),
                                None => bot.status = "theme unchanged".into(),
                            }
                        }
                        // View cycling: 'l' or → next, ← previous. Reset scroll.
                        KeyCode::Char('l') | KeyCode::Right => { view = match view { Panel::Orders => Panel::Tape, Panel::Tape => Panel::Logs, Panel::Logs => Panel::Orders }; orders_scroll = 0; }
                        KeyCode::Left => { view = match view { Panel::Orders => Panel::Logs, Panel::Logs => Panel::Tape, Panel::Tape => Panel::Orders }; orders_scroll = 0; }
                        // Scroll the active panel (↑ older, ↓ newer) — orders or tape.
                        KeyCode::Up => {
                            let n = match view { Panel::Tape => tape.lock().unwrap().len(), Panel::Logs => bot.logs.len(), _ => bot.orders.len() };
                            orders_scroll = (orders_scroll + 1).min(n.saturating_sub(1));
                        }
                        KeyCode::Down => { orders_scroll = orders_scroll.saturating_sub(1); }
                        // Refresh the live USD feed (ETH + stablecoin quotes) now.
                        KeyCode::Char('R') => {
                            bot.status = "refreshing prices…".into();
                            let f = pricing::fetch_usd(&feed_ids).await;
                            if !f.is_empty() { feed = f; apply_prices(bot, &feed); }
                            bot.status = format!(
                                "prices: ETH ${:.0}   {} ${:.4}",
                                bot.eth_usd, bot.pool.quote_sym, bot.pool.quote_usd
                            );
                        }
                        // Live knobs, shown as percentages:
                        //   [ ]  BUY size (% of ETH balance)    — 0.5% steps
                        //   ( )  SELL size (% of token balance) — 10% steps
                        //   { }  impact cap (% price move);  0 = off
                        // In copy mode, [ ] walk the deployer's buy ladder (rungs);
                        // otherwise they nudge buy_frac (% of wallet) as usual.
                        KeyCode::Char(']') => {
                            if bot.is_copy() {
                                bot.copy_manual = true;
                                if !bot.copy_tiers.is_empty() { bot.copy_idx = (bot.copy_idx + 1).min(bot.copy_tiers.len() - 1); }
                                bot.status = copy_status(bot);
                            } else {
                                bot.buy_frac = (bot.buy_frac + 0.005).min(1.0); bot.status = format!("Buy size is now {:.1} percent of your ETH balance", bot.buy_frac * 100.0);
                            }
                        }
                        KeyCode::Char('[') => {
                            if bot.is_copy() {
                                bot.copy_manual = true;
                                bot.copy_idx = bot.copy_idx.saturating_sub(1);
                                bot.status = copy_status(bot);
                            } else {
                                bot.buy_frac = (bot.buy_frac - 0.005).max(0.005); bot.status = format!("Buy size is now {:.1} percent of your ETH balance", bot.buy_frac * 100.0);
                            }
                        }
                        // Bracket family, paired with the header labels:
                        // [] buy · () sell · {} slippage · <> impact cap.
                        // `()` is sell again — slippage had taken it.
                        KeyCode::Char(')') => { bot.sell_frac = (bot.sell_frac + 0.10).min(1.0); bot.status = format!("Sell size is now {:.0} percent of your {} balance", bot.sell_frac * 100.0, bot.pool.sym); }
                        KeyCode::Char('(') => { bot.sell_frac = (bot.sell_frac - 0.10).max(0.10); bot.status = format!("Sell size is now {:.0} percent of your {} balance", bot.sell_frac * 100.0, bot.pool.sym); }
                        KeyCode::Char('}') => { bot.slippage_pct = (bot.slippage_pct + 1.0).min(50.0); bot.status = format!("Slippage tolerance is now {:.0} percent", bot.slippage_pct); }
                        KeyCode::Char('{') => { bot.slippage_pct = (bot.slippage_pct - 1.0).max(1.0); bot.status = format!("Slippage tolerance is now {:.0} percent", bot.slippage_pct); }
                        KeyCode::Char('>') => { bot.max_price_move = (bot.max_price_move + 0.005).min(0.50); bot.status = format!("A single swap may now move the price at most {:.1} percent", bot.max_price_move * 100.0); }
                        KeyCode::Char('<') => { bot.max_price_move = (bot.max_price_move - 0.005).max(0.0); bot.status = format!("A single swap may now move the price at most {:.1} percent", bot.max_price_move * 100.0); }
                        KeyCode::Char('0') => { bot.max_price_move = 0.0; bot.status = "Price impact limit is off, swaps now go out at full size".into(); }
                        // Toggle the profitability filter — off lets you force genuine buys/sells.
                        KeyCode::Char('g') => { bot.profit_guard = !bot.profit_guard; bot.status = if bot.profit_guard {
                                "Profit filter is on, trades that would lose money are held back".into()
                            } else {
                                "Profit filter is off, be careful, losing trades are no longer blocked".to_string()
                            }; }
                        KeyCode::Char('n') => { bot.guard_dup = !bot.guard_dup; bot.status = if bot.guard_dup {
                                "Duplicate guard is on, the same buy will not repeat".into()
                            } else {
                                "Duplicate guard is off, be careful, the same buy can repeat".to_string()
                            }; }
                        // Quick mode toggle (Shift-M): flip mode 0 (manual) <-> mode 1.
                        KeyCode::Char('M') => {
                            bot.strategy = if bot.is_copy() { Strategy::Manual } else { Strategy::CopyBuyAmount };
                            bot.copy_manual = false;
                            bot.status = if bot.is_copy() { copy_status(bot) } else { "Switched to manual mode".into() };
                        }
                        // Mode picker (m): choose the trading mode from a menu.
                        KeyCode::Char('m') => {
                            let opts = vec![
                                "mode 0 (manual)  — buys use buy_frac".to_string(),
                                "mode 1           — [ ] steps the value".to_string(),
                            ];
                            if let Some(i) = ui::select(terminal, "Select mode", &opts)? {
                                bot.strategy = if i == 1 { Strategy::CopyBuyAmount } else { Strategy::Manual };
                                bot.copy_manual = false;
                                bot.status = if bot.is_copy() { copy_status(bot) } else { "Switched to manual mode".into() };
                            }
                        }
                        // Clear the board. Everything on screen — pool, tape,
                        // position, routes, arb pair — belonged to a coin you are
                        // done with, and a half-cleared screen is worse than none:
                        // it still trades.
                        KeyCode::Delete => {
                            let blank = blank_pool(&bot.net.clone());
                            bot.pool = to_poolcfg(&blank);
                            bot.routes.clear();
                            bot.bought_qty = 0.0;
                            bot.bought_cost = 0.0;
                            bot.lp_permit2_done = false;
                            bot.v3_covered = false;
                            bot.meta = Default::default();
                            bot.pool_launch_block = None;
                            bot.arb_mode = false;
                            bot.pool_b = None;
                            *pool_b_cell.lock().unwrap() = None;
                            *pool_cell.lock().unwrap() = bot.pool.as_ref();
                            prices.clear();
                            bot.status = "pool deselected — press [f] to find pools".into();
                        }
                        KeyCode::Char('b') => { bot.status = "buying…".into(); let _ = tokio::time::timeout(Duration::from_secs(5), bot.place(provider, Side::Buy)).await; }
                        KeyCode::Char('s') => { bot.status = "selling…".into(); let _ = tokio::time::timeout(Duration::from_secs(5), bot.place(provider, Side::Sell)).await; }
                        KeyCode::Char('a') => { bot.status = "adding LP…".into(); let wei = (bot.eth * bot.lp_frac * 1e18).max(0.0) as u128; let _ = tokio::time::timeout(Duration::from_secs(8), bot.add_liquidity(provider, wei)).await; }
                        KeyCode::Char('r') => { bot.status = "removing one LP…".into(); let _ = tokio::time::timeout(Duration::from_secs(8), bot.remove_liquidity(provider)).await; }
                        KeyCode::Char('x') => { bot.status = "closing ALL LP…".into(); let _ = tokio::time::timeout(Duration::from_secs(12), bot.close_all(provider)).await; }
                        // The PnL calendar. Reads the fill ledger and nothing
                        // else — no RPC, no wallet — so opening it cannot cost
                        // a trade and it works with the network down.
                        KeyCode::Char('L') => {
                            pnl::screen(terminal)?;
                            bot.status = "back from PnL".into();
                        }
                        KeyCode::Char('S') => {
                            // Liquidate ALL token holdings: sweep every known v3 pool
                            // and sell any nonzero balance. Restores the active pool after.
                            bot.status = "liquidating ALL token holdings…".into();
                            let saved = bot.pool.clone();
                            let saved_routes = std::mem::take(&mut bot.routes);
                            let saved_covered = bot.v3_covered;
                            let mut swept = 0u32;
                            let mut seen: std::collections::HashSet<alloy::primitives::Address> = std::collections::HashSet::new();
                            for p in pools.clone() {
                                if !p.kind.is_v3() || !seen.insert(p.token) { continue; }
                                let bal = contracts::IERC20::new(p.token, provider)
                                    .balanceOf(bot.trader).call().await.map(|b| b._0).unwrap_or_default();
                                if bal.is_zero() { continue; }
                                bot.pool = to_poolcfg(&p);
                                trace_pool("switch", &bot.pool);
                                bot.routes = routes_for(&pools, p.token);
                                bot.v3_covered = false;
                                let _ = tokio::time::timeout(Duration::from_secs(10), bot.sell_all(provider)).await;
                                swept += 1;
                            }
                            bot.pool = saved;
                            bot.routes = saved_routes;
                            bot.v3_covered = saved_covered;
                            // Read the token real decimals BEFORE publishing to the market
                            // reader, so its first read is scaled correctly.
                            refresh_token_decimals(provider, bot).await;
                            *pool_cell.lock().unwrap() = bot.pool.as_ref();
                            bot.status = format!("liquidate: swept {swept} holding(s)");
                        }
                        KeyCode::Char('h') => {
                            // Wallet holdings: leftover tokens you still hold, with live
                            // ETH value — spot forgotten winners/dust. Enter to trade/sell.
                            bot.status = "reading wallet holdings…".into();
                            let trader = bot.trader;
                            let mut seen = std::collections::HashSet::new();
                            let cands: Vec<SelPool> = pools
                                .iter()
                                .filter(|p| p.kind.is_v3() && seen.insert(p.token))
                                .cloned()
                                .collect();
                            let rows: Vec<(SelPool, f64, f64)> = futures::stream::iter(cands)
                                .map(|p| async move {
                                    // Decimals must be read per token: dividing a
                                    // 6-dec balance (USDG) by 1e18 puts it below the
                                    // "not held" threshold, so real holdings vanish.
                                    let erc = contracts::IERC20::new(p.token, provider);
                                    let dec = erc
                                        .decimals()
                                        .call()
                                        .await
                                        .map(|d| d._0)
                                        .ok()
                                        .filter(|d| (1..=36).contains(d))
                                        .unwrap_or(18);
                                    let bal = erc
                                        .balanceOf(trader)
                                        .call()
                                        .await
                                        .map(|b| {
                                            b._0.to_string().parse::<f64>().unwrap_or(0.0)
                                                / 10f64.powi(dec as i32)
                                        })
                                        .unwrap_or(0.0);
                                    if bal <= 1e-9 {
                                        return (p, 0.0, 0.0); // not held — skip the price call
                                    }
                                    // Only held tokens pay for a price read (→ ETH value).
                                    let (sqrt, w0) = match p.kind {
                                        engine::PoolKind::V3 { pool_addr, weth_is_token0 } => {
                                            let s = contracts::IV3Pool::new(pool_addr, provider)
                                                .slot0()
                                                .call()
                                                .await
                                                .map(|r| r.sqrtPriceX96.to_string().parse::<f64>().unwrap_or(0.0) / 2f64.powi(96))
                                                .unwrap_or(0.0);
                                            (s, weth_is_token0)
                                        }
                                        _ => (0.0, false),
                                    };
                                    let p_raw = sqrt * sqrt;
                                    let tpe = if w0 { p_raw } else if p_raw > 0.0 { 1.0 / p_raw } else { 0.0 };
                                    // sqrtPriceX96 is a ratio of BASE units, so converting
                                    // it to ETH-per-whole-token needs the decimal gap
                                    // between the token (dec) and WETH (18).
                                    let ept_raw = if tpe > 0.0 { 1.0 / tpe } else { 0.0 };
                                    let ept = ept_raw * 10f64.powi(dec as i32 - 18);
                                    (p, bal, bal * ept)
                                })
                                .buffered(24)
                                .collect()
                                .await;
                            let mut held: Vec<(SelPool, f64, f64)> =
                                rows.into_iter().filter(|(_, bal, _)| *bal > 1e-9).collect();
                            held.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
                            if held.is_empty() {
                                bot.status = "no leftover tokens in wallet".into();
                            } else {
                                let labels: Vec<String> = held
                                    .iter()
                                    .map(|(p, bal, val)| format!("{:<12} {:>14.2}  ~{:.6} ETH", p.sym, bal, val))
                                    .collect();
                                if let Some(i) = ui::select(terminal, "Wallet holdings — Enter to trade/sell", &labels)? {
                                    let p = held[i].0.clone();
                                    bot.pool = to_poolcfg(&p);
                                    trace_pool("switch", &bot.pool);
                                    bot.bought_qty = 0.0;
                                    bot.bought_cost = 0.0;
                                    bot.lp_permit2_done = false;
                                    bot.v3_covered = false;
                                    apply_prices(bot, &feed);
                                    // Read the token's real decimals BEFORE publishing to the
                                    // market reader, so its first read is correctly scaled.
                                    // Read the token real decimals BEFORE publishing to the market
                                    // reader, so its first read is scaled correctly.
                                    refresh_token_decimals(provider, bot).await;
                                    *pool_cell.lock().unwrap() = bot.pool.as_ref();
                                    prices.clear();
                                    bot.meta = engine::fetch_token_meta(provider, bot.pool.token).await;
                                    bot.pool_launch_block = discover::fetch_launch_block(bot.pool.token).await;
                                    bot.status = format!("Now trading {}", pool_sentence(&p.label));
                                    bot.routes = routes_for(&pools, bot.pool.token);
                                }
                            }
                        }
                        // Arb mode: pick a 2nd pool (same token, other venue) to
                        // watch side-by-side with a live price gap. Press again to exit.
                        KeyCode::Char('d') => {
                            if bot.arb_mode {
                                bot.arb_mode = false;
                                bot.pool_b = None;
                                *pool_b_cell.lock().unwrap() = None;
                                bot.status = "Arb mode is off".into();
                            } else {
                                // Arb is ONE token across TWO venues — offer only
                                // already-added pools for the SAME token, minus the
                                // current pool A.
                                let cands: Vec<usize> = pools
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, p)| p.token == bot.pool.token && p.kind != bot.pool.kind)
                                    .map(|(i, _)| i)
                                    .collect();
                                if cands.is_empty() {
                                    bot.status = format!("There is no second {} pool. Add one with p first", bot.pool.sym);
                                } else {
                                    let labels: Vec<String> = cands.iter().map(|&i| pools[i].label.clone()).collect();
                                    if let Some(sel) = ui::select(terminal, "Arb: 2nd pool (same token)", &labels)? {
                                        let i = cands[sel];
                                        let cfg = to_poolcfg(&pools[i]);
                                        *pool_b_cell.lock().unwrap() = Some(cfg.as_ref());
                                        bot.pool_b = Some(cfg);
                                        bot.arb_mode = true;
                                        apply_prices(bot, &feed); // value the new pool's quote
                                        bot.status = format!("arb: B = {}", pools[i].label);
                                    }
                                }
                            }
                        }
                        // Execute the two-leg arb: buy cheap pool, sell dear pool.
                        KeyCode::Char('e') => {
                            bot.status = "arb: executing…".into();
                            let _ = tokio::time::timeout(Duration::from_secs(14), bot.arb(provider)).await;
                        }
                        KeyCode::Char('c') => {
                            // Live buyer scatter for the current pool (dots, scope-tui style).
                            let v3 = match &bot.pool.kind {
                                engine::PoolKind::V3 { pool_addr, weth_is_token0 } => Some((*pool_addr, *weth_is_token0)),
                                _ => None,
                            };
                            let lb = bot.pons_launch();
                            let ours: std::collections::HashSet<alloy::primitives::TxHash> =
                                bot.orders.iter().filter_map(|o| o.hash).collect();
                            if let Some((pa, w0)) = v3 {
                                discover::screen_clusters(terminal, pa, w0, lb, ours).await?;
                            } else {
                                bot.status = "The cluster view supports Uniswap V3 pools only for now".into();
                            }
                        }
                        KeyCode::Char('f') | KeyCode::Char('F') | KeyCode::Char('t') => {
                            // 'f' = live Pons v3 trenches; Shift-'F' = static Verified pools;
                            // 't' = top tokens (leaderboard + big-fish, established tokens).
                            let grad = if k.code == KeyCode::Char('F') {
                                bot.status = "Loading verified tokens".into();
                                discover::screen_verified(terminal, verified.clone()).await?
                            } else if k.code == KeyCode::Char('t') {
                                bot.status = "loading top tokens…".into();
                                discover::screen_top_tokens(terminal, provider, discovery_rpc.clone()).await?
                            } else {
                                bot.status = "scanning Pons graduations…".into();
                                discover::screen(terminal, provider, bot.trader, discovery_rpc.clone(), bot.eth_usd, verified.clone()).await?
                            };
                            match grad {
                                Some(g) => {
                                    let quote_sym = match &g.quote {
                                        engine::Quote::Eth => "ETH".to_string(),
                                        engine::Quote::Stable { token, .. } => stable_symbol(*token),
                                    };
                                    let p = SelPool {
                                        label: pool_label(false, g.kind.proto(), &quote_sym, &g.sym, g.fee, ""),
                                        kind: g.kind,
                                        token: g.token, sym: g.sym.clone(), fee: g.fee, owned: false,
                                        quote: g.quote.clone(), quote_sym,
                                    };
                                    bot.pool = to_poolcfg(&p);
                                    trace_pool("switch", &bot.pool);
                                    bot.meta = g.meta.clone(); // socials already read during discovery
                                    bot.pool_launch_block = Some(g.launch_block); // for the age display
                                    bot.bought_qty = 0.0; // reset cost basis — realized is per-token, not cross-token
                                    bot.bought_cost = 0.0;
                                    bot.lp_permit2_done = false;
                                    bot.v3_covered = false;
                                    apply_prices(bot, &feed);
                                    // Read the token's real decimals BEFORE publishing to the
                                    // market reader, so its first read is correctly scaled.
                                    // Read the token real decimals BEFORE publishing to the market
                                    // reader, so its first read is scaled correctly.
                                    refresh_token_decimals(provider, bot).await;
                                    *pool_cell.lock().unwrap() = bot.pool.as_ref();
                                    prices.clear();
                                    bot.status = format!("Now trading {}", pool_sentence(&p.label));
                                    match persist_pool(&network, &p) {
                                        Ok(()) => bot.note(format!("Saved {} to your pool list", pool_sentence(&p.label))),
                                        Err(e) => bot.note(format!("Kept {} for this session only because saving failed. {e}", pool_sentence(&p.label))),
                                    }
                                    if !pools.iter().any(|q| q.label == p.label) {
                                        pools.push(p);
                                    }
                                    bot.routes = routes_for(&pools, bot.pool.token);
                                    // Land on the Tape — the new pool's live flow.
                                    view = Panel::Tape;
                                    orders_scroll = 0;
                                }
                                None => bot.status = "discovery cancelled".into(),
                            }
                        }
                        KeyCode::Char('p') => {
                            let mut labels = vec![
                                "＋ Add token by contract address".to_string(),
                                "＋ Add pool (select assets)".to_string(),
                                "＋ Create new v4 pool (select assets)".to_string(),
                            ];
                            // Prune sold-out tokens from the picker: keep pools we still
                            // hold a real balance of, plus any we own (LP). A SELL-ALL often
                            // leaves a few wei of dust (rounding / post-tx airdrops), so we
                            // treat anything below ~0.001 token (18-dec) as empty rather than
                            // exact-zero — otherwise sold pools linger. Concurrent reads,
                            // order preserved; on a read error we keep the pool.
                            let trader = bot.trader;
                            // 1e15 wei = 0.001 token at 18 decimals (these launch tokens are 18-dec).
                            let dust = alloy::primitives::U256::from(1_000_000_000_000_000u64);
                            // The registry accumulates every pool ever traded (hundreds), so this
                            // is a big burst of balance reads. Parallelize hard and bound each call
                            // so the menu opens fast; a slow/failed read keeps the pool (never hide
                            // a real holding behind a timeout).
                            let flags: Vec<bool> = futures::stream::iter(pools.iter().cloned())
                                .map(|p| async move {
                                    if p.owned { return true; }
                                    let erc = contracts::IERC20::new(p.token, provider);
                                    match tokio::time::timeout(
                                        std::time::Duration::from_millis(1500),
                                        erc.balanceOf(trader).call(),
                                    ).await {
                                        Ok(Ok(b)) => b._0 > dust,
                                        _ => true,
                                    }
                                })
                                .buffered(64)
                                .collect()
                                .await;
                            let visible: Vec<SelPool> = pools.iter().cloned()
                                .zip(flags).filter_map(|(p, keep)| keep.then_some(p)).collect();
                            labels.extend(visible.iter().map(|p| p.label.clone()));
                            if let Some(i) = ui::select(terminal, "Pools", &labels)? {
                                // Optionally produce a new SelPool to switch to + append.
                                let new_pool: Option<SelPool> = match i {
                                    0 => {
                                        // Paste a token address → auto-find its liquid WETH
                                        // v3 pool. Fast path for tokens found in the wild.
                                        match ui::input(terminal, "Add token by contract address", "paste the CA (0x…, 20 bytes) — a 32-byte v4 pool id will not work here")? {
                                            Some(s) => match s.trim().parse::<alloy::primitives::Address>() {
                                                Ok(token) => {
                                                    let sym = read_symbol(provider, token).await;
                                                    if let Some((addr, fee, w0)) = find_v3_pool(provider, token).await {
                                                        bot.status = format!("found v3 {} pool for {sym}", fee_label(fee));
                                                        Some(SelPool {
                                                            label: pool_label(false, "v3", "ETH", &sym, fee, ""),
                                                            kind: engine::PoolKind::V3 { pool_addr: addr, weth_is_token0: w0 },
                                                            token, sym, fee, owned: false,
                                                            quote: engine::Quote::Eth, quote_sym: "ETH".to_string(),
                                                        })
                                                    } else {
                                                        bot.status = format!("No liquid Uniswap V3 pool for {sym}. Try selecting assets to use V4");
                                                        None
                                                    }
                                                }
                                                Err(_) => { bot.status = "invalid address".into(); None }
                                            },
                                            None => None,
                                        }
                                    }
                                    1 => {
                                        // Add an existing pool for a wallet-selected pair.
                                        match pick_pair(terminal, provider, &assets, bot.trader).await? {
                                            Pick::Token(token, sym) => match fee_tier_select(terminal)? {
                                                Some((fee, spacing)) => Some(SelPool {
                                                    label: pool_label(false, "v4", "ETH", &sym, fee, ""),
                                                    kind: engine::PoolKind::V4 { pool_id: compute_pool_id(token, fee, spacing), tick_spacing: spacing },
                                                    token, sym, fee, owned: false,
                                                    quote: engine::Quote::Eth, quote_sym: "ETH".to_string(),
                                                }),
                                                None => None,
                                            },
                                            Pick::NeedsEth => { bot.status = "select ETH + one token".into(); None }
                                            Pick::Cancelled => None,
                                        }
                                    }
                                    2 => {
                                        // Create (initialize) a new v4 pool for a selected pair.
                                        match pick_pair(terminal, provider, &assets, bot.trader).await? {
                                            Pick::Token(token, sym) => match fee_tier_select(terminal)? {
                                                Some((fee, spacing)) => {
                                                    let price = ui::input(terminal, "Initial price (token per ETH)", "e.g. 1.0")?
                                                        .and_then(|s| s.parse::<f64>().ok())
                                                        .unwrap_or(1.0);
                                                    let sp96 = price_to_sqrtx96(price);
                                                    let _ = tokio::time::timeout(Duration::from_secs(10), bot.initialize_pool(provider, token, fee, spacing, sp96)).await;
                                                    Some(SelPool {
                                                        label: pool_label(true, "v4", "ETH", &sym, fee, ""),
                                                        kind: engine::PoolKind::V4 { pool_id: compute_pool_id(token, fee, spacing), tick_spacing: spacing },
                                                        token, sym, fee, owned: true,
                                                        quote: engine::Quote::Eth, quote_sym: "ETH".to_string(),
                                                    })
                                                }
                                                None => None,
                                            },
                                            Pick::NeedsEth => { bot.status = "select ETH + one token".into(); None }
                                            Pick::Cancelled => None,
                                        }
                                    }
                                    _ => Some(visible[i - 3].clone()),
                                };
                                if let Some(p) = new_pool {
                                    bot.pool = to_poolcfg(&p);
                                    trace_pool("switch", &bot.pool);
                                    bot.bought_qty = 0.0; // reset cost basis — realized is per-token, not cross-token
                                    bot.bought_cost = 0.0;
                                    bot.lp_permit2_done = false;
                                    bot.v3_covered = false;
                                    apply_prices(bot, &feed); // value the new pool's quote
                                    // Read the token's real decimals BEFORE publishing to the
                                    // market reader, so its first read is correctly scaled.
                                    // Read the token real decimals BEFORE publishing to the market
                                    // reader, so its first read is scaled correctly.
                                    refresh_token_decimals(provider, bot).await;
                                    *pool_cell.lock().unwrap() = bot.pool.as_ref();
                                    prices.clear();
                                    bot.status = format!("Now trading {}", pool_sentence(&p.label));
                                    if i < 3 {
                                        // Persist created/added pools to the registry so
                                        // they survive restarts (not just this session).
                                        match persist_pool(&network, &p) {
                                            Ok(()) => bot.note(format!("Saved {} to your pool list", pool_sentence(&p.label))),
                                            Err(e) => bot.note(format!("Kept {} for this session only because saving failed. {e}", pool_sentence(&p.label))),
                                        }
                                        pools.push(p);
                                    }
                                    // Refresh venues AFTER the new pool is in `pools`, so a
                                    // freshly-added pool is itself a routing candidate.
                                    bot.routes = routes_for(&pools, bot.pool.token);
                                    bot.meta = engine::fetch_token_meta(provider, bot.pool.token).await;
                                    bot.pool_launch_block = discover::fetch_launch_block(bot.pool.token).await;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

/// Which panel fills the middle of the dashboard (cycled with 'l').
#[derive(Clone, Copy, PartialEq)]
enum Panel {
    Orders, // our own actions
    Tape,   // all traders' swaps on the pool
    Logs,   // raw session log
}


/// Draws the dashboard and returns where the header logo goes, so the caller
/// can place a real terminal image there after the frame.
fn draw(f: &mut Frame, bot: &Bot, block: u64, round_ms: f64, view: Panel, orders_scroll: usize, tape: &[engine::Swap], show_help: bool) -> Option<Rect> {
    // Paint the theme background FIRST. Without this a light theme renders dark
    // text on the terminal's own dark background — unreadable.
    ui::widgets::paint_bg(f);
    // One info row: [wallet | market] normally, [wallet | market A | market B]
    // in arb mode. Wallet keeps its familiar vertical format and stays on the left.
    let arb = bot.arb_mode;
    let c = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),  // header (logo + large venue type)
            Constraint::Length(14), // info row (fits wallet's 12 lines incl. status + last fill)
            Constraint::Length(7),  // settings — every adjustable knob on its
                                    // own row, with the latest message beneath
            Constraint::Min(4),     // orders/tape/logs
            Constraint::Length(3),  // footer
        ])
        .split(f.area());
    let info_area = c[1];
    let status_area = c[2];
    let mid_area = c[3];
    let foot_area = c[4];

    // Under 200ms is a healthy round trip to a remote node — the old 10ms floor
    // meant a perfectly good connection always showed amber.
    let lat_color = if round_ms < 200.0 {
        ui::widgets::tone_color(view::Tone::Good)
    } else if round_ms < 800.0 {
        ui::widgets::tone_color(view::Tone::Warn)
    } else {
        ui::widgets::tone_color(view::Tone::Bad)
    };
    // Left: the venue in large type beside its mark. Right: live chain state.
    // The knobs moved to their own Settings box, which freed these rows.
    let (logo_box, indent_cols) = ui::image::header_box(ui::widgets::themed_block("").inner(c[0]));
    let venue = header_venue(bot);
    let avail = c[0].width.saturating_sub(indent_cols + 32);
    let name = match venue {
        ui::image::Venue::Uniswap => format!("UNISWAP {}", bot.pool.kind.proto().to_uppercase()),
        // The launchpad is called pons.family, and on a wide terminal there is
        // room to say so. On a narrow one the full name would drop out of large
        // type altogether and render as small text, which is a worse trade than
        // the short form set properly — so the fuller name is used only when it
        // actually fits.
        ui::image::Venue::Pons if ui::bigtext::width("PONS.FAMILY") <= avail => {
            "PONS.FAMILY".to_string()
        }
        v => v.display_name(&bot.net),
    };

    // Large type only if it fits; on a narrow terminal the plain name is
    // better than three rows of clipped blocks.
    let name_style = Style::default()
        .fg(ui::widgets::tone_color(view::Tone::Accent))
        .add_modifier(Modifier::BOLD);
    let head_left = if ui::bigtext::width(&name) <= avail {
        Paragraph::new(ui::bigtext::render(&name, name_style))
    } else {
        Paragraph::new(Line::from(Span::styled(name.clone(), name_style)))
    };

    let head_right = Paragraph::new(vec![
        // The chain leads: it is the thing that never changes while the two
        // below it change constantly, so it anchors the column.
        Line::from(vec![Span::styled(
            format!("{} ", bot.net),
            Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD),
        )]),
        // Value first, label last: right-aligned, that puts the labels flush
        // against the edge as a column you read down, with the numbers beside
        // them. Labels take the primary colour, like every other label.
        Line::from(vec![
            Span::styled(format!("{round_ms:.0}ms "), Style::default().fg(lat_color)),
            Span::styled(
                "latency ",
                Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::raw(format!("{block} ")),
            Span::styled(
                "block ",
                Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD),
            ),
        ]),
    ])
    .alignment(Alignment::Right);

    let head_block = ui::widgets::themed_block(" Trenches Bot [C] ");
    let head_inner = head_block.inner(c[0]);
    f.render_widget(head_block, c[0]);
    let head_cols = Layout::horizontal([
        Constraint::Length(indent_cols),
        Constraint::Min(20),
        Constraint::Length(30),
    ])
    .split(head_inner);
    f.render_widget(head_left, head_cols[1]);
    f.render_widget(head_right, head_cols[2]);

    // Real image where the terminal supports one; block art is the fallback.
    if !ui::image::supported() {
        if let Some(l) = ui::logo::for_venue(venue, &bot.net) {
            f.render_widget(
                Paragraph::new(l.render_fit(logo_box.width, logo_box.height)),
                logo_box,
            );
        }
    }

    // Everything adjustable in one box, grouped by what it does, with the most
    // recent message underneath — so "what can I change" and "what just
    // happened" are one place instead of scattered across the header.
    // Key hints share the border colour: they are chrome that tells you what to
    // press, distinct from the values they act on.
    let hint = |k: &'static str| Span::styled(
        k,
        Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD),
    );
    // Labels take the primary colour with the keys; the VALUES stay normal text
    // so the number you are reading is the thing that stands out against them.
    let sbold = |t: String| Span::styled(
        t,
        Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD),
    );
    let val = |t: String| Span::styled(t, Style::default().fg(ui::widgets::tone_color(view::Tone::Normal)));
    f.render_widget(
        Paragraph::new(vec![
            // Left column is what you tune while trading; right column is the
            // two modes that change how it behaves. Grouping them that way
            // beats one long ragged list.
            Line::from(vec![
                hint("[ ] "),
                sbold(format!("{:<10}", "buy")),
                val(format!("{:<10}", format!("{:.1}%", bot.buy_frac * 100.0))),
                hint("[M] "),
                sbold(format!("{:<10}", "mode")),
                val(strat_name(bot.strategy).to_string()),
            ]),
            Line::from(vec![
                hint("( ) "),
                sbold(format!("{:<10}", "sell")),
                val(format!("{:<10}", format!("{:.0}%", bot.sell_frac * 100.0))),
                hint("[g] "),
                sbold(format!("{:<10}", "guard")),
                Span::styled(
                    if bot.profit_guard { "ON" } else { "OFF" },
                    Style::default()
                        .fg(if bot.profit_guard {
                            ui::widgets::tone_color(view::Tone::Good)
                        } else {
                            ui::widgets::tone_color(view::Tone::Bad)
                        })
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                hint("{ } "),
                sbold(format!("{:<10}", "slippage")),
                val(format!("{:<10}", format!("{:.0}%", bot.slippage_pct))),
                // Under the guard it sits beside, because they are the same kind
                // of switch: both refuse a trade rather than shaping one.
                hint("[n] "),
                sbold(format!("{:<10}", "dedup")),
                Span::styled(
                    if bot.guard_dup { "ON" } else { "OFF" },
                    Style::default()
                        .fg(if bot.guard_dup {
                            ui::widgets::tone_color(view::Tone::Good)
                        } else {
                            ui::widgets::tone_color(view::Tone::Bad)
                        })
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                hint("< > "),
                sbold(format!("{:<10}", "Impact")),
                val(if bot.max_price_move > 0.0 {
                    format!("{:.1}%", bot.max_price_move * 100.0)
                } else {
                    "off".into()
                }),
            ]),
            Line::from(vec![
                // Not padded to the settings column: it is a message, not a
                // value in that grid, so aligning it just opens a gap.
                Span::styled(
                    "Status  ",
                    Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD),
                ),
                // Normal text: it is the message, not a control.
                val(bot.status.clone()),
            ]),
        ])
        .block(ui::widgets::themed_block(" Settings ")),
        status_area,
    );

    let pnl = bot.pnl();
    let pnl_color = if pnl >= 0.0 { ui::widgets::tone_color(view::Tone::Good) } else { ui::widgets::tone_color(view::Tone::Bad) };

    // Columns: [wallet | market] normally, [wallet | market A | market B] in arb.
    // Wallet is always column 0 (left).
    let cols = if arb {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(34), Constraint::Percentage(33), Constraint::Percentage(33)])
            .split(info_area)
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(info_area)
    };
    let (wallet_col, mkt_a_col, mkt_b_col) = (0usize, 1usize, 2usize);

    // Bold, fixed-width label + plain value, matching the Wallet column.
    let mlbl = |t: &str| {
        Span::styled(
            format!("{t:<9}"),
            Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD),
        )
    };
    let mut mkt: Vec<Line> = Vec::new();
    // Addresses first — always FULL so they can be copy-pasted into an explorer.
    mkt.push(Line::from(vec![mlbl("Token"), Span::raw(bot.pool.token.to_string())]));
    match bot.pool.kind {
        engine::PoolKind::V3 { pool_addr, .. } =>
            mkt.push(Line::from(vec![mlbl("Pool"), Span::raw(format!("{pool_addr}"))])),
        engine::PoolKind::V4 { pool_id, .. } =>
            mkt.push(Line::from(vec![mlbl("Pool"), Span::raw(format!("{pool_id}"))])),
    }
    // Which venue and pair, first — it moved off the header to make room for
    // the large type, and it belongs with the rest of the pool's identity.
    mkt.insert(
        0,
        Line::from(vec![
            mlbl("Protocol"),
            Span::styled(
                format!("{} ETH/{} {}", bot.pool.kind.venue_label(), bot.pool.sym, fee_label(bot.pool.fee)),
                Style::default().fg(ui::widgets::tone_color(view::Tone::Info)).add_modifier(Modifier::BOLD),
            ),
        ]),
    );
    // One piece of information per row — priced in the pool's quote currency.
    mkt.push(Line::from(vec![mlbl("Price"), Span::raw(format!("{:.4} {}/{}", bot.price(), bot.pool.sym, bot.pool.quote_sym))]));
    mkt.push(Line::from(vec![mlbl("Tick"), Span::raw(format!("{}", bot.tick))]));
    mkt.push(Line::from(vec![mlbl("Mkt Cap"), Span::raw(format!("~${:.2}M", bot.market_cap_usd() / 1e6))]));
    if let Some(lb) = bot.pons_launch() {
        let s = block.saturating_sub(lb) / 10; // ~10 blocks/sec since graduation
        let a = if s < 60 { format!("{s}s") } else if s < 3600 { format!("{}m", s / 60) } else { format!("{}h", s / 3600) };
        mkt.push(Line::from(vec![mlbl("Age"), Span::raw(format!("{a} (since graduation)"))]));
    }
    // Pooled reserves (like dexscreener/gmgn) — each side its own row.
    // These come from L and the current price (`L/√P`, `L·√P`), which is what
    // the pool WOULD hold if its liquidity spanned the whole curve. For a
    // concentrated position that overstates real depth — sometimes past the
    // token's entire supply, which is the giveaway. Flag it when that happens
    // rather than presenting a number that cannot be true as exit liquidity.
    let notional = bot.token_supply > 0.0 && bot.r1 > bot.token_supply;
    mkt.push(Line::from(vec![mlbl("Pooled"), Span::raw(format!("{:.3} {}", bot.r0, bot.pool.quote_sym))]));
    mkt.push(Line::from(vec![
        mlbl("Pooled"),
        Span::styled(
            if notional {
                format!("{:.0} {} (estimate, above supply)", bot.r1, bot.pool.sym)
            } else {
                format!("{:.0} {}", bot.r1, bot.pool.sym)
            },
            Style::default().fg(if notional {
                ui::widgets::tone_color(view::Tone::Warn)
            } else {
                ui::widgets::tone_color(view::Tone::Normal)
            }),
        ),
    ]));
    // Token metadata (Pons socials) — confirm the details of what you're trading.
    if !bot.meta.is_empty() {
        mkt.push(Line::from(vec![mlbl("Socials"), Span::raw(format!("{}/7 filled", bot.meta.score()))])
            .style(Style::default().fg(ui::widgets::tone_color(view::Tone::Normal))));
        let m = &bot.meta;
        if !m.website.trim().is_empty() { mkt.push(Line::from(vec![mlbl("Web"), Span::raw(m.website.clone())])); }
        if !m.twitter.trim().is_empty() { mkt.push(Line::from(vec![mlbl("X"), Span::raw(m.twitter.clone())])); }
        if !m.telegram.trim().is_empty() { mkt.push(Line::from(vec![mlbl("Telegram"), Span::raw(m.telegram.clone())])); }
        if !m.discord.trim().is_empty() { mkt.push(Line::from(vec![mlbl("Discord"), Span::raw(m.discord.clone())])); }
    }
    // Nothing selected: none of the above is a fact. A zero address, a 0% fee
    // and a venue we are not on read as data, and the whole point of the empty
    // state is that there is none — so throw it away and say what to press.
    if bot.pool.token.is_zero() {
        mkt = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  No pool selected",
                Style::default().fg(ui::widgets::tone_color(view::Tone::Normal)).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  [f] find pools in the trenches",
                Style::default().fg(ui::widgets::tone_color(view::Tone::Info)),
            )),
            Line::from(Span::styled(
                "  [t] top tokens",
                Style::default().fg(ui::widgets::tone_color(view::Tone::Info)),
            )),
            Line::from(Span::styled(
                "  [p] add a token by contract address",
                Style::default().fg(ui::widgets::tone_color(view::Tone::Info)),
            )),
        ];
    }
    // (profit / edge / status moved to the wallet panel — they're bot-wide.)
    // "Pool" — it is the pool you are trading, and `p` is what changes it.
    let mkt_title = if bot.arb_mode {
        format!(" Pool A [{}] ", bot.pool.kind.proto())
    } else {
        " Pool [p] ".to_string()
    };
    let market = Paragraph::new(mkt).block(ui::widgets::themed_block(mkt_title));
    f.render_widget(market, cols[mkt_a_col]);

    // Arb mode: second pool's own panel in the middle column.
    if bot.arb_mode {
        if let Some(pb) = bot.pool_b.as_ref() {
            let mut mb: Vec<Line> = Vec::new();
            mb.push(Line::from(Span::styled(format!("{:<9}{}", "Token", pb.token), Style::default().fg(ui::widgets::tone_color(view::Tone::Normal)))));
            match pb.kind {
                engine::PoolKind::V3 { pool_addr, .. } =>
                    mb.push(Line::from(vec![mlbl("Pool"), Span::raw(format!("{pool_addr}"))])),
                engine::PoolKind::V4 { pool_id, .. } =>
                    mb.push(Line::from(vec![mlbl("Pool"), Span::raw(format!("{pool_id}"))])),
            }
            let pbp = bot.price_b();
            mb.push(Line::from(format!("{:<9}{:.4} {}/{}", "Price", pbp, pb.sym, pb.quote_sym)));
            mb.push(Line::from(format!("{:<9}{}", "Tick", bot.mkt_b.tick)));
            let mcap_b = if pbp > 0.0 { bot.mkt_b.supply / pbp * pb.quote_usd } else { 0.0 };
            mb.push(Line::from(format!("{:<9}~${:.2}M", "Mkt Cap", mcap_b / 1e6)));
            mb.push(Line::from(format!("{:<9}{:.3} {}", "Pooled", bot.mkt_b.r0, pb.quote_sym)));
            mb.push(Line::from(format!("{:<9}{:.0} {}", "Pooled", bot.mkt_b.r1, pb.sym)));
            let gap = bot.arb_gap_pct();
            mb.push(Line::from(vec![
                Span::styled("gap A/B  ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!("{gap:+.3}%"),
                    Style::default().fg(if gap.abs() > 0.5 { ui::widgets::tone_color(view::Tone::Good) } else { ui::widgets::tone_color(view::Tone::Warn) }).add_modifier(Modifier::BOLD),
                ),
            ]));
            // Precalculated best arb: optimal size + net profit (0 = not worth it).
            let (opt_in, opt_net) = bot.arb_optimal();
            if opt_net > 0.0 {
                mb.push(Line::from(vec![
                    Span::styled(format!("arb +{opt_net:.6} ETH", ), Style::default().fg(ui::widgets::tone_color(view::Tone::Good)).add_modifier(Modifier::BOLD)),
                    Span::styled(format!("  @ {opt_in:.4} in  (e=exec)", ), Style::default().fg(ui::widgets::tone_color(view::Tone::Dim))),
                ]));
            } else {
                mb.push(Line::from(Span::styled("arb  none — spread < fees", Style::default().fg(ui::widgets::tone_color(view::Tone::Dim)))));
            }
            let panel_b = Paragraph::new(mb)
                .block(ui::widgets::themed_block(format!("Market B [{}] {}", pb.kind.proto(), fee_label(pb.fee))));
            f.render_widget(panel_b, cols[mkt_b_col]);
        }
    }

    let day_color = if bot.daily_pnl() >= 0.0 { ui::widgets::tone_color(view::Tone::Good) } else { ui::widgets::tone_color(view::Tone::Bad) };
    let real_color = if bot.realized_pnl >= 0.0 { ui::widgets::tone_color(view::Tone::Good) } else { ui::widgets::tone_color(view::Tone::Bad) };
    // Live, slippage-aware unrealized P&L of selling the whole holding NOW —
    // updates every refresh, independent of the profit filter being on/off.
    let live = bot.live_edge();
    let edge_span = Span::styled(
        format!("{:+.7}", live),
        Style::default()
            .fg(if live > 0.0 { ui::widgets::tone_color(view::Tone::Good) } else if live < 0.0 { ui::widgets::tone_color(view::Tone::Bad) } else { ui::widgets::tone_color(view::Tone::Dim) })
            .add_modifier(Modifier::BOLD),
    );
    // Wallet — same vertical format in both modes, always on the left (col 0).
    // Row labels are bold so the eye lands on them first; values carry the
    // colour. `lbl` keeps the column alignment in one place.
    let lbl = |t: &str| {
        Span::styled(
            format!("{t:<11}"),
            Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD),
        )
    };
    // Nothing to report without an account.
    //
    // A row of zeroes is not "empty", it is a claim: zero balance, zero
    // realized, zero trades. None of that is known until a key is unlocked, and
    // showing it invites someone to read a wallet they have not opened. Same
    // shape as the Pool panel's empty state — say what is missing, then the key
    // that fixes it.
    if bot.trader.is_zero() {
        let empty = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  No account",
                Style::default()
                    .fg(ui::widgets::tone_color(view::Tone::Normal))
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  [W] unlock or create an account",
                Style::default().fg(ui::widgets::tone_color(view::Tone::Info)),
            )),
        ])
        .block(ui::widgets::themed_block(" Wallet [W] "));
        f.render_widget(empty, cols[wallet_col]);
    } else {
    let wallet = Paragraph::new(vec![
        // "Which account am I?" belongs with the balances, not in the header.
        Line::from(vec![
            lbl("Account"),
            // A zero address is not an account, and printing forty characters of
            // zeroes says "something is wrong" rather than "you have not
            // unlocked one". Name the key that fixes it instead.
            if bot.trader.is_zero() {
                Span::styled(
                    "no account — press [W] to unlock one",
                    Style::default()
                        .fg(ui::widgets::tone_color(view::Tone::Warn))
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(
                    format!("{}", bot.trader),
                    Style::default()
                        .fg(ui::widgets::tone_color(view::Tone::Info))
                        .add_modifier(Modifier::BOLD),
                )
            },
        ]),
        Line::from(vec![lbl("ETH"), Span::raw(format!("{:.6}", bot.eth))]),
        Line::from(vec![lbl(&bot.pool.sym), Span::raw(format!("{:.4}", bot.token_bal))]),
        Line::from(vec![
            lbl("Our Liq"),
            Span::raw(format!("{:.4} {} ({} pos)", bot.our_liq_eth(), bot.pool.quote_sym, bot.positions.len())),
        ]),
        Line::from(vec![
            lbl("Basis"),
            Span::raw(format!("{:.6} {}/{}", bot.avg_basis(), bot.pool.quote_sym, bot.pool.sym)),
        ]),
        Line::from(vec![
            lbl("Inventory"),
            Span::raw(format!("{:.2} {} (bought)", bot.bought_qty, bot.pool.sym)),
        ]),
        Line::from(vec![
            lbl("Realized"),
            Span::styled(
                format!("{:+.6} {}", bot.realized_pnl, bot.pool.quote_sym),
                Style::default().fg(real_color).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            lbl("Last Fill"),
            match bot.last_fill_pnl {
                Some(v) => Span::styled(
                    format!("{:+.6} {}", v, bot.pool.quote_sym),
                    Style::default()
                        .fg(if v >= 0.0 { ui::widgets::tone_color(view::Tone::Good) } else { ui::widgets::tone_color(view::Tone::Bad) })
                        .add_modifier(Modifier::BOLD),
                ),
                None => Span::styled(
                    "—".to_string(),
                    Style::default()
                        .fg(ui::widgets::tone_color(view::Tone::Dim))
                        .add_modifier(Modifier::BOLD),
                ),
            },
        ]),
        Line::from(vec![
            lbl("PnL/Day"),
            Span::styled(
                format!("{:+.6} {}", bot.daily_pnl(), bot.pool.quote_sym),
                Style::default().fg(day_color).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            lbl("PnL/Sesh"),
            Span::styled(
                format!("{pnl:+.6} {}", bot.pool.quote_sym),
                Style::default().fg(pnl_color).add_modifier(Modifier::BOLD),
            ),
        ]),
        // Profit above Activity: the guard and edge belong with the PnL lines
        // above them, and Activity reads as the running tally at the bottom.
        Line::from(vec![lbl("Edge"), edge_span]),
        Line::from(vec![
            lbl("Activity"),
            Span::raw(format!(
                "{} trades  {} fails  {} skips  {} pending",
                bot.trades, bot.fails, bot.skips, bot.pending.len()
            )),
        ]),
    ])
    .block(ui::widgets::themed_block(" Wallet [W] "));
    f.render_widget(wallet, cols[wallet_col]);
    }

    match view {
        Panel::Logs => {
        // Logs view: last N lines (fit to panel), errors/skips highlighted.
        let h = mid_area.height.saturating_sub(2) as usize;
        let scroll = orders_scroll.min(bot.logs.len().saturating_sub(1));
        let mut lines: Vec<Line> = Vec::new();
        for l in bot.logs.iter().rev().skip(scroll).take(h.max(1)).rev() {
            let color = if l.contains("REVERT") || l.contains("FAIL") || l.contains("failed") || l.contains("error") {
                ui::widgets::tone_color(view::Tone::Bad)
            } else if l.contains("SKIP") || l.contains("skipped") || l.contains("WARN") || l.contains("would revert") {
                ui::widgets::tone_color(view::Tone::Warn)
            } else if l.contains("CONFIRMED") || l.contains("LIVE") {
                ui::widgets::tone_color(view::Tone::Good)
            } else {
                ui::widgets::tone_color(view::Tone::Normal)
            };
            lines.push(Line::from(Span::styled(l.clone(), Style::default().fg(color))));
        }
        if lines.is_empty() {
            lines.push(Line::from("  (no log lines yet)"));
        }
        let logs = Paragraph::new(lines)
            .block(ui::widgets::themed_block(" Logs [l] "));
        f.render_widget(logs, mid_area);
        }
        Panel::Tape => {
            // Live tape: every trader's swaps on the current pool, newest first.
            // Our own trades (tx hash matches an order) get a ★ marker.
            let ours: std::collections::HashSet<alloy::primitives::TxHash> =
                bot.orders.iter().filter_map(|o| o.hash).collect();
            let h = mid_area.height.saturating_sub(3).max(1) as usize;
            // ~10 blocks/sec on Robinhood Chain — estimate age from block delta.
            let age = |blk: u64| -> String {
                let d = block.saturating_sub(blk);
                let secs = d / 10;
                if secs < 60 { format!("{secs}s") } else { format!("{}m", secs / 60) }
            };
            // Single-market mode shows only the active venue's swaps; arb mode
            // keeps the merged v3+v4 tape.
            let pool_is_v4 = matches!(bot.pool.kind, engine::PoolKind::V4 { .. });
            let shown: Vec<&engine::Swap> =
                tape.iter().filter(|s| bot.arb_mode || s.is_v4 == pool_is_v4).collect();
            let scroll = orders_scroll.min(shown.len().saturating_sub(1));
            let mut t = view::TableView::new(
                format!(" Trades ({}) ⭐ = you [l] ", shown.len()),
                vec![
                    view::Col::fixed("", 2),
                    // Age leads, as on the Solana tape: a tape is read
                    // newest-first, so "how long ago" is the first thing wanted.
                    view::Col::fixed("age", 6),
                    view::Col::fixed("pool", 4),
                    view::Col::fixed("action", 7),
                    view::Col::fixed("amount", 14),
                    view::Col::fixed("price / tick", 24),
                    view::Col::fixed("pooled", 12),
                    view::Col::fixed("mkt cap $", 12),
                    view::Col::fixed("trader", 14),
                    view::Col::min("tx", 12),
                ],
            );
            t.empty_note = "no trades on this pool yet\nthey stream in as they happen".into();
            for s in shown.iter().rev().skip(scroll).take(h) {
                let (lbl, atone) = match s.action {
                    engine::TapeAction::Buy => ("BUY", view::Tone::Good),
                    engine::TapeAction::Sell => ("SELL", view::Tone::Bad),
                    engine::TapeAction::Add => ("ADD", view::Tone::Info),
                    engine::TapeAction::Remove => ("REMOVE", view::Tone::Accent),
                };
                let mine = ours.contains(&s.tx);
                // For LP add/remove, show the tick range in the price column.
                let is_lp = matches!(s.action, engine::TapeAction::Add | engine::TapeAction::Remove);
                let mid = if is_lp {
                    format!("tick [{}, {}]", s.tick_lo, s.tick_hi)
                } else if s.price > 0.0 {
                    format!("{:.6}", s.price)
                } else {
                    "—".into()
                };
                let (venue, vtone) = if s.is_v4 { ("v4", view::Tone::Info) } else { ("v3", view::Tone::Accent) };
                // The whole row carries the trade's colour, so a buy reads as
                // one green unit and a sell as one red one. Age stays neutral:
                // it says when, not what.
                t.push_mine(
                    vec![
                        view::Cell::toned(if mine { view::MINE_MARK } else { "" }, view::Tone::Warn),
                        view::Cell::new(age(s.block)),
                        view::Cell::bold(venue, vtone),
                        view::Cell::bold(lbl, atone),
                        view::Cell::new(format!("{:.6}", s.eth)),
                        view::Cell::new(mid),
                        view::Cell::toned(
                            if s.liq_eth > 0.0 { view::sol_compact(s.liq_eth) } else { String::new() },
                            atone,
                        ),
                        // Dollars, matching the Solana tape: pooled ETH is the
                        // exit liquidity and belongs in ETH, but a cap in ETH
                        // says nothing at a glance.
                        // supply/price is in the QUOTE currency, so it converts
                        // at the quote's USD value — not ETH's. Using eth_usd on
                        // a USDG pool inflated every cap by ~1850x.
                        view::Cell::toned(
                            if s.price > 0.0 && bot.token_supply > 0.0 {
                                let mc = bot.token_supply / s.price;
                                let usd = bot.pool.quote_usd;
                                if usd > 0.0 {
                                    view::usd_compact(mc * usd)
                                } else {
                                    format!("{mc:.3} {}", bot.pool.quote_sym)
                                }
                            } else {
                                String::new()
                            },
                            atone,
                        ),
                        view::Cell::toned(short_addr(s.trader), view::Tone::Normal),
                        view::Cell::toned(format!("{}", s.tx), view::Tone::Normal),
                    ],
                    mine,
                );
            }
            ui::widgets::table(f, mid_area, &t, None);
        }
        Panel::Orders => {
        // Orders queue as an aligned table: STATUS | ACTION | SIDE | AMOUNT | PRICE | TX.
        let h = mid_area.height.saturating_sub(3).max(1) as usize; // minus borders + header
        let total = bot.orders.len();
        let scroll = orders_scroll.min(total.saturating_sub(1));
        // Built as a chain-agnostic TableView and drawn by the shared widget —
        // same code path as the Solana orders panel.
        let title = if total > h {
            format!(" Orders {}–{} of {} ↑/↓ scroll [l] ", scroll + 1, (scroll + h).min(total), total)
        } else {
            format!(" Orders ({total}) [l] ")
        };
        let mut t = view::TableView::new(
            title,
            vec![
                view::Col::fixed("status", 9),
                view::Col::fixed("pool", 4),
                // Orders carry full labels — "SELL ALL", "REMOVE LP #505",
                // "CLOSE ALL" — not the tape's 4-character BUY/SELL.
                view::Col::fixed("action", 14),
                view::Col::fixed("amount ETH", 14),
                view::Col::fixed("price / tick", 24),
                view::Col::fixed("pooled ETH", 12),
                view::Col::fixed("mkt cap $", 12),
                view::Col::min("tx", 66),
            ],
        );
        t.empty_note = "no orders yet\npress b to buy  ·  s to sell  ·  a add LP  ·  x sell all".into();
        for o in bot.orders.iter().rev().skip(scroll).take(h) {
            let (st, stone) = match o.status {
                engine::OrderStatus::Confirmed => ("confirmed", view::Tone::Good),
                engine::OrderStatus::Pending => ("pending", view::Tone::Warn),
                engine::OrderStatus::Reverted => ("reverted", view::Tone::Bad),
                engine::OrderStatus::Failed => ("failed", view::Tone::Bad),
                engine::OrderStatus::Skipped => ("skipped", view::Tone::Dim),
            };
            let (action, _amount, price) = parse_order(&o.label);
            let atone = match action.to_ascii_uppercase().as_str() {
                s if s.starts_with("BUY") => view::Tone::Good,
                s if s.starts_with("SELL") => view::Tone::Bad,
                s if s.starts_with("ADD") => view::Tone::Info,
                s if s.starts_with("REMOVE") || s.starts_with("CLOSE") => view::Tone::Accent,
                _ => view::Tone::Normal,
            };
            let (venue, vtone) = if o.is_v4 { ("v4", view::Tone::Info) } else { ("v3", view::Tone::Accent) };
            t.push(vec![
                view::Cell::bold(st, stone),
                view::Cell::bold(venue, vtone),
                view::Cell::bold(action, atone),
                // Amount always in ETH numeraire (cost in ether).
                view::Cell::new(if o.eth > 0.0 { format!("{:.6}", o.eth) } else { String::new() }),
                view::Cell::new(price),
                view::Cell::new(if o.pooled > 0.0 { format!("{:.4}", o.pooled) } else { String::new() }),
                view::Cell::new(if o.mc > 0.0 {
                    if bot.eth_usd > 0.0 {
                        view::usd_compact(o.mc * bot.eth_usd)
                    } else {
                        format!("{:.3} ETH", o.mc)
                    }
                } else {
                    String::new()
                }),
                view::Cell::toned(
                    o.hash.map(|h| format!("{h:#x}")).unwrap_or_default(),
                    view::Tone::Normal,
                ),
            ]);
        }
        ui::widgets::table(f, mid_area, &t, None);
        }
    }

    // Trimmed footer — essentials only; '?' opens the full shortcut list.
    let footer = Paragraph::new(Line::from(vec![
        Span::styled("[b]", Style::default().fg(ui::widgets::tone_color(view::Tone::Good)).add_modifier(Modifier::BOLD)),
        Span::raw(" buy  "),
        Span::styled("[s]", Style::default().fg(ui::widgets::tone_color(view::Tone::Bad)).add_modifier(Modifier::BOLD)),
        Span::raw(" sell  "),
        Span::styled("[x]", Style::default().fg(ui::widgets::tone_color(view::Tone::Warn)).add_modifier(Modifier::BOLD)),
        Span::raw(" sell-all/close  "),
        Span::styled("[f]", Style::default().fg(ui::widgets::tone_color(view::Tone::Accent)).add_modifier(Modifier::BOLD)),
        Span::raw(" find  "),
        Span::styled("[l]", Style::default().fg(ui::widgets::tone_color(view::Tone::Normal)).add_modifier(Modifier::BOLD)),
        Span::raw(" logs  "),
        Span::styled("[?]", Style::default().fg(ui::widgets::tone_color(view::Tone::Info)).add_modifier(Modifier::BOLD)),
        Span::raw(" help  "),

        Span::styled("[W]", Style::default().fg(ui::widgets::tone_color(view::Tone::Info)).add_modifier(Modifier::BOLD)),
        Span::raw(" wallet  "),
        Span::styled("[D]", Style::default().fg(ui::widgets::tone_color(view::Tone::Info)).add_modifier(Modifier::BOLD)),
        Span::raw(" docs  "),
        Span::styled("[q]", Style::default().fg(ui::widgets::tone_color(view::Tone::Info)).add_modifier(Modifier::BOLD)),
        Span::raw(" quit"),
    ]))
    .block(ui::widgets::themed_block(""));
    f.render_widget(footer, foot_area);

    // Full shortcut list overlay ('?') — a grouped two-column list: key on the
    // left, description on the right. Related knobs ( [ ] ( ) { } ) grouped.
    if show_help {
        // (section, key, description). Empty key = section header.
        let items: [(&str, &str); 33] = [
            ("TRADE", ""),
            ("", "b|buy"),
            ("", "s|sell"),
            ("", "x|sell all"),
            ("LIQUIDITY", ""),
            ("", "a|add liquidity"),
            ("", "r|remove last liquidity"),
            ("DISCOVER", ""),
            ("", "f|find market"),
            ("", "F|verified tokens"),
            ("", "t|leaderboard"),
            ("POOL / ARB", ""),
            ("", "p|select pool"),
            ("", "Del|deselect the pool"),
            ("", "d|toggle multi-pool view"),
            ("", "e|auto arbitrage"),
            ("VIEW", ""),
            ("", "l  → ←|cycle orders · trades · logs"),
            ("", "↑ ↓|scroll"),
            ("", "c|cluster graph"),
            ("", "L|PnL calendar"),
            ("SIZE", ""),
            ("", "[  ]|buy size −/+"),
            ("", "(  )|sell size −/+"),
            ("", "{  }  0|slippage −/+"),
            ("MODE", ""),
            ("", "T|theme picker"),
            ("", "m|mode"),
            ("", "M|toggle mode"),
            ("", "g|toggle profit guard"),
            ("", "n|toggle buy dedup"),
            ("", "q|quit"),
            ("", "?|help"),
        ];
        // Shared with the Solana dashboard — one renderer, one look.
        ui::widgets::help(f, &items, " Shortcuts  (any key to close) ");
    }
    Some(logo_box)
}

/// Ask for a v4 fee tier; returns (fee, tickSpacing).
fn fee_tier_select(
    term: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) -> eyre::Result<Option<(u32, i32)>> {
    let opts = vec![
        "0.05%   (tickSpacing 10)".to_string(),
        "0.30%   (tickSpacing 60)".to_string(),
        "1%      (tickSpacing 200)".to_string(),
    ];
    Ok(match ui::select(term, "Fee tier", &opts)? {
        Some(0) => Some((500, 10)),
        Some(1) => Some((3000, 60)),
        Some(2) => Some((10000, 200)),
        _ => None,
    })
}

/// price (token per ETH) -> sqrtPriceX96 for pool initialization.
fn price_to_sqrtx96(price: f64) -> alloy::primitives::aliases::U160 {
    use std::str::FromStr;
    type U160 = alloy::primitives::aliases::U160;
    let x = price.max(1e-18).sqrt() * 2f64.powi(96);
    U160::from_str(&format!("{x:.0}")).unwrap_or_else(|_| U160::from(1u8) << 96) // ~price 1.0
}

fn wei_f64(x: alloy::primitives::U256) -> f64 {
    x.to_string().parse::<f64>().unwrap_or(0.0) / 1e18
}

/// Result of the two-asset wallet picker.
enum Pick {
    Token(alloy::primitives::Address, String),
    NeedsEth,
    Cancelled,
}

/// Let the user pick a pair from their wallet assets (ETH + known tokens, with
/// live balances). ETH is currency0; returns the non-ETH token of the pair.
async fn pick_pair<P: Provider>(
    term: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    provider: &P,
    assets: &[(alloy::primitives::Address, String)],
    trader: alloy::primitives::Address,
) -> eyre::Result<Pick> {
    use alloy::primitives::Address;
    let mut labels = Vec::new();
    for (addr, sym) in assets {
        let bal = if *addr == Address::ZERO {
            provider.get_balance(trader).await.map(wei_f64).unwrap_or(0.0)
        } else {
            contracts::IERC20::new(*addr, provider)
                .balanceOf(trader)
                .call()
                .await
                .map(|b| wei_f64(b._0))
                .unwrap_or(0.0)
        };
        labels.push(format!("{:<8} balance {:.6}", sym, bal));
    }
    let picked = match ui::multi_select(term, "Tick the two assets to pair", &labels, 2)? {
        Some(v) => v,
        None => return Ok(Pick::Cancelled),
    };
    if picked.len() != 2 {
        return Ok(Pick::NeedsEth); // must tick exactly two (one being ETH)
    }
    let aa = assets[picked[0]].0;
    let bb = assets[picked[1]].0;
    if aa == Address::ZERO && bb != Address::ZERO {
        Ok(Pick::Token(bb, assets[picked[1]].1.clone()))
    } else if bb == Address::ZERO && aa != Address::ZERO {
        Ok(Pick::Token(aa, assets[picked[0]].1.clone()))
    } else {
        Ok(Pick::NeedsEth)
    }
}

/// Split an order label into (action, amount, price) columns for the table.
/// Handles trades ("BUY ~0.00017 ETH @ 1.0"), skips ("BUY (no edge +0.0001)"),
/// LP ops ("ADD LP ~0.001 ETH", "REMOVE LP #505"), and pool ops.
/// The deployer's buy ladder: EXACT on-chain buy amounts (keyed by integer wei —
/// no rounding), counted only on exact matches, kept where an amount repeats
/// (count >= 2), sorted ascending so [ ] steps monotonically. Each step is a real
/// wei amount someone bought N times. Falls back to the latest buy if none repeat.
fn buy_tiers(buys_wei: &[u128]) -> Vec<(f64, usize)> {
    if buys_wei.is_empty() {
        return Vec::new();
    }
    let mut counts: std::collections::HashMap<u128, usize> = std::collections::HashMap::new();
    for &w in buys_wei {
        if w > 0 {
            *counts.entry(w).or_insert(0) += 1;
        }
    }
    let mut v: Vec<(f64, usize)> = counts
        .into_iter()
        .filter(|&(_, c)| c >= 2)
        .map(|(w, c)| (w as f64 / 1e18, c))
        .collect();
    if v.is_empty() {
        return vec![(*buys_wei.last().unwrap() as f64 / 1e18, 1)]; // nothing repeated — shadow latest
    }
    v.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    v
}

/// Status line for mode 1 (masked): selected step, how many steps, frequency.
/// Plain-sentence description of the copy mode, for the status bar.
///
/// The header used to carry a cryptic "copy: waiting for buys" tag. Explaining
/// a mode belongs in the status line, in words, not as a symbol in a row of
/// numbers.
fn copy_status(bot: &Bot) -> String {
    if bot.copy_tiers.is_empty() {
        return "Watching for a buy size that repeats. Until one appears, buys use your own buy size"
            .into();
    }
    let (sz, c) = bot.copy_tiers[bot.copy_idx.min(bot.copy_tiers.len() - 1)];
    format!(
        "Copying buy step {} of {}, {:.9} ETH, seen {} times. Press the bracket keys to change it",
        bot.copy_idx + 1,
        bot.copy_tiers.len(),
        sz,
        c
    )
}

fn parse_order(label: &str) -> (String, String, String) {
    // Trade with a price.
    if let Some((lhs, price)) = label.split_once(" @ ") {
        let mut it = lhs.splitn(2, ' ');
        let action = it.next().unwrap_or("").to_string();
        let amount = it.next().unwrap_or("").trim_start_matches('~').to_string();
        // The label carries "[v3 0.3%] liq_eth=…" after the price for the log
        // line's benefit. The table already has dedicated pool / pooled columns,
        // so cut it here rather than repeating it inside the price cell.
        let price = price.split(" [").next().unwrap_or(price).trim().to_string();
        return (action, amount, price);
    }
    // Skipped trade with an edge note: "BUY (no edge +0.0001)".
    if let Some((side, rest)) = label.split_once(" (") {
        return (side.to_string(), rest.trim_end_matches(')').to_string(), String::new());
    }
    // Multi-word LP / pool actions.
    for pref in ["ADD LP", "REMOVE LP", "CLOSE LP", "CLOSE ALL", "CREATE POOL"] {
        if let Some(rest) = label.strip_prefix(pref) {
            return (pref.to_string(), rest.trim().trim_start_matches('~').to_string(), String::new());
        }
    }
    (label.to_string(), String::new(), String::new())
}

/// Fee tier as a human label: 10000 -> "1%", 3000 -> "0.3%", 500 -> "0.05%".
fn fee_label(fee: u32) -> String {
    let pct = fee as f64 / 10_000.0;
    let s = format!("{pct:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{s}%")
}

/// The mode name shown beside `[M]` in Settings. "manual" and "copy" are the
/// words the app uses everywhere else for these two.
fn strat_name(s: engine::Strategy) -> &'static str {
    match s {
        engine::Strategy::Manual => "manual",
        engine::Strategy::CopyBuyAmount => "copy",
    }
}

// ---------------- interactive selection (arrow-key menus via inquire) -------

