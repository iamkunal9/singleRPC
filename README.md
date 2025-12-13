```
       .__               .__        _____________________________  
  _____|__| ____    ____ |  |   ____\______   \______   \_   ___ \ 
 /  ___/  |/    \  / ___\|  | _/ __ \|       _/|     ___/    \  \/ 
 \___ \|  |   |  \/ /_/  >  |_\  ___/|    |   \|    |   \     \____
 /____  >__|___|  /\___  /|____/\___  >____|_  /|____|    \______  /
      \/        \/_____/           \/       \/                  \/ 

                                                            iamkunal9.in
```
Robust, blazing-fast single RPC for multiple chains with smart load balancing, health checks, continuous retries, and per-request timeouts built in.

Features
--------
- Hyper v1 + hyper-util async server
- Reqwest client for outbound requests
- Round-robin load balancing with simple failure backoff and health tracking
- Continuous retry across endpoints (no 503) until a healthy response is obtained
- Per-request timeout (default 5s, configurable with `-t`)
- Verbose logging levels with `-v`/`-vv`
- CLI flags for config, port, timeout, verbosity

Install
-------
```bash
cargo install --path .
```

Usage
-----
```bash
singlerpc -c path/to/config.json -p 3000

# with custom timeout (seconds)
singlerpc -c config.json -p 3000 -t 10

# verbose logs: endpoints and statuses
singlerpc -c config.json -v

# very verbose: also prints upstream response body
singlerpc -c config.json -vv

# lock the proxy behind a token (clients must send the same token)
singlerpc -c config.json -a supersecret
```

- -c, --config <FILE>: Path to JSON config mapping chain IDs to arrays of RPC URLs
- -p, --port <PORT>: Port to listen on (default: 3000)
- -t, --timeout <SECONDS>: Per-RPC request timeout (default: 5)
- -a, --auth <TOKEN>: Require clients to include the matching token (via `Authorization: Bearer <token>`, `X-SingleRPC-Auth: <token>`, or `?auth=<token>`); omit to keep the proxy open
- -v: Log incoming JSON, endpoint URL, and upstream status
- -vv: Also log upstream response body
- -h, --help: Show help
- -V, --version: Show version

Config format
-------------
Example config.json:
```json
{
  "eth-mainnet": [
    "https://rpc1.example.com",
    "https://rpc2.example.com"
  ],
  "polygon": [
    "https://polygon-rpc.example.com"
  ]
}
```

Notes
-----
- The proxy rotates through endpoints in a round-robin loop. If all fail in a pass, it keeps retrying (with short sleeps) until one succeeds.
- Endpoints are marked unhealthy on repeated failures and temporarily deprioritized; they will be retried later.
- Failures are categorized/logged (timeouts, connect errors, 429 rate-limit, 5xx server errors, JSON-RPC error objects).

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.