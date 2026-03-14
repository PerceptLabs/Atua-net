/**
 * Simple static file server for Playwright tests.
 * Serves the test HTML page and WASM/JS assets.
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

const server = createServer(async (req, res) => {
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
      'Cross-Origin-Opener-Policy': 'same-origin',
      'Cross-Origin-Embedder-Policy': 'require-corp',
    });
    res.end(content);
  } catch {
    res.writeHead(404);
    res.end('Not found');
  }
});

server.listen(3456, () => {
  console.log('Test server running on http://localhost:3456');
});
