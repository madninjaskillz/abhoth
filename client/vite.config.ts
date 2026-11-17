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
    rollupOptions: {
      output: {
        manualChunks: {
          monaco: ["monaco-editor/esm/vs/editor/editor.api"],
          xterm: ["@xterm/xterm", "@xterm/addon-fit", "@xterm/addon-webgl"],
        },
      },
    },
  },
  server: {
    proxy: {
      "/api": { target: "http://127.0.0.1:8192", ws: true, headers: { "X-OpenOrca-Token": "dev-token-1" } },
      "/hub": { target: "http://127.0.0.1:8192", ws: true, headers: { "X-OpenOrca-Token": "dev-token-1" } },
    },
  },
});
