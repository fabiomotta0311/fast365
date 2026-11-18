import DOMPurify from "dompurify";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import type { Message } from "../types";

/** Corpo da mensagem: HTML do Teams passa por DOMPurify; texto vira Markdown (sem HTML cru). */
export function MessageBody({ message }: { message: Message }) {
  const isHtml = /html|richtext/i.test(message.message_type);
  if (isHtml) {
    // O Teams manda RichText/Html: sanitizar é obrigatório antes de injetar.
    const clean = DOMPurify.sanitize(message.body, { USE_PROFILES: { html: true } });
    return <div className="fx-body fx-body--html" dangerouslySetInnerHTML={{ __html: clean }} />;
  }
  return (
    <div className="fx-body">
      <Markdown remarkPlugins={[remarkGfm]}>{message.body}</Markdown>
    </div>
  );
}
