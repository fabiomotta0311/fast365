import { SearchRegular, WeatherMoonRegular, WeatherSunnyRegular } from "@fluentui/react-icons";
import type { Theme } from "../useTheme";

type Props = {
  query: string;
  onQuery: (value: string) => void;
  theme: Theme;
  onToggleTheme: () => void;
};

/** Marca própria do Fast365 (não usa o logotipo do Teams). */
function Mark() {
  return (
    <svg className="fx-mark" viewBox="0 0 24 24" width="24" height="24" aria-hidden="true">
      <rect x="1" y="1" width="22" height="22" rx="6" fill="currentColor" />
      <path d="M8 7h8v2.2h-5.4v1.9H15v2.1h-4.4V17H8z" fill="var(--fx-app-bg)" />
    </svg>
  );
}

export function TitleBar({ query, onQuery, theme, onToggleTheme }: Props) {
  return (
    <header className="fx-titlebar">
      <div className="fx-titlebar__logo" title="Fast365">
        <Mark />
      </div>

      <label className="fx-search">
        <SearchRegular fontSize={16} aria-hidden="true" />
        <input
          value={query}
          onChange={(event) => onQuery(event.target.value)}
          placeholder="Pesquisar conversas"
          aria-label="Pesquisar conversas"
        />
      </label>

      <div className="fx-titlebar__end">
        <button
          type="button"
          className="fx-iconbtn"
          onClick={onToggleTheme}
          aria-label={theme === "dark" ? "Usar tema claro" : "Usar tema escuro"}
          title={theme === "dark" ? "Tema claro" : "Tema escuro"}
        >
          {theme === "dark" ? <WeatherSunnyRegular fontSize={20} /> : <WeatherMoonRegular fontSize={20} />}
        </button>
      </div>
    </header>
  );
}
