# License

Trenches
Copyright (C) 2026 AsyncSwap Labs, Inc.

Licensed under the **GNU Affero General Public License v3.0** (AGPL-3.0-only).
The full text ships with every release as `LICENSE`, and is at
<https://www.gnu.org/licenses/agpl-3.0.txt>.

Official repository: <https://github.com/asyncswap/trenches>

## What you may do

- **Use it** — for anything, including trading your own money commercially
- **Read it** — the implementation is public, all of it
- **Modify it** — fork it, rewrite it, strip it for parts
- **Redistribute it** — as long as it stays under the same license
- **Run it yourself** — your own RPC, your own keys, no account, nothing
  phoning home

## What the AGPL asks in return

If you modify Trenches and offer it to other people **as a network service**, you
have to make your modified source available to those users. That is the one
clause the AGPL adds over the GPL, and it is the reason for choosing it: a hosted
fork should give its improvements back rather than take them private.

Running it on your own machine, for yourself, triggers none of this. Modifying it
privately and never handing it to anyone triggers none of this either.

## Why AGPL

Trading infrastructure should be readable by the people whose money it moves. You
should be able to see exactly what gets signed, what gets sent, and where. That
is not a promise worth making about closed code, because nobody can check it.

It also settles the bring-your-own-RPC question honestly. There is no vendor lock
-in here: point it at whichever endpoint you want, and nothing in the software
prefers one over another.

## Commercial services

Services built *around* Trenches — hosted RPC endpoints, analytics, AI models —
are independent services that talk to the bot over a network API. They carry
their own licenses and are not covered by this one.

The distinction that matters: the AGPL covers **the bot** — this source and any
modified version of it. It does not reach across an API boundary to cover a
separate service on the other side. A fork of the client is covered. A backend it
calls is not.

## Trademarks

"Trenches", "AsyncSwap" and trenches.sh are trademarks of AsyncSwap Labs, Inc., held
separately from the code license. The AGPL grants rights in the software; it
grants none in the name.

Fork it freely, and say your fork is based on Trenches. Do not ship it *as*
Trenches, or imply AsyncSwap Labs, Inc. published it.

## Contributing

Contributions come in under the same license, signed off under the Developer
Certificate of Origin. No CLA and no copyright assignment — you keep your
copyright. See `CONTRIBUTING.md`.

## Third-party notices

This bot builds on other people's work, each under its own license. Their terms
apply to their code, not to this. Run `cargo tree` for the full list, and check
licenses before redistributing binaries.

Brand marks shown in the interface — Uniswap, Solana, pump.fun, Robinhood,
pons.family, Flaunch — belong to their respective owners and are used here only to
identify the venue being traded.

## No warranty

Section 15, in plain terms: this software comes with none. It is beta software
that signs real transactions against real chains with real money. Trenches is a
tool, not advice. What you trade with it is your decision and your risk.
