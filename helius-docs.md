> ## Documentation Index
>
> Fetch the complete documentation index at: <https://www.helius.dev/docs/llms.txt>
> Use this file to discover all available pages before exploring further.

# Helius for Agents

> Everything AI agents need to build on Solana with Helius: programmatic signup, API access, SDKs, MCP integration, and recommended workflows.

Helius provides first-class support for AI agents building on Solana. From programmatic account creation to real-time data streaming, agents can access the full power of Helius without any manual intervention.

* [Helius MCP](/docs/agents/mcp) — 10 routed tools that cover querying the blockchain, sending transactions, streaming, and more
* [Claude Code Plugin](/docs/agents/claude-code-plugin) — The first, and currently the only, official Claude Code plugin from a crypto company. One install: MCP servers + skills + reference files
* [Skills](/docs/agents/skills/overview) — Expert instruction sets for Claude: [Build](/docs/agents/skills/build), [Phantom](/docs/agents/skills/phantom), [Jupiter](/docs/agents/skills/jupiter), [DFlow](/docs/agents/skills/dflow), [OKX](/docs/agents/skills/okx), [SVM](/docs/agents/skills/svm)
* [TypeScript SDK](/docs/agents/typescript-sdk) — Type-safe methods for all Helius APIs
* [Rust SDK](/docs/agents/rust-sdk) — High-performance Rust SDK for Helius APIs
* [Helius CLI](/docs/agents/cli) — Account management and shell scripting

