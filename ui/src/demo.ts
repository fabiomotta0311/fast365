import { mockIPC } from "@tauri-apps/api/mocks";
import type { CalEvent, Conversation, Derivative, Message, Round } from "./types";

/**
 * Modo demo: o frontend roda num navegador comum (`npm run dev` + `?demo`) com dados sintéticos,
 * sem core, sem rede e sem sessão. Só entra no build de desenvolvimento (ver `main.tsx`).
 */
const T0 = Date.now() - 3_600_000;

const NOW = Date.now();
const ME = "Fabio (demo)";

const conversations: Conversation[] = [
  { id: "19:demo@thread.v2", title: "Time de produto (demo)", kind: "chat", last_activity_ms: NOW - 120_000, preview: "Reunião movida para as 15h.", preview_sender: "Bruno Alves" },
  { id: "19:outra@thread.v2", title: "Ana Souza (demo)", kind: "oneOnOne", last_activity_ms: NOW - 86_400_000, preview: "Can you send the quarterly report by Thursday?", preview_sender: "Ana Souza" },
  { id: "19:cliente@thread.v2", title: "Projeto Ábax — Implantação", kind: "chat", last_activity_ms: NOW - 3 * 86_400_000, preview: "subiu a versão com o relatório de performance", preview_sender: ME },
];

const messages: Message[] = [
  { id: "m1", conversation_id: "19:demo@thread.v2", client_message_id: null, sender: "Ana Souza", body: "Can you send the quarterly report by Friday?", message_type: "Text", created_ms: T0 },
  { id: "m2", conversation_id: "19:demo@thread.v2", client_message_id: null, sender: "Ana Souza", body: "Can you send the quarterly report by Thursday?", message_type: "Text", created_ms: T0 + 300_000 },
  { id: "m3", conversation_id: "19:demo@thread.v2", client_message_id: null, sender: "Bruno Alves", body: "Reunião movida para as 15h.", message_type: "Text", created_ms: T0 + 600_000 },
  { id: "m4", conversation_id: "19:demo@thread.v2", client_message_id: null, sender: "Bruno Alves", body: "Quem consegue revisar o deck antes?", message_type: "Text", created_ms: T0 + 660_000 },
  { id: "m5", conversation_id: "19:demo@thread.v2", client_message_id: null, sender: ME, body: "Eu reviso. Mando até as 14h.", message_type: "Text", created_ms: T0 + 900_000 },
];

// Conversa longa para exercitar a rolagem e o "carregar mais".
for (let i = 0; i < 45; i++) {
  messages.push({
    id: `long-${i}`,
    conversation_id: "19:cliente@thread.v2",
    client_message_id: null,
    sender: i % 4 === 0 ? ME : "Camila Rocha",
    body: `Mensagem sintética ${i + 1} da conversa longa.`,
    message_type: "Text",
    created_ms: T0 - 86_400_000 + i * 60_000,
  });
}

let nextId = 100;
let olderRounds = 0;
const drafts: Record<string, string> = {};

/** Gera mensagens antigas sintéticas (antes da mais antiga) para testar "carregar mais". */
function addOlder(conversationId: string, count: number): void {
  const mine = messages.filter((m) => m.conversation_id === conversationId);
  const oldest = Math.min(...mine.map((m) => m.created_ms));
  for (let i = 1; i <= count; i++) {
    messages.push({
      id: `old-${nextId++}`,
      conversation_id: conversationId,
      client_message_id: null,
      sender: i % 3 === 0 ? ME : "Bruno Alves",
      body: `Mensagem antiga sintética ${olderRounds}-${i}`,
      message_type: "Text",
      created_ms: oldest - i * 120_000,
    });
  }
}
const derivatives: Record<string, Derivative[]> = {
  m1: [
    { id: 1, kind: "translation", model: "demo@1", lang: "pt", body: "Você pode enviar o relatório trimestral até sexta?", created_ms: T0, approved_ms: null, stale: false, source_message_ids: ["m1"] },
  ],
  m2: [
    { id: 3, kind: "translation", model: "demo@1", lang: "pt", body: "Você pode enviar o relatório trimestral até quinta?", created_ms: T0 + 1, approved_ms: null, stale: false, source_message_ids: ["m2"] },
    { id: 2, kind: "translation", model: "demo@1", lang: "pt", body: "Você pode enviar o relatório trimestral até sexta?", created_ms: T0, approved_ms: null, stale: true, source_message_ids: ["m2"] },
    { id: 4, kind: "proposed_action", model: "demo@1", lang: "en", body: "Sure, I'll send it by Thursday.", created_ms: T0 + 2, approved_ms: null, stale: false, source_message_ids: ["m2"] },
  ],
  m3: [],
};

