/**
 * @aspect/atua-net — Browser WASM networking library
 *
 * Production client: AtuaNetClient class with fetch(), connect(), websocket().
 * Module-level default client + bare function exports for backward compat.
 */

// ─── WASM Module Loader (singleton) ──────────────────────────────

let wasmModule = null;

async function ensureWasm() {
  if (wasmModule) return wasmModule;
  const wasm = await import('../wasm-pkg/atua_net.js');
  await wasm.default();
  wasmModule = wasm;
  return wasmModule;
}

// ─── Wisp Bridge (shared per client) ─────────────────────────────

class WispBridge {
  constructor(wispUrl) {
    this.wispUrl = wispUrl;
    this._client = null;
    this._connectPromise = null;
  }

  async ensureConnected() {
    if (this._connectPromise) return this._connectPromise;
    if (this._client?.connected) return;

    const { WispClient } = await import('wisp-client-js');
    this._client = new WispClient(this.wispUrl);
    this._connectPromise = new Promise((resolve, reject) => {
      // WispClient connect is sync — it starts the WebSocket
      // We check readyState or use the client's connect method
      this._client.connect().then(() => {
        this._client.connected = true;
        this._connectPromise = null;
        resolve();
      }).catch(e => {
        this._connectPromise = null;
        reject(e);
      });
    });
    return this._connectPromise;
  }

  createCallbacks() {
    const bridge = this;
    const streams = new Map();

    const wisp_open = async (host, port) => {
      await bridge.ensureConnected();
      const stream = await bridge._client.createStream(host, port);
      const id = stream.id || Math.random().toString(36).slice(2);
      streams.set(id, stream);
      return id;
    };

    const wisp_send = async (streamId, data) => {
      const stream = streams.get(streamId);
      if (!stream) throw new Error(`stream ${streamId} not found`);
      await stream.send(data);
    };

    const wisp_recv = async (streamId) => {
      const stream = streams.get(streamId);
      if (!stream) throw new Error(`stream ${streamId} not found`);
      return await stream.recv();
    };

    const wisp_close = async (streamId) => {
      const stream = streams.get(streamId);
      if (stream) {
        try { stream.close(); } catch (_) {}
        streams.delete(streamId);
      }
    };

    return { wisp_open, wisp_send, wisp_recv, wisp_close };
  }

  destroy() {
    if (this._client) {
      try { this._client.close(); } catch (_) {}
      this._client = null;
    }
  }
}

// ─── AtuaNetClient ───────────────────────────────────────────────

export class AtuaNetClient {
  /**
   * @param {object} [options]
   * @param {string} [options.wispUrl='wss://relay.atua.dev/']
   * @param {number} [options.timeout=30000]
   * @param {number} [options.maxRedirects=10]
   * @param {boolean} [options.cookies=false]
   * @param {Array} [options.middleware=[]]
   * @param {object} [options.pins={}]
   * @param {string[]} [options.ca=[]]
   * @param {object} [options.tls={}]
   * @param {object} [options.retry={ maxAttempts: 1 }]
   */
  constructor(options = {}) {
    this.wispUrl = options.wispUrl || 'wss://relay.atua.dev/';
    this.timeout = options.timeout || 30000;
    this.maxRedirects = options.maxRedirects ?? 10;
    this.cookies = !!options.cookies;
    this.middleware = options.middleware || [];
    this.pins = options.pins || {};
    this.ca = options.ca || [];
    this.tls = options.tls || {};
    this.retry = options.retry || { maxAttempts: 1 };
    this.nativeWisp = !!options.nativeWisp;
    this._wasm = null;
    this._bridge = this.nativeWisp ? null : new WispBridge(this.wispUrl);
    this._circuitBreakers = new Map();
  }

  async _ensureWasm() {
    if (!this._wasm) this._wasm = await ensureWasm();
    return this._wasm;
  }

  _callbacks() {
    if (this.nativeWisp) {
      return { wisp_send: undefined, wisp_recv: undefined, wisp_open: undefined, wisp_close: undefined };
    }
    return this._bridge.createCallbacks();
  }

  _nativeParams() {
    if (this.nativeWisp) {
      return { use_native_wisp: true, wisp_url: this.wispUrl };
    }
    return { use_native_wisp: undefined, wisp_url: undefined };
  }