<Note>
  A machine-readable version of this section is available at [agents/llms.txt](https://www.helius.dev/docs/agents/llms.txt) for AI agent consumption.
</Note>

## MCP vs CLI

The [Helius MCP server](/docs/agents/mcp) is the recommended way for AI agents to interact with Helius. It provides 10 routed tools that give AI direct, structured access to Solana — no shell commands, no output parsing, no manual API calls.

|                   | [MCP](/docs/agents/mcp)                                                                                                                                                        | [CLI](/docs/agents/cli)                                                           |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------- |
| **Best for**      | AI agents in Claude Code, Cursor, Claude Desktop, and any MCP-compatible tool                                                                                             | Shell scripts, CI/CD pipelines, terminal workflows                           |
| **Interface**     | Structured tool calls with typed inputs/outputs                                                                                                                           | Command-line with `--json` output                                            |
| **Capabilities**  | 10 routed tools (`heliusWallet`, `heliusAsset`, `heliusTransaction`, …) covering blockchain queries, transactions, webhooks, streaming, wallet analysis, docs, and signup | 95+ commands: same capabilities plus config management and interactive flows |
| **Account setup** | Built-in: `heliusAccount` actions `generateKeypair` → `signup` (link or autopay) — no external tools needed                                                               | `helius keygen` → `helius signup`                                            |
| **When to use**   | Default choice for any AI agent                                                                                                                                           | When you need shell-level automation or are not using an MCP-compatible tool |

<Tip>
  **Start with MCP.** If your AI tool supports MCP (Claude Code, Cursor, Claude Desktop, etc.), use the [MCP server](/docs/agents/mcp) or the [Claude Code Plugin](/docs/agents/claude-code-plugin). The CLI is useful for shell scripting and CI/CD, but for AI-driven workflows the MCP provides a more seamless experience — the AI calls tools directly rather than spawning shell commands and parsing output.
</Tip>

## Quick Start: Agent Signup

Agents can create a Helius account and get an API key in four steps using the [Helius CLI](/docs/agents/cli):

```bash theme={"system"}
npm install -g helius-cli    # Install CLI
helius keygen                 # Generate keypair
# (Autopay only) Fund wallet with 1 USDC + ~0.001 SOL — skip if paying via the hosted link
helius signup --email you@example.com --first-name Jane --last-name Doe --json          # Get API key (JSON output)
```

On success, your agent receives an API key, RPC endpoints, and 1,000,000 credits. See the [full CLI guide](/docs/agents/cli) for details.

## Authentication

All Helius API requests require an API key passed as a query parameter:

```
?api-key=YOUR_API_KEY
```

Append this to any RPC or API endpoint. For example: `https://mainnet.helius-rpc.com/?api-key=YOUR_API_KEY`

Get an API key from the [Helius Dashboard](https://dashboard.helius.dev) or programmatically via the [Helius CLI](/docs/agents/cli).

<Tip>
  **Use Gatekeeper for lower latency** — [Gatekeeper (Beta)](/docs/gatekeeper/overview) removes Cloudflare from the critical path, reducing response times by tens to hundreds of milliseconds. Same API key, same methods — just swap the endpoint:

  ```
  https://beta.helius-rpc.com/?api-key=YOUR_API_KEY
  wss://beta.helius-rpc.com/?api-key=YOUR_API_KEY
  ```

  Supports all RPC, DAS, WebSocket, ZK Compression, Priority Fee, and Enhanced Transaction methods. See the [migration guide](/docs/gatekeeper/migration-guide) for details.
</Tip>

## Helius-Specific API Guidance

Use these Helius-optimized APIs instead of chaining standard Solana RPC methods:

| Instead of...                                | Use this                                                                                                       | Why                                                                              |
| -------------------------------------------- | -------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| `getSignaturesForAddress` + `getTransaction` | [`getTransactionsForAddress`](/docs/rpc/gettransactionsforaddress)                                                  | Single call returns full transaction history with token account data             |
| `getTokenAccountsByOwner`                    | [`getAssetsByOwner`](/docs/api-reference/das/getassetsbyowner) (DAS API)                                            | Returns rich metadata, not just raw accounts                                     |
| `getRecentPrioritizationFees`                | [`getPriorityFeeEstimate`](/docs/api-reference/priority-fee/getpriorityfeeestimate)                                 | Pre-calculated optimal fees, no manual computation                               |
| `getSignaturesForAddress` (for cNFTs)        | [`getSignaturesForAsset`](/docs/api-reference/das/getsignaturesforasset) (DAS API)                                  | Standard RPC doesn't work for compressed NFTs                                    |
| `getProgramAccounts` (for NFT search)        | [`searchAssets`](/docs/api-reference/das/searchassets) or [`getAssetsByGroup`](/docs/api-reference/das/getassetsbygroup) | Faster, cheaper, indexed data                                                    |
| Polling for real-time data                   | [LaserStream WebSocket](/docs/rpc/websocket) or [LaserStream gRPC](/docs/laserstream)                                    | Lower latency, more efficient                                                    |
| Standard `sendTransaction`                   | [Helius Sender](/docs/sending-transactions/sender)                                                                  | Multi-path routing (Helius, Jito, Harmonic, Rakurai, etc.), higher landing rates |

## Recommended Workflows

| Building...         | Helius Products to Use                                                                                                                                                                                         |
| ------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Trading bot         | [Gatekeeper](/docs/gatekeeper/overview) (lowest latency RPC) + [Sender](/docs/sending-transactions/sender) (fast tx submission) + [Priority Fee API](/docs/priority-fee-api) + [LaserStream](/docs/laserstream) (real-time prices) |
| Wallet app          | [DAS API](/docs/das-api) (`getAssetsByOwner`) + [`getTransactionsForAddress`](/docs/rpc/gettransactionsforaddress) (complete history)                                                                                    |
| NFT marketplace     | [DAS API](/docs/das-api) (`searchAssets`, `getAssetsByGroup`) + [Webhooks](/docs/webhooks) (track sales/listings)                                                                                                        |
| Token sniper        | [Gatekeeper](/docs/gatekeeper/overview) (edge-routed RPC) + [LaserStream gRPC](/docs/laserstream) (lowest latency) + [Sender](/docs/sending-transactions/sender) (staked connections)                                         |
| Portfolio tracker   | [DAS API](/docs/das-api) (`getAssetsByOwner` with `showFungible`) + [Enhanced Transactions](/docs/enhanced-transactions/overview)                                                                                        |
| Wallet monitor      | [LaserStream WebSocket](/docs/rpc/websocket) or [Webhooks](/docs/webhooks) for real-time notifications                                                                                                                   |
| Analytics dashboard | [Enhanced Transactions API](/docs/enhanced-transactions/overview) + [`getTransactionsForAddress`](/docs/rpc/gettransactionsforaddress)                                                                                   |
| Airdrop tool        | [AirShip](https://airship.helius.dev) (95% cheaper with ZK compression)                                                                                                                                        |

## Rate Limits Quick Reference

Rate limits depend on your [plan](/docs/billing/plans). Agents start on the Agent tier with 1,000,000 credits. The Agent tier requires a \$1 payment to prevent abuse.

| Plan         | Price      | Monthly Credits | RPC Rate Limit | DAS & Enhanced APIs |
| ------------ | ---------- | --------------- | -------------- | ------------------- |
| Agent        | \$1 signup | 1M              | 10 req/s       | 2 req/s             |
| Developer    | \$49/mo    | 10M             | 50 req/s       | 10 req/s            |
| Business     | \$499/mo   | 100M            | 200 req/s      | 50 req/s            |
| Professional | \$999/mo   | 200M            | 500 req/s      | 100 req/s           |

For detailed rate limits per API, see [Rate Limits](/docs/billing/rate-limits).

## Credits Per API Call

| API                         | Credits | Notes                                                                                               |
| --------------------------- | ------- | --------------------------------------------------------------------------------------------------- |
| Standard RPC calls          | 1       | Most Solana RPC methods                                                                             |
| `getProgramAccounts`        | 10      | Use DAS API instead when possible                                                                   |
| DAS API                     | 10      | All DAS endpoints                                                                                   |
| Enhanced Transactions       | 100     | Parsed transaction data                                                                             |
| `getTransactionsForAddress` | 10+     | Full transactions cost 10 credits per 100 returned; signatures-only responses cost 10 credits flat. |
| `getTransfersByAddress`     | 10      | Developer+ plans only                                                                               |
| Wallet API                  | 100     | All Wallet API endpoints                                                                            |
| Priority Fee API            | 1       | Fee estimation                                                                                      |
| Sender                      | 0       | Free on all plans                                                                                   |
| Webhook events              | 1       | Per event delivered                                                                                 |
| Webhook management          | 100     | Create, edit, delete                                                                                |

For the full breakdown, see [Credits](/docs/billing/credits).

## Retries and Error Handling

### HTTP Status Codes

| Code | Meaning      | Action                         |
| ---- | ------------ | ------------------------------ |
| 200  | Success      | Process response               |
| 400  | Bad request  | Fix request parameters         |
| 401  | Unauthorized | Check API key                  |
| 429  | Rate limited | Back off and retry             |
| 5xx  | Server error | Retry with exponential backoff |

### Retry Pattern

```typescript theme={"system"}
async function heliusRequest(url: string, data: object, maxRetries = 3) {
  for (let attempt = 0; attempt < maxRetries; attempt++) {
    const response = await fetch(url, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(data),
    });

    if (response.ok) return response.json();

    if (response.status === 429) {
      const retryAfter = response.headers.get('Retry-After');
      const delay = retryAfter ? parseInt(retryAfter) * 1000 : Math.pow(2, attempt) * 1000;
      await new Promise(resolve => setTimeout(resolve, delay));
      continue;
    }

    if (response.status >= 500) {
      await new Promise(resolve => setTimeout(resolve, Math.pow(2, attempt) * 1000));
      continue;
    }

    throw new Error(`Request failed: ${response.status} ${await response.text()}`);
  }
  throw new Error('Max retries exceeded');
}
```

### Monitor Credit Usage

```bash theme={"system"}
helius usage --json
```

## Quick Reference

* **Mainnet RPC**: `https://mainnet.helius-rpc.com/?api-key=YOUR_API_KEY`
* **Mainnet RPC (Gatekeeper Beta)**: `https://beta.helius-rpc.com/?api-key=YOUR_API_KEY`
* **Devnet RPC**: `https://devnet.helius-rpc.com/?api-key=YOUR_API_KEY`
* **Mainnet WSS**: `wss://mainnet.helius-rpc.com/?api-key=YOUR_API_KEY`
* **Mainnet WSS (Gatekeeper Beta)**: `wss://beta.helius-rpc.com/?api-key=YOUR_API_KEY`
* **Devnet WSS**: `wss://devnet.helius-rpc.com/?api-key=YOUR_API_KEY`
* **Sender endpoint**: `https://sender.helius-rpc.com/fast`
* **MCP server**: `https://www.helius.dev/docs/mcp`
* **Dashboard**: [dashboard.helius.dev](https://dashboard.helius.dev)
* **Status**: [helius.statuspage.io](https://helius.statuspage.io)
