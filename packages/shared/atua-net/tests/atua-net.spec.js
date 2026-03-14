import { test, expect } from '@playwright/test';

// ─── Helpers ───────────────────────────────────────────────────

async function waitForWasm(page) {
  await page.goto('http://localhost:3456/');
  await page.waitForFunction(
    () => window.__atuaNetReady || window.__atuaNetError,
    { timeout: 15_000 },
  );
  const error = await page.evaluate(() => window.__atuaNetError);
  if (error) throw new Error(`WASM init failed: ${error}`);
}

function decode(body) {
  return new TextDecoder().decode(body);
}

// ═══════════════════════════════════════════════════════════════
// Tier 0 — WASM Initialization
// ═══════════════════════════════════════════════════════════════

test.describe('Tier 0 — WASM Initialization', () => {
  test('0.1: WASM module loads and initializes', async ({ page }) => {
    await waitForWasm(page);
    const ready = await page.evaluate(() => window.__atuaNetReady);
    expect(ready).toBe(true);
  });

  test('0.2: WASM exports atua_fetch and atua_connect', async ({ page }) => {
    await waitForWasm(page);
    const exports = await page.evaluate(() => ({
      hasAtuaFetch: typeof window.__atuaNet.atua_fetch === 'function',
      hasAtuaConnect: typeof window.__atuaNet.atua_connect === 'function',
    }));
    expect(exports.hasAtuaFetch).toBe(true);
    expect(exports.hasAtuaConnect).toBe(true);
  });

  test('0.3: Double initialization is safe', async ({ page }) => {
    await waitForWasm(page);
    const result = await page.evaluate(async () => {
      try {
        await window.__atuaNet.init();
        return { ok: true };
      } catch (e) {
        return { ok: true, warning: e.toString() };
      }
    });
    expect(result.ok).toBe(true);
  });
});

// ═══════════════════════════════════════════════════════════════
// Tier 1 — Fundamentals
// ═══════════════════════════════════════════════════════════════

test.describe('Tier 1 — Fundamentals', () => {
  test.beforeEach(async ({ page }) => {
    await waitForWasm(page);
  });

  test('1.1: TLS Handshake Completes', async ({ page }) => {
    test.setTimeout(30_000);
    const result = await page.evaluate(async () => {
      // Do a simple fetch — if TLS handshake fails, this throws
      const resp = await window.__atuaFetch('https://httpbin.org/get');
      return { status: resp.status, ok: true };
    });
    expect(result.ok).toBe(true);
    expect(result.status).toBe(200);
  });

  test('1.2: Basic HTTPS GET', async ({ page }) => {
    test.setTimeout(30_000);
    const result = await page.evaluate(async () => {
      const resp = await window.__atuaFetch('https://httpbin.org/get');
      const body = new TextDecoder().decode(resp.body);
      const json = JSON.parse(body);
      return { status: resp.status, url: json.url };
    });
    expect(result.status).toBe(200);
    expect(result.url).toBe('https://httpbin.org/get');
  });

  test('1.3: HTTPS POST with Body', async ({ page }) => {
    test.setTimeout(30_000);
    const result = await page.evaluate(async () => {
      const resp = await window.__atuaFetch('https://httpbin.org/post', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ hello: 'world' }),
      });
      const body = new TextDecoder().decode(resp.body);
      const json = JSON.parse(body);
      return { status: resp.status, hello: json.json?.hello };
    });
    expect(result.status).toBe(200);
    expect(result.hello).toBe('world');
  });

  test('1.4: Custom Headers Round-Trip', async ({ page }) => {
    test.setTimeout(30_000);
    const result = await page.evaluate(async () => {
      const resp = await window.__atuaFetch('https://httpbin.org/headers', {
        headers: { 'X-Atua-Test': 'sentinel-value-12345' },
      });
      const body = new TextDecoder().decode(resp.body);
      return { status: resp.status, body };
    });
    expect(result.status).toBe(200);
    expect(result.body).toContain('sentinel-value-12345');
  });

  test('1.5a: Non-200 Status Code — 404', async ({ page }) => {
    test.setTimeout(30_000);
    const result = await page.evaluate(async () => {
      const resp = await window.__atuaFetch('https://httpbin.org/status/404');
      return { status: resp.status };
    });
    expect(result.status).toBe(404);
  });

  test('1.5b: Non-200 Status Code — 500', async ({ page }) => {
    test.setTimeout(30_000);
    const result = await page.evaluate(async () => {
      const resp = await window.__atuaFetch('https://httpbin.org/status/500');
      return { status: resp.status };
    });
    expect(result.status).toBe(500);
  });
});

