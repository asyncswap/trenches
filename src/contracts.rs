// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! On-chain contract interfaces for Robinhood Chain (Uniswap v4 stack) via
//! alloy's `sol!` macro — real ABI encoding, no hand-rolled calldata.

use alloy::primitives::{address, Address};
use alloy::sol;

// The addresses that differ per chain.
//
// These were `const`, which is another way of saying "this app runs on exactly
// one chain". The config has always accepted several networks, so pointing it
// at a second EVM chain produced a bot that looked configured and swapped
// against contracts that are not there.
//
// One table per chain, chosen by chain id. Resolved per call rather than cached:
// `C` changes chain without restarting, and a value latched at startup would
// quietly outlive the chain it belongs to.
//
// PERMIT2 is NOT here. It is deployed deterministically, at the same address on
// every chain, so it stays a constant — the one thing about this that genuinely
// does not vary.
pub struct Venues {
    pub pool_manager: Address,
    pub position_manager: Address,
    pub state_view: Address,
    pub universal_router: Address,
    /// Uniswap v3 factory and router, and the wrapped-native token that is the
    /// ETH side of every v3 pool.
    pub v3_factory: Address,
    pub swap_router_02: Address,
    pub weth: Address,
    /// Flaunch: the hook-and-factory, its wrapped ETH, and the conversion hook.
    /// ZERO where Flaunch is not deployed.
    pub flaunch_pm: Address,
    pub fleth: Address,
    pub fleth_hooks: Address,
    /// pons, Robinhood Chain's launchpad. ZERO elsewhere — discovery skips a
    /// launchpad whose address is zero rather than scanning for an event that
    /// cannot be emitted.
    pub pons_factory: Address,
    pub pons_v2_factory: Address,
    pub pons_v2_hook: Address,
}

/// Official Robinhood Chain deployment.
static ROBINHOOD: Venues = Venues {
    pool_manager: address!("8366a39CC670B4001A1121B8F6A443A643e40951"),
    position_manager: address!("58daec3116aae6D93017bAAea7749052E8a04fA7"),
    state_view: address!("f3334192d15450cdd385c8b70e03f9a6bd9e673b"),
    universal_router: address!("8876789976dEcBfCbBbe364623C63652db8C0904"),
    v3_factory: address!("1f7d7550b1b028f7571e69a784071f0205fd2efa"),
    swap_router_02: address!("caf681a66d020601342297493863e78c959e5cb2"),
    weth: address!("0Bd7D308f8E1639FAb988df18A8011f41EAcAD73"),
    // Flaunch: FLAUNCH_PM is the v4 hook and the launch factory in one. It emits
    // PoolCreated per coin, and every coin trades in a PoolManager pool keyed
    // { flETH, coin, fee: 0, tickSpacing: 60, hooks: this }.
    flaunch_pm: address!("5Cf8e499C7c466C7E2cf127BDF129F57151E65Dc"),
    // flETH — 18-decimal wrapped ETH, redeemable 1:1, so flETH amounts ARE ETH
    // amounts everywhere prices and reserves are read.
    fleth: address!("00000000043C1117DAFA3A3D0C7148Eb48B30130"),
    // The ETH/flETH conversion pool's hook (first hop of every Flaunch swap).
    fleth_hooks: address!("EA22Ae03085CAf74Ac3393f9902539fbE9786888"),
    // pons v1 — emits TokenLaunched on graduation (token + its v3 pool).
    pons_factory: address!("A5aAb3F0c6EeadF30Ef1D3Eb997108E976351feB"),
    // pons v2. A launch no longer starts life as a pool: the whole supply sits
    // on a bonding CURVE, and a Uniswap v4 pool is created only at graduation,
    // seeded from what the curve collected. So a v2 launch trades in two
    // different places over its life, and `phase` on the factory record says
    // which — 0 curve, 1 swept (closed, pool not built yet), 2 pool, 3 rescued.
    //
    // v1 is a different contract with a different event; both are live, so
    // discovery watches both.
    pons_v2_factory: address!("7E1EAbd52Ae29598e6483F72dCf1a70b14284dB8"),
    // The v4 hook every graduated v2 pool carries. The pool's own fee is ZERO —
    // the hook charges instead, so it can split the fee under the same policy
    // the curve used rather than paying a liquidity provider that does not
    // exist.
    pons_v2_hook: address!("8e99D2009D60A917e9B1c00C04C077b8c0c3a044"),
};

