import { defineConfig } from 'vitest/config'
import { fileURLToPath, URL } from 'node:url'

export default defineConfig({
  resolve: {
    alias: { '@': fileURLToPath(new URL('./src', import.meta.url)) },
  },
  test: {
    environment: 'jsdom',
    exclude: ['e2e/**', 'node_modules/**', 'dist/**'],
    coverage: {
      provider: 'v8',
      reporter: ['text', 'json-summary'],
      include: [
        'src/shared/lib/http.ts',
        'src/shared/simulation/simulation-provider.tsx',
        'src/i18n/format.ts',
        'src/i18n/load.ts',
        'src/i18n/locale.ts',
        'src/lib/network.ts',
        'src/theme/theme.tsx',
      ],
      thresholds: {
        statements: 80,
        branches: 80,
        functions: 80,
        lines: 80,
      },
    },
  },
})
