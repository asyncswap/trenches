# CONFIG

One file, one directory:

```
~/.config/trenches/config.json    config — networks, RPC URLs, API keys
~/.trenches/                      state — logs, theme, token history, PnL
```

Config is what you decide. State is what the app works out and can work out
again — delete `~/.trenches` and you lose history, nothing else.

The installer writes a starter config that works as-is on public endpoints.
`trenches --init` writes one too, prints both paths, and never overwrites an
existing file.

Two environment variables move things, if you want them moved:
`$TRENCHES_CONFIG` points at a different config file, `$TRENCHES_STATE` at a
different state directory — useful for a second profile.

## RPC endpoints

Two fields per network. Each takes one URL or a list — a list forms a pool,
and every request goes to whichever endpoint answers fastest.

**`rpc`** — where requests go. **`ws`** — websockets, Solana only, for the
live launch feed.

The app is free, and the endpoints are your choice. Bring your own RPC from
any provider, or use a [Trenches RPC](https://rpc.trenches.sh) key — free at
10 requests a second, or the paid plan for uncapped rates. One key covers
every chain and the websocket in one URL:

```json
{
  "networks": [
    {
      "name": "solana-mainnet",
      "kind": "solana",
      "rpc": "https://rpc.trenches.sh/YOUR_KEY/solana",
      "ws": "wss://rpc.trenches.sh/YOUR_KEY/solana-ws"
    },
    {
      "name": "robinhood-mainnet",
      "chain_id": 4663,
      "rpc": "https://rpc.trenches.sh/YOUR_KEY/robinhood"
    }
  ]
}
```

Provider URLs from Alchemy, Helius, QuickNode or Ankr work the same way, alone
or mixed into the list. Restart the app after editing.

Or skip the editor: press **D** for docs, then **e** — the app asks for each
endpoint in turn and writes the config itself.

Every config carries a `$schema` line; any editor that understands JSON Schema
validates fields and completes names as you type.

## Adding a chain

Anything in `networks` appears in the chain picker. Any EVM chain is three
fields:

```json
{
  "name": "anvil-local",
  "chain_id": 31337,
  "rpc": "http://127.0.0.1:8545"
}
```

The name drives the label — `robinhood-mainnet` shows as **Robinhood Chain**.
To hide a chain, delete its entry.

## No seed phrases

There is no field for one, deliberately. A config file gets backed up, synced,
and pasted into bug reports — none of that should be able to cost you funds.
Accounts are password-encrypted keystores; make or import them in the app
with **W**.

## Keys stay off screen

The config holds API keys; screenshots and logs should not leak them. URLs are
stripped of their query string before anything is printed, and session logs
never record keys, passwords or private keys.
