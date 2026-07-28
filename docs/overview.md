# OVERVIEW

The screens, and the two numbers that decide whether a trade is a good idea.

What each venue actually lets you do, since it is not the same everywhere:

- **Uniswap V3** — trade only
- **Uniswap V4** — trade, and add or remove liquidity
- **pons.family** — launch discovery, and graduations into Uniswap
- **pump.fun** — bonding curves, live launches
- **PumpSwap AMM** — graduated coins

## The screens

**Chain picker** — pick the network. Mainnets first, then test and local.

**Dashboard** — the main screen.

- *Header* — venue in large type, then the chain, latency and block height.
- *Wallet* — balances, cost basis, realized and unrealized PnL.
- *Pool* — the pair, price, market cap, pooled liquidity and age.
- *Settings* — every adjustable knob, and the most recent message.
- *Trades / Orders / Logs* — cycle with `l`.

**Trenches** (`f`) — live launches as they happen. Enter to trade one.

## Keys

The **Shortcuts** page lists every binding in full, including the shifted variants.
The essentials:

Sizing and protection:

```
[ ]   buy size
( )   sell size
{ }   slippage
< >   price impact cap (EVM) / priority fee (Solana)
```

Trading:

```
b     buy
s     sell the configured slice
x     sell everything
```

Navigation:

```
f     find a coin or pool
p     change pool
l     cycle Trades / Orders / Logs
T     theme picker
C     change chain
D     these docs
?     shortcuts
q     quit (asks first)
Q     quit immediately
```

## Two numbers worth understanding

**Pooled** is derived from the pool's liquidity and current price, not read as a
balance. On a full-range position the two match. On a **concentrated** position
it is the amount that would be there if liquidity spanned the whole curve, so it
overstates real depth — sometimes past the token's entire supply, which is when
the bot marks it `estimate, above supply` in amber.

Treat an unmarked figure as a good estimate of what you can get out, and a
marked one as an upper bound, not a promise.

**Price impact cap** shrinks a swap so a single trade cannot move the pool price
more than the set percentage. `off` means uncapped: swaps go out at full size.
On a thin pool that is the difference between a fill and a self-inflicted dump.

## Market cap on a fresh launch

A brand-new pump coin reads about 28 SOL of market cap with nothing pooled. That
is not a bug: the bonding curve seeds virtual reserves that set a price before
anyone has bought. Watch **pooled**, not cap, on a fresh launch.
