# Onboarding dos agentes (Fast365)

Prompts para colocar cada agente a trabalhar neste projeto **compartilhando a mesma memória**
(ai-memory) e dentro das premissas do `AGENTS.md`.

| Agente | Prompt | Onde roda |
|--------|--------|-----------|
| Hermes | [`hermes.md`](hermes.md) | VPS (`/docker/fast365-dev/workspace`), headless |
| Antigravity | [`antigravity.md`](antigravity.md) | Windows local (`C:\Users\fabio\Projects\fast365`) |
| DSH | [`dsh.md`](dsh.md) | Windows local (cliente MCP estático) |

## O que todos compartilham

- **Escopo único de memória** (em [`.ai-memory.toml`](../../.ai-memory.toml) na raiz):
  `workspace = "default"`, `project = "fabiomotta0311-fast365"`.
  O store é server-side chaveado por `(workspace, project)` → o mesmo escopo = a mesma memória, em
  qualquer máquina. **Nunca** usar escopos legados (`hermes/hermes-default`, `antigravity/*`, `dsh/*`).
- **Regras do projeto:** [`AGENTS.md`](../../AGENTS.md) (invariantes, segurança, validação) e
  [`docs/ARQUITETURA.md`](../ARQUITETURA.md) (o porquê).
- **Trabalho em Issues** com label `ai-ready` sem `blocked` pendente (roadmap no README do repo).

## Pré-requisito que o prompt NÃO resolve

O `.ai-memory.toml` define apenas **qual escopo** usar. Para colaborarem de fato, o **plugin/MCP
ai-memory de cada agente precisa estar habilitado e apontando para o MESMO backend**. Confirme em
cada um (`ai_memory_status` / `memory_*`) que `project = fabiomotta0311-fast365` e o escopo é
conhecido antes de confiar na memória compartilhada.
