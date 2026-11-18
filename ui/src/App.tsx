import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { ChatList } from "./components/ChatList";
import { Rail } from "./components/Rail";
import { Thread } from "./components/Thread";
import { TitleBar } from "./components/TitleBar";
import { describe, type Conversation, type Derivative, type Message, type Round } from "./types";
import { useTheme } from "./useTheme";

const LIST_LIMIT = 50;
const PAGE_LIMIT = 100;

export default function App() {
  const [theme, toggleTheme] = useTheme();
  const [conversations, setConversations] = useState<Conversation[]>([]);
  const [selected, setSelected] = useState<Conversation | null>(null);
  const [messages, setMessages] = useState<Message[]>([]);
  const [draft, setDraft] = useState("");
  const [query, setQuery] = useState("");
  const [me, setMe] = useState<string | null>(null);
  const [status, setStatus] = useState("iniciando…");
  const [capabilities, setCapabilities] = useState<string[]>([]);
  const [realtime, setRealtime] = useState<boolean | null>(null);
  // Sobe quando o motor mexe em derivados: cada linha visível relê o seu do cache.
  const [derivativesVersion, setDerivativesVersion] = useState(0);

  // Referência para os ouvintes de evento não capturarem estado velho.
  const selectedRef = useRef<Conversation | null>(null);

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

  const reloadMe = useCallback(async () => {
    try {
      setMe(await invoke<string | null>("me"));
    } catch {
      /* sem core/sessão: as mensagens aparecem todas como de outra pessoa */
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

      // O motor completa o título de um 1:1 sem nome ao buscar o histórico (`persisted`); a prévia
      // da lista também muda quando chega mensagem (`fetched`).
      if (round.effects.some((effect) => effect.kind === "persisted" || effect.kind === "fetched")) {
        void reloadConversations();
        void reloadMe();
      }
    },
    [reloadConversations, reloadMe, reloadMessages],
  );

  // Primeiro frame: só cache. Nada de rede na montagem.
  useEffect(() => {
    void (async () => {
      await reloadConversations();
      await reloadMe();
      try {
        setCapabilities(await invoke<string[]>("capabilities"));
      } catch {
        /* sem sessão ainda */
      }
      setStatus("cache carregado");
    })();
  }, [reloadConversations, reloadMe]);

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
      setMessages([]);
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
      await reloadMe();
      const current = selectedRef.current;
      if (current) await reloadMessages(current);
    } catch (error) {
      setStatus(String(error));
    }
  }, [applyRound, reloadConversations, reloadMe, reloadMessages]);

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

  return (
    <div className="fx-app">
      <TitleBar query={query} onQuery={setQuery} theme={theme} onToggleTheme={toggleTheme} />
      <Rail />

      <div className="fx-body-area">
        <ChatList
          conversations={conversations}
          selectedId={selected?.id ?? null}
          me={me}
          query={query}
          onSelect={(conversation) => void select(conversation)}
          onRefresh={() => void refresh()}
        />
        <Thread
          conversation={selected}
          messages={messages}
          me={me}
          derivativesVersion={derivativesVersion}
          draft={draft}
          onDraft={setDraft}
          onSend={() => void send()}
          onApprove={(derivative, conversationId) =>
            void approveAndSend(derivative, conversationId)
          }
        />
      </div>

      <footer className="fx-status">
        <span>{status}</span>
        <span>
          {realtime === null ? "" : realtime ? "tempo real ok · " : "tempo real reconectando · "}
          {capabilities.join(" · ")}
        </span>
      </footer>
    </div>
  );
}
