# FINISH

That is the tour. Press **enter** to pick a chain and start trading.

## Before you do

**An account.** Press `W` from this page if you have not made one. It writes an
encrypted keystore and asks for a password — nothing leaves your machine.

**An endpoint.** Press `e` to paste your RPC URL. The defaults are public and
rate limited — fine for looking around, slow when it matters. A
[Trenches RPC](https://rpc.trenches.sh) key or your own provider fixes that.

**A size you would shrug at.** This is beta software signing real transactions
against real chains. Start small enough that a bug is an annoyance rather than a
loss.

## The three keys that matter

```
b     buy the configured size
s     sell a slice
x     sell the lot — the coin you are on
```

Those three are the ones you will reach for. They are not the only keys that
sign: `S` sweeps the wallet — every position closed, every token sold — while
`a` and `r` move liquidity, `e` runs an arb, `M` sends funds out, and `u` swaps
USDC for SOL. Every one of them asks first or acts on a size you set.

What none of them do is act on their own. No timer, no strategy, no background
loop opens a position while you are away from the keyboard. The bot is in
manual mode — the only mode there is.

## When something goes wrong

`?` lists every binding. `D` brings these docs back, from anywhere.

Your session log is in `~/.trenches/`, and it is the single most useful thing to
attach to a bug report. It records addresses and amounts, never keys or
passwords. Skim it anyway.

**https://github.com/asyncswap/trenches/issues**
