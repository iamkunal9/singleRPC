---
name: singlerpc
description: Use whenever the user needs a Web3 / EVM JSON-RPC endpoint (Ethereum, BSC, Polygon, Base, Arbitrum, or any mainnet chain), is being rate-limited by a public RPC, wants a unified local RPC proxy across many chains with automatic failover, or wants to discover which chains a single contract address is deployed on. Trigger on phrases like "I need an RPC", "rpc for <chain>", "rate limited by infura/alchemy", "find what chains this contract exists on", "multi-chain rpc", or any direct mention of `singlerpc` / `singleRPC`. Bootstraps the install if the binary is missing, otherwise runs it in the background and shows the user how to call it.
---

# singlerpc

`singlerpc` is a local JSON-RPC proxy that fans requests across many public RPC endpoints per chain with round-robin load balancing, health tracking, automatic failover, and continuous retry. The user gets one stable local URL (`http://localhost:3000/<chain-id>`) instead of juggling Infura/Alchemy keys or hardcoded public RPCs. A single bundled snapshot from chainlist.org gives it coverage of every EVM mainnet out of the box.

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
- No `-c` flag is needed — singlerpc ships with a built-in Chainlist mainnet snapshot, so every common EVM chain works immediately. Only pass `-c <file>` if the user has a custom config.
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

`exists: true` means the address has bytecode on that chain (so it's a contract there, not just an EOA). Useful for cross-chain deployment audits, detecting which forks of a protocol are live, etc.

## Supported chains

By default, singlerpc loads a compile-time snapshot of **every mainnet on chainlist.org** (testnets are excluded). Common ones:

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
| ...      | + every other Chainlist mainnet |

To see exactly which chain IDs are wired up at runtime, send any chain id and check the status — `404 Chain not supported` means it's missing. The repo's sample `config.json` is just an *override example* listing 6 chains (1, 56, 137, 8453, 42161, 719); it is **not** the default. The defaults are far broader.

If the user wants a curated subset (or wants to add a private RPC), have them write their own JSON like:

```json
{
  "1":   ["https://eth.llamarpc.com", "https://rpc.ankr.com/eth"],
  "137": ["https://polygon.llamarpc.com"]
}
```

…and start with `singlerpc -c /path/to/config.json`.

## CLI reference (cheat sheet)

| Flag | Meaning | Default |
|------|---------|---------|
| `-c, --config <FILE>` | Override the bundled chain → URLs map | bundled Chainlist snapshot |
| `-p, --port <PORT>`   | Listen port | `3000` |
| `-t, --timeout <SEC>` | Per-RPC request timeout | `5` |
| `-v` / `-vv`          | Verbose / very verbose logs | off |
| `-a, --auth <TOKEN>`  | Require this token from clients | open |
| `-h` / `-V`           | Help / version | — |

## Behavior notes worth knowing

- Round-robin across endpoints; an endpoint is marked unhealthy after 3 failures and deprioritized for ~3 hours, then retried.
- The proxy keeps retrying across all endpoints until one succeeds — clients should not see 503s, but a slow chain can block until timeout × N endpoints.
- A JSON-RPC error in the upstream body is treated as a failure and the next endpoint is tried.
- Auth, when enabled, is checked before any chain routing.

## Things to avoid

- Don't run `singlerpc` in the foreground in the assistant's session — it blocks. Always background it (Step 3).
- Don't auto-install for the user. The install script writes to `/usr/local/bin` and may prompt for sudo; that's a decision for the user, not the assistant.
- Don't assume only the 6 chains in the repo's `config.json` are supported. The default is the full Chainlist mainnet set.
- Don't recommend `singlerpc` for testnets — the bundled snapshot filters them out.
