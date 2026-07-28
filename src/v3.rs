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
        params: params(WETH, token, fee, recipient, amount_in, min_out),
    }
    .abi_encode()
    .into()
}

/// SELL: token->WETH into the router, then unwrapWETH9 to send native ETH to us.
/// Requires the token to be approved to SWAP_ROUTER_02 first.
pub fn v3_sell_calldata(token: Address, fee: u32, amount_in: u128, min_out: u128, recipient: Address) -> Bytes {
    let swap = ISwapRouter02::exactInputSingleCall {
        params: params(token, WETH, fee, ADDRESS_THIS, amount_in, min_out),
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
