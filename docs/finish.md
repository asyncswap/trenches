# FINISH

That is the tour. Press **enter** to pick a chain and start trading.

## Before you do

**An account.** Press `W` from this page if you have not made one. It writes an
encrypted keystore and asks for a password — nothing leaves your machine.

**An endpoint.** Press `e` to paste your RPC URL. The defaults are public and
rate limited; they work, they are just slow when it matters. Discovery — new
pools, buyer charts — stays quiet until you set `discovery_rpc`.

**A size you would shrug at.** This is beta software signing real transactions
against real chains. Start small enough that a bug is an annoyance rather than a
loss.

## The three keys that matter

```
b     buy the configured size
s     sell a slice
x     sell the lot        (Solana)  ·  close all LP positions  (EVM)
```

Everything else moves you around. Those three spend money.

Nothing else does. No timer, no strategy, no background loop opens a position
while you are away from the keyboard — the copy modes only pre-fill a size, and
you still press the key.

## When something goes wrong

`?` lists every binding. `D` brings these docs back, from anywhere.

Your session log is in `~/.trenches/`, and it is the single most useful thing to
attach to a bug report. It records addresses and amounts, never keys or
passwords. Skim it anyway.

**https://github.com/asyncswap/trenches/issues**
