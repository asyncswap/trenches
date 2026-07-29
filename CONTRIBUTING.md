# Contributing

Trenches is AGPL-3.0-only. Contributions come in under the same license — there
is no CLA, and no copyright assignment. You keep your copyright; you grant
everyone the AGPL rights to what you wrote.

## Sign off your commits

Every commit needs a `Signed-off-by` line:

```
Signed-off-by: Your Name <you@example.com>
```

`git commit -s` adds it from your git config. To make it automatic:

```sh
git config --global user.name  "Your Name"
git config --global user.email "you@example.com"
git config --global format.signOff true
```

Forgot on the last commit:

```sh
git commit --amend -s --no-edit
```

Or across a branch:

```sh
git rebase --signoff main
```

The sign-off is the [Developer Certificate of Origin](https://developercertificate.org)
(DCO) 1.1. It is a statement, not a formality — you are asserting that you wrote
the change, or have the right to submit it, and that you are content for it to
ship under this project's license. Use your real name and an address that reaches
you.

We use a DCO rather than a CLA deliberately. A CLA asks you to hand us rights we
would not need; the DCO only asks you to confirm what you are already entitled to
give.

## SPDX headers

Every source file starts with:

```rust
// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
```

Keep it on files you edit, add it to files you create. If you are contributing
something substantial and want your own copyright line, add it beneath — do not
replace the existing one.

## What lands

A few things this codebase is strict about, because they are the ones that cost
real money when they are wrong.

**Nothing auto-trades.** Every order originates in a keypress: `b`, `s`, `x`.
No timer, no strategy, no background loop may open or close a position. Copy
modes pre-fill a size and stop there. A change that trades on its own will be
rejected however good it is.

**Decimals are load-bearing.** SOL has 9, pump.fun coins have 6, ERC-20s vary.
Getting this wrong is the classic way to send 1000× the intended size. New
conversion paths need tests, not care.

**No secrets in config.** There is no field for a seed phrase and there will not
be one. Keys live in password-encrypted keystores. Anything that logs a key, a
password or a full endpoint URL is a bug.

**Real values only.** If you do not know a constant — a program address, a chain
id, an account layout — find it in the official IDL or ABI, or leave it out. A
plausible-looking invented value is worse than a missing one, because it will be
believed.

## Before you open a PR

```sh
cargo fmt
cargo clippy --all-targets --features solana
cargo test  --release --features solana
```

The release profile is what ships and what you should test against.

## Reporting a security issue

Do not open a public issue. Email <meek.dev3@gmail.com> with what you found
and how to reproduce it.

## Reporting a bug

The session log in `~/.trenches/` is the single most useful attachment. It
records addresses and amounts, never keys or passwords — skim it anyway before
posting.

<https://github.com/asyncswap/trenches/issues>
