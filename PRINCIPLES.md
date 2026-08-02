# Principles

Rules this codebase holds itself to, why each one exists, and where it is
enforced. Written down because a principle nobody can point at is a preference,
and preferences lose arguments to convenience.

This list is incomplete on purpose. It grows when we find a rule we have been
following without saying, or one we should have been.

---

## 1. No key acts on a token other than the one on screen

A trading screen shows one pool, one position, one price. Any key pressed there
acts on **that** coin and nothing else. Nothing iterates over the wallet's
holdings and trades them.

**Why.** What a screen shows is what a person believes they are acting on. A key
that sweeps every token in the wallet from a screen displaying one of them is
not a shortcut, it is a surprise — and the surprise costs money in a direction
that cannot be undone. It also makes the blast radius of a mistyped key
unbounded: one wrong press should cost you a position, not a portfolio.

**What this rules out.** Sell-everything keys, batch approvals across holdings,
"close all" from a screen that is not a positions screen. Selling something you
are not looking at means going to it first — `h` lists what you hold, and each
one is entered deliberately.

**Where enforced.** No loop in `src/` calls a signing path. `pre_approve_exit`
(`src/engine.rs`) touches `self.pool.token` only. A wallet-wide sweep key
existed and was removed for this reason; `close_all` survives behind the
`liquidity` feature with no caller, waiting for a positions screen in V2 where
closing positions is what the screen is about.

---

## 2. Nothing signs without a keypress

No timer, no strategy, no background loop opens or closes a position. Every
transaction traces back to a key a person pressed.

**Why.** It is the whole safety model. Anything else means the app can lose
money while nobody is watching, and no amount of care elsewhere compensates for
that.

**Where enforced.** Every signing call sits in a `KeyCode` match arm. Background
tasks read — market state, tape, prices, launches — and never write.

---

## 3. Confirm against the number the chain enforces

Where an action quotes an outcome, the confirmation shows the guaranteed figure,
not the hoped-for one.

**Why.** An estimate is what a route expects at the instant it was asked. By the
time the transaction lands the pools have moved. Confirming against the estimate
is agreeing to a number nobody can be held to.

**Where enforced.** `Quote::min_out` (`src/sol/jupiter.rs`) carries the on-chain
threshold and the swap confirmation quotes it alongside the estimate, saying
which is which.

---

## 4. Irreversible actions state the whole thing before they happen

A transfer has no counterparty and no appeal. Before one is signed the
confirmation spells out the full destination address, the amount, and any cost
the user would not otherwise see.

**Why.** An abbreviated address is exactly as reassuring for the right one as for
an attacker's lookalike. The moment before an irreversible action is the moment
that distinction is worth the screen width.

**Where enforced.** `Plan::sentence` (`src/sol/send.rs`) prints the full
destination and flags the rent cost of creating a recipient's token account.
`check_destination` refuses the mint, this wallet, and program accounts outright
rather than warning about them.

---

## 5. One source of truth, or a test that one exists

Where the same fact appears in more than one place, either it is generated from
a single file or a test fails when the copies disagree.

**Why.** Copies do not drift because anyone was careless. They drift because
keeping several lists in step by remembering to is not a thing people can do.
The shortcuts were the proof: four hand-maintained lists, and the marketing site
was advertising a key the app no longer had.

**Where enforced.** `shortcuts.json` is the only place a binding is written
down; the in-app help and the Shortcuts docs page render from it at runtime, and
tests fail if `docs/shortcuts.md` or the website's copy has drifted
(`src/shortcuts.rs`). A pre-commit hook regenerates them.

---

## 6. Secrets never reach the screen or the log

API keys, passwords and private keys are not printed, not logged, and not
written to any file the user is likely to share.

**Why.** Session logs get attached to bug reports and screenshots get posted.
Anything that can be pasted will be.

**Where enforced.** URLs are stripped of their query string before printing.
Session logs record addresses and amounts only. There is no config field for a
seed phrase, deliberately — a config file gets backed up, synced and pasted, and
none of that should be able to cost someone their funds.

---

## 7. Scope the release to what can be reviewed

Code that is not part of a release is compiled out of it, not merely left
unreachable.

**Why.** "That is out of scope, please ignore it" is a promise. "That is not in
the binary" is checkable. For anything security-relevant the second is worth
more, and a reviewer should never have to work out whether a path can be
reached.

**Where enforced.** Cargo features: `liquidity` (providing liquidity — V2, with
launching), `agent` (the in-app copilot), `testnet` (chains nobody trades).
All three are off in the release build.
