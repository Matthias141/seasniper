import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import { VitePWA } from 'vite-plugin-pwa';

// Dev: `npm run dev` here, `cargo run` in the repo root. Vite proxies
// /api and /ws to the Rust binary on 4117 so the browser never needs CORS
// gymnastics in dev (the Rust side has a permissive CorsLayer for when
// you skip the proxy in prod too, but same-origin is cleaner).
//
// Prod: `npm run build` outputs to dist/. Point tower_http::ServeDir at
// this directory from the Rust binary so one process serves both the API
// and the PWA — no second server, no extra hop, matters when "latency" is
// the entire point of this tool.
export default defineConfig({
  plugins: [
    react(),
    VitePWA({
      registerType: 'autoUpdate',
      includeAssets: ['icons/icon-192.png', 'icons/icon-512.png'],
      manifest: {
        name: 'Mint Sniper — Control Deck',
        short_name: 'Sniper',
        description: 'Multi-wallet EVM free-mint control panel',
        theme_color: '#0A0B0D',
        background_color: '#0A0B0D',
        display: 'standalone',
        orientation: 'portrait',
        icons: [
          { src: 'icons/icon-192.png', sizes: '192x192', type: 'image/png' },
          { src: 'icons/icon-512.png', sizes: '512x512', type: 'image/png' },
          { src: 'icons/icon-512.png', sizes: '512x512', type: 'image/png', purpose: 'maskable' },
        ],
      },
      workbox: {
        // Never cache /api or /ws — this panel is worthless if it serves
        // stale wallet balances or a stale armed state from the SW cache.
        navigateFallbackDenylist: [/^\/api/, /^\/ws/],
        runtimeCaching: [
          {
            urlPattern: /^\/api\//,
            handler: 'NetworkOnly',
          },
        ],
      },
    }),
  ],
  server: {
    proxy: {
      '/api': { target: 'http://127.0.0.1:4117', changeOrigin: true },
      '/ws': { target: 'ws://127.0.0.1:4117', ws: true },
      // step 10: Google Sign-In, TOTP, WebAuthn, session/logout routes —
      // same reasoning as /api above, plus these set/read cookies, which
      // only works cleanly same-origin through this proxy in dev.
      '/auth': { target: 'http://127.0.0.1:4117', changeOrigin: true },
    },
  },
});
