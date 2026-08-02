# MANIFESTO

What this bot promises, and how each promise is kept.

These are not aspirations. Each one names the code that holds it, so you can
check rather than trust — and so can anyone reviewing this.

The list is incomplete on purpose. It grows when we find a rule we have been
following without saying so, or one we should have been.

## 1. One token at a time

Every key acts on the token you are looking at. One token, in one pool — or
across several pools of **that same token**, which is what an arb is. Never a
different token. Nothing walks your holdings and trades them.

What a screen shows is what a person believes they are acting on. A key that
sells every token in your wallet from a screen displaying one of them is not a
shortcut, it is a surprise, and it costs money in a direction nobody can undo.
One wrong press should cost you a position, not a portfolio.

So there is no sell-everything key. Selling something you are not looking at
means going to it first — `h` lists what you hold, and each one is entered
deliberately.

*Kept by:* no loop anywhere in the source reaches a signing path. `e` is the
only key touching more than one pool, and both pools hold the same token.

## 2. No unlimited approvals, with one named exception

An approval is permission to move your tokens. This bot asks for what the trade
in front of you needs, and no more.

The convenient thing is to approve the maximum once and never think about it
again. It means the approved contract can move your whole balance of that token
forever, long after you have forgotten the trade that granted it — and stale
approvals are one of the most common ways people lose funds without ever signing
anything that looked wrong.

Every approval here passes a computed amount, except one.

**The exception: the ERC-20 allowance to Permit2.** Permit2 cannot move anything
on its own. It moves tokens only where it holds a grant naming the spender, the
amount and an expiry — and this bot's grants are the exact amount, expiring a
day out. That grant is the bound.

Bounding this leg too would cost an approval before every single trade, because
an exact grant is consumed by the trade that uses it: an exit would be three
transactions instead of two. In a market where getting out is measured in
seconds, that is not a safety improvement. It swaps a narrow risk for a broader
one.

**The allowance is given back.** When a sell empties your position, the bot
revokes the Permit2 allowance for that token. Not because the balance is zero —
zero is not a durable fact. Tokens arrive: an airdrop, a transfer from another
wallet, a buy somewhere else. An unlimited allowance nobody remembers granting,
sitting over funds that were never approved for anything, is exactly the failure
this promise is about.

It happens after the exit has confirmed, so it costs nothing that matters — the
money is already out. Bounding the allowance up front would instead put a
transaction in front of the sell, which is the one place a delay is expensive.

*Kept by:* every `approve` in `src/engine.rs` passes a computed amount except
the two Permit2 legs, which are commented as this exception.
`revoke_permit2_if_empty` runs on the confirmed sell that empties the position.
Grants carry a 24-hour expiry by default — that used to read as the year 2100,
which was the real defect here. It is yours to set: `permit2_hours` in the
config, clamped to between an hour and a year, because the tradeoff between
approving often and leaving a permission standing is a judgement about your own
risk, not one this bot should make silently on your behalf.

## 3. Nothing signs without a keypress

No timer, no strategy, no background loop opens or closes a position. Every
transaction traces back to a key a person pressed.

Background work reads — market state, the tape, prices, launches. It never
writes.

*Kept by:* every signing call sits in a `KeyCode` match arm.

## 4. You confirm against the number the chain enforces

Where an action quotes an outcome, the confirmation shows the guaranteed figure,
not the hoped-for one.

An estimate is what a route expects at the instant it was asked. By the time the
transaction lands, the pools have moved. Confirming against an estimate is
agreeing to a number nobody can be held to.

*Kept by:* `Quote::min_out` in `src/sol/jupiter.rs` carries the on-chain
threshold, and the swap confirmation shows it beside the estimate, saying which
is which.

## 5. Irreversible actions state the whole thing first

A transfer has no counterparty and no appeal. Before one is signed you see the
full destination address, the amount, and any cost you would not otherwise know
about.

An abbreviated address is exactly as reassuring for the right one as for an
attacker's lookalike. The moment before an irreversible action is the moment
that distinction is worth the screen width.

*Kept by:* `Plan::sentence` in `src/sol/send.rs` prints the full destination and
flags the rent cost of creating a recipient's account. `check_destination`
refuses the token's own mint, your own address, and program accounts outright.

## 6. Your keys and secrets never leave your machine

Signing happens locally. API keys, passwords and private keys are never printed,
never logged, and never written anywhere you are likely to share.

Session logs get attached to bug reports and screenshots get posted. Anything
that can be pasted will be.

*Kept by:* URLs are stripped of their query string before printing. Logs record
addresses and amounts only. There is no config field for a seed phrase,
deliberately — a config file gets backed up, synced and pasted, and none of that
should be able to cost you funds.

## 7. One source of truth, or a test that one exists

Where the same fact appears in more than one place, either it is generated from
a single file or a test fails when the copies disagree.

Copies do not drift because anyone was careless. They drift because keeping
several lists in step by remembering to is not a thing people can do. The
shortcuts proved it: four hand-maintained lists, and the website was advertising
a key the app no longer had.

*Kept by:* `shortcuts.json` is the only place a binding is written down. The
in-app help and the Shortcuts page render from it; tests fail if the docs or the
website copy have drifted.

## 8. What ships is what was reviewed

Code that is not part of a release is compiled out of it, not merely left
unreachable.

"That is out of scope, please ignore it" is a promise. "That is not in the
binary" is checkable. For anything that can move money, the second is worth
more, and a reviewer should never have to work out whether a path is reachable.

*Kept by:* Cargo features. `liquidity` (providing liquidity — returns in V2
alongside token launching), `agent` (the in-app copilot), `testnet` (chains
nobody trades). All three are off in the release build.
