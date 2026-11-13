# Fast365 — desenvolvimento na VPS

Ambiente headless isolado para compilar e testar o workspace Rust. **Não use este ambiente para login Microsoft:** autenticação MFA/WebView2 só é executável no Windows com GUI. Nenhuma credencial ou sessão real deve ser copiada para a VPS.

## Estado e componentes

- Host: Ubuntu 24.04 LTS; Docker/Compose já instalados. Nenhum pacote do host foi instalado. A VPS mantém outros serviços; este Compose não publica portas e usa uma rede Docker dedicada, com saída à internet necessária para baixar dependências do crates.io.
- Container `fast365-dev`, imagem Rust oficial Debian Bookworm: toolchain stable via rustup, componentes `rustfmt`/`clippy`, target `x86_64-unknown-linux-gnu`; instala git, build-essential, clang, pkg-config, curl, ca-certificates e headers DBus/libsecret para dependências Linux do `keyring`.
- Volume nomeado `fast365-dev-cargo-home` para cache do Cargo e `fast365-dev-target` para `target/`.
- `wry`/`tao` são opcionais pela feature `webview` e desligados por padrão. Headless não requer GTK/WebKit; ativar WebView em Linux precisará das dependências gráficas correspondentes. O gate real WebView2 permanece Windows-only.

## Conexão e uso

1. No computador local, abra SSH com o alias já configurado `vps-hermes` e o diretório `/docker/fast365-dev/workspace` no VS Code Remote-SSH. Alternativamente, abra terminal remoto no host e rode:

```sh
cd /docker/fast365-dev/workspace
```

2. O container `fast365-dev` já deve estar ativo. No terminal SSH, execute comandos Rust pelo container:

```sh
docker exec -it -w /workspace fast365-dev bash
cargo fmt --check
cargo clippy --workspace -- -D warnings
cargo check --workspace
cargo test --workspace
```

O diretório de trabalho no host está montado no container. Mudanças de código persistem em `/docker/fast365-dev/workspace`.

## Ciclo de vida e limpeza

No host VPS:

```sh
cd /docker/fast365-dev
docker compose -f compose.dev.yaml ps
docker compose -f compose.dev.yaml down
```

Remoção integral do ambiente e dos caches/artefatos (irreversível; apaga os dois volumes nomeados):

```sh
cd /docker/fast365-dev
docker compose -f compose.dev.yaml down -v --rmi local
```

A árvore do projeto fica em `/docker/fast365-dev/workspace`; remova-a separadamente se também quiser apagar o código. Não execute `docker system prune` para limpar este projeto, pois a VPS hospeda outros serviços.

## Segurança

A VPS não deve receber tokens, cookies, refresh tokens, `*.jwt`, `*.har` ou qualquer sessão real. Não execute comandos de login Microsoft. `Auth`/WebView2 e MFA só devem ser validados no Windows com GUI.
