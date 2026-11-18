import { colorOf, initialsOf } from "../format";

type Props = {
  name: string;
  size?: number;
};

/** Avatar de iniciais (ainda não há foto no cache). A cor é estável por nome. */
export function Avatar({ name, size = 36 }: Props) {
  return (
    <span
      className="fx-avatar"
      aria-hidden="true"
      style={{
        width: size,
        height: size,
        background: colorOf(name),
        fontSize: Math.round(size * 0.36),
      }}
    >
      {initialsOf(name)}
    </span>
  );
}