/** Agenda sintética ancorada em "hoje" (semana corrente), com sobreposição, dia inteiro e sem link. */
function at(dayOffset: number, hour: number, minutes = 0): number {
  const d = new Date();
  d.setDate(d.getDate() + dayOffset);
  d.setHours(hour, minutes, 0, 0);
  return d.getTime();
}
const ev = (id: string, subject: string, start: number, end: number, extra: Partial<CalEvent> = {}): CalEvent => ({
  id, subject, start_ms: start, end_ms: end, all_day: false, location: null, organizer: null, has_join_link: false, ...extra,
});
const mon = -((new Date().getDay() + 6) % 7);
const agenda: CalEvent[] = [
  ev("e1", "Daily do time de produto", at(0, 9), at(0, 9, 30), { organizer: "Bruno Alves", has_join_link: true }),
  ev("e2", "Revisão do relatório trimestral", at(0, 10), at(0, 11, 30), { organizer: "Ana Souza", location: "Sala 3", has_join_link: true }),
  ev("e3", "1:1 com Camila", at(0, 10, 30), at(0, 11), { organizer: "Camila Rocha", has_join_link: true }),
  ev("e4", "Almoço com o cliente Ábax", at(0, 12, 30), at(0, 14), { location: "Restaurante Central" }),
  ev("e5", "Planejamento da sprint", at(1, 14), at(1, 15, 30), { organizer: "Bruno Alves", has_join_link: true }),
  ev("e6", "Workshop de arquitetura", at(2, 9), at(2, 12), { organizer: "Fabio (demo)", has_join_link: true }),
  ev("e7", "Retrospectiva", at(mon + 4, 16), at(mon + 4, 17), { has_join_link: true }),
  ev("e8", "Feriado (demo)", at(mon + 2, 0), at(mon + 3, 0), { all_day: true }),
];

const round = (): Round => ({ effects: [], wake_in_ms: null, capabilities: ["Histórico: ok", "Envio: ok"] });

export function startDemo(): void {
  mockIPC(
    (command, payload) => {
      const args = (payload ?? {}) as Record<string, unknown>;
      switch (command) {
        case "conversations":
          return conversations;
        case "messages": {
          const before = typeof args.beforeMs === "number" ? args.beforeMs : Infinity;
          const limit = typeof args.limit === "number" ? args.limit : 100;
          return messages
            .filter((m) => m.conversation_id === args.conversationId && m.created_ms < before)
            .sort((a, b) => b.created_ms - a.created_ms)
            .slice(0, limit);
        }
        case "older": {
          olderRounds += 1;
          addOlder(String(args.conversationId), olderRounds === 1 ? 40 : 10);
          return { round: round(), reached_start: olderRounds >= 2 };
        }
        case "draft":
          return drafts[String(args.conversationId)] ?? null;
        case "set_draft":
          if (String(args.text).trim() === "") delete drafts[String(args.conversationId)];
          else drafts[String(args.conversationId)] = String(args.text);
          return null;
        case "capabilities":
          return round().capabilities;
        case "me":
          return ME;
        case "agenda": {
          const fromMs = Number(args.fromMs);
          const toMs = Number(args.toMs);
          return agenda.filter((e) => e.start_ms < toMs && e.end_ms > fromMs);
        }
        case "join_meeting": {
          const found = agenda.find((e) => e.id === args.eventId);
          if (!found?.has_join_link) throw "evento sem link de reunião";
          console.info("[demo] abriria o navegador para o evento", found.id);
          return null;
        }
        case "derivatives":
          return derivatives[String(args.messageId)] ?? [];
        case "approve_derivative": {
          const found = Object.values(derivatives)
            .flat()
            .find((d) => d.id === args.derivativeId);
          if (!found) throw "derivado inexistente";
          if (found.stale) throw "derivado obsoleto: o original mudou";
          found.approved_ms ??= Date.now();
          return found.body;
        }
        case "send": {
          messages.push({
            id: `m${nextId++}`,
            conversation_id: String(args.conversationId),
            client_message_id: null,
            sender: ME,
            body: String(args.text),
            message_type: "Text",
            created_ms: Date.now(),
          });
          return { ...round(), effects: [{ kind: "sent", client_id: "demo" }] };
        }
        case "load":
        case "refresh":
          return round();
        default:
          throw `comando desconhecido no demo: ${command}`;
      }
    },
    { shouldMockEvents: true },
  );
}
