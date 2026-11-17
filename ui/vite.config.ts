import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// O Tauri carrega este dev server na janela; o build gera `dist/`, que é o `frontendDist`.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: 5173, strictPort: true },
  build: { outDir: "dist" },
});
