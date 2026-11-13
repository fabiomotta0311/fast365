# Prompt para o Hermes — Ambiente de Dev na VPS (Fast365)

> Cole o bloco abaixo para o Hermes. Antes, substitua `<REPO_URL>` pela URL do repositório
> (ou deixe como está se ainda não houver repo — o prompt instrui a criar a estrutura).

---

## Tarefa: montar um ambiente de desenvolvimento reproduzível e isolado na VPS para o projeto **Fast365**

Você tem acesso à minha VPS. Quero que configure nela um ambiente de dev para **compilar e testar o core headless** do Fast365 (um workspace Rust). Siga este brief à risca.

### Contexto que você PRECISA entender antes de agir
- **Fast365** é um cliente desktop nativo para Microsoft 365 (Teams primeiro), escrito em **Rust**, organizado como workspace Cargo com os crates `f365-auth`, `f365-teams` e `f365-cli` (binário `f365`).
- A autenticação real do produto usa uma **WebView2 + login MFA interativo**, que é **exclusivo de Windows com GUI**. **Isso NÃO roda nesta VPS Linux e você NÃO deve tentar rodar.**
- O papel da VPS é **somente**: compilar (`cargo check`/`build`), rodar testes (unitários/integração com mocks), `fmt`, `clippy` e servir de ambiente de CI/dev remoto. O gate de login/WebView fica no Windows, fora do seu escopo.

### Regras de segurança (inegociáveis)
- **Nunca** autentique em nenhuma conta Microsoft a partir da VPS.
- **Nunca** armazene tokens, cookies, refresh tokens, arquivos `*.jwt`, `*.har` ou qualquer sessão real na VPS. Esse ambiente não deve ver credenciais.
- Não exfiltre nem registre segredos. Se o repo contiver algum, pare e me avise.

### Restrições operacionais
- **Idempotente**: rodar de novo não deve duplicar nem quebrar nada.
- **Não toque** em outros serviços/apps já existentes na VPS. Não altere firewall/portas além do estritamente necessário para dev remoto, e me avise antes se for preciso.
- **Isolamento e disco**: prefira ambiente **conteinerizado (Docker)** para fácil remoção; use **volume nomeado** para o cache do Cargo (`CARGO_HOME`) e para o diretório `target/`, de modo que dê pra purgar sem resíduo. Reporte o uso de disco **antes e depois**.
- Documente tudo que instalar.

### Passos
1. **Inventário**: detecte distro/versão do SO, CPU, RAM e **espaço livre em disco**. Reporte.
2. **Pré-requisitos do sistema**: `git`, toolchain de build C (`build-essential` ou `clang`), `pkg-config`, `curl`, `ca-certificates`. **Não** instale `libssl-dev`/openssl — o projeto usa TLS via `rustls`, não OpenSSL.
3. **Rust via rustup**: toolchain **estável**, respeitando um `rust-toolchain.toml` do repo se existir. Adicione os componentes `rustfmt` e `clippy`. Target `x86_64-unknown-linux-gnu`.
4. **Obter o código**:
   - Se houver repositório: `git clone <REPO_URL>`.
   - Se **não** houver repo ainda: crie a estrutura do workspace do zero (`cargo new`/scaffold) com os 3 crates citados e faça `git init`. A fonte da verdade da estrutura e das dependências é o arquivo `docs/ARQUITETURA.md` (seção "Workspace & Estrutura de Crates") — siga os `Cargo.toml` de lá.
5. **Dependência de WebView — JÁ RESOLVIDA no repo (não reabra a decisão).** Os crates `wry`/`tao`
   (usados em `f365-auth`) são *Windows-primary* e já estão **opcionais, atrás da feature `webview`,
   desligada por padrão** (`default = []`). O `f365-cli` reexporta via `webview = ["f365-auth/webview"]`.
   Portanto, na VPS:
   - Builde e teste **SEM** a feature (padrão). O core compila **sem GTK/WebKit**.
   - **NÃO instale** `libwebkit2gtk-*`, `libgtk-3-dev` nem nenhuma lib de GUI — não são necessárias e
     só pesariam o servidor.
   - **Nunca** ligue a feature `webview` na VPS (ela só faz sentido no Windows, e nem rodaria aqui).
6. **Validação** (tudo deve passar, headless): `cargo fmt --check`, `cargo clippy --workspace -- -D warnings` (ou reporte os warnings), `cargo check --workspace` e `cargo test --workspace`.
7. **Dev remoto**: garanta que eu consiga desenvolver remotamente — acesso SSH estável e/ou compatível com **VS Code Remote-SSH** ou um **devcontainer**. Documente exatamente como me conectar.
8. **Entregável de documentação**: deixe um `README-DEV-VPS.md` (no repo ou na home do projeto) com: o que foi instalado, como buildar/testar, como conectar remotamente, onde fica o cache/volume, como derrubar/limpar o ambiente, e o aviso de que **o gate WebView2 é Windows-only e não roda aqui**.

### Critérios de aceite
- `cargo check --workspace` e `cargo test --workspace` **verdes** na VPS, em modo headless.
- Nenhuma credencial Microsoft e nenhum token/sessão real presentes na VPS.
- Uso de disco reportado (antes/depois) e ambiente **removível** (container + volumes) sem deixar resíduo.
- Um `README-DEV-VPS.md` claro, e um relatório final do que você fez, instalou e decidiu (incluindo a decisão do passo 5).

Se em qualquer ponto algo exigir uma decisão que eu não cobri aqui (versão de distro incompatível, falta de espaço, porta ocupada), **pare e me pergunte** em vez de assumir.
