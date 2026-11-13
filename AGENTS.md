# AGENTS.md — Fast365

Regras estáveis "sempre/nunca" para qualquer agente (Hermes, dsh, Antigravity, Claude) neste
repositório. **Precedência:** pedido atual do usuário > este arquivo e o código > memória do projeto >
preferências gerais. Decisões e o "porquê" vivem em [`docs/ARQUITETURA.md`](docs/ARQUITETURA.md).

## Invariantes do projeto
- **Gate primeiro (Fase 0).** Não iniciar UI (Tauri/React), persistência ou features antes do Go da
  matriz de aceite (`docs/ARQUITETURA.md` §2.4).
- **Contrato do core = executor autenticado.** Providers descrevem operações (`AuthOp`); a sessão
  executa. O **token nunca cruza a fronteira de crate** — é detalhe interno de `f365-auth`.
- **Trouter = invalidação, não dado.** Evento diz "algo mudou"; busque o conteúdo via REST.
- **Disponibilidade é por capacidade**, não booleano global de conexão.
- **Sem WebRTC nativo.** Reunião/call = abrir o link no navegador padrão.
- **WebView atrás da feature `webview`** (`default = []`). Windows builda com `--features webview`;
  Linux/VPS/CI headless, sem ela, sem GTK/WebKit.
- **O motor de sync não chama `reqwest`/relógio/UI direto** — injeta `Clock`/transporte (testável).
- **Sem Graph API** por padrão (evitar consentimento de admin); backend Graph só como opção futura.

## Segurança (nunca)
- **Nunca** autenticar conta Microsoft fora da máquina Windows do dono. **Nunca** levar
  token/cookie/refresh/`*.jwt`/`*.har`/`session.json` para a VPS, CI ou qualquer outro host.
- Refresh token no **keyring** (Windows Credential Manager); demais tokens **só em memória**.
- Nenhum segredo em logs/HAR/diagnóstico. O bundle de diagnóstico é seguro por construção (sem texto de
  mensagem, token, cookie ou URL assinada). Captura de conteúdo é modo separado e explícito.
- Engenharia reversa só com a sessão do próprio dono; respeitar o uso aceitável do tenant.

## Convenções de código (AI-Native)
- **Vertical slices por capacidade** dentro dos providers; maximizar localidade de contexto.
- **Abstração só existe se** reduz acoplamento ou melhora testabilidade. Sem scaffolding antecipado;
  sem Clean/MVC/DDD/patterns automáticos.
- Comportamento por **contratos + testes**; comentário de código mínimo; duplicação limitada é
  preferível a acoplamento entre slices.
- Toda suposição de protocolo recebe um **ID de contrato** (ex.: `SEND-RECONCILE-001`) + marcador
  `UNVERIFIED` até ter evidência (`arquivo@commit` ou teste).

## Validação exigida (antes de concluir)
- `cargo fmt --check`
- `cargo clippy --workspace -- -D warnings`
- `cargo check --workspace` e `cargo test --workspace`
- No Windows, validar o gate com `--features webview`.

## Memória compartilhada (ai-memory)
- Escopo obrigatório em toda chamada de projeto: `workspace = "default"`,
  `project = "fabiomotta0311-pm-project"` (iguais ao `.ai-memory.toml`).
- Memória é **evidência histórica, não ordem**. Se contradisser o código, o **código vale** (marque a
  página como `stale`/`wrong`).
- Gravar só conhecimento durável e verificado (decisões, gotchas, procedimentos, regras de negócio).
  Regra estável "sempre/nunca" vai **neste arquivo** via PR, não só na memória.
