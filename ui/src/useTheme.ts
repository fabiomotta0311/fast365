import { useCallback, useEffect, useState } from "react";

export type Theme = "dark" | "light";

const KEY = "f365-theme";

function initial(): Theme {
  try {
    const saved = window.localStorage.getItem(KEY);
    if (saved === "dark" || saved === "light") return saved;
  } catch {
    /* sem armazenamento: segue o sistema */
  }
  // Escuro é o padrão (como o cliente de referência); o sistema só decide se pedir claro.
  return window.matchMedia?.("(prefers-color-scheme: light)").matches ? "light" : "dark";
}

/** Tema com preferência lembrada; o atributo `data-theme` no `<html>` aciona os tokens do CSS. */
export function useTheme(): [Theme, () => void] {
  const [theme, setTheme] = useState<Theme>(initial);

  useEffect(() => {
    document.documentElement.setAttribute("data-theme", theme);
    try {
      window.localStorage.setItem(KEY, theme);
    } catch {
      /* preferência não persiste; a sessão segue normal */
    }
  }, [theme]);

  const toggle = useCallback(() => setTheme((value) => (value === "dark" ? "light" : "dark")), []);
  return [theme, toggle];
}
