import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import fs from "node:fs";
import path from "node:path";

// The router's own admin pages (D151): router-admin.html -> dist-router/, embedded in the router
// binary (crates/orca-router, rust-embed) and served at /admin/. `base: "./"` makes every URL
// relative, so the page works behind a reverse proxy under any prefix and through the Abhoth
// app's proxy. Shares src/ (components, styles, the router panes and forms) with the app.
// `npm run dev:router` proxies to a router on :8010 (e.g. `abhoth router admin-demo`).
export default defineConfig({
  plugins: [
    react(),
    {
      // The router serves the page as index.html.
      name: "router-admin-index",
      apply: "build",
      closeBundle() {
        const out = path.resolve(__dirname, "dist-router");
        const page = path.join(out, "router-admin.html");
        if (fs.existsSync(page)) fs.renameSync(page, path.join(out, "index.html"));
      },
    },
  ],
  base: "./",
  resolve: { alias: { "@": path.resolve(__dirname, "src") } },
  build: {
    outDir: "dist-router",
    emptyOutDir: true,
    sourcemap: false,
    chunkSizeWarningLimit: 4096,
    rollupOptions: {
      input: { index: path.resolve(__dirname, "router-admin.html") },
    },
  },
  server: {
    proxy: {
      "/admin/session": { target: "http://127.0.0.1:8010" },
      "/admin/login": { target: "http://127.0.0.1:8010" },
      "/admin/logout": { target: "http://127.0.0.1:8010" },
      "/admin/api": { target: "http://127.0.0.1:8010" },
    },
  },
});
