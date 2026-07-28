<!--
  This is the README for the PUBLIC repo (github.com/asyncswap/trenches).
  Copy it there.

  That repo holds the source, tagged releases with binaries attached, this README
  and the issue tracker.
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

All of it is here. Trenches is licensed **AGPL-3.0-only** — read it, fork it,
modify it, run it yourself.

Money moves through this software, so you should be able to see exactly what it
signs and where it sends it. That is not a claim worth making about code nobody
can read.

Build it yourself:

```sh
git clone https://github.com/asyncswap/trenches
cd trenches
cargo build --release --features solana
```

Patches welcome — see [CONTRIBUTING.md](CONTRIBUTING.md). Commits are signed off
under the DCO; there is no CLA and no copyright assignment.

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

**AGPL-3.0-only.** Copyright (C) 2026 AsyncSwap Labs. Full text in
[LICENSE](LICENSE).

Use it, modify it, redistribute it, run it yourself. If you modify Trenches and
offer it to other people as a network service, the AGPL asks you to publish your
changes to those users — a hosted fork gives its improvements back rather than
taking them private. Running it on your own machine asks nothing of you.

Services built *around* Trenches — hosted RPCs, analytics, AI models — are
independent services reached over an API. They carry their own licenses. The AGPL
covers the bot, not what is on the other side of a network call.

"Trenches" and trenches.sh are trademarks, held separately from the code license.
Fork it and say so; do not ship your fork *as* Trenches.

Trenches is a tool, not advice. What you trade with it is your decision and your
risk.
