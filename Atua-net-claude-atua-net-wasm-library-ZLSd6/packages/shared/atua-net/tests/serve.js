/**
 * Test server: serves static files + runs a local Wisp relay.
 *
 * Static server: http://localhost:3456
 * Wisp relay:    ws://localhost:3457
 */
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { join, extname } from 'node:path';
import { fileURLToPath } from 'node:url';

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

// ── Static file server ─────────────────────────────────────────

const staticServer = createServer(async (req, res) => {
  let filePath;
  const url = req.url.split('?')[0];

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

// ── Local Wisp relay ───────────────────────────────────────────

try {
  const { server: wisp } = await import('@mercuryworkshop/wisp-js/server');

  const wispServer = createServer((req, res) => {
    res.writeHead(200, { 'Content-Type': 'text/plain' });
    res.end('Wisp relay');
  });

  wispServer.on('upgrade', (req, socket, head) => {
    wisp.routeRequest(req, socket, head);
  });

  wispServer.listen(3457, () => {
    console.log('Wisp relay on ws://localhost:3457/');
  });
} catch (e) {
  console.error('Failed to start Wisp relay:', e.message);
  console.error('Integration tests will not work without the relay.');
}
