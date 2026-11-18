import { mockIPC } from "@tauri-apps/api/mocks";
import type { Conversation, Derivative, Message, Round } from "./types";

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

let nextId = 100;
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

const round = (): Round => ({ effects: [], wake_in_ms: null, capabilities: ["Histórico: ok", "Envio: ok"] });

export function startDemo(): void {
  mockIPC(
    (command, payload) => {
      const args = (payload ?? {}) as Record<string, unknown>;
      switch (command) {
        case "conversations":
          return conversations;
        case "messages":
          return messages
            .filter((m) => m.conversation_id === args.conversationId)
            .sort((a, b) => b.created_ms - a.created_ms);
        case "capabilities":
          return round().capabilities;
        case "me":
          return ME;
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
