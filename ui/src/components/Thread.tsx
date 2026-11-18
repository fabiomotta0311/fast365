import { useEffect, useMemo, useRef } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { clock, dayLabel, sameDay } from "../format";
import type { Conversation, Derivative, Message } from "../types";
import { Avatar } from "./Avatar";
import { Composer } from "./Composer";
import { Derivatives } from "../Derivatives";
import { MessageBody } from "./MessageBody";

/** Mensagens do mesmo autor, no mesmo dia e a menos de 5 min, viram um bloco só. */
const GROUP_WINDOW_MS = 5 * 60_000;

type Props = {
  conversation: Conversation | null;
  /** Do mais novo para o mais antigo (como o cache devolve). */
  messages: Message[];
  me: string | null;
  derivativesVersion: number;
  draft: string;
  onDraft: (value: string) => void;
  onSend: () => void;
  onApprove: (derivative: Derivative, conversationId: string) => void;
};

export function Thread({
  conversation,
  messages,
  me,
  derivativesVersion,
  draft,
  onDraft,
  onSend,
  onApprove,
}: Props) {
  const scrollerRef = useRef<HTMLDivElement | null>(null);

  // Cronológico e virtualizado: conversas longas renderizam só o que está à vista.
  const chronological = useMemo(() => [...messages].reverse(), [messages]);
  const virtualizer = useVirtualizer({
    count: chronological.length,
    getScrollElement: () => scrollerRef.current,
    estimateSize: () => 60,
    overscan: 8,
  });

  // Ao abrir a conversa ou chegar mensagem nova no fim, acompanha o fim da lista.
  useEffect(() => {
    if (chronological.length === 0) return;
    virtualizer.scrollToIndex(chronological.length - 1, { align: "end" });
  }, [conversation?.id, chronological.length, virtualizer]);

  if (!conversation) {
    return (
      <section className="fx-thread" aria-label="Conversa">
        <div className="fx-empty">
          <div className="fx-empty__title">Escolha uma conversa</div>
          <div className="fx-empty__sub">As conversas em cache aparecem na lista ao lado.</div>
        </div>
      </section>
    );
  }

  const title = conversation.title || "(sem título)";

  return (
    <section className="fx-thread" aria-label={title}>
      <header className="fx-thread__head">
        <Avatar name={title} size={32} />
        <h2 className="fx-thread__name">{title}</h2>
      </header>

      {chronological.length === 0 ? (
        <div className="fx-empty">
          <Avatar name={title} size={96} />
          <div className="fx-empty__title">Iniciar uma nova conversa</div>
          <div className="fx-empty__sub">Ainda não há mensagens em cache aqui.</div>
        </div>
      ) : (
        <div className="fx-thread__scroll fx-scroll" ref={scrollerRef}>
          <div style={{ height: virtualizer.getTotalSize(), position: "relative" }}>
            {virtualizer.getVirtualItems().map((item) => {
              const message = chronological[item.index];
              const previous = chronological[item.index - 1];
              const sender = message.sender ?? "Desconhecido";
              const mine = me !== null && message.sender === me;
              const previousMine = previous !== undefined && me !== null && previous.sender === me;
              const newDay = !previous || !sameDay(previous.created_ms, message.created_ms);
              const compact =
                !mine &&
                !newDay &&
                previous !== undefined &&
                !previousMine &&
                previous.sender === message.sender &&
                message.created_ms - previous.created_ms < GROUP_WINDOW_MS;

              return (
                <div
                  key={message.id}
                  className="fx-slot"
                  data-index={item.index}
                  ref={virtualizer.measureElement}
                  style={{ transform: `translateY(${item.start}px)` }}
                >
                  {newDay ? <div className="fx-divider">{dayLabel(message.created_ms)}</div> : null}
                  <div className={`fx-msg${mine ? " fx-msg--mine" : ""}${compact ? " fx-msg--cont" : ""}`}>
                    {mine ? null : compact ? (
                      <span className="fx-msg__gutter" />
                    ) : (
                      <Avatar name={sender} size={32} />
                    )}
                    <div className="fx-msg__col">
                      {mine || compact ? null : (
                        <div className="fx-msg__head">
                          <span className="fx-msg__author">{sender}</span>
                          <span className="fx-msg__time">{clock(message.created_ms)}</span>
                        </div>
                      )}
                      <div className="fx-msg__bubble">
                        <MessageBody message={message} />
                      </div>
                      {mine ? <span className="fx-msg__meta">{clock(message.created_ms)}</span> : null}
                      <Derivatives
                        message={message}
                        version={derivativesVersion}
                        onApprove={onApprove}
                      />
                    </div>
                  </div>
                </div>
              );
            })}
          </div>
        </div>
      )}

      <Composer value={draft} onChange={onDraft} onSend={onSend} />
    </section>
  );
}
