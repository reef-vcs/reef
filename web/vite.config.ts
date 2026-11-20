// The web UI's build (R2.18, ADR 0047). `npm run build` writes the app into
// crates/reef-web/assets, which reef-web's build.rs embeds in reef-daemon.
import { defineConfig } from "vitest/config";

// `npm run dev` proxies API calls to a daemon's web listener: the `url` that
// `reef ui --print` prints. The proxy sends the daemon's own Host and Origin,
// so the daemon's gate needs no dev mode.
const daemon = process.env.REEF_UI_URL?.replace(/\/+$/, "");

export default defineConfig({
  // Preact runs the app behind React's API, about 200 KB less JavaScript per
  // page (#252). The source imports `react` and type-checks against React's
  // types; each alias also covers its subpaths, as `react/jsx-runtime` to
  // `preact/compat/jsx-runtime` and `react-dom/client` to
  // `preact/compat/client`.
  resolve: { alias: { react: "preact/compat", "react-dom": "preact/compat" } },
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