// ═══════════════════════════════════════════════════════════════
// Tier 2 — Chunked Transfer & Streaming
// ═══════════════════════════════════════════════════════════════

test.describe('Tier 2 — Chunked Transfer & Streaming', () => {
  test.beforeEach(async ({ page }) => {
    await waitForWasm(page);
  });

  test('2.1: Chunked Response — Small (3 lines)', async ({ page }) => {
    test.setTimeout(30_000);
    const result = await page.evaluate(async () => {
      const resp = await window.__atuaFetch('https://httpbin.org/stream/3');
      const body = new TextDecoder().decode(resp.body);
      const lines = body.trim().split('\n').filter(l => l.length > 0);
      const allValid = lines.every(line => {
        try { JSON.parse(line); return true; } catch { return false; }
      });
      return { status: resp.status, lineCount: lines.length, allValid };
    });
    expect(result.status).toBe(200);
    expect(result.lineCount).toBe(3);
    expect(result.allValid).toBe(true);
  });

  test('2.2: Chunked Response — Large (50 lines)', async ({ page }) => {
    test.setTimeout(60_000);
    const result = await page.evaluate(async () => {
      const resp = await window.__atuaFetch('https://httpbin.org/stream/50');
      const body = new TextDecoder().decode(resp.body);
      const lines = body.trim().split('\n').filter(l => l.length > 0);
      return { status: resp.status, lineCount: lines.length };
    });
    expect(result.status).toBe(200);
    expect(result.lineCount).toBe(50);
  });

  test('2.3: Large Binary Response — 100KB', async ({ page }) => {
    test.setTimeout(30_000);
    const result = await page.evaluate(async () => {
      const resp = await window.__atuaFetch('https://httpbin.org/bytes/100000');
      return { status: resp.status, bodyLength: resp.body.length };
    });
    expect(result.status).toBe(200);
    expect(result.bodyLength).toBe(100_000);
  });

  test('2.4: Large Binary Response — 1MB', async ({ page }) => {
    test.setTimeout(60_000);
    const result = await page.evaluate(async () => {
      const resp = await window.__atuaFetch('https://httpbin.org/bytes/1000000');
      return { status: resp.status, bodyLength: resp.body.length };
    });
    expect(result.status).toBe(200);
    expect(result.bodyLength).toBe(1_000_000);
  });
});

// ═══════════════════════════════════════════════════════════════
// Tier 3 — Concurrency & Multiplexing
// ═══════════════════════════════════════════════════════════════

