import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { fileURLToPath, URL } from "node:url";

const entry = (p: string) => fileURLToPath(new URL(p, import.meta.url));

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) },
  },
  build: {
    // One entry per page. A docs page is its own document rather than a route inside the
    // landing bundle: there is no router here, and a reader who lands on it should not pay
    // for the benchmark charts to see a `curl` line.
    rollupOptions: {
      input: {
        index: entry("./index.html"),
        "docs/create-table": entry("./docs/create-table/index.html"),
      },
    },
  },
});
