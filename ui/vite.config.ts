import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// O Tauri carrega este dev server na janela; o build gera `dist/`, que é o `frontendDist`.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    watch: {
      // O alvo do Cargo fica dentro de `ui/`. No Windows o Vite tenta observar o `.exe` que acabou
      // de ser compilado (e está em uso) e o dev server morre com `EBUSY`. Nada disso é fonte do
      // frontend.
      ignored: ["**/src-tauri/**", "**/dist/**"],
    },
  },
  build: { outDir: "dist" },
});
