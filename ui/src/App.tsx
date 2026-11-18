import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Calendar } from "./components/Calendar";
import { ChatList } from "./components/ChatList";
import { Rail, type Area } from "./components/Rail";
import { Thread } from "./components/Thread";
import { TitleBar } from "./components/TitleBar";
import {
  describe,
  type Conversation,
  type Derivative,
  type Message,
  type OlderResult,
  type Round,
} from "./types";
import { useTheme } from "./useTheme";

const LIST_LIMIT = 50;
const PAGE_LIMIT = 100;
/** Espera antes de gravar o rascunho (cada tecla não vira uma escrita). */
const DRAFT_DEBOUNCE_MS = 400;

/** Mais novas primeiro (como o cache devolve). */
function byNewest(a: Message, b: Message): number {
  return b.created_ms - a.created_ms || (a.id < b.id ? 1 : -1);
}

/** Atualiza a página mais nova sem perder as mais antigas já carregadas. */
function mergeNewest(previous: Message[], page: Message[]): Message[] {
  if (page.length === 0) return previous.length === 0 ? page : previous;
  const oldestInPage = page[page.length - 1].created_ms;
  const older = previous.filter((message) => message.created_ms < oldestInPage);
  return [...page, ...older];
}

/** Acrescenta páginas mais antigas, sem duplicar. */
function mergeOlder(previous: Message[], older: Message[]): Message[] {
  const seen = new Set(previous.map((message) => message.id));
  return [...previous, ...older.filter((message) => !seen.has(message.id))].sort(byNewest);
}

export default function App() {
  const [theme, toggleTheme] = useTheme();
  const [conversations, setConversations] = useState<Conversation[]>([]);
  const [selected, setSelected] = useState<Conversation | null>(null);
  const [messages, setMessages] = useState<Message[]>([]);
  const [draft, setDraft] = useState("");
  const [query, setQuery] = useState("");
  const [area, setArea] = useState<Area>("chat");
  const [me, setMe] = useState<string | null>(null);
  const [status, setStatus] = useState("iniciando…");
  const [capabilities, setCapabilities] = useState<string[]>([]);
  const [realtime, setRealtime] = useState<boolean | null>(null);
  // Sobe quando o motor mexe em derivados: cada linha visível relê o seu do cache.
  const [derivativesVersion, setDerivativesVersion] = useState(0);
  const [loadingOlder, setLoadingOlder] = useState(false);
  const [reachedStart, setReachedStart] = useState(false);

  // Referência para os ouvintes de evento não capturarem estado velho.
  const selectedRef = useRef<Conversation | null>(null);
  const messagesRef = useRef<Message[]>([]);
  const draftRef = useRef("");
  const savedDraftRef = useRef<{ id: string; text: string } | null>(null);
  const olderBusyRef = useRef(false);
  const reachedStartRef = useRef(false);

  messagesRef.current = messages;
  draftRef.current = draft;
  reachedStartRef.current = reachedStart;

  const reloadMessages = useCallback(async (conversation: Conversation) => {
    try {
      const page = await invoke<Message[]>("messages", {
        conversationId: conversation.id,
        beforeMs: null,
        limit: PAGE_LIMIT,
      });
      // A conversa pode ter mudado durante a espera: não mistura mensagens de outra.
      if (selectedRef.current?.id !== conversation.id) return;
      setMessages((previous) => mergeNewest(previous, page));
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

  /** Grava o rascunho (vazio apaga). Falha não atrapalha a digitação. */
  const saveDraft = useCallback(async (conversationId: string, text: string) => {
    if (savedDraftRef.current?.id === conversationId && savedDraftRef.current.text === text) return;
    try {
      await invoke("set_draft", { conversationId, text });
      savedDraftRef.current = { id: conversationId, text };
    } catch {
      /* sem core/sessão: o rascunho fica só na tela */
    }
  }, []);

  // Rascunho com atraso: grava ~400 ms depois da última tecla.
  useEffect(() => {
    const current = selected;
    if (!current) return;
    const timer = window.setTimeout(() => void saveDraft(current.id, draft), DRAFT_DEBOUNCE_MS);
    return () => window.clearTimeout(timer);
  }, [draft, selected, saveDraft]);

  // Ao perder o foco da janela, grava na hora (não espera o atraso).
  useEffect(() => {
    const flush = () => {
      const current = selectedRef.current;
      if (current) void saveDraft(current.id, draftRef.current);
    };
    window.addEventListener("blur", flush);
    return () => window.removeEventListener("blur", flush);
  }, [saveDraft]);

  /** Carrega mensagens mais antigas: primeiro do cache; se acabou, pede mais ao provider. */
  const loadOlder = useCallback(async () => {
    const current = selectedRef.current;
    if (!current || olderBusyRef.current || reachedStartRef.current) return;
    const oldest = messagesRef.current[messagesRef.current.length - 1];
    if (!oldest) return;
    olderBusyRef.current = true;
    setLoadingOlder(true);
    const fromCache = () =>
      invoke<Message[]>("messages", {
        conversationId: current.id,
        beforeMs: oldest.created_ms,
        limit: PAGE_LIMIT,
      });
    try {
      let page = await fromCache();
      let providerDone = false;
      if (page.length === 0) {
        const older = await invoke<OlderResult>("older", { conversationId: current.id });
        applyRound(older.round);
        providerDone = older.reached_start;
        page = await fromCache();
        if (page.length === 0 && !providerDone) {
          setStatus("não foi possível carregar mais mensagens agora");
        }
      }
      if (selectedRef.current?.id === current.id) {
        if (page.length > 0) setMessages((previous) => mergeOlder(previous, page));
        // O provider não tem mais e o cache se esgotou nesta página: acabou o histórico.
        if (providerDone && page.length < PAGE_LIMIT) setReachedStart(true);
      }
    } catch (error) {
      setStatus(String(error));
    } finally {
      olderBusyRef.current = false;
      setLoadingOlder(false);
    }
  }, [applyRound]);

  const select = useCallback(
    async (conversation: Conversation) => {
      // Rascunho da conversa que está saindo: grava antes de trocar.
      const leaving = selectedRef.current;
      if (leaving && leaving.id !== conversation.id) {
        void saveDraft(leaving.id, draftRef.current);
      }
      selectedRef.current = conversation;
      setSelected(conversation);
      setMessages([]);
      setReachedStart(false);
      setDraft("");
      // O rascunho salvo desta conversa (se não digitaram nada nesse meio-tempo).
      void invoke<string | null>("draft", { conversationId: conversation.id })
        .then((saved) => {
          savedDraftRef.current = { id: conversation.id, text: saved ?? "" };
          if (saved && selectedRef.current?.id === conversation.id && draftRef.current === "") {
            setDraft(saved);
          }
        })
        .catch(() => {
          /* sem rascunho salvo */
        });
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
      void saveDraft(current.id, ""); // enviado: o rascunho some
      await reloadMessages(current);
    } catch (error) {
      setStatus(String(error));
    }
  }, [applyRound, draft, reloadMessages, saveDraft]);

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
      <Rail area={area} onArea={setArea} />

      {area === "calendar" ? (
        <Calendar onStatus={setStatus} />
      ) : (
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
          onReachTop={() => void loadOlder()}
          loadingOlder={loadingOlder}
          reachedStart={reachedStart}
        />
      </div>
      )}

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
