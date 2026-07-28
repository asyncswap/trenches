# Privacy

**Draft — not reviewed by a lawyer.** Have this reviewed before public release.

## What stays on your machine

Everything by default. There is no account, no telemetry, and no analytics.

- Keys never leave your machine and are never written anywhere by the bot.
- Trading history, cost basis and PnL live only in memory and the local logs.
- Logs are written to `.trenches/` beside the binary: a trading log and a separate
  diagnostic trace.

## What leaves your machine

Only what a trade or a price lookup requires:

- **RPC providers** (Helius, Flux) see your queries and your submitted
  transactions, including your address. That is inherent to using a remote node.
- **RugCheck** receives the mint address of a coin when a risk score is fetched.
- **Blockscout** receives address queries for wallet analytics.
- **A price feed** is queried for the SOL/USD rate.

Each of these is a third party with its own privacy policy. The bot does not
send them anything beyond what the request needs.

## What is on screen

Addresses, balances and transaction signatures are displayed in full so they can
be checked against an explorer. Be aware of that when screen sharing.

Secrets are not printed: API keys are stripped from log lines and error
messages. If you ever see a key on screen or in a log, that is a bug — report it.
