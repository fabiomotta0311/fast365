import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { describe, type Conversation, type Message, type Round } from "./types";

const LIST_LIMIT = 50;
const PAGE_LIMIT = 30;

export default function App() {
  const [conversations, setConversations] = useState<Conversation[]>([]);
  const [selected, setSelected] = useState<Conversation | null>(null);
  const [messages, setMessages] = useState<Message[]>([]);
  const [draft, setDraft] = useState("");
  const [status, setStatus] = useState("iniciando…");
  const [capabilities, setCapabilities] = useState<string[]>([]);

  const applyRound = useCallback((round: Round) => {
    const text = describe(round);
    if (text) setStatus(text);
    if (round.capabilities.length > 0) setCapabilities(round.capabilities);
  }, []);

  const reloadConversations = useCallback(async () => {
    try {
      const list = await invoke<Conversation[]>("conversations", { limit: LIST_LIMIT });
      setConversations(list);
      return list;
    } catch (error) {
      setStatus(String(error));
      return [];
    }
  }, []);

  const reloadMessages = useCallback(async (conversation: Conversation) => {
    try {
      const page = await invoke<Message[]>("messages", {
        conversationId: conversation.id,
        beforeMs: null,
        limit: PAGE_LIMIT,
      });
      setMessages(page);
    } catch (error) {
      setStatus(String(error));
    }
  }, []);

  // Primeiro frame: só cache. Nada de rede na montagem.
  useEffect(() => {
    void (async () => {
      await reloadConversations();
      try {
        setCapabilities(await invoke<string[]>("capabilities"));
      } catch {
        /* estado inicial sem sessão ainda */
      }
      setStatus("cache carregado");
    })();
  }, [reloadConversations]);

  // Enquanto o empurrão do tempo real não está ligado, o motor precisa de um "acorda" periódico:
  // é isso que resolve reconciliação de envio e renovação de token vencidos.
  useEffect(() => {
    const id = window.setInterval(() => {
      void (async () => {
        try {
          applyRound(await invoke<Round>("tick"));
        } catch {
          /* sem sessão: o status já explica */
        }
      })();
    }, 15_000);
    return () => window.clearInterval(id);
  }, [applyRound]);

  const select = useCallback(
    async (conversation: Conversation) => {
      setSelected(conversation);
      await reloadMessages(conversation);
    },
    [reloadMessages],
  );

  /** Atualiza a lista pelo provider e recarrega o histórico da conversa aberta. */
  const refresh = useCallback(async () => {
    try {
      const round = await invoke<Round>("refresh");
      applyRound(round);
      await reloadConversations();
      if (selected) await reloadMessages(selected);
    } catch (error) {
      setStatus(String(error));
    }
  }, [applyRound, reloadConversations, reloadMessages, selected]);

  const send = useCallback(async () => {
    if (!selected || draft.trim().length === 0) return;
    try {
      const round = await invoke<Round>("send", {
        conversationId: selected.id,
        text: draft,
      });
      applyRound(round);
      setDraft("");
      await reloadMessages(selected);
    } catch (error) {
      setStatus(String(error));
    }
  }, [applyRound, draft, reloadMessages, selected]);

  const conversationItems = useMemo(
    () =>
      conversations.map((conversation) => (
        <li key={conversation.id}>
          <button
            className={conversation.id === selected?.id ? "conversation selected" : "conversation"}
            onClick={() => void select(conversation)}
          >
            <span className="title">{conversation.title || "(sem título)"}</span>
            <span className="kind">{conversation.kind}</span>
          </button>
        </li>
      )),
    [conversations, select, selected],
  );

  return (
    <div className="app">
      <aside className="sidebar">
        <header>
          <h1>Fast365</h1>
          <button onClick={() => void refresh()} title="Atualizar pelo provider">
            Atualizar
          </button>
        </header>
        {conversations.length === 0 ? (
          <p className="empty">
            Nada em cache ainda. Clique em <strong>Atualizar</strong> (precisa de sessão:{" "}
            <code>f365 auth</code>).
          </p>
        ) : (
          <ul>{conversationItems}</ul>
        )}
      </aside>

      <main className="chat">
        {selected ? (
          <>
            <header>
              <h2>{selected.title || "(sem título)"}</h2>
              <span className="kind">{selected.kind}</span>
            </header>
            <ol className="messages">
              {[...messages].reverse().map((message) => (
                <li key={message.id}>
                  <span className="sender">{message.sender ?? "?"}</span>
                  <span className="body">{message.body}</span>
                </li>
              ))}
            </ol>
            <form
              className="composer"
              onSubmit={(event) => {
                event.preventDefault();
                void send();
              }}
            >
              <input
                value={draft}
                onChange={(event) => setDraft(event.target.value)}
                placeholder="Escreva uma mensagem (texto puro)"
              />
              <button type="submit" disabled={draft.trim().length === 0}>
                Enviar
              </button>
            </form>
          </>
        ) : (
          <p className="empty">Escolha uma conversa.</p>
        )}
      </main>

      <footer className="status">
        <span className="status-line">{status}</span>
        {capabilities.length > 0 && <span className="caps">{capabilities.join(" · ")}</span>}
      </footer>
    </div>
  );
}