/// Base mainnet.
///
/// Uniswap addresses from the Uniswap/contracts registry (commit 3793618).
/// The same file lists Robinhood Chain, and five of the six addresses we
/// already trade through match it exactly — which is why it is trusted for a
/// chain we cannot test by eye.
///
/// Flaunch addresses from flaunchgg-contracts' own README, Base column. Base is
/// Flaunch's home chain, so these are the canonical deployment rather than a
/// port of it.
///
/// WETH is the OP Stack predeploy. Not in the Uniswap registry — corroborated
/// instead by Flaunch's Base fork tests, which name it as ETH_TOKEN.
static BASE: Venues = Venues {
    pool_manager: address!("498581fF718922c3f8e6A244956aF099B2652b2b"),
    position_manager: address!("7C5f5A4bBd8fD63184577525326123B519429bDc"),
    state_view: address!("A3c0c9b65baD0b08107Aa264b0f3dB444b867A71"),
    universal_router: address!("6fF5693b99212Da76ad316178A184AB56D299b43"),
    v3_factory: address!("33128a8fC17869897dcE68Ed026d694621f6FDfD"),
    swap_router_02: address!("2626664c2603336E57B271c5C0b26F421741e481"),
    weth: address!("4200000000000000000000000000000000000006"),
    flaunch_pm: address!("23321f11a6d44fd1ab790044fdfde5758c902fdc"),
    fleth: address!("000000000d564d5be76f7f0d28fe52605afc7cf8"),
    fleth_hooks: address!("9e433f32bb5481a9ca7dff5b3af74a7ed041a888"),
    // pons is Robinhood Chain's launchpad and is not deployed here. Zero, not
    // omitted: discovery reads these to decide what to scan for, and a zero
    // says "not on this chain" in a way a wrong address cannot.
    pons_factory: Address::ZERO,
    pons_v2_factory: Address::ZERO,
    pons_v2_hook: Address::ZERO,
};

#[allow(dead_code)] // the pair reads as a pair; only one is matched on
pub const ROBINHOOD_MAINNET: u64 = 4663;
pub const BASE_MAINNET: u64 = 8453;

/// The addresses for the chain currently selected.
pub fn venues() -> &'static Venues {
    match crate::chain_id() {
        BASE_MAINNET => &BASE,
        _ => &ROBINHOOD,
    }
}

pub fn pool_manager() -> Address { venues().pool_manager }
pub fn position_manager() -> Address { venues().position_manager }
pub fn state_view() -> Address { venues().state_view }
pub fn universal_router() -> Address { venues().universal_router }
pub fn v3_factory() -> Address { venues().v3_factory }
pub fn swap_router_02() -> Address { venues().swap_router_02 }
pub fn weth() -> Address { venues().weth }
pub fn flaunch_pm() -> Address { venues().flaunch_pm }
pub fn fleth() -> Address { venues().fleth }
pub fn fleth_hooks() -> Address { venues().fleth_hooks }
pub fn pons_factory() -> Address { venues().pons_factory }
pub fn pons_v2_factory() -> Address { venues().pons_v2_factory }
pub fn pons_v2_hook() -> Address { venues().pons_v2_hook }

// Existing tokens placed straight into a v2-style pool, skipping the curve.
// Nothing reads these yet; they are recorded so the addresses are not looked up
// again when something does.
#[allow(dead_code)]
pub const PONS_MIGRATION_FACTORY: Address = address!("050e5C224466e2d377a7E555E139D51268239b39");
#[allow(dead_code)]
pub const PONS_MIGRATION_HOOK: Address = address!("107251FFCC1fc808643DC8dA345e901f59EC2044");

