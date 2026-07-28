# trenches

Fast memecoin scalper for Robinhood Chain — buys freshly-graduated Pons
launchpad tokens and sells them quickly. Terminal TUI (ratatui), Uniswap v3/v4
via alloy.

## Run (local)

```shell
cargo build --release        # always release; the live bot runs the release binary
./target/release/trenches
```

Then pick **chain → account → pool**. On a keystore account you'll be prompted
for the password (typed, never stored).

- **Config**: `deployments.json` in this directory (networks/RPC, accounts,
  pools), read relative to the working directory. Uses the **raw public RPC** by
  default. Contains a `mnemonics` field — keep it gitignored.
- **Keys**: a Foundry keystore under `~/.foundry/keystores/<name>`, or an HD
  mnemonic in the registry. Never a raw private key.
- **Shortcuts**: the footer shows essentials (`b` buy · `s` sell · `x` sell-all ·
  `p` pool · `l` view · `q` quit); press **`?`** for the full list (`h` holdings,
  `Shift-S` liquidate all, size knobs, toggles, `f` graduation discovery…).

Changes only go live on the **next restart** — a running session never
hot-reloads.

## Running in a datacenter / your own node

Not needed for local use, and free RPCs rate-limit datacenter IPs. If you want
the colocated-latency setup or your own node, see
[docs/run-your-own-node.md](../docs/run-your-own-node.md).

## Roadmap — venues to add

Each of these is a separate venue behind the same dashboard: discovery, tape,
and the `b`/`s`/`x` trade path, wired to that chain's own instruction format.

| Venue | Chain | Reference |
|---|---|---|
| **Four Meme** | BNB Chain | `pnpm add -g @four-meme/four-meme-ai@latest` |
| **Clanker** | Base | launch-and-trade, same shape as Pons |
| **Bankr** | Base | see [launchpad landscape](../src/pons/README.md) |
| **Hyperliquid** | Hyperliquid L1 | [hyperliquid-rust-sdk](https://github.com/hyperliquid-dex/hyperliquid-rust-sdk) |

Notes for whoever picks these up:

- **Hyperliquid is the odd one out** — a central limit order book, not an AMM or
  a bonding curve. Price comes from the book rather than reserves, so the quote,
  slippage and PnL paths all need an order-book variant; it has an official Rust
  SDK, so it needs no hand-rolled instruction encoding.
- **Clanker and Bankr are both Base**, so they share an EVM client and differ
  only in factory and pool wiring — take them together, not separately.
- **Four Meme** ships a CLI/agent package rather than a documented on-chain ABI;
  budget time to derive the contract calls the way the pump.fun path was
  verified (build a known-good trade with the official tooling, then diff our
  encoding against it account by account).
