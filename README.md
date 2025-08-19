# OpenAI OSS Relay

A tiny reverse‑proxy that forwards HTTP requests to an upstream service while rewriting OpenAI‑style chat completions. Non‑chat endpoints are proxied unchanged. For `/v1/chat/completions`, the proxy inspects the response and replaces configured open/close patterns with custom tags.

## Features

- **Transparent proxy** for all HTTP traffic.
- **Chat completion rewrite**:
  - Supports both JSON responses and Server‑Sent Events (SSE) streams.
  - Configurable pattern matching and replacement tags.
- **Environment variable configuration** for listening address, port, upstream host/port.

## Configuration

The proxy reads its settings from environment variables. The defaults are shown below:

### Architecture Diagram

```mermaid
flowchart TD
    %% -------------------------------------------------
    %% Entry point
    %% -------------------------------------------------
    A[main] --> B[ProxyConfig::from_env()]
    B --> C[Initialize logger &amp; Hyper client]
    C --> D[Server::bind(listen_addr)]
    D --> E[make_service_fn -> forward_request]

    %% -------------------------------------------------
    %% Request routing
    %% -------------------------------------------------
    E --> F{Request path}
    F -- non-chat endpoint --> G[proxy request unchanged]
    F -- "/v1/chat/completions" --> H[handle_chat_completions]

    %% -------------------------------------------------
    %% Chat completion handling
    %% -------------------------------------------------
    H --> I{Response Content-Type}
    I -- "application/json" --> J[rewrite_full_json]
    I -- "text/event-stream" --> K[rewrite_streaming]

    %% -------------------------------------------------
    %% JSON rewrite path
    %% -------------------------------------------------
    J --> L[Parse JSON -> iterate choices]
    L --> M[PatternReplacer::rewrite on each message.content]
    M --> N[Serialize JSON & send response]

    %% -------------------------------------------------
    %% SSE (stream) rewrite path
    %% -------------------------------------------------
    K --> O[SseTransformer stream wrapper]
    O --> P[Buffer until \\n\\n event boundary]
    P --> Q[PatternReplacer::rewrite on event payload]
    Q --> R[Emit rewritten Bytes -> client]

    %% -------------------------------------------------
    %% Common response flow back to client
    %% -------------------------------------------------
    G --> S[Send upstream response unchanged]
    N --> T[Send modified JSON response]
    R --> T

    %% -------------------------------------------------
    %% Configuration structs (shown as subgraph for clarity)
    %% -------------------------------------------------
    subgraph Config [Configuration]
        direction TB
        B1[EnvConfig::default()] --> B2[Read env vars]
        B3[ConnectionConfig::default()] --> B4[listen_addr, listen_port, upstream_host, upstream_port]
        B5[ReplacementConfig::default()] --> B6[open_pattern, close_pattern, open_tag, close_tag]
    end
    %% Connect to a concrete node inside the subgraph (B1) instead of the cluster itself
    B --> B1

    %% -------------------------------------------------
    %% Pattern replacer component
    %% -------------------------------------------------
    subgraph Replacer [PatternReplacer]
        direction TB
        X1[opened flag] --> X2[rewrite(input)]
        X2 -->|if !opened && contains open_pattern| Y1[replace first open_pattern -> open_tag, set opened=true]
        Y1 -->|if opened && contains close_pattern| Y2[replace all close_pattern -> close_tag, set opened=false]
    end
    %% Multiple sources → single target: use two explicit edges for compatibility
    M --> Replacer
    Q --> Replacer

    %% -------------------------------------------------
    %% Final output
    %% -------------------------------------------------
    T --> U[Client receives response]
    S --> U
```

| Variable        | Default   |
|-----------------|-----------|
| `LISTEN_ADDR`   | `0.0.0.0` |
| `LISTEN_PORT`   | `8080`    |
| `UPSTREAM_HOST` | `127.0.0.1` |
| `UPSTREAM_PORT` | `1234`    |

You can override any of these by setting the corresponding environment variable before running the binary.

### Pattern replacement

The patterns and tags used for rewriting chat completions are defined in `ReplacementConfig::default()`:

```rust
open_pattern: "<|channel|>analysis<|message|>",
close_pattern: "<|end|><|start|>assistant<|channel|>final<|message|>",
open_tag: "<think>",
close_tag: "</think>"
```

Edit `src/main.rs` to change these defaults if needed.

## Building

```sh
# Build the binary (debug)
cargo build

# Or build a release binary
cargo build --release
```

## Running

```sh
# Example using default configuration
cargo run

# Using custom environment variables
LISTEN_ADDR=127.0.0.1 LISTEN_PORT=8081 UPSTREAM_HOST=my.upstream.com UPSTREAM_PORT=5000 cargo run
```

The proxy will start listening on `http://<LISTEN_ADDR>:<LISTEN_PORT>` and forward requests to the upstream service at `http://<UPSTREAM_HOST>:<UPSTREAM_PORT>`.

## License

This project is licensed under the MIT License. See the [LICENSE](LICENSE) file for details.
