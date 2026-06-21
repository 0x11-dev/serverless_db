import { fileURLToPath } from "node:url";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

const dashboardRoot = fileURLToPath(new URL(".", import.meta.url));

export default defineConfig({
  root: dashboardRoot,
  plugins: [react()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
  },
});
