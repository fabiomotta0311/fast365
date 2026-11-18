# Gate da Fase 0 — protocolo e registro da decisão

> **Nota (2026-11-18):** as Fases 2 e 3 e parte da 4 foram implementadas em branches de integração
> (`next/fase-2`, `next/fase-3`) antes do Go, por decisão do dono. O `main` não as recebe até o Go.

Regra reitora: **nada de Fase 1+ (UI, persistência, features) antes do Go**. O critério está em
[`ARQUITETURA.md`](ARQUITETURA.md) §2.4 e a decisão é **Go** só com A–F verdes e **reprodutíveis em
duas execuções independentes**.

Este documento é o registro durável: o protocolo da rodada, o que já foi medido, o que cada evidência
prova e o bloco de decisão. A ferramenta `f365 gate` (Issue
[#13](https://github.com/fabiomotta0311/fast365/issues/13)) gera o relatório sanitizado de cada rodada.

## Como cada checkpoint se prova

| # | Critério (§2.4) | Como se prova | Quem executa | `f365 gate` cobre? |
|---|---|---|---|---|
| A | WebView fecha sozinha; RT + cookies no keyring; `whoami` com UPN correto | `f365 auth` / `f365 auth --silent` | **Dono** (só ele abre a janela) | Só a metade passiva (lê o keyring); a janela fechando sozinha fica fora |
| B | `aud` de chat válido **sem** reabrir WebView | `f365 token --resource teams` | máquina do dono com sessão | Sim |
| C | ≥1 conversa real, id + título, HTTP 200 do `chatsvcagg` | `f365 chats` | idem | Sim (conta conversas; ids omitidos) |
| D | Evento de nova mensagem em < 5 s | `f365 listen` + mensagem de outro cliente | idem (precisa de outro cliente) | Sim, com janela (`--wait-d`) |
| E | Mensagem aparece no Teams oficial; `messageId` retornado | `f365 send <chatId> "ping do f365"` | idem + conferência no Teams | Envia e registra o `messageId`; a conferência visual é humana |
| F | Repetir B–E após 1 h com token expirado, sem nova WebView | segunda execução de B–E | idem | Sim: rodar duas vezes com ≥1 h e comparar |

## Rodada 1 — 2026-11-16, A–F verdes

**Fonte:** comentários do dono na
[#13](https://github.com/fabiomotta0311/fast365/issues/13) (evidência relatada, **não** reexecutada
por agente).

| # | Resultado relatado |
|---|---|
| A | `auth --silent` re-logou **sem janela** com RT novo (após o fix #49) |
| B | `aud https://api.spaces.skype.com`, RT rotacionado |
| C | 3 conversas via `chatsvcagg` |
| D | `mudança em 48:notes`; queda por modo avião detectada com **reconexão automática** (+646 s, 10 tentativas sem desistir) |
| E | `messageId` devolvido, mensagem no Teams; `send` pós-queda notificado em +764,7 s |
| F | ~24 h depois, `token` sozinho renovou o login **sem interação** (#48 + #49); `listen` + `send` pós-renovação OK |

Falta, para a decisão: **rodada 2 independente** (adiada — ver a seção seguinte) e o preenchimento do
bloco de decisão abaixo.

## Rodada 2 — adiada (2026-11-18)

A rodada 2 **não foi executada**. O que se sabe (relato do dono, não reexecutado por agente):

- **Conta.** A conta de trabalho da rodada 1 deixou de estar disponível. Até o tenant de desenvolvedor
  do Microsoft 365 ser aprovado, a única conta é uma **conta pessoal Microsoft** (tenant de consumidor).
- **O que a conta pessoal mostrou:**
  - `f365 auth` abriu e fechou a janela e gravou a sessão; `f365 whoami` lista a conta, mas
    `Região` fica "indisponível — authsvc respondeu HTTP 401".
  - `f365 token --resource teams` renova o RT e informa a expiração, porém `aud`, `tid`, `scp`,
    `appid` e `upn` saem `?` (o token não decodifica como o de uma conta de trabalho).
  - `f365 chats` falha com `AADSTS70011` (escopo inválido).
- **Leitura — hipótese, `UNVERIFIED`:** o f365 foi construído e verificado só contra Teams de
  trabalho/escola; o Teams pessoal usa outros endpoints e escopos. Logo C–F não se aplicam a essa conta,
  e isto **não** é falha do gate nem prova de No-Go: a rodada 2 exige conta de trabalho com Teams.
- **Gotcha de ambiente (uma máquina, não confirmado):** o `f365 auth` só concluiu num terminal
  **elevado**; num terminal normal dava `Acesso negado (os error 5)` ao criar o perfil da WebView e,
  depois, `0x8000FFFF` na WebView2. Suspeitos: antivírus (Norton) ou política de pasta controlada. Isso
  não é causa confirmada; a regra de `AGENTS.md` (testar fora de sandbox) continua valendo.

**Para retomar** (quando houver conta de trabalho com Teams): `f365 logout --cache` para limpar a conta
antiga, `f365 auth` pelo dono num terminal normal (ou elevado, se o ambiente exigir) e seguir o
protocolo abaixo.

## Protocolo da rodada 2

Tudo em **PowerShell normal aberto pelo dono**, fora de qualquer sandbox de agente (`AGENTS.md`).
Antes: **encerre qualquer `f365` em execução** (`listen` antigo, por exemplo) — no Windows ele segura
`target\debug\f365.exe` e o `cargo run` falha com "Acesso negado (os error 5)" ao regravar o binário.

```powershell
# 1) Primeira execução: A–E no mesmo minuto.
cargo run -p f365-cli --features webview -- gate `
  --chat "<chatId de f365 chats>" `
  --commit $(git rev-parse --short HEAD)

# 2) ≥1 h depois (token expirado): repetir B–E sem `f365 auth` manual.
cargo run -p f365-cli --features webview -- gate `
  --chat "<chatId>" `
  --commit $(git rev-parse --short HEAD)
```

Cada execução grava `gate-reports/gate-<utc>.md` e `.json` (pasta no `.gitignore`). `--skip-d` pula o
tempo real e `--wait-d <s>` muda a janela de espera do D.

Conferência e registro:

1. **D** — durante a janela do comando, mande uma mensagem de outro dispositivo/conta. Se a conexão
   cair e voltar dentro da janela, o relatório **conta as reconexões em vez de reprovar**: queda com
   recuperação automática é o comportamento esperado (#50/#51), não falha do checkpoint.
2. **E** — confira a mensagem `ping f365 gate …` no Teams oficial e anote o `messageId` do relatório.
3. Cole os `.md` das **duas** execuções (a primeira e a de ≥1 h depois) como comentário na #13. O
   relatório **não** declara Go: ele mede, marca F como não provado e deixa a decisão para o dono.

Em F, o que importa no segundo relatório: B–E voltam a passar **e** o contador de
`reparos silenciosos` mostra que o re-pareamento foi automático, sem `f365 auth` manual.

## Bloco de decisão

Preencher só quando as duas rodadas estiverem anexadas na #13.

- Rodada 1: 2026-11-16 (A–F verdes) — comentários da #13
- Rodada 2: **adiada** (2026-11-18) — conta de trabalho indisponível; ver "Rodada 2 — adiada". _(data + link do comentário quando executada)_
- A ☐ · B ☐ · C ☐ · D ☐ · E ☐ · F ☐
- **Decisão:** ☐ Go ☐ No-Go
- Motivo estrutural (obrigatório no No-Go; ver §2.1 — antes de No-Go em B/F, tentar a rota `InPage`):
- Data e assinatura (dono):

Depois do **Go**: atualizar o `Status` do `README.md`, fechar a #13 e destravar as issues de Fase 2
(#14–#18), removendo o label `blocked`.

## O que `f365 gate` não prova

- **A** — que a janela da WebView fechou sozinha: a ferramenta nunca abre WebView (login é só do dono).
- **D** — que o evento veio de outro cliente: quem dispara a mensagem é o dono, numa janela de tempo.
- **E** — que a mensagem apareceu no Teams oficial: a conferência é visual, no Teams.
- **F** — nada, numa execução só: o que prova F é o intervalo entre as duas rodadas.
- A rota de fallback **InPage** (§2.1) não é exercitada: a rota Rust é a padrão.

## Segurança do relatório

O relatório nunca contém token, cookie, URL assinada ou conteúdo de mensagem (`AGENTS.md`). Ele
contém: ids de conversa (não são credenciais; são o que correlaciona as rodadas), UPN **omitido**,
contadores, durações e status. O `Debug` de request não entra no arquivo.
