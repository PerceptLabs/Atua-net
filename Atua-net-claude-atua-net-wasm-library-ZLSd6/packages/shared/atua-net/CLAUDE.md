# CLAUDE.md — atua-net

## The Spec Is the Contract

The spec (`atua-net-v2-final.md`) is the source of truth. It has 8 priority tiers (0-7), 18 ordered implementation items, a JS wrapper architecture, and a test battery. **Every specced item gets implemented. No cherry-picking. No declaring victory early.**

## Anti-Drift Rules

1. **Test-only doesn't count.** A feature exists in production when it's in BOTH `src/lib.rs` (Rust WASM) AND `pkg/atua-net.js` (production JS wrapper).
2. **No collapsing spec tests.** Every test must have the specific endpoint and assertion from the spec.
3. **No stopping after a sub-plan.** Audit the spec against the codebase after every session.
4. **Find a crate first.** If you're about to write a parser, decoder, hasher, or matcher — check crates.io.

## Git Workflow — Snapshots

When the user says "push", "snapshot", or "checkpoint":

1. `git add -A`
2. `git commit -m "Snapshot: <description>"` — use context from recent work
3. Create snapshot branch using **today's actual date**:
   ```bash
   git branch "snapshot-$(date +%Y-%m-%d)-<short-description>"
   ```
4. Push it:
   ```bash
   git push origin "snapshot-$(date +%Y-%m-%d)-<short-description>"
   ```
5. **Stay on current branch** — do NOT checkout the snapshot
6. Tell the user: what was committed, the snapshot branch name, and confirm still on working branch

These are frozen checkpoints. Never switch to them. Keep working on main.
**Always use `$(date +%Y-%m-%d)` for the date. Never hardcode a date.**
**Use kebab-case for short descriptions.**

## Progress Tracker

### Priority 0 — Critical Bug Fix
- [x] TLS lifecycle fix (atua_connect stores TLS streams, JS routes through atua_stream_send/recv/close)

### Priority 1 — Replace Hand-Rolled Code
- [x] URL parsing via `url` crate
- [x] JSON header parsing via `serde_json`

### Priority 2 — Logging & Diagnostics
- [x] Structured logging (log + console_log + console_error_panic_hook)
- [x] Request timing (6-field Timing struct on every response)

### Priority 3 — Resilience
- [x] Timeouts via wasmtimer (WASM layer)
- [x] Retry with backoff (production AtuaNetClient.fetch())
- [x] Circuit breaker (production AtuaNetClient per-host tracking)

### Priority 4 — HTTP Features
- [x] Redirect following (301-308, relative resolution via url crate)
- [x] Response decompression (gzip, deflate, brotli)
- [x] Cookie jar (cookie_store + cookie crates, opt-in per request)
- [x] Connection keep-alive / pooling (PooledSender in thread-local HashMap, 60s idle timeout)

### Priority 5 — Streaming & WebSocket
- [x] Streaming response API with on_chunk callback (atua_fetch_streaming, no Accept-Encoding in streaming mode)
- [x] WebSocket client (hand-rolled RFC 6455 framing, random masks, Accept verification)

### Priority 6 — Security Features
- [x] Certificate pinning (x509-parser for SPKI extraction, ring::digest for SHA256, base64 for encoding)
- [x] Custom CA certificates (rustls-pemfile for PEM parsing, custom_ca_pem parameter)
- [x] TLS configuration API (tls_config_json parameter: minVersion, alpn overrides)

### Priority 7 — Middleware & Advanced API
- [x] AtuaNetClient class in pkg/atua-net.js (constructor options, shared WispBridge, backward-compat exports)
- [x] Middleware chain (onRequest / onResponse hooks in fetch())
- [x] Shared WispBridge per client instance

### v3 — Native Rust Wisp Client
- [x] Wisp v1 frame codec (encode/decode, 14 unit tests)
- [x] WispClient (web_sys::WebSocket, stream demux, flow control + Notify)
- [x] WispStream::from_native constructor (dual write backend via enum)
- [x] Dual-path WASM exports (use_native_wisp + wisp_url params, Option<Function> callbacks)
- [x] AtuaNetClient nativeWisp option (pkg/atua-net.js)
- [x] Flow control enforcement (buffer_remaining check before send, Notify wake on CONTINUE)
- [x] Unbounded channels for DATA frames (fixes 102KB truncation — bounded channel silently dropped frames)
- [x] Self-contained Wisp v1 relay in tests/serve.js (replaces wisp-js, no pause/resume, TCP + UDP)
- [x] All 8 native path integration tests passing (Tier 16)
- [x] 16 native hardening tests passing (Tier 17)
- [x] 12 pressure tests passing (Tier 18: 200 sequential, 50 burst, 20 concurrent, mixed errors)

### Bonus (not in original spec)
- [x] HTTP/2 via hyper ALPN auto-detection

## Key Files

| File | Role |
|------|------|
| `src/lib.rs` | WASM exports: fetch, fetch_streaming, connect, websocket, stream ops, cert pinning, connection pool |
| `src/wisp_stream.rs` | WispStream (AsyncRead/Write via unbounded mpsc channels) + TokioIo adapter |
| `src/wisp.rs` | Wisp v1 frame codec + WispClient (web_sys::WebSocket, flow control, stream demux) |
| `src/websocket.rs` | Hand-rolled RFC 6455 framing (~230 lines) |
| `pkg/atua-net.js` | Production JS: AtuaNetClient class with retry, circuit breaker, middleware |
| `tests/index.html` | Browser test harness (dual JS/native Wisp path support) |
| `tests/atua-net.spec.js` | Playwright tests — 100 tests across 18+ tiers |
| `tests/serve.js` | Static server + local /bytes/ endpoint + self-contained Wisp v1 relay |
| `Cargo.toml` | Rust dependencies (30+ crates) |

## Build & Test

```bash
export PATH="/c/Program Files/LLVM/bin:$PATH"  # LLVM needed for ring WASM build
cargo test                                       # 21 Rust unit tests
wasm-pack build --target web --out-dir wasm-pkg  # Build WASM
npx playwright test --reporter=list              # 100 integration tests
```

## Current Stats

- 100 Playwright tests, 98 passed, 0 failed, 2 skipped (API key, WS echo server)
- 21 Rust unit tests passing (7 lib + 14 wisp frame codec)
- JS path: 62/64 pass (2 skip)
- Native Rust Wisp path: 36/36 pass (Tier 16 + 17 + 18)
