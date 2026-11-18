import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  CalendarLtrRegular,
  ChevronLeftRegular,
  ChevronRightRegular,
  LocationRegular,
  PeopleRegular,
  VideoRegular,
} from "@fluentui/react-icons";
import {
  HOUR_PX,
  addDays,
  allDayOn,
  layoutDay,
  rangeTitle,
  startOfDay,
  startOfWeek,
} from "../calendar";
import { clock } from "../format";
import type { CalEvent } from "../types";

type View = "week" | "day";

const WEEKDAY = new Intl.DateTimeFormat("pt-BR", { weekday: "short" });
const HOURS = Array.from({ length: 24 }, (_, hour) => hour);

type Props = {
  onStatus: (message: string) => void;
};

/**
 * Agenda (somente leitura). Lê do cache; "Entrar" pede ao core o link **pelo id do evento** e o
 * navegador padrão abre — a tela nunca vê nem escolhe a URL. Dados reais dependem do provider de
 * agenda (Outlook), que ainda não existe: sem ele a agenda aparece vazia.
 */
export function Calendar({ onStatus }: Props) {
  const [view, setView] = useState<View>("week");
  const [anchor, setAnchor] = useState(() => startOfDay(Date.now()));
  const [events, setEvents] = useState<CalEvent[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [now, setNow] = useState(() => Date.now());
  const scroller = useRef<HTMLDivElement>(null);

  const from = view === "week" ? startOfWeek(anchor) : startOfDay(anchor);
  const count = view === "week" ? 7 : 1;
  const days = useMemo(() => Array.from({ length: count }, (_, i) => addDays(from, i)), [from, count]);
  const to = addDays(from, count);

  const load = useCallback(async () => {
    try {
      setEvents(await invoke<CalEvent[]>("agenda", { fromMs: from, toMs: to }));
    } catch (error) {
      onStatus(String(error));
    }
  }, [from, to, onStatus]);

  useEffect(() => {
    void load();
  }, [load]);

  // A linha de "agora" anda; a agenda em si só relê ao navegar.
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 60_000);
    return () => window.clearInterval(timer);
  }, []);

  // Abre perto do início do expediente em vez da meia-noite.
  useEffect(() => {
    if (scroller.current) scroller.current.scrollTop = 7 * HOUR_PX;
  }, [view]);

  const selected = events.find((event) => event.id === selectedId) ?? null;
  const today = startOfDay(now);
  const step = view === "week" ? 7 : 1;
  const hasAllDay = days.some((day) => allDayOn(events, day).length > 0);

  const join = async (event: CalEvent) => {
    try {
      await invoke("join_meeting", { eventId: event.id });
    } catch (error) {
      onStatus(String(error));
    }
  };

  return (
    <section className="fx-cal" aria-label="Calendário">
      <div className="fx-cal__bar">
        <h1 className="fx-cal__title">Calendário</h1>
        <button type="button" className="fx-btn" onClick={() => setAnchor(startOfDay(Date.now()))}>
          Hoje
        </button>
        <button
          type="button"
          className="fx-iconbtn"
          aria-label={view === "week" ? "Semana anterior" : "Dia anterior"}
          onClick={() => setAnchor(addDays(anchor, -step))}
        >
          <ChevronLeftRegular fontSize={20} />
        </button>
        <button
          type="button"
          className="fx-iconbtn"
          aria-label={view === "week" ? "Próxima semana" : "Próximo dia"}
          onClick={() => setAnchor(addDays(anchor, step))}
        >
          <ChevronRightRegular fontSize={20} />
        </button>
        <span className="fx-cal__range">{rangeTitle(from, count)}</span>
        <div className="fx-seg" role="group" aria-label="Visualização">
          <button
            type="button"
            className={view === "day" ? "is-on" : ""}
            aria-pressed={view === "day"}
            onClick={() => setView("day")}
          >
            Dia
          </button>
          <button
            type="button"
            className={view === "week" ? "is-on" : ""}
            aria-pressed={view === "week"}
            onClick={() => setView("week")}
          >
            Semana
          </button>
        </div>
      </div>

      <div className="fx-cal__main">
        <div className="fx-cal__grid-wrap">
          <div className="fx-cal__heads" style={{ gridTemplateColumns: `52px repeat(${count}, 1fr)` }}>
            <span />
            {days.map((day) => (
              <div key={day} className={`fx-cal__head${day === today ? " is-today" : ""}`}>
                <span className="fx-cal__wd">{WEEKDAY.format(day).replace(".", "")}</span>
                <span className="fx-cal__dn">{new Date(day).getDate()}</span>
              </div>
            ))}
          </div>

          {hasAllDay && (
            <div className="fx-cal__allday" style={{ gridTemplateColumns: `52px repeat(${count}, 1fr)` }}>
              <span className="fx-cal__gutter-label">dia todo</span>
              {days.map((day) => (
                <div key={day} className="fx-cal__allday-cell">
                  {allDayOn(events, day).map((event) => (
                    <button
                      key={event.id}
                      type="button"
                      className="fx-cal__chip"
                      onClick={() => setSelectedId(event.id)}
                    >
                      {event.subject}
                    </button>
                  ))}
                </div>
              ))}
            </div>
          )}

          <div className="fx-cal__scroll fx-scroll" ref={scroller}>
            <div
              className="fx-cal__grid"
              style={{ gridTemplateColumns: `52px repeat(${count}, 1fr)`, height: 24 * HOUR_PX }}
            >
              <div className="fx-cal__gutter">
                {HOURS.slice(1).map((hour) => (
                  <span key={hour} style={{ top: hour * HOUR_PX }}>
                    {String(hour).padStart(2, "0")}:00
                  </span>
                ))}
              </div>
              {days.map((day) => (
                <div key={day} className={`fx-cal__col${day === today ? " is-today" : ""}`}>
                  {HOURS.map((hour) => (
                    <div key={hour} className="fx-cal__hour" style={{ top: hour * HOUR_PX }} />
                  ))}
                  {layoutDay(events, day).map(({ event, top, height, col, cols }) => (
                    <button
                      key={event.id}
                      type="button"
                      className={`fx-cal__event${event.id === selectedId ? " is-selected" : ""}${height < 40 ? " is-short" : ""}`}
                      style={{
                        top,
                        height,
                        left: `calc(${(col / cols) * 100}% + 2px)`,
                        width: `calc(${100 / cols}% - 4px)`,
                      }}
                      onClick={() => setSelectedId(event.id)}
                      title={event.subject}
                    >
                      <span className="fx-cal__event-time">
                        {clock(event.start_ms)}
                        {event.has_join_link && <VideoRegular fontSize={12} aria-label="reunião online" />}
                      </span>
                      <span className="fx-cal__event-subject">{event.subject}</span>
                    </button>
                  ))}
                  {day === today && (
                    <div
                      className="fx-cal__now"
                      style={{ top: ((now - today) / 3_600_000) * HOUR_PX }}
                      aria-hidden="true"
                    />
                  )}
                </div>
              ))}
            </div>
          </div>

          {events.length === 0 && (
            <p className="fx-cal__empty">
              <CalendarLtrRegular fontSize={20} aria-hidden="true" /> Nenhum evento em cache neste
              período. A agenda real depende do provider do Outlook, que ainda não existe.
            </p>
          )}
        </div>

        {selected && (
          <aside className="fx-cal__detail" aria-label="Detalhes do evento">
            <h2>{selected.subject}</h2>
            <p className="fx-cal__when">
              {selected.all_day
                ? "Dia inteiro"
                : `${clock(selected.start_ms)} – ${clock(selected.end_ms)}`}
              {" · "}
              {new Date(selected.start_ms).toLocaleDateString("pt-BR", {
                weekday: "long",
                day: "numeric",
                month: "long",
              })}
            </p>
            {selected.location && (
              <p className="fx-cal__meta">
                <LocationRegular fontSize={16} aria-hidden="true" /> {selected.location}
              </p>
            )}
            {selected.organizer && (
              <p className="fx-cal__meta">
                <PeopleRegular fontSize={16} aria-hidden="true" /> {selected.organizer}
              </p>
            )}
            {selected.has_join_link ? (
              <button type="button" className="fx-btn fx-btn--primary" onClick={() => void join(selected)}>
                <VideoRegular fontSize={16} aria-hidden="true" /> Entrar no navegador
              </button>
            ) : (
              <p className="fx-cal__meta">Sem link de reunião online.</p>
            )}
            <button type="button" className="fx-btn" onClick={() => setSelectedId(null)}>
              Fechar
            </button>
          </aside>
        )}
      </div>
    </section>
  );
}
