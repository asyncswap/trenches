<!--
  This is the README for the PUBLIC releases repo (github.com/asyncswap/trenches),
  not for this source tree. Copy it there; nothing else from this repo goes with it.

  That repo holds three things: tagged releases with binaries attached, this
  README, and the issue tracker. No source.
-->

# Trenches

A terminal for trading memecoins on Robinhood Chain and Solana.

No browser, no wallet pop-up, no clicking through a confirm dialog while the
candle you wanted moves. You press a key and the order goes.

**[trenches.sh](https://trenches.sh)**

---

## Install

macOS and Linux:

```sh
curl -fsSL https://trenches.sh/install | sh
```

That fetches the build for your machine, checks it against the release's
`SHA256SUMS`, and puts `trenches` in `~/.local/bin`.

Windows: download the `.zip` from [Releases](../../releases) and unzip it.

Prefer to do it yourself? Every binary is on the
[releases page](../../releases) with its checksum. Verify before you run it —
that is true of anything you download, and doubly so of something that holds
keys.

### Pick a version

```sh
TRENCHES_VERSION=v0.1.0 curl -fsSL https://trenches.sh/install | sh
TRENCHES_BIN_DIR=/usr/local/bin curl -fsSL https://trenches.sh/install | sh
```

---

## Beta

Trenches is in beta. It signs real transactions against real chains with real
money, and it has bugs we have not found yet.

- Start with an amount you would shrug at.
- Use a fresh wallet, not the one holding everything else.
- Read what a key does before you press it — `?` in the app lists every binding,
  and `D` opens the docs.

Nothing trades on its own. Every order in Trenches comes from a keypress; there
is no automation that opens a position while you are away from the keyboard.

---

## Issues and requests

The tracker here is open and read. Please use it for:

- **Bugs** — what you pressed, what happened, what you expected. Your session
  log lives in `~/.trenches/` and is the single most useful thing to attach.
  Read it first: it records addresses and amounts, though never keys or
  passwords.
- **Chains and venues** you want supported.
- **Anything confusing** — a key that did not do what its name suggested is a
  bug in the app, not in you.

Include your OS, your architecture (`uname -m`), and the version (`trenches
--version`).

### Security

Found something that could cost somebody their funds? Do not open a public
issue. Email **meek.dev3@gmail.com** and we will deal with it before it is
described anywhere.

---

## Source

The source is private. Releases, this README and the tracker are what live here.

We read every issue, and feedback shapes what gets built — that part is genuinely
open even though the code is not.

---

## What it does

- **Robinhood Chain** — Uniswap V3 and V4, and [pons.family](https://pons.family)
  launches
- **Solana** — pump.fun bonding curves and PumpSwap AMM
- **Base, BNB Chain, Hyperliquid** — next
- Keys stay on your machine, in a password-encrypted keystore
- One set of shortcuts, the same on every chain
- A PnL calendar that remembers what each day made

---

## Licence

See the licence bundled with each release, and at
[trenches.sh/license](https://trenches.sh/license).

Trenches is a tool, not advice. What you trade with it is your decision and your
risk.
