import { AlertRegular, CalendarLtrFilled, CalendarLtrRegular, ChatFilled, ChatRegular } from "@fluentui/react-icons";

export type Area = "chat" | "calendar";

type Props = {
  area: Area;
  onArea: (area: Area) => void;
};

/**
 * Coluna de navegação. Chat e Calendário existem; Atividade aparece desabilitada para mostrar onde
 * vai ficar (precisa de fonte no cache: não lido/menções) em vez de fingir que funciona.
 */
export function Rail({ area, onArea }: Props) {
  const item = (target: Area, label: string, active: React.ReactNode, idle: React.ReactNode) => (
    <button
      type="button"
      className={`fx-rail__item${area === target ? " fx-rail__item--active" : ""}`}
      aria-current={area === target ? "page" : undefined}
      title={label}
      onClick={() => onArea(target)}
    >
      {area === target ? active : idle}
      <span className="fx-sr">{label}</span>
    </button>
  );

  return (
    <nav className="fx-rail" aria-label="Navegação">
      {item("chat", "Chat", <ChatFilled fontSize={24} />, <ChatRegular fontSize={24} />)}
      {item(
        "calendar",
        "Calendário",
        <CalendarLtrFilled fontSize={24} />,
        <CalendarLtrRegular fontSize={24} />,
      )}
      <button type="button" className="fx-rail__item" disabled title="Atividade — ainda não disponível">
        <AlertRegular fontSize={24} />
        <span className="fx-sr">Atividade (ainda não disponível)</span>
      </button>
    </nav>
  );
}
