// The web UI's build (R2.18, ADR 0047). `npm run build` writes the app into
// crates/reef-web/assets, which reef-web's build.rs embeds in reef-daemon.
import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

// `npm run dev` proxies API calls to a daemon's web listener: the `url` that
// `reef ui --print` prints. The proxy sends the daemon's own Host and Origin,
// so the daemon's gate needs no dev mode.
const daemon = process.env.REEF_UI_URL?.replace(/\/+$/, "");

export default defineConfig({
  plugins: [react()],
  base: "/",
  build: {
    outDir: "../crates/reef-web/assets",
    emptyOutDir: true,
    sourcemap: false,
    // No `data:` scripts or styles: everything is a file under `assets/`,
    // which the CSP's 'self' allows.
    assetsInlineLimit: 0,
    modulePreload: { polyfill: false },
    // The chunk graph, which reef-web's budget test reads to sum what each
    // page loads (R2.21). Embedded like any file (not under a dot directory,
    // which reef-web skips); it names only files index.html already exposes.
    manifest: "manifest.json",
  },
  server: {
    host: "127.0.0.1",
    port: 5173,
    strictPort: true,
    proxy: daemon
      ? {
          "/reef.api.v1.": {
            target: daemon,
            changeOrigin: true,
            configure: (proxy) => {
              proxy.on("proxyReq", (request) => {
                request.setHeader("origin", daemon);
                request.removeHeader("sec-fetch-site");
              });
            },
          },
        }
      : {},
  },
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.{ts,tsx}"],
    restoreMocks: true,
  },
});