// Deployed deterministically — the same address on every chain it exists on.
pub const PERMIT2: Address = address!("000000000022D473030F116dDEE9F6B43aC78BA3");

pub const FLAUNCH_TICK_SPACING: i32 = 60;
// The Flaunch swap fee is charged by the hook, not the pool (lpFee reads 0),
// so quotes need it supplied out-of-band: ~1% standard, in hundredths of a bip.
// Display/estimate only — PoolKey and PathKey carry the real on-chain fee, 0.
pub const FLAUNCH_FEE_EST: u32 = 10_000;

// SwapRouter02 recipient sentinels for multicall chaining.
pub const ADDRESS_THIS: Address = address!("0000000000000000000000000000000000000002");

sol! {
    #[sol(rpc)]
    interface IStateView {
        function getSlot0(bytes32 poolId) external view returns (uint160 sqrtPriceX96, int24 tick, uint24 protocolFee, uint24 lpFee);
        function getLiquidity(bytes32 poolId) external view returns (uint128 liquidity);
    }

    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address owner) external view returns (uint256);
        function totalSupply() external view returns (uint256);
        function approve(address spender, uint256 value) external returns (bool);
        function transfer(address to, uint256 value) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
        function symbol() external view returns (string);
        function decimals() external view returns (uint8);
    }

    #[sol(rpc)]
    interface IPoolManager {
        function initialize(PoolKey key, uint160 sqrtPriceX96) external returns (int24 tick);
    }

    // ---- Pons launchpad ----
    #[sol(rpc)]
    interface IPonsFactory {
        event TokenLaunched(
            address indexed token,
            address indexed deployer,
            address indexed dexFactory,
            address pairToken,
            address pool,
            uint256 dexId,
            uint256 launchConfigId,
            uint256 positionId,
            uint256 restrictionsEndBlock,
            uint256 initialBuyAmount
        );
    }

    // Pons launch token — on-chain socials/metadata (for a completeness score).
    #[sol(rpc)]
    interface IPonsToken {
        function logo() external view returns (string);
        function description() external view returns (string);
        function socials() external view returns (string twitter, string telegram, string discord, string website, string farcaster);
    }

    // ---- Flaunch launchpad ----
    // Launch parameters echoed in PoolCreated. Robinhood's multichain deploy
    // has no fair-launch phase, so a pool trades as soon as it exists (unless
    // flaunchAt schedules it later).
    struct FlaunchParams {
        string name;
        string symbol;
        string tokenUri;
        uint256 premineAmount;
        address creator;
        uint24 creatorFeeAllocation;
        uint256 flaunchAt;
        bytes initialPriceParams;
        bytes feeCalculatorParams;
    }

    #[sol(rpc)]
    interface IFlaunchPositionManager {
        event PoolCreated(
            bytes32 indexed _poolId,
            address _memecoin,
            address _memecoinTreasury,
            uint256 _tokenId,
            bool _currencyFlipped,
            uint256 _flaunchFee,
            FlaunchParams _params
        );
        // Empty answer (a token Flaunch never launched) reads tickSpacing == 0.
        function poolKey(address _token) external view returns (PoolKey key);
    }

    // ---- pons v2 ----
    // A DIFFERENT event from v1's TokenLaunched, so the two are told apart by
    // topic0 rather than by address alone. `curve` is where the launch trades
    // until it graduates; there is no pool to read before then.
    #[sol(rpc)]
    interface IPonsV2Factory {
        event TokenLaunched(
            address indexed token,
            address indexed curve,
            address indexed deployer,
            address pairToken,
            uint256 launchConfigId,
            uint256 graduationThreshold
        );
        function getLaunchedToken(address token) external view returns (PonsV2Launch);
    }

    // The factory's record of a launch. `phase` is authoritative for routing —
    // do not infer it from balances or events.
    struct PonsV2Launch {
        address token;
        address curve;
        address deployer;
        address creatorFeeRecipient;
        address pairToken;
        uint256 graduationThreshold;
        uint24 poolFee;
        int24 tickSpacing;
        uint16 creatorTaxBps;
        bool buybackEnabled;
        uint8 phase;
        uint256 sweptQuote;
        uint256 sweptTokens;
        uint256 sweptAt;
        bool exists;
    }

    // The bonding curve a v2 launch trades on before graduation. `quoteReserve`
    // includes a PHANTOM balance that sets the opening price without anyone
    // depositing up front, so it always reads higher than what was actually
    // collected — price is quoteReserve/tokenReserve, funds raised is
    // realQuoteReserve.
    #[sol(rpc)]
    interface IPonsCurve {
        function getReserves() external view returns (uint256 quoteReserve, uint256 tokenReserve);
        function realQuoteReserve() external view returns (uint256);
        function graduationThreshold() external view returns (uint256);
        function sellableTokens() external view returns (uint256);
        function readyToGraduate() external view returns (bool);
        function graduated() external view returns (bool);
        function feeBps() external view returns (uint256);
        function creatorTaxBps() external view returns (uint256);
        function isNativeQuote() external view returns (bool);
        function pairToken() external view returns (address);
        function buy(uint256 quoteIn, uint256 minTokensOut, address recipient) external payable returns (uint256 tokensOut);
        function sell(uint256 tokensIn, uint256 minQuoteOut, address recipient) external returns (uint256 quoteOut);
    }

    // ---- Uniswap v3 ----
    #[sol(rpc)]
    interface IV3Factory {
        function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address pool);
    }

    #[sol(rpc)]
    interface IV3Pool {
        function slot0() external view returns (uint160 sqrtPriceX96, int24 tick, uint16 observationIndex, uint16 observationCardinality, uint16 observationCardinalityNext, uint8 feeProtocol, bool unlocked);
        function liquidity() external view returns (uint128);
        function token0() external view returns (address);
        function token1() external view returns (address);
        function fee() external view returns (uint24);
    }

    struct V3ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }

    #[sol(rpc)]
    interface ISwapRouter02 {
        function exactInputSingle(V3ExactInputSingleParams params) external payable returns (uint256 amountOut);
        function multicall(bytes[] data) external payable returns (bytes[] results);
        function unwrapWETH9(uint256 amountMinimum, address recipient) external payable;
    }

    #[sol(rpc)]
    interface IPermit2 {
        function approve(address token, address spender, uint160 amount, uint48 expiration) external;
        function allowance(address user, address token, address spender) external view returns (uint160 amount, uint48 expiration, uint48 nonce);
    }

    #[sol(rpc)]
    interface IUniversalRouter {
        function execute(bytes commands, bytes[] inputs, uint256 deadline) external payable;
    }

    #[sol(rpc)]
    interface IPositionManager {
        function modifyLiquidities(bytes unlockData, uint256 deadline) external payable;
        function nextTokenId() external view returns (uint256);
        function ownerOf(uint256 tokenId) external view returns (address);
        function balanceOf(address owner) external view returns (uint256);
        // `info` is bit-packed; see `engine::position_ticks` for the layout.
        // Needed to price a burn: without the position's range there is no way
        // to say what it should return, and no way to set a minimum.
        function getPoolAndPositionInfo(uint256 tokenId) external view returns (PoolKey poolKey, uint256 info);
        function getPositionLiquidity(uint256 tokenId) external view returns (uint128 liquidity);
    }

    // v4 PoolKey and the v4 Router exact-input params (for planner encoding).
    struct PoolKey {
        address currency0;
        address currency1;
        uint24 fee;
        int24 tickSpacing;
        address hooks;
    }

    struct ExactInputSingleParams {
        PoolKey poolKey;
        bool zeroForOne;
        uint128 amountIn;
        uint128 amountOutMinimum;
        uint256 minHopPriceX36;
        bytes hookData;
    }

    // Multi-hop exact-in (SWAP_EXACT_IN). Each PathKey names the currency the
    // hop lands on plus the pool that gets it there. minHopPriceX36 is the
    // Robinhood router's per-hop limit extension; empty means no limits.
    struct PathKey {
        address intermediateCurrency;
        uint24 fee;
        int24 tickSpacing;
        address hooks;
        bytes hookData;
    }

    struct ExactInputParams {
        address currencyIn;
        PathKey[] path;
        uint256[] minHopPriceX36;
        uint128 amountIn;
        uint128 amountOutMinimum;
    }

    struct MintPositionParams {
        PoolKey poolKey;
        int24 tickLower;
        int24 tickUpper;
        uint256 liquidity;
        uint128 amount0Max;
        uint128 amount1Max;
        address owner;
        bytes hookData;
    }
}

