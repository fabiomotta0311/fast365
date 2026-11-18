import { SendRegular } from "@fluentui/react-icons";

type Props = {
  value: string;
  onChange: (next: string) => void;
  onSend: () => void;
};

/** Caixa de mensagem (texto puro). Enter envia; Shift+Enter quebra a linha. */
export function Composer({ value, onChange, onSend }: Props) {
  const empty = value.trim().length === 0;
  return (
    <div className="fx-composer">
      <div className="fx-composer__box">
        <textarea
          className="fx-composer__input"
          value={value}
          rows={1}
          placeholder="Digite uma mensagem"
          aria-label="Digite uma mensagem"
          onChange={(event) => onChange(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
              event.preventDefault();
              if (!empty) onSend();
            }
          }}
        />
        <button
          type="button"
          className="fx-composer__send"
          aria-label="Enviar"
          title="Enviar"
          disabled={empty}
          onClick={onSend}
        >
          <SendRegular fontSize={20} />
        </button>
      </div>
    </div>
  );
}
