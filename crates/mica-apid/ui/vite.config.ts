import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'
import { tanstackRouter } from '@tanstack/router-plugin/vite'

export default defineConfig({
  base: '/_ui/',
  server: { allowedHosts: ['station'] },
  plugins: [
    tanstackRouter({
      target: 'react',
      autoCodeSplitting: true,
      routesDirectory: './src/app/routes',
      generatedRouteTree: './src/app/routeTree.gen.ts',
    }),
    react(),
    tailwindcss(),
  ],
  resolve: { tsconfigPaths: true },
  build: {
    assetsInlineLimit: 0,
    rollupOptions: {
      output: {
        entryFileNames: 'assets/[name]-[hash].js',
        chunkFileNames: 'assets/[name]-[hash].js',
        assetFileNames: 'assets/[name]-[hash][extname]',
        // The console is served from the device's own flash, so the cost of a
        // single 594 kB entry chunk was parse and compile time on a weak core,
        // not transfer. Grouping the vendors lets the browser fetch and compile
        // them in parallel behind the modulepreload links Vite emits, and keeps
        // an application change from invalidating React and Base UI with it.
        advancedChunks: {
          groups: [
            { name: 'react', test: /node_modules\/(react|react-dom|scheduler)\// },
            { name: 'router', test: /node_modules\/@tanstack\// },
            { name: 'ui-primitives', test: /node_modules\/(@base-ui\/react|@floating-ui)\// },
            { name: 'i18n', test: /node_modules\/(i18next|react-i18next)\// },
          ],
        },
      },
    },
  },
})
