// Tipos que espelham os DTOs do `f365-ipc`. `EffectDto` é tagueado por `kind`, como no Rust.
export type Conversation = {
  id: string;
  title: string;
  kind: string;
  last_activity_ms: number | null;
  /** Texto da última mensagem em cache (sem marcação), para a linha da lista. */
  preview: string | null;
  preview_sender: string | null;
};

export type Message = {
  id: string;
  conversation_id: string;
  client_message_id: string | null;
  sender: string | null;
  body: string;
  message_type: string;
  created_ms: number;
};

/** Tradução/resumo/ata de uma mensagem. Nunca substitui o original: aparece ao lado dele. */
export type Derivative = {
  id: number;
  kind: string;
  model: string;
  lang: string | null;
  body: string;
  created_ms: number;
  approved_ms: number | null;
  /** O original mudou depois que isto foi gerado: não é a tradução atual nem pode ser aprovado. */
  stale: boolean;
  source_message_ids: string[];
};

/** Resultado de "carregar mais histórico". `reached_start` = o provider não tem mais nada. */
export type OlderResult = {
  round: Round;
  reached_start: boolean;
};

export type Effect =
  | { kind: "conversations_refreshed"; count: number }
  | { kind: "fetched"; conversation: string; messages: number }
  | { kind: "persisted"; conversation: string }
  | { kind: "published"; conversation: string }
  | { kind: "send_attempted"; client_id: string }
  | { kind: "sent"; client_id: string }
  | { kind: "uncertain"; client_id: string }
  | { kind: "reconnect_attempt" }
  | {
      kind: "derived";
      message_id: string;
      derivative_kind: string;
      derivative_id: number;
    }
  | { kind: "middleware_failed"; message_id: string }
  | { kind: "token_refreshed" }
  | { kind: "token_refresh_failed"; retry_in_ms: number }
  | { kind: "session_expired" };

export type Round = {
  effects: Effect[];
  wake_in_ms: number | null;
  capabilities: string[];
};

/** Resumo legível de uma rodada, para a barra de status. */
export function describe(round: Round): string {
  const parts: string[] = [];
  for (const effect of round.effects) {
    switch (effect.kind) {
      case "conversations_refreshed":
        parts.push(`${effect.count} conversa(s)`);
        break;
      case "fetched":
        parts.push(`${effect.messages} mensagem(ns)`);
        break;
      case "sent":
        parts.push("enviado");
        break;
      case "derived":
        parts.push(`${effect.derivative_kind} gerado`);
        break;
      case "uncertain":
        // Nunca dizer "enviado" quando o motor não provou entrega.
        parts.push("incerto — confira no Teams");
        break;
      case "token_refresh_failed":
        parts.push(`renovação falhou (${Math.round(effect.retry_in_ms / 1000)}s)`);
        break;
      case "session_expired":
        parts.push("sessão expirada — rode `f365 auth`");
        break;
      default:
        break;
    }
  }
  return parts.join(" · ");
}