  _prepareBody(body) {
    if (body == null) return null;
    if (typeof body === 'string') return new TextEncoder().encode(body);
    return body instanceof Uint8Array ? body : new Uint8Array(body);
  }

  // ── Circuit Breaker ──────────────────────────────────────────

  _getCircuit(host) {
    if (!this._circuitBreakers.has(host)) {
      this._circuitBreakers.set(host, { failures: 0, lastFailure: 0, open: false });
    }
    return this._circuitBreakers.get(host);
  }

  _checkCircuit(url) {
    const match = url.match(/https?:\/\/([^/:]+)/);
    if (!match) return;
    const host = match[1];
    const cb = this._getCircuit(host);
    if (cb.open) {
      if (Date.now() - cb.lastFailure < 30000) {
        throw new Error(`circuit open for ${host}`);
      }
      cb.open = false; // cooldown passed
    }
  }

  _recordSuccess(url) {
    const match = url.match(/https?:\/\/([^/:]+)/);
    if (!match) return;
    const cb = this._getCircuit(match[1]);
    cb.failures = 0;
    cb.open = false;
  }

  _recordFailure(url) {
    const match = url.match(/https?:\/\/([^/:]+)/);
    if (!match) return;
    const cb = this._getCircuit(match[1]);
    cb.failures++;
    cb.lastFailure = Date.now();
    if (cb.failures >= 5) cb.open = true;
  }

  // ── Fetch with Retry + Backoff + Circuit Breaker ────────────

  /**
   * HTTPS fetch with E2E TLS encryption through Wisp relay.
   * Supports retry with exponential backoff and per-host circuit breaker.
   */
  async fetch(url, options = {}) {
    this._checkCircuit(url);

    // Run onRequest middleware
    const req = { url, ...options };
    for (const mw of this.middleware) {
      if (mw.onRequest) mw.onRequest(req);
    }
    // Apply any middleware mutations
    if (req.url !== url) url = req.url;
    if (req.headers) options.headers = req.headers;

    const retry = options.retry || this.retry;
    const maxAttempts = retry.maxAttempts || 1;
    const backoffBase = retry.backoffBaseMs || 1000;
    const retryStatuses = retry.retryOnStatus || [429, 502, 503, 504];

    let lastError = null;
    let lastResp = null;

    for (let attempt = 0; attempt < maxAttempts; attempt++) {
      if (attempt > 0) {
        const backoff = Math.min(backoffBase * Math.pow(2, attempt - 1), 30000);
        await new Promise(r => setTimeout(r, backoff));
      }

      try {
        const resp = await this._doFetch(url, options);
        this._recordSuccess(url);

        if (retryStatuses.includes(resp.status) && attempt < maxAttempts - 1) {
          lastResp = resp;
          continue;
        }

        // Run onResponse middleware
        for (const mw of this.middleware) {
          if (mw.onResponse) mw.onResponse(resp);
        }

        return resp;
      } catch (e) {
        this._recordFailure(url);
        lastError = e;
        if (attempt < maxAttempts - 1) continue;
      }
    }

    if (lastResp) return lastResp;
    throw lastError || new Error('fetch failed');
  }

  /**
   * Internal fetch — single attempt, no retry.
   */
  async _doFetch(url, options = {}) {
    const wasm = await this._ensureWasm();
    const cb = this._callbacks();

    const method = options.method || 'GET';
    const headers = options.headers || {};
    const headersJson = JSON.stringify(headers);
    const body = this._prepareBody(options.body);
    const timeoutMs = options.timeout_ms ?? this.timeout;
    const maxRedirects = options.maxRedirects ?? this.maxRedirects;

    const useCookies = options.cookies ?? this.cookies;
    const pins = options.pins || this.pins;
    const pinsJson = Object.keys(pins).length > 0 ? JSON.stringify(pins) : undefined;
    const customCaPem = this.ca.length > 0 ? this.ca.join('\n') : undefined;
    const tlsConfigJson = Object.keys(this.tls).length > 0 ? JSON.stringify(this.tls) : undefined;

    const np = this._nativeParams();
    const result = await wasm.atua_fetch(
      url, method, headersJson, body,
      cb.wisp_send, cb.wisp_recv, cb.wisp_open, cb.wisp_close,
      timeoutMs, maxRedirects, useCookies || undefined, pinsJson,
      customCaPem, tlsConfigJson,
      np.use_native_wisp, np.wisp_url,
    );

    return {
      status: result.status,
      headers: result.headers,
      body: result.body instanceof Uint8Array ? result.body : new Uint8Array(result.body),
      timing: result.timing || null,
    };
  }

