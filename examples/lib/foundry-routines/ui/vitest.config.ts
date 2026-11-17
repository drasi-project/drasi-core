import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: { environment: 'jsdom', clearMocks: true, include: ['tests/**/*.test.{ts,tsx}'] },
});