// Universal Router command + v4 planner action bytes.
pub const V4_SWAP: u8 = 0x10;
pub const SWAP_EXACT_IN_SINGLE: u8 = 0x06;
pub const SWAP_EXACT_IN: u8 = 0x07; // multi-hop exact-in (PathKey route)
pub const SETTLE_ALL: u8 = 0x0c;
pub const TAKE_ALL: u8 = 0x0f;
#[cfg(feature = "liquidity")]
pub const MINT_POSITION: u8 = 0x02;
#[cfg(feature = "liquidity")]
pub const SETTLE_PAIR: u8 = 0x0d;
#[cfg(feature = "liquidity")]
pub const SWEEP: u8 = 0x14;
#[cfg(feature = "liquidity")]
pub const BURN_POSITION: u8 = 0x03;
#[cfg(feature = "liquidity")]
pub const TAKE_PAIR: u8 = 0x11;

#[cfg(test)]
mod venue_tests {
    use super::*;

    /// A chain's table must be complete for what that chain HAS. A zero where a
    /// contract exists routes a swap at nothing; the whole point of the table
    /// is that this is checkable rather than discovered by a failed trade.
    #[test]
    fn every_chain_names_the_venues_it_trades_through() {
        for (name, v) in [("robinhood", &ROBINHOOD), ("base", &BASE)] {
            for (what, a) in [
                ("pool_manager", v.pool_manager),
                ("position_manager", v.position_manager),
                ("state_view", v.state_view),
                ("universal_router", v.universal_router),
                ("v3_factory", v.v3_factory),
                ("swap_router_02", v.swap_router_02),
                ("weth", v.weth),
                ("flaunch_pm", v.flaunch_pm),
                ("fleth", v.fleth),
                ("fleth_hooks", v.fleth_hooks),
            ] {
                assert!(!a.is_zero(), "{name} has no {what}");
            }
        }
    }

