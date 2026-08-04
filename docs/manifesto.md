# MANIFESTO

The list is incomplete on purpose. It grows when we find a rule we have been
following without saying so, or one we should have been.

## 1. Trade One Token pool token at a time

## 2. No infinite approvals, only one exception

Every approval is the exact amount the trade needs. One is not.

**The exception:** the ERC-20 allowance to Permit2, at
`0x000000000022D473030F116dDEE9F6B43aC78BA3`. That one is `U256::MAX`.

Permit2 cannot move tokens with it alone. It needs a grant naming the spender,
the amount, and an expiry. Ours are exact, and expire in 24 hours. Set
`permit2_expiry` in the config — hours, 1 to 8760.

An expired grant is renewed by the next trade, so an exit is never blocked.

When a sell empties the position, the allowance goes to zero.

## 3. No keypress, No execution

## 4. Only password protected wallet and account management is allowed
