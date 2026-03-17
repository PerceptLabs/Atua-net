import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: './tests',
  timeout: 30000,
  retries: 0,
  use: {
    browserName: 'chromium',
    headless: true,
  },
  webServer: {
    command: 'node tests/serve.js',
    port: 3456,
    reuseExistingServer: true,
    timeout: 15000,
  },
});
