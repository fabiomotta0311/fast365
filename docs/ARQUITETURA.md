# Fast365 — Plano de Arquitetura e Execução

> Cliente desktop nativo, ultrarrápido e de baixo consumo para o ecossistema Microsoft 365.
> Entrega final: **aplicação desktop nativa (`.exe`)** para Windows primeiro; Linux/macOS depois.
> CLI de validação: `f365`. Core em Rust puro; UI visual em **Tauri v2 + React** (ver §0.1).
> Abordagem: **emulação do cliente web oficial** (sem Graph API, sem aprovação de admin de tenant).
> Identidade técnica: **local-first · execução adaptativa · sincronização reproduzível** · manutenção AI-Native (§0.2).

---

## 0.1 Decisão de stack de UI — Tauri v2 + React (não `egui`)

A camada visual (Fase 3) usa **Tauri v2** como shell desktop + **React/Vite/TypeScript** dentro do
WebView2. Decisão tomada após avaliar `egui` (immediate mode), que foi **descartado para a UI** —
embora a ideia original.

**Por quê trocar:** um mensageiro precisa de seleção parcial de texto, renderização de HTML/RichText,
reflow fluido, emoji colorido, cópia de trechos e links clicáveis. Em `egui` cada um desses itens é
reimplementado na unha; em web/React são nativos. Além disso: (a) o `wry`/WebView2 **já** entra no
projeto na Fase 0 (login MFA), então o Tauri só consolida uma dependência existente; (b) as mensagens
chegam do `chatsvcagg` como `messagetype: RichText/Html`, que uma UI web renderiza nativamente; (c)
**Fluent UI v9** é a mesma biblioteca que o Teams oficial usa.

**Trade-offs assumidos (olhos abertos):**
- **Cold start:** a meta original de < 100 ms **não se aplica** com WebView2. Meta revisada: **~200–400 ms**.
- **RAM vs `egui`:** `egui` seria mais leve (~15–30 MB) que Tauri+React (~45–90 MB). Troca-se ~30–60 MB
  pela produtividade de UI. (Contra o Teams oficial, 1–2.5 GB, ambos ganham com folga — essa é a
  comparação de valor ao usuário e é legítima no pitch do produto.)
- **Segurança — obrigatório:** renderizar HTML vindo de mensagens do Teams dentro do WebView é
  superfície de **XSS/injeção**. **Sanitização (DOMPurify) é requisito**, não opcional. Preferir
  `react-markdown` (não renderiza HTML cru por padrão) e sanitizar qualquer HTML que for exibido.
- **Portabilidade tem asterisco:** Tauri **não usa a mesma engine de webview nos 3 SOs** — Windows
  WebView2 (Chromium), macOS WKWebView, Linux WebKitGTK (o mais problemático). Começar Windows-only é
  a decisão certa; ao portar, **reteste a UI por SO** (ao contrário de `egui`, que renderiza idêntico
  via wgpu). A portabilidade é real, só não é de graça.

A fronteira de crates **não muda** com essa decisão: o core Rust (`f365-auth`/`f365-teams`/`f365-storage`)
é idêntico; a UI apenas fala com ele por **commands/events do Tauri** em vez de canais `mpsc`/`watch` —
a mesma fronteira com outro nome.

---

## 0.2 Premissa transversal — AI-Native & manutenção por agentes

O Fast365 é mantido majoritariamente por **agentes de IA** (Hermes, dsh, Antigravity, Claude). A
arquitetura otimiza o **custo esperado de manutenção automatizada** — menos arquivos e contexto para
compreender, modificar e validar uma mudança — sem abrir mão de correção. Regras operacionais estáveis
vivem em [`AGENTS.md`](../AGENTS.md); isto aqui é o resumo de arquitetura.

**Princípios adotados:**
- **Localidade de contexto:** uma funcionalidade se entende/modifica olhando o mínimo de arquivos.
  Dentro de `f365-teams`, organizar por **capacidade** (`chats/`, `history/`, `send/`, `realtime/`),
  slices auto-contidos — não por camada técnica (`models/`+`services/`+`repos/`).
- **Abstração só paga o próprio custo:** uma trait/interface sobrevive se **reduz acoplamento** ou
  **torna a validação mais barata** (testabilidade). Senão, não existe. É o filtro que decide quais
  abstrações da §1/Fase 2 ficam — o **executor autenticado**, a **fronteira do motor** e o **modelo de
  capacidade** passam; qualquer trait "para um provider hipotético" não.
- **Sem scaffolding antecipado:** nada de estrutura para necessidade futura hipotética (o *crate*
  `f365-outlook` só nasce na Fase 4; uma variante de enum como placeholder é barata e permitida).
- **Contratos + checagens executáveis no lugar de narrativa:** comportamento expresso por tipos,
  invariantes e testes; comentário de código mínimo. (Este doc é registro de *decisão*, não narrativa
  redundante do código.)
- **Duplicação limitada > acoplamento:** repetir um punhado de linhas é melhor que criar dependência
  entre slices só para não repetir.

**Legibilidade humana não é meta separada:** contribuidores open-source também desenvolvem via agentes,
então otimizar para manutenção barata por IA (contratos explícitos + testes determinísticos) já serve o
humano. Não há trade-off a equilibrar aqui.

---

## 0. Princípio Reitor — Gate de Viabilidade (Fase 0 / PoC)

**Nada da camada visual (Tauri/React), persistência (`rusqlite`) ou features secundárias
começa antes do PoC de conexão passar no Go/No-Go.**

O primeiro — e único — entregável de código da Fase 0 é um binário CLI (`f365`) que prova,
de ponta a ponta:

```
login (WebView efêmera) → captura de sessão → cadeia de troca de tokens
      → request REST real no chatsvcagg → conexão WebSocket viva no Trouter → envio de mensagem
```

Se qualquer elo dessa cadeia não fechar de forma reprodutível, o projeto é **No-Go** na forma
proposta e precisa ser repensado (ex.: cair para automação de navegador headless). Só com o
**Go** avançamos para Fases 1–4.

