/** Helpers de apresentação: nada aqui fala com o core. */

const AVATAR_COLORS = [
  "#5b5fc7",
  "#8764b8",
  "#0f6cbd",
  "#038387",
  "#ca5010",
  "#986f0b",
  "#c239b3",
  "#498205",
];

/** Cor estável por nome (o mesmo nome tem sempre a mesma cor). */
export function colorOf(name: string): string {
  let hash = 0;
  for (const ch of name) hash = (hash * 31 + ch.codePointAt(0)!) >>> 0;
  return AVATAR_COLORS[hash % AVATAR_COLORS.length];
}

export function initialsOf(name: string): string {
  // Primeira letra/dígito de cada palavra: "Time de produto (demo)" → "TD", "(demo)" não vira "(".
  const marks = name
    .split(/\s+/)
    .map((part) => part.match(/\p{L}|\p{N}/u)?.[0] ?? "")
    .filter((mark) => mark !== "");
  if (marks.length === 0) return "?";
  const first = marks[0];
  const last = marks.length > 1 ? marks[marks.length - 1] : "";
  return (first + last).toUpperCase();
}

const MS_DAY = 86_400_000;

function startOfDay(ms: number): number {
  const d = new Date(ms);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

/** Hora do dia, no fuso local (`14:05`). */
export function clock(ms: number): string {
  return new Date(ms).toLocaleTimeString("pt-BR", { hour: "2-digit", minute: "2-digit" });
}

/** Rótulo da lista: hoje = hora, ontem = "Ontem", antes = dia/mês. */
export function listStamp(ms: number | null, now = Date.now()): string {
  if (ms === null) return "";
  const days = Math.round((startOfDay(now) - startOfDay(ms)) / MS_DAY);
  if (days <= 0) return clock(ms);
  if (days === 1) return "Ontem";
  return new Date(ms).toLocaleDateString("pt-BR", { day: "2-digit", month: "2-digit" });
}

/** Divisor de dia dentro da conversa ("Hoje", "Ontem", "17 de novembro"). */
export function dayLabel(ms: number, now = Date.now()): string {
  const days = Math.round((startOfDay(now) - startOfDay(ms)) / MS_DAY);
  if (days <= 0) return "Hoje";
  if (days === 1) return "Ontem";
  return new Date(ms).toLocaleDateString("pt-BR", { day: "numeric", month: "long" });
}

export function sameDay(a: number, b: number): boolean {
  return startOfDay(a) === startOfDay(b);
}
