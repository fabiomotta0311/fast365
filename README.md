# Fast365 · `f365`

Cliente desktop nativo, ultrarrápido e de baixo consumo para o Microsoft 365 — começando pelo
**Teams**, preparado para acoplar o **Outlook**. Core em **Rust**; UI em **Tauri v2 + React**.
Abordagem: **emular o cliente web oficial** (sem Graph API, sem aprovação de admin de tenant).

[![CI](https://github.com/fabiomotta0311/fast365/actions/workflows/ci.yml/badge.svg)](https://github.com/fabiomotta0311/fast365/actions/workflows/ci.yml)

> **Identidade técnica:** cliente *local-first*, com **execução adaptativa** e **sincronização
> reproduzível** — mantido majoritariamente por **agentes de IA**.

> **Interoperabilidade / uso pessoal.** O Fast365 autentica a **sua própria conta** pelo fluxo oficial
> da Microsoft (MFA respeitado) e reproduz o que o cliente web já faz. Endpoints internos não têm
> contrato público e podem mudar sem aviso; use com a sua conta/autorização. Ver `AGENTS.md` (segurança).

## Documentos

| Documento | Para quê |
|-----------|----------|
| [`docs/ARQUITETURA.md`](docs/ARQUITETURA.md) | Plano de arquitetura e execução (o "porquê" de cada decisão). |
| [`docs/GATE-FASE0.md`](docs/GATE-FASE0.md) | Protocolo e registro da decisão Go/No-Go (matriz A–F). |
| [`AGENTS.md`](AGENTS.md) | Regras estáveis "sempre/nunca" para qualquer agente que contribuir. |
| [`README-DEV-VPS.md`](README-DEV-VPS.md) | Ambiente de dev headless na VPS (core sem GUI). |
| [`docs/onboarding/`](docs/onboarding/) | Prompts para integrar Hermes/Antigravity/DSH à memória compartilhada. |

## Status

**Fase 0 — gate de viabilidade: Go pendente.** O código dos checkpoints A–E existe e a **rodada 1**
(A–F verdes) está registrada nos comentários da
[#13](https://github.com/fabiomotta0311/fast365/issues/13), relatada pelo dono. Falta a **rodada 2**
independente e o bloco de decisão; ela está **adiada** porque a conta de trabalho não está mais
disponível (ver [`docs/GATE-FASE0.md`](docs/GATE-FASE0.md)). `f365 gate` roda os checkpoints
mecanizáveis e grava o relatório sanitizado de cada rodada.

**Já implementado nas branches de integração** (`next/fase-2`, `next/fase-3`), **ainda fora do `main`**:

- **Fase 2:** cache SQLite (WAL + FTS5), motor de sync determinístico (FSM), disponibilidade por
  capacidade, driver, refresh em background, reconexão do tempo real e `f365 sync`.
- **Fase 3:** contrato IPC, runtime, shell Tauri e frontend React no estilo do Teams (conversa
  virtualizada, HTML sanitizado com DOMPurify), histórico sob demanda e rascunhos.
- **Fase 4 (parcial):** modelo de fatos × derivados (#22), seam de middleware `pre_send`/`post_receive`
  e derivados na conversa. Ainda não há provedor de tradução.
- **Ferramentas:** `f365 logout` (troca de conta); empacotamento MSI/NSIS e CSP configurados.

**Não verificado:** o instalador e o cold start (precisam de `tauri build` num Windows) e o app com
dados reais do Teams. A UI foi conferida só com dados sintéticos (`?demo`).

**Regra do gate:** as Fases 2–4 avançaram antes do Go por decisão do dono; o merge delas no `main`
continua bloqueado até o Go (`AGENTS.md`, "Gate primeiro").

## Roadmap (por fase → Issues)

As Issues são autocontidas: cada uma linka a seção da arquitetura, o path local, critérios de aceite,
validação e dependências. Filtrar por [`ai-ready`](https://github.com/fabiomotta0311/fast365/issues?q=is%3Aissue+is%3Aopen+label%3Aai-ready)
para o que um agente pode pegar agora.

### Setup · Infra
- [#1](https://github.com/fabiomotta0311/fast365/issues/1) Instalar MSVC Build Tools (blocker local do Windows)
- [#2](https://github.com/fabiomotta0311/fast365/issues/2) Endurecer o CI
- [#3](https://github.com/fabiomotta0311/fast365/issues/3) Seams testáveis (Clock, fronteira do motor)

### Fase 0 · Gate (prioridade máxima)
- DevTools: [#4](https://github.com/fabiomotta0311/fast365/issues/4) login/token · [#5](https://github.com/fabiomotta0311/fast365/issues/5) REST chatsvcagg · [#6](https://github.com/fabiomotta0311/fast365/issues/6) Trouter (WS)
- Core: [#7](https://github.com/fabiomotta0311/fast365/issues/7) `f365 auth` (A) · [#8](https://github.com/fabiomotta0311/fast365/issues/8) troca de tokens (B) · [#9](https://github.com/fabiomotta0311/fast365/issues/9) `AuthenticatedExecutor`
- Teams: [#10](https://github.com/fabiomotta0311/fast365/issues/10) `chats` (C) · [#11](https://github.com/fabiomotta0311/fast365/issues/11) `listen` (D) · [#12](https://github.com/fabiomotta0311/fast365/issues/12) `send` (E)
- **[#13](https://github.com/fabiomotta0311/fast365/issues/13) Rodar a matriz A–F → decisão Go/No-Go**

### Fase 2 · Persistência & resiliência *(após o Go — implementada em `next/fase-2`, fora do `main`)*
- [#14](https://github.com/fabiomotta0311/fast365/issues/14) SQLite WAL + FTS5 · [#15](https://github.com/fabiomotta0311/fast365/issues/15) motor de sync reproduzível · [#16](https://github.com/fabiomotta0311/fast365/issues/16) refresh/reconexão · [#17](https://github.com/fabiomotta0311/fast365/issues/17) catálogo de evidências · [#18](https://github.com/fabiomotta0311/fast365/issues/18) disponibilidade por capacidade

### Fase 3 · UI (Tauri v2 + React) *(implementada em `next/fase-3`; instalador não verificado)*
- [#19](https://github.com/fabiomotta0311/fast365/issues/19) shell Tauri + empacotamento · [#20](https://github.com/fabiomotta0311/fast365/issues/20) frontend React/Fluent UI · [#21](https://github.com/fabiomotta0311/fast365/issues/21) fronteira IPC

### Fase 4 · Busca, Outlook & features de IA *(#22 feito em `next/fase-3`; #23–#26 pendentes)*
- [#22](https://github.com/fabiomotta0311/fast365/issues/22) fatos × interpretações · [#23](https://github.com/fabiomotta0311/fast365/issues/23) provider Outlook · [#24](https://github.com/fabiomotta0311/fast365/issues/24) ata automática · [#25](https://github.com/fabiomotta0311/fast365/issues/25) tradução em tempo real · [#26](https://github.com/fabiomotta0311/fast365/issues/26) FTS5→tantivy

## Para quem contribui (humano ou agente)

1. Leia **`AGENTS.md`** (invariantes, segurança, validação) e a seção citada na Issue.
2. Pegue uma Issue `ai-ready` sem `blocked` pendente.
3. Valide antes de concluir:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace
cargo test --workspace
```

No Windows, valide o caminho da WebView: `cargo check -p f365-cli --features webview`.

## CI e releases

| Workflow | Quando | O quê |
|----------|--------|-------|
| `ci.yml` | push na `main`, PR | fmt · clippy/test/doc Linux headless · clippy/test Windows `--features webview` · MSRV Linux headless e Windows com todas as features (webview) · `cargo deny` · cobertura (`cargo-llvm-cov`, resumo no job + `lcov.info`) |
| `audit.yml` | diário | `cargo deny check advisories` (vulnerabilidades novas sem precisar de push) |
| `release.yml` | tag `vX.Y.Z` (ou manual) | `f365.exe` release com webview → zip + SHA-256; na tag, publica a GitHub Release |

Para lançar: suba `version` em `crates/f365-cli/Cargo.toml`, faça merge e `git tag vX.Y.Z && git push --tags`
(a tag precisa bater com a versão, ou o workflow falha).

## Workspace

```
crates/f365-auth    # executor autenticado: SSO, WebView (feature `webview`), keyring, troca de tokens
crates/f365-teams   # provider Teams: REST chatsvcagg + Trouter (WS)
crates/f365-sync    # motor de sync: FSM sem I/O (Event → Decision) + disponibilidade por capacidade
crates/f365-storage # cache local: SQLite WAL + FTS5 (fonte da verdade, offline-first)
crates/f365-driver  # costura motor ⇄ executor ⇄ cache (busca, envia, grava, publica)
crates/f365-cli     # binário `f365` (gate da Fase 0)
```
