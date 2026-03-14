/**
 * @aspect/atua-net — Browser WASM networking library
 *
 * Provides fetch() and connect() that bypass CORS via Wisp relay,
 * with end-to-end TLS encryption in the browser's WASM sandbox.
 */

let wasmModule = null;
let wispClients = new Map(); // keyed by wispUrl

/**
 * Initialize the WASM module. Safe to call multiple times.
 */
async function ensureWasm() {
  if (wasmModule) return wasmModule;

  // Dynamic import of the wasm-pack generated bindings
  const wasm = await import('../wasm-pkg/atua_net.js');
  await wasm.default();
  wasmModule = wasm;
  return wasmModule;
}

/**
 * Get or create a Wisp client for the given relay URL.
 * @param {string} wispUrl - WebSocket URL of the Wisp relay
 * @returns {Promise<object>} - Wisp client with stream management
 */
async function ensureWispClient(wispUrl) {
  if (wispClients.has(wispUrl)) {
    const client = wispClients.get(wispUrl);
    if (client.connected) return client;
    wispClients.delete(wispUrl);
  }

  // Import wisp-client-js
  const { WispClient } = await import('wisp-client-js');
  const client = new WispClient(wispUrl);
  await client.connect();
  client.connected = true;

  wispClients.set(wispUrl, client);
  return client;
}

/**
 * Create Wisp I/O callbacks bound to a specific client.
 * These are passed into the WASM module for TLS + HTTP operations.
 */
function createWispCallbacks(wispClient) {
  const streams = new Map();

  const wisp_open = async (host, port) => {
    const stream = await wispClient.createStream(host, port);
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
    const data = await stream.recv();
    return data;
  };

  const wisp_close = async (streamId) => {
    const stream = streams.get(streamId);
    if (stream) {
      try { stream.close(); } catch (_) { /* ignore */ }
      streams.delete(streamId);
    }
  };

  return { wisp_open, wisp_send, wisp_recv, wisp_close, streams };
}

/**
 * HTTPS fetch that bypasses CORS via Wisp relay with end-to-end TLS.
 *
 * @param {string} url - The HTTPS URL to fetch
 * @param {object} [options] - Fetch options
 * @param {string} [options.method='GET'] - HTTP method
 * @param {Record<string, string>} [options.headers={}] - Request headers
 * @param {string|Uint8Array} [options.body] - Request body
 * @param {string} [wispUrl='wss://relay.atua.dev/'] - Wisp relay URL
 * @returns {Promise<{status: number, headers: Record<string, string>, body: Uint8Array}>}
 */
export async function atuaFetch(url, options = {}, wispUrl = 'wss://relay.atua.dev/') {
  const wasm = await ensureWasm();
  const wisp = await ensureWispClient(wispUrl);
  const callbacks = createWispCallbacks(wisp);

  const method = options.method || 'GET';
  const headers = options.headers || {};
  const headersJson = JSON.stringify(headers);

  let body = null;
  if (options.body != null) {
    if (typeof options.body === 'string') {
      body = new TextEncoder().encode(options.body);
    } else {
      body = options.body instanceof Uint8Array
        ? options.body
        : new Uint8Array(options.body);
    }
  }

  const result = await wasm.atua_fetch(
    url,
    method,
    headersJson,
    body,
    callbacks.wisp_send,
    callbacks.wisp_recv,
    callbacks.wisp_open,
    callbacks.wisp_close,
  );

  return {
    status: result.status,
    headers: result.headers,
    body: result.body instanceof Uint8Array
      ? result.body
      : new Uint8Array(result.body),
  };
}

// Also export as `fetch` for convenience
export { atuaFetch as fetch };

/**
 * Open a raw TCP/TLS stream through Wisp relay.
 *
 * @param {string} host - Destination hostname
 * @param {number} port - Destination port
 * @param {boolean} [tls=true] - Whether to use TLS
 * @param {string} [wispUrl='wss://relay.atua.dev/'] - Wisp relay URL
 * @returns {Promise<{send(data: Uint8Array): Promise<void>, recv(): Promise<Uint8Array>, close(): void}>}
 */
export async function connect(host, port, tls = true, wispUrl = 'wss://relay.atua.dev/') {
  const wasm = await ensureWasm();
  const wisp = await ensureWispClient(wispUrl);
  const callbacks = createWispCallbacks(wisp);

  const handle = await wasm.atua_connect(
    host,
    port,
    tls,
    callbacks.wisp_send,
    callbacks.wisp_recv,
    callbacks.wisp_open,
    callbacks.wisp_close,
  );

  const streamId = handle.streamId;

  return {
    async send(data) {
      const arr = data instanceof Uint8Array ? data : new Uint8Array(data);
      await callbacks.wisp_send(streamId, arr);
    },
    async recv() {
      const data = await callbacks.wisp_recv(streamId);
      return data instanceof Uint8Array ? data : new Uint8Array(data);
    },
    close() {
      callbacks.wisp_close(streamId);
    },
  };
}