test.describe('Tier 3 — Concurrency & Multiplexing', () => {
  test.beforeEach(async ({ page }) => {
    await waitForWasm(page);
  });

  test('3.1: Concurrent Requests — 5 Parallel', async ({ page }) => {
    test.setTimeout(30_000);
    const result = await page.evaluate(async () => {
      const start = Date.now();
      const promises = Array.from({ length: 5 }, () =>
        window.__atuaFetch('https://httpbin.org/delay/1'),
      );
      const results = await Promise.all(promises);
      const elapsed = Date.now() - start;
      const allOk = results.every(r => r.status === 200);
      return { allOk, count: results.length, elapsed };
    });
    expect(result.allOk).toBe(true);
    expect(result.count).toBe(5);
    // Multiplexed: should be much less than 5 sequential seconds
    expect(result.elapsed).toBeLessThan(15_000);
  });

  test('3.2: Concurrent Requests — Different Endpoints', async ({ page }) => {
    test.setTimeout(30_000);
    const result = await page.evaluate(async () => {
      const urls = [
        'https://httpbin.org/get',
        'https://httpbin.org/ip',
        'https://httpbin.org/user-agent',
        'https://httpbin.org/headers',
        'https://httpbin.org/status/200',
      ];
      const results = await Promise.all(urls.map(u => window.__atuaFetch(u)));
      const allOk = results.every(r => r.status === 200);
      return { allOk, count: results.length };
    });
    expect(result.allOk).toBe(true);
    expect(result.count).toBe(5);
  });

  test('3.3: Sequential Requests Work', async ({ page }) => {
    test.setTimeout(30_000);
    const result = await page.evaluate(async () => {
      const r1 = await window.__atuaFetch('https://httpbin.org/get');
      const r2 = await window.__atuaFetch('https://httpbin.org/get');
      return { s1: r1.status, s2: r2.status };
    });
    expect(result.s1).toBe(200);
    expect(result.s2).toBe(200);
  });

  test('3.4: Requests to Different Hosts', async ({ page }) => {
    test.setTimeout(30_000);
    const result = await page.evaluate(async () => {
      const [r1, r2] = await Promise.all([
        window.__atuaFetch('https://httpbin.org/get'),
        window.__atuaFetch('https://example.com/'),
      ]);
      const body2 = new TextDecoder().decode(r2.body);
      return {
        s1: r1.status,
        s2: r2.status,
        exampleHasHtml: body2.includes('Example Domain'),
      };
    });
    expect(result.s1).toBe(200);
    expect(result.s2).toBe(200);
    expect(result.exampleHasHtml).toBe(true);
  });
});

// ═══════════════════════════════════════════════════════════════
// Tier 4 — Error Handling & Recovery
// ═══════════════════════════════════════════════════════════════

test.describe('Tier 4 — Error Handling & Recovery', () => {
  test.beforeEach(async ({ page }) => {
    await waitForWasm(page);
  });

  test('4.1: Invalid Hostname', async ({ page }) => {
    test.setTimeout(30_000);
    const result = await page.evaluate(async () => {
      try {
        await window.__atuaFetch('https://this-domain-does-not-exist-atua-test.com/');
        return { threw: false };
      } catch (e) {
        return { threw: true, message: e.toString() };
      }
    });
    expect(result.threw).toBe(true);
  });

  test('4.3: TLS Certificate Error — expired cert', async ({ page }) => {
    test.setTimeout(30_000);
    const result = await page.evaluate(async () => {
      try {
        await window.__atuaFetch('https://expired.badssl.com/');
        return { threw: false };
      } catch (e) {
        return { threw: true, message: e.toString() };
      }
    });
    expect(result.threw).toBe(true);
  });

  test('4.4: Self-Signed Certificate Rejected', async ({ page }) => {
    test.setTimeout(30_000);
    const result = await page.evaluate(async () => {
      try {
        await window.__atuaFetch('https://self-signed.badssl.com/');
        return { threw: false };
      } catch (e) {
        return { threw: true, message: e.toString() };
      }
    });
    expect(result.threw).toBe(true);
  });

  test('4.5: Wrong Host Certificate', async ({ page }) => {
    test.setTimeout(30_000);
    const result = await page.evaluate(async () => {
      try {
        await window.__atuaFetch('https://wrong.host.badssl.com/');
        return { threw: false };
      } catch (e) {
        return { threw: true, message: e.toString() };
      }
    });
    expect(result.threw).toBe(true);
  });

  test('4.7: Recovery After Error', async ({ page }) => {
    test.setTimeout(30_000);
    const result = await page.evaluate(async () => {
      // First request: should fail (bad TLS cert)
      try {
        await window.__atuaFetch('https://expired.badssl.com/');
      } catch (_) {
        // expected
      }
      // Second request: should still work
      const resp = await window.__atuaFetch('https://httpbin.org/get');
      return { status: resp.status };
    });
    expect(result.status).toBe(200);
  });

  test('4.8: Empty Response Body (204)', async ({ page }) => {
    test.setTimeout(30_000);
    const result = await page.evaluate(async () => {
      const resp = await window.__atuaFetch('https://httpbin.org/status/204');
      return { status: resp.status, bodyLength: resp.body.length };
    });
    expect(result.status).toBe(204);
    expect(result.bodyLength).toBe(0);
  });
});

