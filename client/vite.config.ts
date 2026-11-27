import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import path from "node:path";

// Built into dist/, which OpenOrca.WebUi embeds into openorca.exe.
// `npm run dev` proxies the API to a running `openorca --hostui=8192 --hostui-token=dev-token-1`.
export default defineConfig({
  plugins: [react()],
  resolve: { alias: { "@": path.resolve(__dirname, "src") } },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    sourcemap: false,
    chunkSizeWarningLimit: 4096,
    // The brand images keep their own hashed files (never inlined): the favicon and the mascot
    // are fetched by URL, and a hashed name is what keeps a stale cached copy from showing.
    assetsInlineLimit: (file) => (file.replace(/\\/g, "/").includes("/assets/brand/") ? false : undefined),
    rollupOptions: {
      // glance.html: the public Glance page's own bundle (SUPERVISOR.md 12.1).
      input: { main: path.resolve(__dirname, "index.html"), glance: path.resolve(__dirname, "glance.html"), machine: path.resolve(__dirname, "machine.html") },
      output: {
        // By path, so shared helpers stay out of these chunks (the Glance page loads neither).
        manualChunks(id) {
          // Monaco's core; its languages stay lazy chunks of their own.
          if (id.includes("/node_modules/monaco-editor/") && !/\/(basic-languages|language)\//.test(id)) return "monaco";
          if (id.includes("/node_modules/@xterm/")) return "xterm";
          if (id.includes("commonjsHelpers")) return "cjs";
          return undefined;
        },
      },
    },
  },
  server: {
    proxy: {
      "/api": { target: "http://127.0.0.1:8192", ws: true, headers: { "X-OpenOrca-Token": "dev-token-1" } },
      "/public": { target: "http://127.0.0.1:8192" },
      "/hub": { target: "http://127.0.0.1:8192", ws: true, headers: { "X-OpenOrca-Token": "dev-token-1" } },
    },
  },
});
