# SHORTCUTS

Every binding, and what separates a lowercase key from its shifted twin.

The pattern throughout: **lowercase acts, uppercase escalates.** `s` sells a
slice, `S` liquidates everything. `q` asks before quitting, `Q` does not.

## Trading

```
b        buy the configured size
s        sell the configured slice of your balance
x        sell the entire balance of this token
S        liquidate EVERY token holding, across every known pool  (EVM)
```

`x` empties one position. `S` empties the wallet — it sweeps every pool the bot
knows about. It is the only key that can touch tokens you are not currently
looking at.

## Sizing and protection

```
[  ]     buy size          smaller / larger
(  )     sell size         smaller / larger, in 10% steps
{  }     slippage          looser / tighter
<  >     price impact cap  (EVM)  ·  priority fee  (Solana)
0        turn the price impact cap off entirely  (EVM)
```

**Slippage** is how far the fill may drift from the quote before the trade
reverts. **Price impact** is different: it shrinks the trade itself so a single
swap cannot move the pool price more than the set percentage. `off` means
uncapped — full-size swaps.

## Finding something to trade

```
f        live trenches — Pons + Flaunch launches as they happen
F        verified pools — the static, curated list          (EVM)
k        top tokens — leaderboard and big-fish scan         (EVM)
p        pools — switch pool, add by address, create new    (EVM)
p        add a token by contract address (CA)                (Solana)
h        wallet holdings — leftover tokens you still hold   (EVM)
Del      deselect the current pool — back to the empty state  (EVM)
```

`f` is the live feed: coins appearing right now. `F` is the hand-curated list
that does not change. `k` is for established tokens, not fresh launches.

Sessions restore where you left off — the last pool or coin reloads on start.
`Del` deselects, and tells the next session to start empty.

## Liquidity

```
a        add liquidity to the current pool     (EVM, V4 only)
r        remove one liquidity position         (EVM)
```

V3 pools are trade-only here; these keys need a V4 pool.

## Arb

```
d        pick a second pool / toggle arb mode  (EVM)
e        execute the arb                       (EVM)
```

## Modes and guards

```
g        profit filter on/off                  (EVM)
n        duplicate-buy guard on/off            (EVM)
```

The profit filter holds back trades that would lose money. With it **off**,
every trade runs, including losing ones.

## View

```
t        trades — the live tape
o        orders — your own actions
l        logs — the raw session log
c or v        candlestick chart
,  .     candle interval  smaller / larger
O  → ←   cycle through the panels
L        PnL calendar — a month of trading at a time
R        refresh everything — market, tape, prices, metadata  (EVM)
T        theme picker
D        these docs
e        set RPC and API keys   (while the docs are open)
?        shortcuts overlay
```

`L` opens the calendar: one cell per day, what it made, and the trades behind
it. `h j k l` move a day, arrows change month, `1` `2` `3` switch the 1/7/30-day
summary, `t` jumps to today.

## Mouse

```
wheel        scroll the active panel, or the docs
click+drag   select text; release copies it to the clipboard
```

Selection works everywhere — dashboards, the finder, these docs. It reads
straight off the screen, so what you copy is exactly what you see.

## Leaving

```
W        change wallet — back to the account list, same chain
C        change chain — back to the picker, without restarting
q        quit, after a yes/no prompt
Q        quit immediately, no prompt
esc      go back one screen (on the trading screen it asks, like q)
```

## Inside lists and tables

```
↑ ↓      or j / k    move
enter    select
h j k l  or tab      switch document  (docs)
↑ ↓ PgUp PgDn        scroll           (docs)
home end             jump to top / bottom
esc      back
```