  /**
   * Streaming fetch — delivers body chunks via onChunk callback.
   * Does not auto-add Accept-Encoding (sends uncompressed for SSE/NDJSON).
   */
  async fetchStreaming(url, options = {}) {
    const wasm = await this._ensureWasm();
    const cb = this._callbacks();

    const method = options.method || 'GET';
    const headers = options.headers || {};
    const headersJson = JSON.stringify(headers);
    const body = this._prepareBody(options.body);
    const timeoutMs = options.timeout_ms ?? this.timeout;
    const maxRedirects = options.maxRedirects ?? this.maxRedirects;
    const onChunk = options.onChunk || (() => {});

    const useCookies = options.cookies ?? this.cookies;
    const pins = options.pins || this.pins;
    const pinsJson = Object.keys(pins).length > 0 ? JSON.stringify(pins) : undefined;
    const customCaPem = this.ca.length > 0 ? this.ca.join('\n') : undefined;
    const tlsConfigJson = Object.keys(this.tls).length > 0 ? JSON.stringify(this.tls) : undefined;
    const np = this._nativeParams();

    const result = await wasm.atua_fetch_streaming(
      url, method, headersJson, body,
      cb.wisp_send, cb.wisp_recv, cb.wisp_open, cb.wisp_close,
      onChunk, timeoutMs, maxRedirects,
      useCookies || undefined, pinsJson, customCaPem, tlsConfigJson,
      np.use_native_wisp, np.wisp_url,
    );

    return {
      status: result.status,
      headers: result.headers,
    };
  }

  /**
   * Open a raw TCP/TLS stream through Wisp relay.
   */
  async connect(host, port, tls = true) {
    const wasm = await this._ensureWasm();
    const cb = this._callbacks();

    const np = this._nativeParams();
    const handle = await wasm.atua_connect(
      host, port, tls,
      cb.wisp_send, cb.wisp_recv, cb.wisp_open, cb.wisp_close,
      np.use_native_wisp, np.wisp_url,
    );

    const streamKey = handle.streamId;
    return {
      async send(data) {
        const arr = data instanceof Uint8Array ? data : new Uint8Array(data);
        await wasm.atua_stream_send(streamKey, arr);
      },
      async recv() {
        const data = await wasm.atua_stream_recv(streamKey);
        return data instanceof Uint8Array ? data : new Uint8Array(data);
      },
      close() {
        wasm.atua_stream_close(streamKey);
      },
    };
  }

  /**
   * Open a WebSocket connection through Wisp relay.
   */
  async websocket(url) {
    const wasm = await this._ensureWasm();
    const cb = this._callbacks();

    const np = this._nativeParams();
    const handle = await wasm.atua_websocket(
      url, cb.wisp_send, cb.wisp_recv, cb.wisp_open, cb.wisp_close,
      np.use_native_wisp, np.wisp_url,
    );

    const key = handle.streamId;
    return {
      async send(msg) { await wasm.atua_ws_send(key, msg); },
      async recv() {
        const data = await wasm.atua_ws_recv(key);
        return data === null ? null : data;
      },
      close() { wasm.atua_ws_close(key); },
    };
  }

  /**
   * Close all connections and clean up.
   */
  destroy() {
    if (this._bridge) this._bridge.destroy();
  }
}

// ─── Default Client + Backward-Compatible Exports ────────────────

const _default = new AtuaNetClient();

export async function atuaFetch(url, options = {}, wispUrl) {
  if (wispUrl && wispUrl !== _default.wispUrl) {
    const client = new AtuaNetClient({ wispUrl });
    return client.fetch(url, options);
  }
  return _default.fetch(url, options);
}

export { atuaFetch as fetch };

export async function connect(host, port, tls = true, wispUrl) {
  if (wispUrl && wispUrl !== _default.wispUrl) {
    const client = new AtuaNetClient({ wispUrl });
    return client.connect(host, port, tls);
  }
  return _default.connect(host, port, tls);
}
