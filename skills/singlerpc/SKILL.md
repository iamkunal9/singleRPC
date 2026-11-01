---
name: singlerpc
description: Use whenever the user needs a Web3 JSON-RPC endpoint — any EVM chain (Ethereum, BSC, Polygon, Base, Arbitrum, Sepolia, Base Sepolia, or any Chainlist mainnet/testnet), Solana mainnet/devnet, or TRON — is being rate-limited by a public RPC, wants a unified local RPC proxy across many chains with automatic failover, wants a reliable upstream for `anvil --fork-url` (including historical/archive forks), or wants to discover which chains a single contract address is deployed on. Trigger on phrases like "I need an RPC", "rpc for <chain>", "solana rpc", "rate limited by infura/alchemy", "fork mainnet with anvil", "archive rpc", "find what chains this contract exists on", "multi-chain rpc", or any direct mention of `singlerpc` / `singleRPC`. Bootstraps the install if the binary is missing, otherwise runs it in the background and shows the user how to call it.
---

# singlerpc

`singlerpc` is a local JSON-RPC proxy that fans requests across many public RPC endpoints per chain with round-robin load balancing, health tracking, automatic failover, and continuous retry. The user gets one stable local URL (`http://localhost:3000/<chain-id>`) instead of juggling Infura/Alchemy keys or hardcoded public RPCs. A single bundled snapshot from chainlist.org gives it coverage of EVM mainnets and testnets out of the box, plus curated keyless endpoints for Solana (`solana:mainnet`, `solana:devnet`) and TRON (`tron:mainnet`).

Repository: <https://github.com/iamkunal9/singleRPC>

## Workflow

Follow these steps in order. Do not skip the install check.

### Step 1 — Check whether `singlerpc` is on the user's PATH

Run:

```bash
command -v singlerpc
```

- If the command prints a path → singlerpc is installed. Skip to **Step 3**.
- If it prints nothing / exits non-zero → singlerpc is not installed. Go to **Step 2**.

### Step 2 — Not installed: hand the user a one-line install command, then stop

Do **not** try to install it yourself. Show the user this and ask them to run it, then re-send their original request:

