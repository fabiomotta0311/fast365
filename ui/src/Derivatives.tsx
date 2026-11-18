import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { Derivative, Message } from "./types";

const KIND_LABEL: Record<string, string> = {
  translation: "Tradução",
  summary: "Resumo",
  minutes: "Ata",
  proposed_action: "Resposta proposta",
};

function label(derivative: Derivative): string {
  const base = KIND_LABEL[derivative.kind] ?? derivative.kind;
  const lang = derivative.lang ? ` · ${derivative.lang}` : "";
  return `${base}${lang} · ${derivative.model}`;
}

type Props = {
  message: Message;
  /** Sobe quando o motor gravou/mudou derivados: força reler do cache. */
  version: number;
  onApprove: (derivative: Derivative, conversationId: string) => void;
};

/**
 * Derivados de uma mensagem (tradução, resumo, resposta proposta), sempre **ao lado** do original,
 * nunca no lugar dele. O texto é exibido como texto puro (`pre-wrap`): conteúdo gerado não passa
 * por HTML/Markdown.
 *
 * - Obsoleto (`stale`): o original mudou depois que foi gerado. Aparece recolhido, riscado, e
 *   não tem ação.
 * - Resposta proposta: só sai o texto que o usuário aprovou, exatamente como mostrado.
 */
export function Derivatives({ message, version, onApprove }: Props) {
  const [list, setList] = useState<Derivative[]>([]);

  useEffect(() => {
    let alive = true;
    invoke<Derivative[]>("derivatives", { messageId: message.id })
      .then((items) => {
        if (alive) setList(items);
      })
      // Sem core/sessão ou sem derivados: a mensagem segue sendo exibida normalmente.
      .catch(() => {
        if (alive) setList([]);
      });
    return () => {
      alive = false;
    };
  }, [message.id, version]);

  if (list.length === 0) return null;

  // Do mais novo ao mais antigo: o primeiro de cada tipo é o vigente.
  const seen = new Set<string>();
  const current = list.filter((derivative) => {
    if (derivative.stale || derivative.kind === "proposed_action") return false;
    if (seen.has(derivative.kind)) return false;
    seen.add(derivative.kind);
    return true;
  });
  const proposals = list.filter((d) => d.kind === "proposed_action" && !d.stale);
  const stale = list.filter((d) => d.stale);

  return (
    <div className="derivatives">
      {current.map((derivative) => (
        <div className="derivative" key={derivative.id}>
          <span className="derivative-label">{label(derivative)}</span>
          <p className="derivative-body">{derivative.body}</p>
        </div>
      ))}

      {proposals.map((derivative) => (
        <div className="derivative proposal" key={derivative.id}>
          <span className="derivative-label">{label(derivative)}</span>
          <p className="derivative-body">{derivative.body}</p>
          <div className="derivative-actions">
            {derivative.approved_ms === null ? (
              <button onClick={() => onApprove(derivative, message.conversation_id)}>
                Aprovar e enviar
              </button>
            ) : (
              <>
                <span className="badge approved">Aprovada</span>
                <button onClick={() => onApprove(derivative, message.conversation_id)}>
                  Reenviar o texto aprovado
                </button>
              </>
            )}
            <span className="hint">O texto enviado é exatamente este.</span>
          </div>
        </div>
      ))}

      {stale.length > 0 && (
        <details className="derivative stale">
          <summary>
            <span className="badge warning">Obsoleta: o original mudou</span>
            <span className="hint">{stale.length === 1 ? "1 versão" : `${stale.length} versões`}</span>
          </summary>
          {stale.map((derivative) => (
            <div key={derivative.id}>
              <span className="derivative-label">{label(derivative)}</span>
              <p className="derivative-body struck">{derivative.body}</p>
            </div>
          ))}
        </details>
      )}
    </div>
  );
}