### Avisos de risco (assumir e documentar, não ignorar)
- Endpoints internos do Teams **não têm contrato público**: formato, headers e hosts mudam sem aviso.
- Pode haver **rate-limiting**, fingerprinting de cliente e Conditional Access bloqueando sessão não-oficial.
- Confirme que a conta é **sua** (pessoal/dev) ou que há autorização de uso — políticas de uso aceitável
  do tenant podem proibir clientes não-oficiais.
- Toda a engenharia reversa deve ser feita **com a sua própria sessão autenticada**, observando o seu
  próprio tráfego no DevTools.

---

## 1. Workspace & Estrutura de Crates

Workspace Cargo multi-crate, com fronteira dura entre **Auth/SSO** e **Providers**, para plugar
o Outlook depois reusando a mesma sessão.

```
fast365/
├── Cargo.toml                      # [workspace]
├── docs/
│   └── ARQUITETURA.md              # este documento
└── crates/
    ├── f365-auth/                  # [Fase 0] executor autenticado: SSO, WebView, keyring, troca de tokens
    ├── f365-teams/                 # [Fase 0] provider Teams: chatsvcagg (REST) + Trouter (WS)
    ├── f365-sync/                  # [Fase 0: fronteira · Fase 2: lógica] motor FSM sem I/O (Event → Decision)
    ├── f365-cli/                   # [Fase 0] binário `f365` — orquestra auth + teams
    ├── f365-storage/               # [Fase 2] placeholder — cache rusqlite + refresh em background
    ├── f365-ui/                    # [Fase 3] placeholder — shell Tauri v2
    │   └── src-ui/                 #           app React + Vite + TypeScript
    └── f365-outlook/               # [Fase 4] placeholder — OWA Substrate API
```

### Responsabilidades e fronteiras

| Crate          | Fase | Responsabilidade | Depende de | NÃO conhece |
|----------------|------|------------------|------------|-------------|
| `f365-auth`    | 0    | **Executor autenticado**: login `wry`, captura, `keyring`, refresh, troca de tokens; **executa operações** (token interno) | `wry`, `reqwest`, `keyring`, `tokio` | nada de Teams/Outlook — só `Resource` |
| `f365-teams`   | 0    | Cadeia Skype/CSA, REST `chatsvcagg`, WS `Trouter` (descreve operações) | `f365-auth` (`AuthenticatedExecutor`) | `wry`, UI, storage, **tokens** |
| `f365-sync`    | 0→2  | Motor de sync: FSM pura `handle(now, Event) -> Vec<Decision>`; reexporta `Clock` e `AuthenticatedExecutor` para o driver | `f365-auth` (só tipos) | rede, relógio de parede, WebView, UI, storage — garantido por teste do manifesto + `clippy.toml` |
| `f365-cli`     | 0    | Binário `f365`, subcomandos, logging | `f365-auth`, `f365-teams`, `clap` | — |
| `f365-storage` | 2    | SQLite WAL (fonte da verdade, offline-first) + busca FTS5 | `rusqlite` | UI |
| `f365-ui`      | 3    | Shell Tauri v2 + UI React isolada do core por commands/events | `tauri`, `f365-*` via IPC | detalhes de rede |
| `f365-outlook` | 4    | OWA/Substrate | `f365-auth` | Teams |

**Contrato central do core — executor autenticado (não "fornecedor de token").**
A interface pública do core **executa uma operação autenticada**; o **token é detalhe interno** e nunca
cruza a fronteira de crate. O provider descreve a operação + seus requisitos de auth; a sessão decide
*como* executá-la (HTTP Rust com token efêmero, ou dentro da página parqueada). Isso reduz a circulação
de segredos e mantém regra de domínio (chats) fora do módulo de sessão.

```rust
// f365-auth/src/lib.rs (esboço do contrato, não implementação)
#[async_trait::async_trait]
pub trait AuthenticatedExecutor: Send + Sync {
    /// Executa a operação, resolvendo auth + transporte internamente.
    async fn execute(&self, op: AuthOp) -> Result<HttpResponse, ExecError>;
}

/// O que o provider declara — NÃO contém token.
pub struct AuthOp {
    pub resource: Resource,         // qual aud/autorização a operação exige
    pub request: HttpRequest,       // método, path, headers de domínio, body
    pub needs_page_context: bool,   // exige cookies / execução na página
    pub idempotent: bool,           // pode repetir com segurança (importa p/ reconciliação)
    pub timeout: Duration,
}

pub enum Resource { TeamsChatSvc, OutlookSubstrate /* Fase 4 */ }
```

Internamente (lição do `rusty-teams`, ver §5), o executor escolhe a **rota de transporte** — e o token
vive só aqui dentro:

```rust
// estratégia INTERNA do executor — não cruza a fronteira de crate
enum Route {
    Rust,     // reqwest + token efêmero (PADRÃO, leve)
    InPage,   // fetch na WebView parqueada (contexto genuíno + cookies) — FALLBACK
}
// Padrão: Rust; cai para InPage em 401/403/challenge de Conditional Access.
// WebSocket e chamadas cookie-bound usam InPage sempre.
```

### `Cargo.toml` raiz (workspace)

```toml
[workspace]
resolver = "2"
members = ["crates/f365-auth", "crates/f365-teams", "crates/f365-cli"]
# storage/ui/outlook adicionados só nas respectivas fases

[workspace.package]
edition = "2021"           # subir p/ 2024 quando estabilizar na toolchain de uso
license = "MIT OR Apache-2.0"
rust-version = "1.89"

[workspace.dependencies]
tokio   = { version = "1.40", features = ["rt-multi-thread", "macros", "sync", "time", "signal"] }
reqwest = { version = "0.12", default-features = false, features = ["json", "cookies", "gzip", "brotli", "rustls-tls", "stream"] }
tokio-tungstenite = { version = "0.24", features = ["rustls-tls-webpki-roots"] }
serde      = { version = "1", features = ["derive"] }
serde_json = "1"
keyring    = "3"
anyhow     = "1"
thiserror  = "2"
tracing    = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
url        = "2"
async-trait = "0.1"
```