> singlerpc isn't installed yet. Run one of these to install it, then re-send your message and I'll pick up where we left off:
>
> ```bash
> curl -fsSL https://raw.githubusercontent.com/iamkunal9/singleRPC/main/install.sh | bash
> ```
>
> Or with wget:
>
> ```bash
> wget -qO- https://raw.githubusercontent.com/iamkunal9/singleRPC/main/install.sh | bash
> ```
>
> The installer downloads the latest GitHub release for your OS (macOS or Linux), drops the `singlerpc` binary into `/usr/local/bin` (or `~/.local/bin` if it can't write there), and prints a PATH hint if needed.

After showing the install command, stop and wait for the user. Do not retry the user's original task in the same turn — the binary won't appear mid-conversation.

### Step 3 — Installed: start it in the background (idempotent)

singlerpc is a long-running server, so it must run in the background. Before starting a fresh instance, check whether one is already listening on the default port (3000) so we don't double-start:

```bash
# Already running?
if curl -fsS -o /dev/null -X POST http://localhost:3000/1 \
     -H 'content-type: application/json' \
     -d '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}'; then
  echo "singlerpc already running on :3000"
else
  mkdir -p /tmp/singlerpc
  nohup singlerpc > /tmp/singlerpc/singlerpc.log 2>&1 &
  echo "started singlerpc (pid $!), log: /tmp/singlerpc/singlerpc.log"
  # Give it a moment to bind the port
  sleep 1
fi
```

Notes:
- No `-c` flag is needed — singlerpc ships with a built-in Chainlist snapshot, so every common EVM mainnet and testnet works immediately. Only pass `-c <file>` if the user has a custom config.
- Default port is **3000** and it binds `0.0.0.0`. Override with `-p <port>`.
- For verbose troubleshooting use `-v` (logs endpoints & status) or `-vv` (also logs upstream bodies).
- To require a token from clients, start with `-a <token>`; clients then send `Authorization: Bearer <token>`, `X-SingleRPC-Auth: <token>`, or `?auth=<token>`.

If the user later asks to stop it: `pkill -f '^singlerpc' || true` (or kill the recorded PID).

### Step 4 — Tell the user what they got, and how to use it

Tailor this summary to what they asked for. Always include the local URL pattern; include the `sr_contract_chains` section only if it's relevant.

**Use as a normal JSON-RPC endpoint**

The URL is `http://localhost:3000/<chain-id>`. Example for Ethereum mainnet (chain id `1`):

```bash
curl -s http://localhost:3000/1 \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}'
```

In ethers / viem / web3.js, point the provider at `http://localhost:3000/<chain-id>`.

**Solana and TRON**

Non-EVM networks use string keys instead of chain ids: `solana:mainnet`, `solana:devnet`, `tron:mainnet` (TRON's eth_-compatible JSON-RPC). Same URL pattern:

```bash
curl -s http://localhost:3000/solana:mainnet \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getLatestBlockhash"}'
```

With `@solana/web3.js`: `new Connection("http://localhost:3000/solana:devnet")`. `sendTransaction`, `simulateTransaction`, `getSignatureStatuses`, `getLatestBlockhash`, etc. are proxied as-is; rate-limited or behind nodes fail over, while transaction rejections (simulation failed, blockhash not found, insufficient funds, bad signature/encoding) come back unchanged, and a `sendTransaction` is never re-broadcast after an error reply.

**Fork a chain with anvil**

singlerpc is a good `anvil --fork-url` upstream, including old blocks:

```bash
anvil --fork-url http://localhost:3000/1                              # latest
anvil --fork-url http://localhost:3000/1 --fork-block-number 12000000 # historical
```

Requests pinned to a block number go first to endpoints already verified to serve state at that depth, endpoints known to be pruned there are skipped, and endpoints caught returning wrong state ("liars") are never used — see *State-capability cache* below. Notes: on pre-London forks (Ethereum < 12,965,000) pass an explicit gas price for sends (`cast send --legacy --gas-price 100gwei …`), because anvil silently drops txs priced at its own tiny `eth_gasPrice`; and if you fork through an unverified full endpoint list, prefer `--no-storage-caching` so a bad read is not cached by anvil.

**Find which chains a contract address is deployed on (the killer feature)**

singlerpc exposes a non-standard endpoint `POST /sr_contract_chains` that calls `eth_getCode` for one address across every configured chain in parallel and returns where bytecode exists. One request, many chains.

```bash
curl -s http://localhost:3000/sr_contract_chains \
  -H 'content-type: application/json' \
  -d '{"address":"0xdAC17F958D2ee523a2206206994597C13D831ec7"}'
```

Optional `chains` field narrows the search:

```bash
curl -s http://localhost:3000/sr_contract_chains \
  -H 'content-type: application/json' \
  -d '{"address":"0x...","chains":["1","56","137"]}'
```

The response shape is:

```json
{
  "jsonrpc": "2.0",
  "id": null,
  "result": {
    "address": "0x...",
    "chains": {
      "1":   { "exists": true,  "code": "0x60806040..." },
      "56":  { "exists": false, "code": "0x" },
      "137": { "exists": true,  "code": "0x60806040..." }
    }
  }
}
```

`sr_contract_chains` only covers EVM chain-id keys (the Solana/TRON keys are skipped). `exists: true` means the address has bytecode on that chain (so it's a contract there, not just an EOA). Useful for cross-chain deployment audits, detecting which forks of a protocol are live, etc.

## Supported chains

By default, singlerpc loads a compile-time snapshot of **mainnets and testnets on chainlist.org**. Common ones:

| Chain ID | Network        |
|----------|----------------|
| 1        | Ethereum       |
| 10       | OP Mainnet     |
| 56       | BNB Smart Chain|
| 100      | Gnosis         |
| 137      | Polygon PoS    |
| 250      | Fantom         |
| 8453     | Base           |
| 42161    | Arbitrum One   |
| 43114    | Avalanche C    |
| 59144    | Linea          |
| 81457    | Blast          |
| 534352   | Scroll         |
| 7777777  | Zora           |
| 11155111 | Sepolia        |
| 84532    | Base Sepolia   |
| 421614   | Arbitrum Sepolia |
| ...      | + other Chainlist mainnets/testnets |

To see exactly which chain IDs are wired up at runtime, send any chain id and check the status — `404 Chain not supported` means it's missing. The repo's sample `config.json` is just an *override example* listing 6 chains (1, 56, 137, 8453, 42161, 719); it is **not** the default. The defaults are far broader.

If the user wants a curated subset (or wants to add a private RPC), have them write their own JSON like:

```json
{
  "1":   ["https://eth.llamarpc.com", "https://rpc.ankr.com/eth"],
  "137": ["https://polygon.llamarpc.com"]
}
```

…and start with `singlerpc -c /path/to/config.json`.

## State-capability cache

singlerpc remembers, per (chain, endpoint), how much historical state the endpoint can serve:

- deepest block verified to serve correct state, shallowest block seen pruned, observed latency;
- a **liar** flag for endpoints that return well-formed but wrong state (e.g. zero balances/empty code for pruned blocks, or that ignore the block tag). Verification is chain-agnostic: for block B it takes a transaction sender S with nonce n and requires `eth_getTransactionCount(S, B) > n` and `eth_getTransactionCount(S, txBlock-1) <= n`, plus valid `eth_getBalance`/`eth_getCode` hex.

Records live **7 days**; inconclusive/failed probes are remembered for **1 hour** so a fresh process does not re-wait on dead endpoints. The file is a sibling of the health file: `~/.singlerpc/health.state.json` by default (`<stem>.state.json` next to `--health-file`), memory-only with `--no-health-file`. Library users can set `SINGLERPC_STATE_FILE` (empty = disabled) via `singlerpc::default_state_store_path()`.

Embedding the crate (Rust), e.g. to pick fork upstreams:

```rust
let config = singlerpc::load_config_from_str(singlerpc::DEFAULT_CONFIG_JSON)?;
let prober = RpcProxy::with_stores(config, 0, Duration::from_secs(5), None,
    singlerpc::default_health_store_path(), singlerpc::default_state_store_path());
let urls = prober.verified_state_endpoints("1", Some(12_000_000)).await; // None = latest; fastest first
let fork = Arc::new(RpcProxy::with_shared_state_store(HashMap::from([("1".into(), urls)]),
    0, Duration::from_secs(5), None, None, prober.state_store().clone()));
singlerpc::run_server(fork, "127.0.0.1:18700".parse()?).await?;   // anvil --fork-url http://127.0.0.1:18700/1
// Non-EVM keys work through the same proxy:
// prober.request_json("solana:mainnet", json!({"jsonrpc":"2.0","id":1,"method":"getSlot"})).await
```

`verified_state_endpoints` returns immediately from cache when at least 3 endpoints are already verified for that depth (`set_min_verified_endpoints`), probing the rest in the background (`set_background_probing`). It only applies to EVM chain-id keys; it returns an empty list for `solana:*` / `tron:*`. Share one store between proxies (`with_shared_state_store`) rather than pointing two at the same file.

## CLI reference (cheat sheet)

| Flag | Meaning | Default |
|------|---------|---------|
| `-c, --config <FILE>` | Override the bundled chain → URLs map | bundled Chainlist snapshot |
| `-p, --port <PORT>`   | Listen port | `3000` |
| `-t, --timeout <SEC>` | Per-RPC request timeout | `5` |
| `-v` / `-vv`          | Verbose / very verbose logs | off |
| `-a, --auth <TOKEN>`  | Require this token from clients | open |
| `--health-file <FILE>` | Persist hard-dead endpoint health | `~/.singlerpc/health.json` |
| `--no-health-file`    | Keep endpoint health (and state capability) in memory only | off |
| `-u, --update`        | Re-run `install.sh` to fetch the latest release, then exit | — |
| `-h` / `-V`           | Help / version | — |

Environment: `SINGLERPC_HEALTH_FILE` overrides the default health file (empty disables it); `SINGLERPC_STATE_FILE` does the same for the state-capability file in library use; `CHAINLIST_RPC_URL` changes the snapshot source at build time (if the fetch fails the build still succeeds with only the curated non-EVM keys).

## Update behavior

- On every start, singlerpc does a quick (≤2s) check against `https://github.com/iamkunal9/singleRPC/releases/latest`. If a newer tag is published than the running binary, it prints a one-line `[!] A newer singlerpc vX.Y.Z is available` warning to stderr, then continues normally — startup is not blocked even if the check fails.
- If the user wants to upgrade, suggest `singlerpc --update`. That flag re-downloads `install.sh` from the repo, runs it (sudo prompt may appear if writing to `/usr/local/bin`), and exits. After the install completes, restart any running singlerpc process to pick up the new binary.
- The installer also detects and removes older `singlerpc` binaries elsewhere on `$PATH` (e.g. a stale `~/.local/bin/singlerpc`) so they can't shadow the fresh install.

## Behavior notes worth knowing

- Round-robin across endpoints (block-pinned EVM reads prefer endpoints verified for that depth); hard-dead endpoints are marked after 3 hard failures, persisted by the CLI, and retried after 24 hours.
- 429 / rate-limit errors use a short in-memory cooldown and are not persisted as dead.
- Non-JSON bodies (HTML parking pages, gateway errors) count as endpoint failures and fail over.
- Node-side JSON-RPC errors fail over to another endpoint: pruned/missing historical state ("missing trie node", "historical state … not available"), lagging heads ("header not found", Solana "minimum context slot"), rate limits, internal errors. Deterministic results are returned unchanged: reverts, method-not-found, Solana simulation/transaction rejections.
- Transaction submissions (`eth_sendRawTransaction`, `eth_sendTransaction`, Solana `sendTransaction`) are never re-sent after a JSON-RPC error reply.
- Retries are bounded (about 6× the per-request timeout, min 10 s); if every endpoint fails the client gets HTTP 502 with a JSON-RPC error carrying the request id.
- Batch (array) requests are forwarded as-is.
- Auth, when enabled, is checked before any chain routing.

## Things to avoid

- Don't run `singlerpc` in the foreground in the assistant's session — it blocks. Always background it (Step 3).
- Don't auto-install for the user. The install script writes to `/usr/local/bin` and may prompt for sudo; that's a decision for the user, not the assistant.
- Don't assume only the 6 chains in the repo's `config.json` are supported. The default is the broad Chainlist mainnet/testnet set plus `solana:mainnet`, `solana:devnet`, `tron:mainnet`.
- Don't send EVM methods to `solana:*` keys (or Solana methods to EVM keys); the key selects the endpoint list, not a translation layer.
