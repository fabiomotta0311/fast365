import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useVirtualizer } from "@tanstack/react-virtual";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import DOMPurify from "dompurify";
import { Derivatives } from "./Derivatives";
import { describe, type Conversation, type Derivative, type Message, type Round } from "./types";

const LIST_LIMIT = 50;
const PAGE_LIMIT = 100;

/** Corpo da mensagem: HTML do Teams passa por DOMPurify; texto vira Markdown (sem HTML cru). */
function MessageBody({ message }: { message: Message }) {
  const isHtml = /html|richtext/i.test(message.message_type);
  if (isHtml) {
    // O Teams manda RichText/Html: sanitizar é obrigatório antes de injetar.
    const clean = DOMPurify.sanitize(message.body, { USE_PROFILES: { html: true } });
    return <div className="body html" dangerouslySetInnerHTML={{ __html: clean }} />;
  }
  return (
    <div className="body md">
      <Markdown remarkPlugins={[remarkGfm]}>{message.body}</Markdown>
    </div>
  );
}

export default function App() {
  const [conversations, setConversations] = useState<Conversation[]>([]);
  const [selected, setSelected] = useState<Conversation | null>(null);
  const [messages, setMessages] = useState<Message[]>([]);
  const [draft, setDraft] = useState("");
  const [status, setStatus] = useState("iniciando…");
  const [capabilities, setCapabilities] = useState<string[]>([]);
  const [realtime, setRealtime] = useState<boolean | null>(null);
  // Sobe quando o motor mexe em derivados: cada linha visível relê o seu do cache.
  const [derivativesVersion, setDerivativesVersion] = useState(0);

  // Referência para os ouvintes de evento não capturarem estado velho.
  const selectedRef = useRef<Conversation | null>(null);
  const scrollerRef = useRef<HTMLDivElement | null>(null);

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

  /** Reação a uma rodada do motor: status, capacidades e recarga do que foi tocado. */
  const applyRound = useCallback(
    (round: Round) => {
      const text = describe(round);
      if (text) setStatus(text);
      if (round.capabilities.length > 0) setCapabilities(round.capabilities);
      if (round.effects.some((effect) => effect.kind === "derived" || effect.kind === "fetched")) {
        setDerivativesVersion((version) => version + 1);
      }

      const current = selectedRef.current;
      const touched =
        current !== null &&
        round.effects.some(
          (effect) =>
            (effect.kind === "fetched" || effect.kind === "published") &&
            effect.conversation === current.id,
        );
      if (touched && current) void reloadMessages(current);

      // O motor completa o título de um 1:1 sem nome ao buscar o histórico (`persisted`).
      if (round.effects.some((effect) => effect.kind === "persisted")) {
        void reloadConversations();
      }
    },
    [reloadConversations, reloadMessages],
  );

  // Primeiro frame: só cache. Nada de rede na montagem.
  useEffect(() => {
    void (async () => {
      await reloadConversations();
      try {
        setCapabilities(await invoke<string[]>("capabilities"));
      } catch {
        /* sem sessão ainda */
      }
      setStatus("cache carregado");
    })();
  }, [reloadConversations]);

  // O core empurra as novidades: rodada do motor e estado do canal.
  useEffect(() => {
    const disposers: Array<() => void> = [];
    let disposed = false;
    void (async () => {
      const offRound = await listen<Round>("f365://round", (event) => applyRound(event.payload));
      const offRealtime = await listen<{ connected: boolean }>("f365://realtime", (event) =>
        setRealtime(event.payload.connected),
      );
      if (disposed) {
        offRound();
        offRealtime();
        return;
      }
      disposers.push(offRound, offRealtime);
    })();
    return () => {
      disposed = true;
      for (const off of disposers) off();
    };
  }, [applyRound]);

  // O título é completado pelo motor depois do primeiro frame: mantém a conversa aberta em
  // sincronia com a lista, senão o cabeçalho fica no `1:1 · …` antigo.
  useEffect(() => {
    const current = selectedRef.current;
    if (!current) return;
    const fresh = conversations.find((conversation) => conversation.id === current.id);
    if (fresh && fresh.title !== current.title) {
      selectedRef.current = fresh;
      setSelected(fresh);
    }
  }, [conversations]);

  const select = useCallback(
    async (conversation: Conversation) => {
      selectedRef.current = conversation;
      setSelected(conversation);
      // Primeiro o que já está em cache (render-before-network)…
      await reloadMessages(conversation);
      // …depois busca o histórico no provider. Invalidação é o que o motor entende por
      // "busque esta conversa".
      try {
        applyRound(await invoke<Round>("load", { conversationId: conversation.id }));
        await reloadMessages(conversation);
      } catch (error) {
        setStatus(String(error));
      }
    },
    [applyRound, reloadMessages],
  );

  const refresh = useCallback(async () => {
    try {
      applyRound(await invoke<Round>("refresh"));
      await reloadConversations();
      const current = selectedRef.current;
      if (current) await reloadMessages(current);
    } catch (error) {
      setStatus(String(error));
    }
  }, [applyRound, reloadConversations, reloadMessages]);

  const send = useCallback(async () => {
    const current = selectedRef.current;
    if (!current || draft.trim().length === 0) return;
    try {
      applyRound(await invoke<Round>("send", { conversationId: current.id, text: draft }));
      setDraft("");
      await reloadMessages(current);
    } catch (error) {
      setStatus(String(error));
    }
  }, [applyRound, draft, reloadMessages]);

  /** Aprova o derivado e envia **exatamente** o texto aprovado (o que o backend devolveu). */
  const approveAndSend = useCallback(
    async (derivative: Derivative, conversationId: string) => {
      try {
        const text = await invoke<string>("approve_derivative", { derivativeId: derivative.id });
        applyRound(await invoke<Round>("send", { conversationId, text }));
        setDerivativesVersion((version) => version + 1);
        const current = selectedRef.current;
        if (current) await reloadMessages(current);
      } catch (error) {
        setStatus(String(error));
      }
    },
    [applyRound, reloadMessages],
  );

  // Cronológico (o cache devolve do mais novo para o mais antigo) e virtualizado.
  const chronological = useMemo(() => [...messages].reverse(), [messages]);
  const virtualizer = useVirtualizer({
    count: chronological.length,
    getScrollElement: () => scrollerRef.current,
    estimateSize: () => 72,
    overscan: 8,
  });

  // Ao chegar mensagem nova no fim, acompanha o fim da lista.
  useEffect(() => {
    if (chronological.length === 0) return;
    virtualizer.scrollToIndex(chronological.length - 1, { align: "end" });
  }, [chronological.length, virtualizer]);

  const conversationItems = conversations.map((conversation) => (
    <li key={conversation.id}>
      <button
        className={conversation.id === selected?.id ? "conversation selected" : "conversation"}
        onClick={() => void select(conversation)}
      >
        <span className="title">{conversation.title || "(sem título)"}</span>
        <span className="kind">{conversation.kind}</span>
      </button>
    </li>
  ));

  const virtualItems = virtualizer.getVirtualItems();

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
            <div className="messages" ref={scrollerRef}>
              <div style={{ height: virtualizer.getTotalSize(), position: "relative" }}>
                {virtualItems.map((item) => {
                  const message = chronological[item.index];
                  return (
                    <div
                      key={message.id}
                      className="row"
                      data-index={item.index}
                      ref={virtualizer.measureElement}
                      style={{ transform: `translateY(${item.start}px)` }}
                    >
                      <div className="bubble">
                        <span className="sender">{message.sender ?? "?"}</span>
                        <MessageBody message={message} />
                        <Derivatives
                          message={message}
                          version={derivativesVersion}
                          onApprove={(derivative, conversationId) =>
                            void approveAndSend(derivative, conversationId)
                          }
                        />
                      </div>
                    </div>
                  );
                })}
              </div>
            </div>
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
        <span className="caps">
          {realtime === null ? "" : realtime ? "tempo real ok · " : "tempo real reconectando · "}
          {capabilities.join(" · ")}
        </span>
      </footer>
    </div>
  );
}
