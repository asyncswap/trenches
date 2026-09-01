# OVERVIEW

The screens, and the two numbers that decide whether a trade is a good idea.

What each venue actually lets you do, since it is not the same everywhere:

- **Uniswap V3** — trade only
- **Uniswap V4** — trade
- **pons.family** — launch discovery, and graduations into Uniswap
- **Flaunch** — on Base. Launch discovery, and trading via Uniswap V4 (fees
  charged by the Flaunch hook)
- **pools.fun** — launch discovery, and trading on SushiSwap V3. Every launch
  opens a 1% pool paired against WETH or USDG, and the launched token is
  always token0. These pools are NOT reachable through Uniswap's router — a v3
  router is bound to one factory — so trades route through Sushi's own path
- **pump.fun** — bonding curves, live launches
- **PumpSwap AMM** — graduated coins

## The screens

**Chain picker** — pick the network. Mainnets first, then test and local.

**Dashboard** — the main screen.

- *Header* — venue in large type, then the chain, latency and block height.
- *Wallet* — balances, cost basis, realized and unrealized PnL.
- *Pool* — the pair, price, market cap, pooled liquidity and age.
- *Settings* — every adjustable knob, and the most recent message.
- *Trades / Orders / Logs / Chart* — `t` `o` `l` `c` jump straight to one,
  `O` cycles.

**Trenches** (`f`) — live launches as they happen. Enter to trade one.

## Keys

The **Shortcuts** page lists every binding in full, including the shifted variants.
The essentials:

Sizing and protection:

```
[ ]   buy size
( )   sell size
{ }   slippage
< >   price impact cap (EVM)  ·  priority fee (Solana)
```

Trading:

```
b     buy
s     sell the configured slice
x     sell everything — the coin on screen
M     move funds out to another address
u     swap USDC and SOL (Solana)
```

Navigation:

```
f     find a coin or pool
p     add a pool by address
O     cycle the panels
T     theme picker
C     change chain
D     these docs
?     shortcuts
q     quit (asks first)
Q     quit immediately
```

## Two numbers worth understanding

**Pooled** is the exit liquidity — a good estimate of what you can get out.
On concentrated positions it can overstate real depth; when it does, the bot
marks it `estimate, above supply` in amber. Treat a marked figure as an upper
bound, not a promise.

**Price impact cap** shrinks a swap so a single trade cannot move the pool price
more than the set percentage. `off` means uncapped: swaps go out at full size.
On a thin pool that is the difference between a fill and a self-inflicted dump.

## Market cap on a fresh launch

A brand-new pump coin reads about 28 SOL of market cap with nothing pooled. That
is not a bug: the bonding curve seeds virtual reserves that set a price before
anyone has bought. Watch **pooled**, not cap, on a fresh launch.