### `crates/f365-auth/Cargo.toml`

```toml
[package]
name = "f365-auth"
version = "0.0.0"
edition.workspace = true

[dependencies]
tokio.workspace = true
reqwest.workspace = true
serde.workspace = true
serde_json.workspace = true
keyring.workspace = true
anyhow.workspace = true
thiserror.workspace = true
tracing.workspace = true
url.workspace = true
async-trait.workspace = true

# WebView nativa — OPCIONAL, atrás da feature `webview` (ver nota abaixo).
wry = { version = "0.47", optional = true }
tao = { version = "0.30", optional = true }  # event loop / janela que hospeda a WebView
base64 = "0.22"
jsonwebtoken = "9"           # inspeção (NÃO validação) de claims: exp, aud, tid
```

> Nota de plataforma (Windows 11, seu caso): `wry` usa **WebView2 (Edge/Chromium)** — garanta o
> runtime WebView2 instalado. Isso é vantagem: o webview compartilha a engine do Edge, então o fluxo
> MSAL/MFA roda igual ao navegador. O `keyring` v3 usa o **Windows Credential Manager** por padrão.

> **Feature `webview` (cross-platform limpo).** `wry`/`tao` são **opcionais**, atrás da feature
> `webview` (`default = []`). Consequências:
> - **Windows (dev/produção):** builde com a feature — `cargo run -p f365-cli --features webview`.
>   O `f365-cli` reexporta via `webview = ["f365-auth/webview"]`.
> - **Linux/VPS/CI (headless):** build padrão **sem** a feature → compila o core **sem GTK/WebKit**,
>   sem nenhuma dependência de GUI no servidor.
> - O código que toca a WebView (ex.: implementação de `f365 auth`) fica atrás de
>   `#[cfg(feature = "webview")]`, com um erro amigável quando a feature está desligada.

### `crates/f365-teams/Cargo.toml`

```toml
[package]
name = "f365-teams"
version = "0.0.0"
edition.workspace = true

[dependencies]
f365-auth = { path = "../f365-auth" }
tokio.workspace = true
reqwest.workspace = true
tokio-tungstenite.workspace = true
serde.workspace = true
serde_json.workspace = true
anyhow.workspace = true
thiserror.workspace = true
tracing.workspace = true
url.workspace = true
async-trait.workspace = true
uuid = { version = "1", features = ["v4"] }     # messageId / clientmessageid
futures-util = "0.3"                             # streams do WS
```

### `crates/f365-cli/Cargo.toml`

```toml
[package]
name = "f365-cli"
version = "0.0.0"
edition.workspace = true

[[bin]]
name = "f365"
path = "src/main.rs"

[dependencies]
f365-auth  = { path = "../f365-auth" }
f365-teams = { path = "../f365-teams" }
tokio.workspace = true
clap = { version = "4", features = ["derive"] }
anyhow.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
serde_json.workspace = true
```

---

## 2. Especificação do PoC — Critério Go/No-Go

### 2.1 Fluxo de captura de sessão + Transport trait (fallback)

```
┌─────────────┐   abre    ┌───────────────────────────────┐
│  f365 auth  │──────────▶│  Janela WebView (wry/WebView2) │
└─────────────┘           │  login.microsoftonline.com/... │
       ▲                  │  + MFA + Conditional Access     │
       │                  └───────────────┬────────────────┘
       │ tokens/cookies                   │ redirect p/ teams.cloud.microsoft
       │ capturados                       ▼
       │                  ┌───────────────────────────────┐
       │                  │  Interceptação:                │
       │                  │  - cookies de sessão (ESTSAUTH…)│
       │                  │  - localStorage MSAL (idtoken, │
       │                  │    accesstoken, refreshtoken)   │
       │                  └───────────────┬────────────────┘
       │                                  │
┌──────┴──────┐  persist   ◀──────────────┘  parqueia WebView (fallback de transporte)
│   keyring   │
└─────────────┘
```

**Passo a passo técnico:**

1. **Abrir** a WebView `wry` apontando para o endpoint de autorização que o próprio Teams Web usa
   (capturado no DevTools — ver §3). Reutilizar `client_id`, `redirect_uri`, `scope` e `code_challenge`
   exatamente como o cliente web envia.
2. **Deixar o usuário autenticar** (senha + MFA + eventual Conditional Access) dentro da janela. Nunca
   automatizar senha/MFA — é o usuário quem faz, igual ao navegador.
3. **Interceptar a sessão** por dois caminhos complementares:
   - **localStorage/sessionStorage**: injetar JS via `wry` (`evaluate_script`) após o redirect final,
     lendo as chaves MSAL (`msal.account.keys`, entradas `*-accesstoken-*`, `*-refreshtoken-*`,
     `*-idtoken-*`). O MSAL.js v2 do Teams guarda os tokens nessas chaves.
   - **Cookies**: ler os cookies de `*.microsoftonline.com` / `*.cloud.microsoft` do WebView2
     (store do perfil efêmero) — `ESTSAUTH`, `ESTSAUTHPERSISTENT`, etc. Úteis para refresh via
     endpoint web e para chamadas que exigem cookie além de Bearer.
4. **Parquear a WebView** em background assim que capturar (esconder a janela, manter o contexto vivo
   em um perfil de dados dedicado do WebView2). Para o PoC mínimo você *pode* destruí-la; **projete,
   porém, para poder mantê-la** — é o seu seguro contra o No-Go de Conditional Access (ver nota abaixo).
5. **Persistir no `keyring`** (Windows Credential Manager): guardar o **refresh token** sob um serviço
   `fast365` / conta `<upn-do-usuario>`. Access/skype tokens ficam **só em memória** e são renovados sob
   demanda (padrão convergente de `ic3`/`ost`/`rusty-teams`).