    /// Two chains must not share an address by accident — a copy-paste from one
    /// table into the other is the likeliest way this file goes wrong, and it
    /// would send Base trades at Robinhood contracts.
    #[test]
    fn the_two_chains_do_not_share_addresses() {
        let r = [
            ROBINHOOD.pool_manager, ROBINHOOD.position_manager, ROBINHOOD.state_view,
            ROBINHOOD.universal_router, ROBINHOOD.v3_factory, ROBINHOOD.swap_router_02,
            ROBINHOOD.weth, ROBINHOOD.flaunch_pm, ROBINHOOD.fleth, ROBINHOOD.fleth_hooks,
        ];
        let b = [
            BASE.pool_manager, BASE.position_manager, BASE.state_view,
            BASE.universal_router, BASE.v3_factory, BASE.swap_router_02,
            BASE.weth, BASE.flaunch_pm, BASE.fleth, BASE.fleth_hooks,
        ];
        for (i, x) in b.iter().enumerate() {
            assert!(!r.contains(x), "base slot {i} carries a Robinhood address: {x}");
        }
    }

    /// pons is Robinhood's launchpad. Base must say so with a zero rather than
    /// inheriting an address that means nothing there.
    #[test]
    fn base_claims_no_pons() {
        assert!(BASE.pons_factory.is_zero());
        assert!(BASE.pons_v2_factory.is_zero());
        assert!(BASE.pons_v2_hook.is_zero());
        assert!(!ROBINHOOD.pons_factory.is_zero(), "and Robinhood still has it");
    }
}