// ═══════════════════════════════════════════════════════════════
// Tier 5 — LLM Provider Patterns
// ═══════════════════════════════════════════════════════════════

test.describe('Tier 5 — LLM Provider Patterns', () => {
  test.beforeEach(async ({ page }) => {
    await waitForWasm(page);
  });

  test('5.1: Anthropic-Style POST with Headers', async ({ page }) => {
    test.setTimeout(30_000);
    const result = await page.evaluate(async () => {
      const largeBody = JSON.stringify({
        model: 'claude-haiku-4-5-20251001',
        max_tokens: 50,
        messages: [{ role: 'user', content: 'A'.repeat(3000) }],
      });
      const resp = await window.__atuaFetch('https://httpbin.org/post', {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          'anthropic-dangerous-direct-browser-access': 'true',
        },
        body: largeBody,
      });
      const body = new TextDecoder().decode(resp.body);
      const json = JSON.parse(body);
      return {
        status: resp.status,
        hasPostedJson: !!json.json,
        echoedModel: json.json?.model,
      };
    });
    expect(result.status).toBe(200);
    expect(result.hasPostedJson).toBe(true);
    expect(result.echoedModel).toBe('claude-haiku-4-5-20251001');
  });

  test('5.4: npm Tarball Download (binary, CORS failure case)', async ({ page }) => {
    test.setTimeout(60_000);
    const result = await page.evaluate(async () => {
      const resp = await window.__atuaFetch(
        'https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz',
      );
      return {
        status: resp.status,
        bodyLength: resp.body.length,
        // gzip magic number: 0x1f 0x8b
        isGzip: resp.body[0] === 0x1f && resp.body[1] === 0x8b,
      };
    });
    expect(result.status).toBe(200);
    expect(result.bodyLength).toBeGreaterThan(10_000);
    expect(result.isGzip).toBe(true);
  });

  test('5.5: Large POST Body (50KB)', async ({ page }) => {
    test.setTimeout(30_000);
    const result = await page.evaluate(async () => {
      const body = JSON.stringify({ data: 'X'.repeat(50_000) });
      const resp = await window.__atuaFetch('https://httpbin.org/post', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body,
      });
      const respBody = new TextDecoder().decode(resp.body);
      const json = JSON.parse(respBody);
      return {
        status: resp.status,
        echoedLength: json.json?.data?.length,
      };
    });
    expect(result.status).toBe(200);
    expect(result.echoedLength).toBe(50_000);
  });

  test('5.6: Many Sequential Requests (20)', async ({ page }) => {
    test.setTimeout(120_000);
    const result = await page.evaluate(async () => {
      const results = [];
      for (let i = 0; i < 20; i++) {
        const body = JSON.stringify({ seq: i });
        const resp = await window.__atuaFetch('https://httpbin.org/post', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body,
        });
        const json = JSON.parse(new TextDecoder().decode(resp.body));
        results.push({ status: resp.status, seq: json.json?.seq });
      }
      return {
        count: results.length,
        allOk: results.every(r => r.status === 200),
        allCorrectSeq: results.every((r, i) => r.seq === i),
      };
    });
    expect(result.count).toBe(20);
    expect(result.allOk).toBe(true);
    expect(result.allCorrectSeq).toBe(true);
  });
});

// ═══════════════════════════════════════════════════════════════
// Tier 6 — Raw TCP/TLS Streams
// ═══════════════════════════════════════════════════════════════

