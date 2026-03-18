/**
 * Test server: static files + local binary endpoint + self-contained Wisp v1 relay
 * with fault injection for Tier 19 chaos tests.
 *
 * Static server: http://localhost:3456
 * Wisp relay:    ws://localhost:3457
 */
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { join, extname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { WebSocketServer } from 'ws';
import net from 'node:net';
import dgram from 'node:dgram';

const __dirname = fileURLToPath(new URL('.', import.meta.url));
const ROOT = join(__dirname, '..');

const MIME_TYPES = {
  '.html': 'text/html',
  '.js': 'application/javascript',
  '.mjs': 'application/javascript',
  '.wasm': 'application/wasm',
  '.json': 'application/json',
  '.css': 'text/css',
};

// ── Relay Fault Injection State ──────────────────────────────

const relayControl = {
  dropStream: false,       // destroy next TCP stream after 1024 bytes
  stallStream: false,      // buffer next stream's data for 5s before forwarding
  killWebSocket: false,    // close WS 500ms after next CONNECT
  sendGarbage: false,      // send malformed frame alongside next DATA
};

// ── Static file server + control routes + binary endpoint ────

const staticServer = createServer(async (req, res) => {
  const url = req.url.split('?')[0];

  // Relay control routes
  if (url === '/relay/drop-stream') {
    relayControl.dropStream = true;
    res.writeHead(200, { 'Access-Control-Allow-Origin': '*' });
    res.end('ok');
    return;
  }
  if (url === '/relay/stall-stream') {
    relayControl.stallStream = true;
    res.writeHead(200, { 'Access-Control-Allow-Origin': '*' });
    res.end('ok');
    return;
  }
  if (url === '/relay/kill-websocket') {
    relayControl.killWebSocket = true;
    res.writeHead(200, { 'Access-Control-Allow-Origin': '*' });
    res.end('ok');
    return;
  }
  if (url === '/relay/send-garbage') {
    relayControl.sendGarbage = true;
    res.writeHead(200, { 'Access-Control-Allow-Origin': '*' });
    res.end('ok');
    return;
  }
  if (url === '/relay/reset') {
    relayControl.dropStream = false;
    relayControl.stallStream = false;
    relayControl.killWebSocket = false;
    relayControl.sendGarbage = false;
    res.writeHead(200, { 'Access-Control-Allow-Origin': '*' });
    res.end('ok');
    return;
  }

  // Soak test page
  if (url === '/soak') {
    try {
      const content = await readFile(join(__dirname, 'soak.html'));
      res.writeHead(200, { 'Content-Type': 'text/html', 'Access-Control-Allow-Origin': '*' });
      res.end(content);
      return;
    } catch {
      res.writeHead(404);
      res.end('soak.html not found');
      return;
    }
  }

  // Local /bytes/:n endpoint for large transfer tests
  const bytesMatch = url.match(/^\/bytes\/(\d+)$/);
  if (bytesMatch) {
    const n = parseInt(bytesMatch[1], 10);
    if (n > 0 && n <= 10_000_000) {
      const data = Buffer.alloc(n);
      for (let i = 0; i < n; i++) data[i] = i & 0xff;
      res.writeHead(200, {
        'Content-Type': 'application/octet-stream',
        'Content-Length': n.toString(),
        'Access-Control-Allow-Origin': '*',
      });
      res.end(data);
      return;
    }
  }

  let filePath;
  if (url === '/' || url === '/index.html') {
    filePath = join(__dirname, 'index.html');
  } else {
    filePath = join(ROOT, url);
  }

  try {
    const content = await readFile(filePath);
    const ext = extname(filePath);
    const mime = MIME_TYPES[ext] || 'application/octet-stream';
    res.writeHead(200, {
      'Content-Type': mime,
      'Access-Control-Allow-Origin': '*',
    });
    res.end(content);
  } catch {
    res.writeHead(404);
    res.end(`Not found: ${url}`);
  }
});

staticServer.listen(3456, () => {
  console.log('Static server on http://localhost:3456');
});

// ── Wisp v1 Relay (self-contained, with fault injection) ────

const BUFFER_SIZE = 128;
const CONTINUE_INTERVAL = 64;

function encodeFrame(type, streamId, payload) {
  const buf = Buffer.alloc(5 + payload.length);
  buf[0] = type;
  buf.writeUInt32LE(streamId, 1);
  payload.copy(buf, 5);
  return buf;
}

function encodeClose(streamId, reason) {
  return encodeFrame(0x04, streamId, Buffer.from([reason]));
}

function encodeContinue(streamId, bufferRemaining) {
  const payload = Buffer.alloc(4);
  payload.writeUInt32LE(bufferRemaining, 0);
  return encodeFrame(0x03, streamId, payload);
}

function encodeData(streamId, data) {
  return encodeFrame(0x02, streamId, data);
}

function parseFrame(buf) {
  if (buf.length < 5) return null;
  const type = buf[0];
  const streamId = buf.readUInt32LE(1);
  const payload = buf.subarray(5);
  return { type, streamId, payload };
}

function mapErrorToCloseReason(code) {
  switch (code) {
    case 'ENOTFOUND': return 0x41;
    case 'ECONNREFUSED': return 0x42;
    case 'ETIMEDOUT': return 0x43;
    case 'EADDRNOTAVAIL': return 0x44;
    case 'ECONNRESET': return 0x47;
    default: return 0x03;
  }
}

function handleConnection(ws) {
  const streams = new Map();

  // Send initial CONTINUE (stream_id=0)
  ws.send(encodeContinue(0, BUFFER_SIZE));

  ws.on('message', (data) => {
    const buf = Buffer.from(data);
    const frame = parseFrame(buf);
    if (!frame) return;

    switch (frame.type) {
      case 0x01: { // CONNECT
        if (frame.payload.length < 3) return;
        const streamType = frame.payload[0];
        const port = frame.payload.readUInt16LE(1);
        const hostname = frame.payload.subarray(3).toString('utf-8').trim();

        // Fault: kill-websocket — schedule WS close after this CONNECT
        if (relayControl.killWebSocket) {
          relayControl.killWebSocket = false;
          setTimeout(() => { try { ws.close(); } catch (_) {} }, 500);
        }

        // Check if this stream should be faulted
        const shouldDrop = relayControl.dropStream;
        const shouldStall = relayControl.stallStream;
        if (shouldDrop) relayControl.dropStream = false;
        if (shouldStall) relayControl.stallStream = false;

        if (streamType === 0x01) {
          // TCP
          const socket = net.createConnection({ port, host: hostname, family: 4 });
          const entry = { socket, type: 'tcp', framesSent: 0, bytesForwarded: 0, stalling: shouldStall, stallBuffer: [] };
          streams.set(frame.streamId, entry);

          socket.on('data', (chunk) => {
            if (ws.readyState !== 1) return;

            // Fault: drop-stream — destroy after threshold
            if (shouldDrop) {
              entry.bytesForwarded += chunk.length;
              if (entry.bytesForwarded > 1024) {
                socket.destroy();
                if (ws.readyState === 1) ws.send(encodeClose(frame.streamId, 0x03));
                streams.delete(frame.streamId);
                return;
              }
            }

            // Fault: stall-stream — buffer data, flush after 5s
            if (entry.stalling) {
              entry.stallBuffer.push(chunk);
              if (!entry.stallTimer) {
                entry.stallTimer = setTimeout(() => {
                  entry.stalling = false;
                  for (const buffered of entry.stallBuffer) {
                    if (ws.readyState === 1) {
                      ws.send(encodeData(frame.streamId, buffered));
                      entry.framesSent++;
                    }
                  }
                  entry.stallBuffer = [];
                  if (entry.framesSent % CONTINUE_INTERVAL === 0 && ws.readyState === 1) {
                    ws.send(encodeContinue(frame.streamId, BUFFER_SIZE));
                  }
                }, 5000);
              }
              return;
            }

            // Fault: send-garbage — inject malformed frame
            if (relayControl.sendGarbage) {
              relayControl.sendGarbage = false;
              ws.send(Buffer.from([0xFF, 0x00, 0x00, 0x00, 0x00, 0xDE, 0xAD]));
            }

            ws.send(encodeData(frame.streamId, chunk));
            entry.framesSent++;
            if (entry.framesSent % CONTINUE_INTERVAL === 0) {
              ws.send(encodeContinue(frame.streamId, BUFFER_SIZE));
            }
          });

          socket.on('end', () => {
            if (ws.readyState === 1) ws.send(encodeClose(frame.streamId, 0x02));
            streams.delete(frame.streamId);
          });

          socket.on('close', () => {
            streams.delete(frame.streamId);
          });

          socket.on('error', (err) => {
            const reason = mapErrorToCloseReason(err.code);
            if (ws.readyState === 1) ws.send(encodeClose(frame.streamId, reason));
            streams.delete(frame.streamId);
          });
        } else if (streamType === 0x02) {
          // UDP
          const socket = dgram.createSocket('udp4');
          const entry = { socket, type: 'udp', framesSent: 0 };
          streams.set(frame.streamId, entry);

          socket.on('message', (msg) => {
            if (ws.readyState !== 1) return;
            ws.send(encodeData(frame.streamId, msg));
            entry.framesSent++;
            if (entry.framesSent % CONTINUE_INTERVAL === 0) {
              ws.send(encodeContinue(frame.streamId, BUFFER_SIZE));
            }
          });

          socket.on('error', (err) => {
            const reason = mapErrorToCloseReason(err.code);
            if (ws.readyState === 1) ws.send(encodeClose(frame.streamId, reason));
            streams.delete(frame.streamId);
          });

          socket.bind(() => { socket.connect(port, hostname); });
        }
        break;
      }

      case 0x02: { // DATA
        const entry = streams.get(frame.streamId);
        if (!entry) return;
        if (entry.type === 'tcp') {
          entry.socket.write(frame.payload);
        } else {
          entry.socket.send(frame.payload);
        }
        break;
      }

      case 0x04: { // CLOSE
        const entry = streams.get(frame.streamId);
        if (!entry) return;
        if (entry.type === 'tcp') {
          entry.socket.destroy();
        } else {
          entry.socket.close();
        }
        streams.delete(frame.streamId);
        break;
      }
    }
  });

  ws.on('close', () => {
    for (const [, entry] of streams) {
      if (entry.type === 'tcp') entry.socket.destroy();
      else try { entry.socket.close(); } catch (_) {}
    }
    streams.clear();
  });

  ws.on('error', () => {
    for (const [, entry] of streams) {
      if (entry.type === 'tcp') entry.socket.destroy();
      else try { entry.socket.close(); } catch (_) {}
    }
    streams.clear();
  });
}

const wispServer = createServer((req, res) => {
  res.writeHead(200, { 'Content-Type': 'text/plain' });
  res.end('Wisp relay');
});

const wss = new WebSocketServer({ server: wispServer });
wss.on('connection', handleConnection);

wispServer.listen(3457, () => {
  console.log('Wisp v1 relay on ws://localhost:3457/');
});
