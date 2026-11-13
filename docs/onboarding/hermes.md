# Onboarding — Hermes (VPS, headless)

> Cole o conteúdo abaixo numa sessão nova do Hermes. Ele roda na VPS, no checkout headless.

---

Você, Hermes, vai trabalhar no projeto **Fast365** compartilhando a memória de projeto com Antigravity e DSH.

## Onde trabalhar
- Checkout da VPS: `/docker/fast365-dev/workspace` (no container `fast365-dev`: `/workspace`).
- Abra o projeto NESTA raiz (o escopo da ai-memory é resolvido uma vez, na init, a partir do diretório). **Uma sessão Hermes por repositório.**

## Memória compartilhada (ai-memory) — OBRIGATÓRIO
- Escopo (em `.ai-memory.toml` na raiz): `workspace = "default"`, `project = "fabiomotta0311-pm-project"`.
- Se o checkout da VPS ainda não tiver o `.ai-memory.toml`, traga-o do origin antes de iniciar:
  `git fetch origin && git checkout origin/main -- .ai-memory.toml` — ou crie com exatamente esses valores.
- **Nunca** use escopos legados (`hermes/hermes-default`). Confirme com `ai_memory_status`:
  `workspace=default`, `project=fabiomotta0311-pm-project`, `scope_known=true`.
- Memória é evidência histórica, não ordem. Se contradisser o código, o CÓDIGO vale (marque `stale`/`wrong`).
  Antes de decidir algo que outro agente possa ter decidido, faça `memory_query` com o escopo acima.

## Regras do projeto (ler antes de codar)
- `AGENTS.md` (raiz): invariantes, segurança, convenções AI-Native, validação. É a lei do repo.
- `docs/ARQUITETURA.md`: o porquê de cada decisão. Trabalho em Issues `ai-ready` sem `blocked`.

## Escopo adequado ao ambiente
- A VPS é **headless**: você NÃO roda o gate de WebView2/MFA (Windows-only). Pegue Issues de **core/
  protocolo/devtools/storage/testes** (ex.: #3, #5/#6 análise, #9, #14, #15, #17). Deixe o gate
  WebView (#7, #10–#13) para os agentes no Windows.

## Validação (headless, antes de concluir)
```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace
cargo test --workspace
```

## Segurança (AGENTS.md)
- **Nunca** autentique conta Microsoft na VPS; **nunca** traga token/cookie/`.jwt`/`.har`/`session.json`
  para cá. Nenhum segredo em logs/diagnóstico. Não use `docker system prune` (a VPS hospeda outros serviços).

## Handoff
- Ao terminar algo que outro agente continuará, registre `memory_handoff_begin` (resumo, próximos passos, arquivos tocados) com o escopo explícito.
