import { ArrowSyncRegular } from "@fluentui/react-icons";
import { listStamp } from "../format";
import type { Conversation } from "../types";
import { Avatar } from "./Avatar";

type Props = {
  conversations: Conversation[];
  selectedId: string | null;
  me: string | null;
  query: string;
  onSelect: (conversation: Conversation) => void;
  onRefresh: () => void;
};

/** Lista de conversas. Não mostra não lido nem presença: o cache ainda não tem essa fonte. */
export function ChatList({ conversations, selectedId, me, query, onSelect, onRefresh }: Props) {
  const needle = query.trim().toLowerCase();
  const visible = needle
    ? conversations.filter((conversation) => conversation.title.toLowerCase().includes(needle))
    : conversations;

  return (
    <section className="fx-list" aria-label="Conversas">
      <div className="fx-list__head">
        <h1 className="fx-list__title">Chat</h1>
        <button
          type="button"
          className="fx-iconbtn"
          onClick={onRefresh}
          aria-label="Atualizar conversas"
          title="Atualizar pelo provider"
        >
          <ArrowSyncRegular fontSize={20} />
        </button>
      </div>

      <div className="fx-list__scroll fx-scroll">
        {conversations.length === 0 ? (
          <p className="fx-note">
            Nada em cache ainda. Clique em atualizar (precisa de sessão: <code>f365 auth</code>).
          </p>
        ) : visible.length === 0 ? (
          <p className="fx-note">Nenhuma conversa com esse nome.</p>
        ) : (
          visible.map((conversation) => {
            const title = conversation.title || "(sem título)";
            const mine = me !== null && conversation.preview_sender === me;
            const preview = conversation.preview
              ? `${mine ? "Você: " : conversation.preview_sender ? `${conversation.preview_sender.split(" ")[0]}: ` : ""}${conversation.preview}`
              : "";
            return (
              <button
                key={conversation.id}
                type="button"
                className={`fx-row${conversation.id === selectedId ? " fx-row--active" : ""}`}
                onClick={() => onSelect(conversation)}
              >
                <Avatar name={title} size={36} />
                <span className="fx-row__main">
                  <span className="fx-row__top">
                    <span className="fx-row__name">{title}</span>
                    <span className="fx-row__time">{listStamp(conversation.last_activity_ms)}</span>
                  </span>
                  <span className="fx-row__preview">{preview}</span>
                </span>
              </button>
            );
          })
        )}
      </div>
    </section>
  );
}