> **Implementado (#7, `f365-auth::capture`).** Em vez de remontar a URL de authorize, a WebView abre
> o próprio Teams Web e deixa a página conduzir o MSAL (client_id/redirect/PKCE idênticos ao oficial).
> O cache MSAL é lido pelo **conteúdo** das entradas; RT + cookies `ESTSAUTH*` vão para o keyring em
> pedaços (limite de 2560 B por credencial). Contratos `UNVERIFIED` até o Checkpoint A:
> `AUTH-ENTRY-001`, `AUTH-MSAL-CACHE-001/002`, `AUTH-COOKIES-001` e **`AUTH-RT-SPA-001`** — o RT do
> cliente web (SPA) dura ~24 h fixas e é preso à origem: risco direto ao Checkpoint F, mitigado pelo
> perfil persistente da WebView (`%LOCALAPPDATA%ast365\webview`) para re-login silencioso.
>
> **Checkpoint A: verde (2026-11-13, conta do dono).** Janela fechou sozinha; RT + 5 cookies no
> keyring; `whoami` correto em processo novo. Confirmados `AUTH-ENTRY-001`, `AUTH-MSAL-CACHE-001`,
> `AUTH-COOKIES-001`. O RT é do cliente **web** `5e3ce6c0-…` (sem family RT), não do nativo
> `1fec8e78-…` citado em §2.2 → `AUTH-RT-SPA-001` segue aberto e é o primeiro ponto do #8.
>
> **Pós-review (#35).** Sessão só é aceita se lida nas origens HTTPS exatas do Teams e no documento
> atual (`AUTH-ORIGIN-001`; geração por navegação de topo); a identidade é a **conta ativa** da página
> (`AUTH-MSAL-ACTIVE-001`), com falha fechada se ambígua; o keyring grava **gerações** completas e só
> publica o ponteiro após reler — uma regravação interrompida nunca mistura RTs.

> **Transporte interno do executor (lição do `rusty-teams`) — seu seguro contra No-Go.** O executor (§1)
> escolhe *como* o request sai: (1) **Rust puro** (`reqwest`) — padrão, leve; (2) **in-page fetch** pela WebView
> parqueada (executa no contexto genuíno do cliente, com cookies e o token que o MSAL da página já
> renova) — para endpoints que fingerprintam/exigem cookie; (3) **cookie-bound/WS** sempre dentro da
> página. Quando a Microsoft barrar o request puro (checkpoint B/F), você **degrada para a página real
> em vez de dar No-Go**. É o detalhe que mais aumenta a robustez do projeto.

### 2.2 Cadeia de troca de tokens (alvo a validar)

```
[1] MSAL Refresh Token  (capturado da WebView)
        │  POST login.microsoftonline.com/{tenant}/oauth2/v2.0/token
        │  grant_type=refresh_token, scope=<scope do Teams/Skype>
        ▼
[2] Access Token "Teams/Skype"  (aud = serviço de chat)
        │  troca interna: endpoint de authz do CSA emite…
        ▼
[3] Skype/Chat Token + Region/CSA endpoint
        │  descobre o host regional (ex.: {region}.ng.msg.teams.microsoft.com)
        ▼
[4] Autorização válida em:
        ├── chatsvcagg (REST)  → Authorization: Bearer / Skype token
        └── Trouter    (WS)    → registro + URL de socket + headers
```

> Os nomes exatos de cada `aud`, host regional e o formato do Skype token **devem vir do DevTools**
> (§3) — o PoC só "copia" o que o cliente web realmente faz. O objetivo do código é **reproduzir**,
> não adivinhar.

**Dados concretos (dos clientes open-source — ver §5):**
- **Client ID público first-party do Teams:** `1fec8e78-bce4-4aaf-ab1b-5451cc387264` — reutilize-o e
  **não** registre app no Entra (fonte: `ic3`).
- **Tríade de tokens confirmada:** `token-teams.jwt`, `token-skype.jwt`, `token-chatsvcagg.jwt`
  (fonte: `fossteams`) — exatamente os passos [2]→[3]→[4] acima.
- **Alternativa de pairing:** o *device-code flow* (ic3/ost) dispensa a WebView, mas admins **costumam
  bloquear** device-code por política de Conditional Access — por isso a WebView continua o caminho
  primário, com device-code como opção secundária.

> **Checkpoint B: verde (2026-11-13, conta do dono, #8).** Cadeia real, sem WebView: RT (cliente web
> `5e3ce6c0-…`, resgatado com `Origin: https://teams.cloud.microsoft`) → token `aud
> https://api.spaces.skype.com` → `authsvc/v1.0/authz` → skype token + `regionGtms.chatService`
> (`https://<região>.ng.msg.teams.microsoft.com`); token `aud https://chatsvcagg.teams.microsoft.com`
> para o REST. O RT rotaciona a cada resgate. Valores no protocol profile
> `crates/f365-auth/src/profile/teams-web.json`. Em aberto: `AUTH-RT-SPA-001` (validade fixa ~24 h
> desde o login) — medir resgatando depois de 24 h da captura.

> **`AUTH-RT-SPA-001` confirmado (2026-11-15).** O RT do cliente web tem validade fixa (~24 h
> desde o login, não estendida pela rotação): recusado com `AADSTS700084` mais de 24 h após a captura. O
> `f365 auth` seguinte re-logou **sem senha** pelos cookies do perfil persistente. Decisão: o
> `TokenBroker` re-pareia sozinho **uma vez por processo** (WebView escondida; a janela só aparece se
> o Microsoft pedir login). Mesmo caminho do prior art `danieldaeschle/rusty-teams`, que não guarda
> RT e recarrega a página parqueada para o MSAL dela re-logar.

### 2.3 Subcomandos mínimos do binário `f365` (os quatro provam o gate)

```
f365 auth            # Fase 0-A: abre WebView, captura sessão, grava no keyring. Imprime UPN + validade.
f365 chats [--top N] # Fase 0-B: troca de tokens + GET chatsvcagg → lista conversas/canais (id + título).
f365 listen          # Fase 0-C: conecta no Trouter (WS), registra, imprime eventos de novas mensagens.
f365 send <chatId> <texto>   # Fase 0-D: POST chatsvcagg → envia mensagem. Imprime messageId retornado.
```

Auxiliares úteis durante a engenharia reversa:
```
f365 token --resource teams     # imprime o access token atual (debug) + claims decodificados (exp/aud/tid)
f365 whoami                      # valida sessão: UPN, tenant, região descoberta
```

> **Trouter = sinal de invalidação, não dado (lição do `rusty-teams`).** Trate cada evento do Trouter
> como "algo mudou em X" e **busque o conteúdo fresco via REST** (§3.4), em vez de parsear o corpo do
> evento. Isso te desacopla do schema indocumentado do payload do WS, que muda sem aviso — robustez
> de graça, e simplifica o `f365 listen`.

### 2.4 Critérios de aceite — matriz Go/No-Go

| # | Checkpoint | Comando | Passa se… |
|---|-----------|---------|-----------|
| A | Captura de sessão | `f365 auth` | WebView fecha sozinha; refresh token + cookies gravados no keyring; `whoami` responde UPN correto |
| B | Troca de tokens | `f365 token --resource teams` | Obtém token com `aud` de chat válido **sem reabrir WebView** (só refresh silencioso) |
| C | REST real | `f365 chats` | Retorna ≥1 conversa real do usuário com id+título, HTTP 200 do `chatsvcagg` |
| D | Tempo real | `f365 listen` + mandar msg de outro cliente | Evento da nova mensagem aparece no terminal em < 5 s |
| E | Envio | `f365 send <id> "ping do f365"` | Mensagem aparece no Teams oficial; `messageId` retornado |
| F | Durabilidade | repetir B–E após 1 h (token expirado) | Refresh silencioso renova sem nova WebView |

**Go** = A→F todos verdes e reprodutíveis em 2 execuções independentes.
**No-Go** = B, D ou F falham **mesmo com o fallback de transporte in-page** (§2.1) — ex.: Conditional
Access que exige device compliance que nem a página real satisfaz, ou Trouter que rejeita qualquer
cliente não-oficial. Só então reavaliar a estratégia de base antes de qualquer outra fase.

> Antes de declarar No-Go em B ou F, **tente a rota `InPage`**: ela resolve a maioria dos bloqueios
> de fingerprint/cookie, porque o request passa a sair do contexto genuíno do cliente web.

### 2.5 Disponibilidade por capacidade (não um booleano global de conexão)

O estado do cliente **não** é "conectado/desconectado". É **por capacidade**, por conta/tenant e por
geração de sessão:

| Capacidade        | Rota             | Estado            |
|-------------------|------------------|-------------------|
| listar conversas  | HTTP Rust        | disponível        |
| ler histórico     | HTTP Rust        | disponível        |
| baixar mídia      | contexto página  | disponível        |
| tempo real        | socket Rust      | reconectando      |
| enviar mensagem   | —                | exige novo login  |

(Nomes/rotas ilustrativos.) Regras:
- Cada capacidade começa numa **rota conhecida e já implementada** — não se descobre endpoint nem se
  escolhe rota por tentativa e erro em operação de escrita.
- Falha em uma capacidade (ex.: mídia) **não** derruba as outras nem dispara troca global de transporte.
- Expiração/logout invalidam o conhecimento daquela geração de sessão.
- A UI mostra **degradação parcial explícita**: "histórico ok; tempo real reconectando; envio aguardando
  auth" — útil ao usuário e torna problema de protocolo diagnosticável.

**Decidir agora:** disponibilidade é por capacidade. A seleção automática de rota cresce incremental
(Fase 2+), não na Fase 0.

---

## 3. Guia Prático de Mapeamento de Rede (DevTools)

Faça tudo logado na **sua** conta, em `teams.cloud.microsoft` (ou `teams.microsoft.com`), com
`F12 → Network`, "Preserve log" e "Disable cache" ligados. Filtre por `Fetch/XHR` e `WS`.

### 3.1 Login / token (aba Network durante o carregamento inicial)
Capture as requisições para `login.microsoftonline.com`:
- [ ] `/{tenant}/oauth2/v2.0/authorize` — anote `client_id`, `redirect_uri`, `scope`, `code_challenge`, `response_type`
- [ ] `/{tenant}/oauth2/v2.0/token` — anote `grant_type`, `scope`, `client_id`, e o **formato da resposta** (quais campos: `access_token`, `refresh_token`, `expires_in`, `ext_expires_in`)
- [ ] Observe os **cookies** setados: `ESTSAUTH`, `ESTSAUTHPERSISTENT`, `ESTSAUTHLIGHT`, `buid`, `esctx`

### 3.2 Troca interna Teams/Skype
- [ ] Requisições para hosts tipo `*.ng.msg.teams.microsoft.com` / `teams.microsoft.com/api/...` que trocam o token AAD por token de chat
- [ ] Identifique **onde o host regional aparece** (header de resposta `Location`, body com `endpoint`/`region`, ou um GET de "configuration")

### 3.3 Headers críticos a copiar (checklist)
Para **cada** request do `chatsvcagg`, anote e replique:
- [ ] `Authorization: Bearer <...>` — qual token (AAD? Skype?) e qual `aud`
- [ ] `X-MS-Client-Version` — versão do cliente web (ex.: `1415/...`)
- [ ] `X-MS-Client-Type` ou `X-MS-Client-Env`
- [ ] `X-MS-Session-Id` / `X-MS-Correlation-Id` / `client-request-id` — como são gerados (UUID por sessão? por request?)
- [ ] `X-MS-Scenario-Id`, `X-MS-User-Type`
- [ ] `User-Agent` — às vezes verificado; copie o do Edge
- [ ] Cookies enviados junto do Bearer (alguns endpoints exigem ambos)

### 3.4 REST `chatsvcagg` (Fetch/XHR)
- [ ] **Listar conversas**: GET para algo como `.../csa/api/v1/teams/users/me/...` ou `.../v2/users/me/conversations` — anote path exato, query params (`view`, `pageSize`, `isPaginationEnabled`) e o **shape do JSON** (onde ficam `id`, `topic`/`title`, `lastMessage`)
- [ ] **Histórico de um chat**: GET `.../conversations/{chatId}/messages?pageSize=...`
- [ ] **Enviar**: POST `.../conversations/{chatId}/messages` — capture o **body exato**: `content`, `messagetype` (`RichText/Html` vs `Text`), `contenttype`, `clientmessageid` (como é gerado), `imdisplayname`

### 3.5 WebSocket Trouter (aba WS)
- [ ] A requisição HTTP de **registro/handshake** antes do WS (`trouter.teams.microsoft.com/.../register` ou similar) — headers e body
- [ ] **Versão do protocolo:** o `purple-teams` documenta **socket.io v1 sobre WebSocket** em `go.trouter.teams.microsoft.com` — confirme o handshake (polling → upgrade) e o formato dos frames contra isso (ver §5)
- [ ] A **URL final do socket** (`wss://...trouter...`) e os query params (ids de sessão, token embutido?)
- [ ] O **primeiro frame** que o cliente envia após abrir (subscribe/ping) e o **formato dos frames** de mensagem recebida (JSON com `resource`/`body`?)
- [ ] O intervalo de **keep-alive/ping** (olhe timing dos frames) e como o servidor sinaliza nova mensagem de chat

> Dica: salve tudo via "Save all as HAR" e extraia os valores do HAR. Sanitize o HAR (ele contém seus
> tokens) antes de guardar fora da máquina.

---

## 4. Roadmap Pós-Validação (só depois do **Go**)

### Fase 1 — Gatekeeper (PoC) ✅ pré-requisito de tudo
Entregue em `f365-auth` + `f365-teams` + `f365-cli`. Critério = matriz §2.4 verde.

### Fase 2 — Persistência & sessão resiliente (`f365-storage`)
- `rusqlite` em **modo WAL** como **fonte da verdade (offline-first, render-before-network)**. Schema:
  `conversations`, `messages`, `accounts`, `sync_state`, `drafts`, `attachments_cache`.
- **Busca: SQLite FTS5 primeiro** (índice no mesmo arquivo, transacional com os dados, zero sincronização
  de índice externo). **Tantivy fica para a Fase 4 e só se a busca virar gargalo** — adiar essa
  complexidade é a escolha correta (lição do `rusty-teams`, que usa FTS5).
- Cache de imagens/anexos em arquivo; **drafts e mensagens não enviadas sobrevivem a restart**.
- Refresh silencioso de token em **task `tokio` dedicada** (renova antes de `exp`, backoff em erro).
- Reconexão automática do Trouter (resubscribe + replay de gap via REST por `lastSyncToken`); eventos
  tratados como **invalidação** (§2.3), não como dado.
- **Motor de sincronização reproduzível (assinatura técnica).** O núcleo de decisão é uma **máquina de
  estados** que recebe *acontecimentos* (resposta HTTP, invalidação do Trouter, rede caiu, prazo
  expirou, usuário enviou) e produz *decisões* (gravar no SQLite, buscar histórico, agendar reconexão,
  tentar envio, publicar à UI). **Relógio e rede são injetáveis** → testes determinísticos de suspensão,
  eventos duplicados/fora de ordem e reconciliação de envio, sem esperar minutos e sem rede real. Não é
  event-sourcing nem framework próprio; é uma FSM explícita com seams.
  - **Invariante de envio:** se a resposta se perde e depois chega uma invalidação, o motor
    **reconcilia** a mensagem já enviada (via `clientmessageid`) em vez de reenviar. Se o protocolo não
    permitir confirmar a correspondência, o estado é **"incerto"** — nunca inventar entrega única.
  - **Fronteira (decidir já):** o motor **não** chama `reqwest`, relógio de parede, WebView ou UI
    diretamente. A Fase 0 já é escrita contra esses seams (injeta `Clock`, usa o executor) para não
    virar código descartável — mas o motor completo é entregue aqui, na Fase 2.
- **Resiliência de protocolo — perfil declarativo × adaptador (separar honestamente):**
  - **Perfil declarativo** (config versionada): hosts permitidos, paths, versões, parâmetros conhecidos,
    client IDs. Mudança da Microsoft **que couber aqui** vira *update de config, não recompilar*.
  - **Adaptador de protocolo** (código): transformação de payload, auth, framing. **Nem toda** mudança
    é corrigível por JSON — não finja que é.
  - **Catálogo de evidências:** cada suposição de protocolo recebe um **ID de contrato estável** (ex.:
    `SEND-RECONCILE-001`) ligado a hipótese, evidência (`arquivo@commit` ou teste), comportamento
    dependente e fallback. Marcadores **`UNVERIFIED`** no código referenciam esse ID (lição do `ic3`).
    Regra estável "sempre/nunca" vai no `AGENTS.md`, não só na memória.
  - **Bundle de diagnóstico seguro por construção:** só IDs de contrato, estados/transições, status HTTP
    e categorias de erro, durações/contagens e IDs locais pseudonimizados. **Sem** texto de mensagem,
    token, cookie ou URL assinada. Captura de conteúdo é modo separado e explícito → dá pra colaborar
    open-source sem o usuário publicar HAR sensível. Testes com **`wiremock`** (mock de chatsvc + Trouter).
- Go/No-Go da fase: sobreviver a 8 h ligado, reconectar após sleep/rede caindo, sem reabrir WebView.

### Fase 3 — UI nativa (`f365-ui`: Tauri v2 + React)

**Shell (Rust):** `tauri` v2 gerencia a(s) janela(s) WebView2 e empacota o `.exe` (instalador MSI/NSIS,
binário ~10–15 MB, sem Chromium embutido). A janela de login MFA passa a ser uma webview do próprio
Tauri (consolida o `wry` da Fase 0).

**Frontend (`src-ui/`):**
- **Build:** Vite + TypeScript. **Framework:** React 19 (ou 18).
- **Design system:** `@fluentui/react-components` (Fluent UI v9 — paridade visual com o Teams) ou
  Tailwind + shadcn/ui (visual mais limpo, estilo Linear/Slack). Escolher na Fase 3.
- **Virtualização (essencial):** `@tanstack/react-virtual` — canais com 10k+ mensagens renderizam só
  o visível, scroll a 60 FPS com memória estática.
- **Estado:** Zustand (leve, sem boilerplate).
- **Markdown/mídia:** `react-markdown` + `remark-gfm`; **DOMPurify obrigatório** para qualquer HTML
  (`RichText/Html`) vindo do Teams — ver §0.1 (segurança).

**Fronteira UI ⇄ core (mesma isolação de antes, via IPC do Tauri):**
- `#[tauri::command]` assíncrono para UI → core (enviar msg, abrir chat, paginar histórico).
- `app_handle.emit("new-message", msg)` para core → UI (push em tempo real do Trouter); o store Zustand
  atualiza na hora.
- A UI **nunca** faz I/O de rede direto: só invoca commands e lê o cache (`f365-storage`).

**Metas:** dark mode nativo, **cold start ~200–400 ms** (meta revisada vs. WebView2 — medir),
RAM-alvo ~45–90 MB.

**Calls/reuniões:** **não** implementar WebRTC — botão "Entrar" só abre o link no navegador padrão.

### Fase 4 — Busca, anexos e Outlook (`f365-outlook`)
- **Busca avançada (opcional):** migrar de FTS5 para `tantivy` **apenas se** a busca FTS5 virar gargalo (ranking/fuzzy/escala). Não é o default.
- Anexos: download via endpoints capturados, cache local.
- **Outlook**: novo provider `f365-outlook` consumindo o **mesmo `AuthenticatedExecutor`** de `f365-auth`
  (resource `OutlookSubstrate`), falando com OWA/Substrate API. Zero mudança em `f365-auth`/`f365-teams`
  valida que a fronteira da Fase 0 foi bem desenhada.

---

## Resumo de sequência (um olhar)

```
Fase 0 (GATE)  ── auth + teams + cli ── matriz §2.4 ──▶ Go? ──┐
                                                    │No        │Sim
                                               repensar    Fase 1 fechada
                                                              │
                                   Fase 2 storage ◀───────────┘
                                   Fase 3 Tauri v2 + React
                                   Fase 4 FTS5→tantivy + outlook
```

---

## 5. Prior Art & Síntese Técnica

Existe um ecossistema de clientes/bibliotecas open-source que já fazem a mesma engenharia reversa
(skypetoken → `chatsvcagg` → Trouter). Eles **validam a viabilidade** do gate (Fase 0) e são fonte de
mapeamento de protocolo. Esta é a destilação do melhor de cada um para o Fast365.

### Mapa de projetos → o que cada um resolve

| Projeto | Stack | O que roubar | Ajuda no checkpoint |
|---------|-------|--------------|---------------------|
| [d0dg3r/rusty-teams](https://github.com/d0dg3r/rusty-teams) | Rust, GPUI | **Transport trait** (Rust puro → in-page via CDP); Trouter como invalidação; SQLite+**FTS5**; render-before-network; drafts persistentes | A, B, C, D, F + Fase 2/3 |
| [RandyNorthrup/ost](https://github.com/RandyNorthrup/ost) | Rust CLI | Fluxo completo teams/chats/mensagens/**Trouter**; device-code; config 0600 | C, D, E |
| [Jezza/ic3](https://github.com/Jezza/ic3) | Rust lib + IRC | **Client ID first-party** `1fec8e78-…`; refresh token em disco 0600, resto em memória; marcadores **`UNVERIFIED`** + testes `wiremock` | A, B + resiliência |
| [fossteams/teams-api](https://github.com/fossteams/teams-api) + [teams-token](https://github.com/fossteams/teams-token) | Go | **Tríade de tokens** (`teams`/`skype`/`chatsvcagg`.jwt); shape do `GetConversations` | B, C |
| [EionRobb/purple-teams](https://github.com/EionRobb/purple-teams) | C (Pidgin) | **Trouter = socket.io v1/WS** em `go.trouter.teams.microsoft.com`; registro/endpoints | D |
| [teams-puppet](https://github.com/jheesbrough/ms-teams-puppet-account-manager) | Python/Selenium | Referência do refresh silencioso de token | A, F |

> **Recomendação de execução:** leia `ost` + `ic3` (Rust) + `fossteams` (token chain) + `purple-teams`
> (Trouter) **antes** de capturar tráfego no DevTools — você já saberá o que procurar, e boa parte do
> §2.2/§3 estará confirmada. Isso pode te dar o **Go do gate quase de graça**. Verifique licença e
> manutenção de cada repo antes de reusar trechos de código.

### A síntese "best-of" (a stack tecnicamente melhor)

1. **Transport trait com degradação graciosa** (§1, §2.1) — a lição nº 1. Rust puro por padrão,
   in-page fetch como fallback. Transforma o maior risco (Conditional Access) em detalhe de impl.
2. **Pairing por WebView** com o **client ID first-party** (sem registro no Entra); refresh token no
   **keyring**, resto em memória. Device-code como alternativa secundária.
3. **Trouter como invalidação**, aberto do Rust, reconnect + gap-replay via REST.
4. **SQLite WAL + FTS5** offline-first como fonte da verdade; tantivy só sob demanda.
5. **Engine único** (`f365-*` core) consumido de forma fina pela CLI (`f365`, Fase 0) **e** pela UI
   Tauri (Fase 3) — a Fase 0 vira o motor, não código descartável.
6. **Resiliência de protocolo** (§Fase 2): protocol profile externalizado + `wiremock` + `UNVERIFIED`
   + redação de tokens. É o que nenhum dos outros faz bem e o que torna o Fast365 o mais robusto.
7. **Sem WebRTC nativo** — `ost`/`rusty-teams` fazem libwebrtc; é poço de manutenção. Abrir o link no
   navegador é a escolha de engenharia correta.

### Onde divergir conscientemente dos outros

- **UI:** o `rusty-teams` usa **GPUI** (melhor footprint/perf bruta, mas imaturo e sofre com
  HTML/rich-text). Para paridade **Fluent UI** e render correto do `RichText/Html` do Teams,
  **Tauri + React** continua superior para os objetivos do Fast365 (ver §0.1).
- **Graph híbrido:** o `rusty-teams` usa Microsoft Graph (documentado) para sync/envio e `chatsvc` só
  para realtime. É **mais estável** nas partes cobertas, **mas** os escopos de chat do Graph costumam
  exigir **consentimento de admin** — o que viola a premissa "sem aprovação de admin". **Decisão:
  manter puro-`chatsvc`**, porém desenhar o provider como trait para **plugar um backend Graph opcional**
  no futuro, se houver consentimento e se quisermos mais estabilidade.

### Assinatura técnica & demo

> **Fast365 = cliente local-first com execução adaptativa e sincronização reproduzível.**

Demo que comunica engenharia (melhor que "usa menos RAM"): (1) abrir histórico local **sem rede**;
(2) escrever mensagem → persistir como **pendente**; (3) reconectar e **reconciliar** o envio;
(4) simular perda da resposta **sem duplicar**; (5) reproduzir o mesmo cenário em **teste determinístico**;
(6) exibir **capacidades por rota** (§2.5). Prioridade: fixar o **executor autenticado** (§1) e a
**fronteira testável do motor** (Fase 2) **antes** da primeira implementação real — são as decisões
caras de mudar depois.

---

## 6. Visão & Features habilitadas pela arquitetura

> **Leia isto como destino, não como escopo atual.** As features abaixo são **ideias de produto do
> dono do projeto** — o motivo de termos um cliente *próprio* em vez de usar o Teams oficial. **Elas
> NÃO são funcionalidades nativas do Teams** e **nada aqui começa antes do Go do gate (Fase 0).** O
> valor desta seção é orientar quem desenvolve: as decisões de arquitetura (isolamento de sessão,
> Trouter como invalidação, cache local, seam de middleware) existem para que estas features pluguem
> **sem refatorar o core** depois. Projetar sabendo onde vamos chegar.

### O princípio que destrava tudo
Como **toda mensagem passa pelo nosso código** antes de ir à tela (recebimento) ou ao servidor (envio),
o Fast365 pode interpor lógica própria no pipeline — coisa que um cliente oficial nunca permite. Para
isso, **a partir da Fase 3/4** o engine deve expor um **seam de middleware** com dois ganchos:
`pre_send` (transforma a mensagem antes do POST no chatsvc) e `post_receive` (transforma o conteúdo
buscado após um evento do Trouter, antes de exibir/cachear). Não construir agora — **Fase 0 não muda** —
mas desenhar o core ciente de que esse seam vai existir.

### Feature 1 — Ata de reunião automática (transcrição → ata → e-mail)
*Ideia do dono: hoje feito via automação externa; trazer para dentro do Fast365.*
- **Fluxo:** obter transcrição (provider Teams) → gerar ata com LLM (lógica local) → enviar por e-mail
  (**provider Outlook da Fase 4, reusando a MESMA sessão autenticada**).
- **Por que valida a arquitetura:** é o *showcase* do isolamento `f365-auth` ↔ providers — a feature usa
  Teams e Outlook sobre uma única sessão, exatamente o que a fronteira foi desenhada para permitir.
- **Realismo / gate próprio:** a origem da transcrição (endpoint de artefato tipo `amssvc`/substrate, ou
  o arquivo da gravação no SharePoint/OneDrive) é **uma engenharia reversa separada do chat**, dependente
  de a transcrição ter sido ativada na reunião. Tratar como **mini-gate de viabilidade próprio** na
  Fase 4+, não como dado garantido.

### Feature 2 — Tradução de conversa em tempo real
*Ideia do dono: escrevo em português, o colega recebe no idioma dele, e vice-versa.*
- **Fluxo:** `pre_send` traduz PT → idioma do destinatário antes do POST; `post_receive` traduz a
  mensagem recebida → PT antes de exibir. Motor de tradução: API (DeepL/Claude) ou modelo local.
- **Por que encaixa limpo:** é puro middleware sobre o pipeline; aproveita o "Trouter como invalidação"
  (o conteúdo fresco já ia ser buscado — basta interpor o tradutor). Cache de original + tradução no
  SQLite para não re-traduzir.
- **Decisões de UX a resolver depois:** (a) enviar o texto **já traduzido** (funciona mesmo que o colega
  use o Teams oficial) vs. enviar o original e cada lado traduzir localmente; (b) exibir original +
  tradução ou só a tradução.

### Decisão de dados para features de IA — fatos × interpretações

**Conteúdo derivado nunca substitui o original.** A mensagem recebida é um **fato** (versionado);
tradução, resumo, ata e ação proposta são **artefatos derivados**, versionados e referenciando o fato +
a versão do modelo:

```
Mensagem original + versão
        ├── tradução + idioma + versão do modelo
        ├── resumo + referências às mensagens
        └── ação proposta + aprovação do usuário
```

Permite invalidar traduções quando a mensagem muda, rastrear a ata até a decisão-fonte, trocar o modelo
sem reescrever histórico e **aprovar exatamente o texto que será enviado**. **Decidir agora** só no
modelo de dados/API (não construir plugin): derivado é separado e versionado desde o schema.

### Outras ideias (backlog aberto)
Espaço para novas features do dono. O padrão se mantém: se a ideia é **transformar/reagir a mensagens**,
provavelmente é middleware (`pre_send`/`post_receive`); se é **agir sobre outro serviço M365**
(e-mail, calendário), provavelmente é **um novo provider reusando a sessão** — nunca uma mudança no
`f365-auth`.
