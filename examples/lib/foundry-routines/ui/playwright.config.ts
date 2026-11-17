import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: './e2e',
  fullyParallel: false,
  workers: 1,
  retries: 0,
  timeout: 45_000,
  reporter: 'list',
  use: {
    baseURL: process.env.FOUNDRY_UI_URL || 'http://127.0.0.1:8780',
    headless: true,
    browserName: 'chromium',
    screenshot: 'only-on-failure',
    trace: 'retain-on-failure',
  },
});
