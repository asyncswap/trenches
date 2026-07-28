# CONFIG

Where the bot's settings and state live, and how to set an API key.

## Two places

```
~/.config/trenches/config.json    config — networks, RPC URLs, API keys
~/.trenches/                      cache and state — logs, theme, tokens, PnL
```

The split matters. Config is what you decide and would carry to another machine.
Cache is what the bot worked out and can work out again — delete `~/.trenches`
and you lose history, nothing else.

The bot never writes to your config. Coins you add by contract address go to
`~/.trenches/tokens-<chain_id>.json`, not into the file you edit, so an RPC URL
you set by hand does not end up buried under three hundred tokens you never
typed.

On first run the bot writes a starter `config.json` and tells you where. It works
as-is on public endpoints.

**Other locations, in the order they are checked:**

1. `$TRENCHES_CONFIG` — a full path, for running more than one profile
2. `./config.json` or `./deployments.json` in the current directory, if present
3. `~/.config/trenches/config.json`

`deployments.json` is the old name and is still read. It described a list of
contracts we had deployed; the file holds endpoints and keys now, so the name
went.

## Setting an API key

Open `~/.config/trenches/config.json` in any editor. Two fields matter.

**`rpc`** — the endpoint every trade goes through. This is the one that decides
how fast you are. The defaults are public and rate limited; under real load they
will refuse you.

```json
{
  "name": "solana-mainnet",
  "chain_id": 900,
  "rpc": "https://mainnet.helius-rpc.com/?api-key=YOUR_KEY",
  "ws": "wss://mainnet.helius-rpc.com/?api-key=YOUR_KEY"
}
```

**`discovery_rpc`** — a separate endpoint for log-heavy scans: new pools, buyer
charts, leaderboards. These ask for logs across wide block ranges, which public
endpoints almost always refuse, so discovery stays quiet until you set one.

```json
"discovery_rpc": "https://robinhood-mainnet.g.alchemy.com/v2/YOUR_KEY"
```

Alchemy, Helius, QuickNode and Ankr all work. Worth keeping separate: a scan that
gets rate limited should never be able to slow down a trade.

**`rugcheck.api_key`** — optional. Token risk scoring works without one; the key
only raises the rate limit.

Restart the bot after editing. Nothing here is sent anywhere except to the
endpoints you name.

### Editor help

Every config carries a `$schema` line pointing at
`https://trenches.sh/config.schema.json`. Any editor that understands JSON Schema
will validate the file, complete field names as you type, and tell you what each
one is for.

## Opening on the docs

The app shows these pages on the way in until you have been through them once,
then goes straight to the chain picker. `D` opens them from anywhere regardless.

To decide it outright:

```json
"start_on_docs": true
```

`true` keeps them on every start, `false` never shows them. Leave it out for the
default. The "have they been read" marker is `~/.trenches/onboarded` — delete it
to get them back on start.

## Where it opens

After the first run the bot goes straight to the chain you used last and asks
for that account's password. `C` changes chain, `W` changes account, and `esc`
from the account list steps back to the chain picker.

The chain is remembered in `~/.trenches/last-chain.txt`, by name — an index
would point at a different chain the moment you reordered the file.

## No seed phrases

There is no field for one. Accounts are password-encrypted keystores, shared with
Foundry at `~/.foundry/keystores` — make or import them in the app with **W**.

Deliberate, not an oversight. A config file gets backed up, synced between
machines, and pasted into a bug report by someone trying to be helpful. None of
that should be able to cost you your funds. A phrase written into this file is
ignored, not honoured.

## Adding a chain

Anything in `networks` appears in the chain picker. Robinhood Chain and Solana
ship configured; append your own.

**A local Anvil node:**

```json
{
  "name": "anvil-local",
  "chain_id": 31337,
  "rpc": "http://127.0.0.1:8545"
}
```

Start it with `anvil`, restart the bot, and it is in the list. Any other EVM
chain is the same three fields.

**A Solana endpoint** wants a websocket too, because providers often serve WS on
a different host than HTTP:

```json
{
  "name": "solana-mainnet",
  "chain_id": 900,
  "rpc": "https://your-endpoint/?api-key=…",
  "rpcs": ["https://a-second-endpoint/?api-key=…"],
  "ws": "wss://your-endpoint/?api-key=…"
}
```

`rpcs` is optional — requests fail over across `rpc` plus that list, so one
provider's rate limit does not cap you.

The name drives the label: `robinhood-mainnet` shows as **Robinhood Chain**, and
anything not `-mainnet` is suffixed, so `robinhood-testnet` reads as **Robinhood
Chain (testnet)**. Mainnets sort first.

To hide a chain, delete its entry. There is no separate production list — what is
in your config is what you see.

## Keeping keys off screen

Your config holds API keys, and a screenshot or a pasted log should not leak
them. URLs are stripped of their query string before anything is printed, which
is where providers put the key.

A session log records addresses and amounts but never keys, passwords or private
keys. Skim it anyway before attaching it to anything.
