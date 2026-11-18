import { AlertRegular, CalendarLtrRegular, ChatFilled } from "@fluentui/react-icons";

/**
 * Coluna de navegação. Só o Chat existe hoje; as outras áreas aparecem desabilitadas para mostrar
 * onde vão ficar (Outlook/calendário são Fase 4) em vez de fingirem funcionar.
 */
export function Rail() {
  return (
    <nav className="fx-rail" aria-label="Navegação">
      <button type="button" className="fx-rail__item fx-rail__item--active" aria-current="page" title="Chat">
        <ChatFilled fontSize={24} />
        <span className="fx-sr">Chat</span>
      </button>
      <button type="button" className="fx-rail__item" disabled title="Calendário — ainda não disponível">
        <CalendarLtrRegular fontSize={24} />
        <span className="fx-sr">Calendário (ainda não disponível)</span>
      </button>
      <button type="button" className="fx-rail__item" disabled title="Atividade — ainda não disponível">
        <AlertRegular fontSize={24} />
        <span className="fx-sr">Atividade (ainda não disponível)</span>
      </button>
    </nav>
  );
}
