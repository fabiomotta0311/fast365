import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import "./styles.css";

const root = document.getElementById("root");
if (!root) throw new Error("elemento #root ausente");

// Dados sintéticos só em desenvolvimento (`npm run dev` + `?demo`); fora do build de produção.
if (import.meta.env.DEV && new URLSearchParams(window.location.search).has("demo")) {
  const { startDemo } = await import("./demo");
  startDemo();
}

createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
