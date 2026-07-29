// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! On-chain contract interfaces for Robinhood Chain (Uniswap v4 stack) via
//! alloy's `sol!` macro — real ABI encoding, no hand-rolled calldata.

use alloy::primitives::{address, Address};
use alloy::sol;

// Official Robinhood Chain deployment.
pub const POOL_MANAGER: Address = address!("8366a39CC670B4001A1121B8F6A443A643e40951");
pub const POSITION_MANAGER: Address = address!("58daec3116aae6D93017bAAea7749052E8a04fA7");
pub const STATE_VIEW: Address = address!("f3334192d15450cdd385c8b70e03f9a6bd9e673b");
pub const UNIVERSAL_ROUTER: Address = address!("8876789976dEcBfCbBbe364623C63652db8C0904");
pub const PERMIT2: Address = address!("000000000022D473030F116dDEE9F6B43aC78BA3");

// Pons launch factory — emits TokenLaunched on graduation (token + its v3 pool).
pub const PONS_FACTORY: Address = address!("A5aAb3F0c6EeadF30Ef1D3Eb997108E976351feB");

// Uniswap v3 (mainnet only). WETH is the ETH side of v3 pools.
pub const V3_FACTORY: Address = address!("1f7d7550b1b028f7571e69a784071f0205fd2efa");
pub const SWAP_ROUTER_02: Address = address!("caf681a66d020601342297493863e78c959e5cb2");
pub const WETH: Address = address!("0Bd7D308f8E1639FAb988df18A8011f41EAcAD73");
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
pub const SETTLE_ALL: u8 = 0x0c;
pub const TAKE_ALL: u8 = 0x0f;
pub const MINT_POSITION: u8 = 0x02;
pub const SETTLE_PAIR: u8 = 0x0d;
pub const SWEEP: u8 = 0x14;
pub const BURN_POSITION: u8 = 0x03;
pub const TAKE_PAIR: u8 = 0x11;
