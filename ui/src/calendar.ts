/** Lógica pura da agenda (datas e posicionamento). Nada aqui fala com o core. */
import type { CalEvent } from "./types";

export const HOUR_PX = 48;
const MIN_EVENT_PX = 22;

/** Meia-noite local do dia de `ms`. */
export function startOfDay(ms: number): number {
  const date = new Date(ms);
  date.setHours(0, 0, 0, 0);
  return date.getTime();
}

/** Soma dias pelo calendário local (não por 24 h: respeita mudança de horário de verão). */
export function addDays(ms: number, days: number): number {
  const date = new Date(ms);
  date.setDate(date.getDate() + days);
  date.setHours(0, 0, 0, 0);
  return date.getTime();
}

/** Segunda-feira da semana de `ms`, à meia-noite local. */
export function startOfWeek(ms: number): number {
  const date = new Date(startOfDay(ms));
  date.setDate(date.getDate() - ((date.getDay() + 6) % 7));
  return date.getTime();
}

export type Placed = {
  event: CalEvent;
  top: number;
  height: number;
  col: number;
  cols: number;
};

/** Eventos com hora que tocam o dia, já cortados nele e em colunas lado a lado quando se sobrepõem. */
export function layoutDay(events: CalEvent[], day: number): Placed[] {
  const next = addDays(day, 1);
  const clipped = events
    .filter((event) => !event.all_day && event.start_ms < next && event.end_ms > day)
    .map((event) => ({
      event,
      start: Math.max(event.start_ms, day),
      end: Math.min(event.end_ms, next),
    }))
    .sort((a, b) => a.start - b.start || b.end - a.end);

  const placed: Placed[] = [];
  let cluster: { item: (typeof clipped)[number]; col: number }[] = [];
  let clusterEnd = -Infinity;
  const columnEnds: number[] = [];

  const flush = () => {
    const cols = columnEnds.length;
    for (const { item, col } of cluster) {
      const top = ((item.start - day) / 3_600_000) * HOUR_PX;
      const height = Math.max(((item.end - item.start) / 3_600_000) * HOUR_PX, MIN_EVENT_PX);
      placed.push({ event: item.event, top, height, col, cols });
    }
    cluster = [];
    columnEnds.length = 0;
    clusterEnd = -Infinity;
  };

  for (const item of clipped) {
    if (cluster.length > 0 && item.start >= clusterEnd) flush();
    let col = columnEnds.findIndex((end) => end <= item.start);
    if (col === -1) {
      col = columnEnds.length;
      columnEnds.push(item.end);
    } else {
      columnEnds[col] = item.end;
    }
    cluster.push({ item, col });
    clusterEnd = Math.max(clusterEnd, item.end);
  }
  flush();
  return placed;
}

/** Eventos de dia inteiro que tocam o dia. */
export function allDayOn(events: CalEvent[], day: number): CalEvent[] {
  const next = addDays(day, 1);
  return events.filter((event) => event.all_day && event.start_ms < next && event.end_ms > day);
}

const FULL = new Intl.DateTimeFormat("pt-BR", { day: "numeric", month: "long" });
const WITH_YEAR = new Intl.DateTimeFormat("pt-BR", { day: "numeric", month: "long", year: "numeric" });

/** "17 – 23 de novembro de 2026" (ou "30 de nov – 6 de dez de 2026" quando cruza o mês). */
export function rangeTitle(from: number, days: number): string {
  if (days === 1) {
    return new Intl.DateTimeFormat("pt-BR", { weekday: "long", day: "numeric", month: "long", year: "numeric" }).format(from);
  }
  const last = addDays(from, days - 1);
  const a = new Date(from);
  const b = new Date(last);
  if (a.getMonth() === b.getMonth() && a.getFullYear() === b.getFullYear()) {
    return `${a.getDate()} – ${WITH_YEAR.format(last)}`;
  }
  return `${FULL.format(from)} – ${WITH_YEAR.format(last)}`;
}
