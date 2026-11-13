# Onboarding — DSH (Windows local, cliente MCP estático)

> Cole o conteúdo abaixo numa sessão nova do DSH. Ele é cliente MCP estático: escopo SEMPRE explícito.

---

Vou começar a trabalhar no projeto **Fast365** agora e preciso de você, DSH, integrado ao ai-memory desde o primeiro prompt.
Pasta do projeto: `C:\Users\fabio\Projects\fast365`
Nome do projeto: Fast365  (escopo compartilhado: `workspace="default"`, `project="fabiomotta0311-pm-project"`)

Contexto: este projeto é mantido por MÚLTIPLOS agentes (Hermes, Antigravity, DSH) que COMPARTILHAM a mesma memória de projeto. O repositório já está montado (arquitetura, AGENTS.md, .ai-memory.toml, Issues: https://github.com/fabiomotta0311/fast365).

## O que já está pronto (NÃO refaça, NÃO sobrescreva)
- MCP do ai-memory no profile: entrada `mcp-ai-memory`.
- Bridge de hooks: `ai-memory-hooks`, com `@deepseek-ai/dsh-hooks-claude-code@0.2.0-rc.2` e `configPath` em `C:\Users\fabio\AppData\Local\ai-memory\hooks\dsh\hooks.json`.
- A captura está em `allowlist`: sem `.ai-memory.toml` não grava nada. **Este projeto JÁ TEM `.ai-memory.toml`** na raiz.
- Você é cliente MCP estático: toda chamada com escopo de projeto precisa de `workspace` e `project` EXPLÍCITOS. Nunca dependa do "último projeto ativo".
- Escopo: **workspace="default", project="fabiomotta0311-pm-project"**. Nunca use escopos legados (`hermes/hermes-default`, `dsh/*` antigos).
- Nunca mostre o token do ai-memory no chat, em arquivo ou em comando.

## Passos
1. **Pré-checagem.** Confirme que o DSH foi reiniciado depois do `- insert:` do bridge (o profile `desktop` não recarrega o patch ao vivo) e que as tools `memory_*` aparecem. Se faltar, PARE e me diga.
2. **Marcador (já existe — só verificar).** Na raiz de `C:\Users\fabio\Projects\fast365`, o `.ai-memory.toml` JÁ existe com `workspace="default"` e `project="fabiomotta0311-pm-project"`. **NÃO rode `ai-memory project init` nem sobrescreva.** Abra, confirme que bate e me mostre o conteúdo. (Só crie se, por algum motivo, não existir.)
3. **Regras de roteamento.** Chame `memory_install_self_routing` (escopo explícito). Grave o `markered_block` no `AGENTS.md` entre `<!-- ai-memory:start -->` e `<!-- ai-memory:end -->`, **PRESERVANDO todo o resto** — o `AGENTS.md` do Fast365 já tem as regras do projeto e uma seção "Memória compartilhada (ai-memory)"; não apague nada (o bloco gerenciado é aditivo). Instale as skills que a tool retornar. Abaixo do bloco, fora dos marcadores, acrescente: "Neste projeto o DSH é cliente MCP estático: passe sempre workspace=default e project=fabiomotta0311-pm-project em toda chamada com escopo de projeto."
4. **Regras do projeto.** Leia `AGENTS.md` (invariantes/segurança/validação) e `docs/ARQUITETURA.md` (o porquê). Precedência: usuário > AGENTS.md e código > memória > preferências; se a memória contradisser o código, o CÓDIGO vale. Trabalhe em Issues `ai-ready` sem `blocked`.
5. **Contexto anterior.** `memory_handoff_list` (escopo explícito). Se houver handoffs pendentes, me mostre e só aceite com `memory_handoff_accept` se eu confirmar. Antes de decidir, `memory_query` (escopo explícito, limit 5).
6. **Teste de captura.** Confirme que a sessão aparece como `agent=dsh` no projeto `default/fabiomotta0311-pm-project` (`memory_read_session_observations` ou `/admin/sessions/by-agent`). Se não aparecer, PARE e diagnostique: o hook disparou? o marcador foi encontrado? o servidor respondeu? apareceu `Invalid chunk size line`?
7. **Validação de código (antes de concluir qualquer Issue).**
   ```sh
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo check --workspace
   cargo test --workspace
   ```
   No Windows, valide o gate/WebView: `cargo check -p f365-cli --features webview` (requer MSVC Build Tools — Issue #1).
8. **Segurança (AGENTS.md).** Login Microsoft e captura de sessão só na máquina Windows do dono. Nunca leve token/cookie/refresh/`.jwt`/`.har` para fora; refresh só no keyring; nada de segredo em logs/diagnóstico.
9. **Encerramento.** Me lembre que, ao terminar o dia, preciso rodar `ai-memory finalize-session --agent dsh` (o bridge não entrega `SessionEnd`).

## Entrega
Resumo curto com: conteúdo do `.ai-memory.toml` (confirmação de que já existia e bate); diff do `AGENTS.md` (só o bloco entre marcadores + a linha do cliente estático; resto intacto); skills instaladas; handoffs encontrados; resultado do passo 6. Depois, siga com as Issues normalmente.
