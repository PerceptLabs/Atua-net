# CLAUDE.md — atua-net

## The Spec Is the Contract

The spec (`atua-net-v2-final.md`) is the source of truth. It has 8 priority tiers (0-7), 18 ordered implementation items, a JS wrapper architecture, and a test battery. **Every specced item gets implemented. No cherry-picking. No declaring victory early.**

## Anti-Drift Rules

These patterns have already happened on this project. Don't repeat them:

1. **Test-only doesn't count.** A feature exists in production when it's in BOTH `src/lib.rs` (Rust WASM) AND `pkg/atua-net.js` (production JS wrapper). The test harness (`tests/index.html`) is NOT production.

2. **No collapsing spec tests.** Every test in the spec's Test Battery section must be written with the specific endpoint and assertion described. Don't merge them into vague placeholders.

3. **No stopping after a sub-plan.** After every session, audit the spec against the codebase. Report what's done, what's partial, what's not started. "All tests pass" proves what exists works — it does not prove everything exists.

4. **Find a crate first.** If you're about to write a parser, decoder, hasher, or matcher — stop and check crates.io. 500KB of audited libraries is better than 300KB of hand-rolled code.

## Progress Tracker

### Priority 0 — Critical Bug Fix
- [x] TLS lifecycle fix (atua_connect stores TLS streams in WASM, JS routes through atua_stream_send/recv/close)

### Priority 1 — Replace Hand-Rolled Code
- [x] URL parsing via `url` crate
- [x] JSON header parsing via `serde_json`

### Priority 2 — Logging & Diagnostics
- [x] Structured logging (log + console_log + console_error_panic_hook)
- [x] Request timing (6-field Timing struct on every response)

### Priority 3 — Resilience
- [x] Timeouts via wasmtimer (WASM layer)
- [ ] Retry with backoff (EXISTS IN TEST HARNESS ONLY — must move to production AtuaNetClient)
- [ ] Circuit breaker (EXISTS IN TEST HARNESS ONLY — must move to production AtuaNetClient)

### Priority 4 — HTTP Features
- [x] Redirect following (301-308, relative resolution via url crate)
- [x] Response decompression (gzip, deflate, brotli via flate2 + brotli-decompressor)
- [ ] Cookie jar (cookie_store + cookie crates — NOT STARTED)
- [ ] Connection keep-alive / pooling (NOT STARTED — every fetch opens fresh TLS)

### Priority 5 — Streaming & WebSocket
- [ ] Streaming response API with on_chunk callback (NOT STARTED — all responses buffer full body)
- [x] WebSocket client (hand-rolled RFC 6455 framing over TLS stream)

### Priority 6 — Security Features
- [ ] Certificate pinning (NOT STARTED)
- [ ] Custom CA certificates (NOT STARTED)
- [ ] TLS configuration API (NOT STARTED)

### Priority 7 — Middleware & Advanced API
- [ ] AtuaNetClient class in pkg/atua-net.js (NOT STARTED — currently bare function exports)
- [ ] Middleware chain (NOT STARTED)
- [x] Shared WispBridge (exists in test harness, not in production wrapper)

### Bonus (not in original spec)
- [x] HTTP/2 via hyper ALPN auto-detection

## Key Files

| File | Role |
|------|------|
| `src/lib.rs` | WASM exports: atua_fetch, atua_connect, atua_websocket, stream ops |
| `src/wisp_stream.rs` | WispStream (AsyncRead/Write over JS callbacks via mpsc channels) + TokioIo adapter |
| `src/websocket.rs` | Hand-rolled RFC 6455 framing (~180 lines) |
| `pkg/atua-net.js` | Production JS wrapper — THIS IS WHAT SHIPS |
| `tests/index.html` | Browser test harness (NOT production) |
| `tests/atua-net.spec.js` | Playwright tests — 57 tests across 12+ tiers |
| `tests/serve.js` | Test server + local Wisp relay |
| `Cargo.toml` | Rust dependencies |

## Build & Test

```bash
export PATH="/c/Program Files/LLVM/bin:$PATH"  # LLVM needed for ring WASM build
cargo test                                       # Rust unit tests
wasm-pack build --target web --out-dir wasm-pkg  # Build WASM
npx playwright test --reporter=list              # Integration tests
```

## Current Stats

- 57 Playwright tests, 56 passed, 0 failed, 1 skipped (Anthropic API key)
- 7 Rust unit tests passing
- WASM binary: ~1.5MB uncompressed (includes hyper, tokio, rustls, flate2, brotli, url, serde_json)
