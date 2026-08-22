// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Uniswap v3 swap calldata via SwapRouter02. v3 pairs WETH (not native ETH):
//! - BUY (ETH->token): send native ETH as value; the router wraps to WETH.
//! - SELL (token->ETH): multicall(exactInputSingle -> router, unwrapWETH9 -> us).

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol_types::SolCall;

use crate::contracts::*;

fn params(
    token_in: Address,
    token_out: Address,
    fee: u32,
    recipient: Address,
    amount_in: u128,
    min_out: u128,
) -> V3ExactInputSingleParams {
    V3ExactInputSingleParams {
        tokenIn: token_in,
        tokenOut: token_out,
        fee: fee.try_into().unwrap(),
        recipient,
        amountIn: U256::from(amount_in),
        amountOutMinimum: U256::from(min_out),
        sqrtPriceLimitX96: alloy::primitives::aliases::U160::ZERO,
    }
}

/// BUY: WETH->token in one call; caller sends `amount_in` as tx value (native
/// ETH), which SwapRouter02 wraps. Output token goes straight to `recipient`.
pub fn v3_buy_calldata(token: Address, fee: u32, amount_in: u128, min_out: u128, recipient: Address) -> Bytes {
    ISwapRouter02::exactInputSingleCall {
        params: params(weth(), token, fee, recipient, amount_in, min_out),
    }
    .abi_encode()
    .into()
}

/// SELL: token->WETH into the router, then unwrapWETH9 to send native ETH to us.
/// Requires the token to be approved to SWAP_ROUTER_02 first.
pub fn v3_sell_calldata(token: Address, fee: u32, amount_in: u128, min_out: u128, recipient: Address) -> Bytes {
    let swap = ISwapRouter02::exactInputSingleCall {
        params: params(token, weth(), fee, ADDRESS_THIS, amount_in, min_out),
    }
    .abi_encode();
    let unwrap = ISwapRouter02::unwrapWETH9Call {
        amountMinimum: U256::from(min_out),
        recipient,
    }
    .abi_encode();
    ISwapRouter02::multicallCall {
        data: vec![swap.into(), unwrap.into()],
    }
    .abi_encode()
    .into()
}

/// BUY on a v3-shaped router whose quote side is `quote_in` — for PancakeSwap,
/// whose SmartRouter takes SwapRouter02's params. Native quote: the caller
/// sends `amount_in` as value and the router wraps it, exactly as
/// [`v3_buy_calldata`]. ERC-20 quote: a plain pull the router does with
/// `transferFrom`, so the quote must be approved to it and no value is sent.
pub fn v3_buy_calldata_via(
    quote_in: Address,
    token: Address,
    fee: u32,
    amount_in: u128,
    min_out: u128,
    recipient: Address,
    _native: bool,
) -> Bytes {
    // Wrapped or not, tokenIn is the quote's ERC-20 address: SwapRouter02
    // and its forks pay from msg.value when it covers the amount, else pull.
    ISwapRouter02::exactInputSingleCall {
        params: params(quote_in, token, fee, recipient, amount_in, min_out),
    }
    .abi_encode()
    .into()
}

/// SELL on a v3-shaped router into `quote_out`. Native quote: swap into the
/// router then `unwrapWETH9` to us, as [`v3_sell_calldata`]. ERC-20 quote:
/// one hop straight to us — an unwrap of a stablecoin would revert.
pub fn v3_sell_calldata_via(
    token: Address,
    quote_out: Address,
    fee: u32,
    amount_in: u128,
    min_out: u128,
    recipient: Address,
    native: bool,
) -> Bytes {
    if !native {
        return ISwapRouter02::exactInputSingleCall {
            params: params(token, quote_out, fee, recipient, amount_in, min_out),
        }
        .abi_encode()
        .into();
    }
    let swap = ISwapRouter02::exactInputSingleCall {
        params: params(token, quote_out, fee, ADDRESS_THIS, amount_in, min_out),
    }
    .abi_encode();
    let unwrap = ISwapRouter02::unwrapWETH9Call {
        amountMinimum: U256::from(min_out),
        recipient,
    }
    .abi_encode();
    ISwapRouter02::multicallCall {
        data: vec![swap.into(), unwrap.into()],
    }
    .abi_encode()
    .into()
}

#[cfg(test)]
mod via_tests {
    use super::*;
    use alloy::primitives::address;

    const TOKEN: Address = address!("7336A64A92BB7a6D8672F47187fCFE2b2Bf17777");
    const WBNB: Address = address!("bb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c");
    const ME: Address = address!("10e951fa67b511d044803c7757da445ddf646f6d");

    /// A native-quoted Pancake sell is byte-identical in SHAPE to a Uniswap
    /// SwapRouter02 sell: multicall(exactInputSingle → router, unwrap → us).
    #[test]
    fn native_sell_is_a_multicall_with_unwrap() {
        let d = v3_sell_calldata_via(TOKEN, WBNB, 2500, 1_000, 900, ME, true);
        assert_eq!(&d[..4], &ISwapRouter02::multicallCall::SELECTOR);
        let inner = ISwapRouter02::multicallCall::abi_decode(&d, true).unwrap().data;
        assert_eq!(inner.len(), 2);
        assert_eq!(&inner[0][..4], &ISwapRouter02::exactInputSingleCall::SELECTOR);
        assert_eq!(&inner[1][..4], &ISwapRouter02::unwrapWETH9Call::SELECTOR);
        let p = ISwapRouter02::exactInputSingleCall::abi_decode(&inner[0], true).unwrap().params;
        assert_eq!(p.recipient, ADDRESS_THIS, "the swap lands in the router, the unwrap sends it on");
        assert_eq!(p.tokenOut, WBNB);
        assert_eq!(p.fee, alloy::primitives::aliases::U24::from(2500u32));
    }

    /// An ERC-20-quoted sell is ONE hop straight to us — no unwrap of a
    /// stablecoin, which would revert.
    #[test]
    fn stable_sell_is_a_single_hop_to_us() {
        let usd1 = address!("8d0D000Ee44948FC98c9B98A4FA4921476f08B0d");
        let d = v3_sell_calldata_via(TOKEN, usd1, 500, 1_000, 900, ME, false);
        assert_eq!(&d[..4], &ISwapRouter02::exactInputSingleCall::SELECTOR);
        let p = ISwapRouter02::exactInputSingleCall::abi_decode(&d, true).unwrap().params;
        assert_eq!(p.recipient, ME);
        assert_eq!(p.tokenOut, usd1);
    }

    /// A buy names the quote's ERC-20 as tokenIn whether or not value is
    /// sent: the router pays from msg.value when it covers the amount.
    #[test]
    fn buy_names_the_quote_as_token_in() {
        let d = v3_buy_calldata_via(WBNB, TOKEN, 2500, 5_000, 1, ME, true);
        let p = ISwapRouter02::exactInputSingleCall::abi_decode(&d, true).unwrap().params;
        assert_eq!(p.tokenIn, WBNB);
        assert_eq!(p.tokenOut, TOKEN);
        assert_eq!(p.recipient, ME);
    }
}