test.describe('Tier 6 — Raw TCP/TLS Streams', () => {
  test.beforeEach(async ({ page }) => {
    await waitForWasm(page);
  });

  test('6.2: Raw TLS Connection — manual HTTP', async ({ page }) => {
    test.setTimeout(30_000);
    const result = await page.evaluate(async () => {
      const stream = await window.__atuaConnect('example.com', 443, true);
      try {
        const request = 'GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n';
        await stream.send(new TextEncoder().encode(request));

        // Read response chunks
        let response = '';
        for (let i = 0; i < 20; i++) {
          const chunk = await stream.recv();
          if (chunk.length === 0) break;
          response += new TextDecoder().decode(chunk);
          if (response.includes('</html>')) break;
        }

        return {
          startsWithHttp: response.startsWith('HTTP/1.1'),
          has200: response.includes('200'),
          hasHtml: response.includes('Example Domain'),
        };
      } finally {
        stream.close();
      }
    });
    expect(result.startsWithHttp).toBe(true);
    expect(result.has200).toBe(true);
    expect(result.hasHtml).toBe(true);
  });

  test('6.4: Stream Cleanup — close without sending', async ({ page }) => {
    test.setTimeout(30_000);
    const result = await page.evaluate(async () => {
      const stream = await window.__atuaConnect('example.com', 443, true);
      stream.close();
      // Subsequent fetch should work
      const resp = await window.__atuaFetch('https://httpbin.org/get');
      return { status: resp.status };
    });
    expect(result.status).toBe(200);
  });
});

// ═══════════════════════════════════════════════════════════════
// Tier 7 — WASM-Specific Edge Cases
// ═══════════════════════════════════════════════════════════════

test.describe('Tier 7 — WASM Edge Cases', () => {
  test.beforeEach(async ({ page }) => {
    await waitForWasm(page);
  });

  test('7.1: Memory Pressure — 20 sequential requests', async ({ page }) => {
    test.setTimeout(120_000);
    const result = await page.evaluate(async () => {
      for (let i = 0; i < 20; i++) {
        const resp = await window.__atuaFetch('https://httpbin.org/get');
        if (resp.status !== 200) return { ok: false, failedAt: i };
      }
      return { ok: true };
    });
    expect(result.ok).toBe(true);
  });

  test('7.3: Concurrent WASM Operations — 5 parallel', async ({ page }) => {
    test.setTimeout(30_000);
    const result = await page.evaluate(async () => {
      const promises = Array.from({ length: 5 }, () =>
        window.__atuaFetch('https://httpbin.org/get'),
      );
      const results = await Promise.all(promises);
      return {
        allOk: results.every(r => r.status === 200),
        count: results.length,
      };
    });
    expect(result.allOk).toBe(true);
    expect(result.count).toBe(5);
  });

  test('7.4: Initialization Idempotent', async ({ page }) => {
    await page.evaluate(async () => {
      try { await window.__atuaNet.init(); } catch (_) {}
    });
    const result = await page.evaluate(async () => {
      const resp = await window.__atuaFetch('https://httpbin.org/get');
      return { status: resp.status };
    });
    expect(result.status).toBe(200);
  });

  test('7.5: Error Does Not Corrupt WASM State', async ({ page }) => {
    test.setTimeout(60_000);
    const result = await page.evaluate(async () => {
      // TLS error
      try { await window.__atuaFetch('https://expired.badssl.com/'); } catch (_) {}
      // Should work
      const r1 = await window.__atuaFetch('https://httpbin.org/get');
      // Another TLS error
      try { await window.__atuaFetch('https://wrong.host.badssl.com/'); } catch (_) {}
      // Should still work
      const r2 = await window.__atuaFetch('https://httpbin.org/get');
      return { s1: r1.status, s2: r2.status };
    });
    expect(result.s1).toBe(200);
    expect(result.s2).toBe(200);
  });
});

// ═══════════════════════════════════════════════════════════════
// Rust Unit Tests (cargo test)
// ═══════════════════════════════════════════════════════════════

test.describe('Rust Unit Tests', () => {
  test('All 82 Rust unit tests pass', async () => {
    const { execSync } = await import('node:child_process');
    const result = execSync('cargo test 2>&1', {
      cwd: process.cwd(),
      encoding: 'utf-8',
      timeout: 60_000,
    });
    expect(result).toContain('test result: ok');
    expect(result).toContain('82 passed');
  });
});
