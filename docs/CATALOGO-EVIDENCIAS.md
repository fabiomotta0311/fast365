# Catálogo de evidências de contratos

Escopo: IDs associados a marcadores **UNVERIFIED** no código Rust em `crates/`, inclusive partes não verificadas de contratos parcialmente ou historicamente verificados, e hipóteses documentais identificadas na investigação #6. Evidência histórica e testes sintéticos não constituem nova validação do serviço. Não foram acessados conta, keyring, perfil ou WebView reais. Não registrar segredos, URLs assinadas ou conteúdo de sessões.

| ID | Hipótese e comportamento dependente | Evidência e limite | Fallback ou comportamento atual |
|---|---|---|---|
| `AUTH-MSAL-CACHE-002` | UNVERIFIED: cache MSAL v4+ criptografado tem formato `{id, nonce, data}`. | [session.rs](../crates/f365-auth/src/session.rs): guarda e testes sintéticos; o formato não ocorreu no checkpoint histórico A. | Detectado sem conta selecionável, fica pendente com bloqueio de cache criptografado; captura persistente informa erro, não sucesso vazio. |
| `AUTH-MSAL-ACTIVE-001` | UNVERIFIED: chaves `active-account-filters` ou `active-account` identificam a conta ativa. | [session.rs](../crates/f365-auth/src/session.rs): parsing e testes sintéticos; sem validação real desta hipótese. | Sem indicação ativa, aceitar apenas identidade inequívoca; ambiguidade bloqueia captura, sem escolher outra conta. |
| `EXEC-CHALLENGE-001` | UNVERIFIED: Conditional Access chega como 401 com `WWW-Authenticate` contendo `claims`, `insufficient_claims` ou `interaction_required`. | [executor.rs](../crates/f365-auth/src/executor.rs): classificador e teste com resposta falsa; não comprova formato real do desafio. | Para desafio reconhecido, tentar InPage se disponível; senão erro explícito de autenticação. Outros formatos não são garantidamente reconhecidos. |
| `TOKEN-SPA-ORIGIN-001` | UNVERIFIED: `Origin` é obrigatório no resgate do RT SPA. | [profile.rs](../crates/f365-auth/src/profile.rs) e [matriz](CONTRATOS-FASE0.md): resgate com Origin relatado no [#36](https://github.com/fabiomotta0311/fast365/pull/36); sem comparação sem Origin. | Enviar Origin configurado; `AADSTS9002327` é diagnosticado, sem afirmar que a ausência sempre causa esse erro. |
| `TEAMS-CSA-SHAPE-001` | UNVERIFIED: `teams[].channels[]` integra a listagem; parte de `chats[]` tem evidência histórica parcial. | [chats.rs](../crates/f365-teams/src/chats.rs), [matriz](CONTRATOS-FASE0.md) e [#43](https://github.com/fabiomotta0311/fast365/pull/43): canais não apareceram nos primeiros itens. | Leitura tolerante de campos ausentes; erro de contrato se nem `chats` nem `teams` forem listas. |
| `HISTORY-PATH-001` | UNVERIFIED: GET regional de mensagens com parâmetros `view`, `pageSize` e `startTime` retorna histórico. | [history.rs](../crates/f365-teams/src/history.rs), [matriz](CONTRATOS-FASE0.md): fixtures sintéticas e [implementação independente](https://github.com/ask-marcel/ask-marcel-office-cli/blob/b07ddcb8e48518e9076128b3fe1e79a816b65600/read/src/use-cases/commands/list-teams-chat-history.ts#L16-L30), não validação do serviço. | Erro HTTP ou de transporte propagado; não há endpoint alternativo validado. |
| `HISTORY-SHAPE-001` | UNVERIFIED: resposta contém `messages[]` com os campos interpretados pelo parser. | [history.rs](../crates/f365-teams/src/history.rs): testes sintéticos apenas; [matriz](CONTRATOS-FASE0.md). | JSON inválido, ausência de lista ou mensagem sem ID causa erro; campos opcionais têm defaults conforme parser. |
| `HISTORY-PAGE-001` | UNVERIFIED: `_metadata.backwardLink`, ou na sua ausência `syncState` em formato URL/caminho, é continuação. | [history.rs](../crates/f365-teams/src/history.rs): fixtures sintéticas; hipótese de URL não comprova o serviço ([matriz](CONTRATOS-FASE0.md)). | Rejeitar cursor inválido ou cíclico e limites excedidos sem sucesso parcial; `backwardLink` tem prioridade. |
| `SEND-RECONCILE-001` | UNVERIFIED: servidor devolve `clientmessageid` na mensagem criada, permitindo identificar envio cuja resposta se perdeu. | [f365-sync](../crates/f365-sync/src/lib.rs), [matriz](CONTRATOS-FASE0.md) e [#44](https://github.com/fabiomotta0311/fast365/pull/44): relato não demonstra reconciliação. | Resultado incerto não autoriza reenvio às cegas; executor não repete erro de rede ou prazo. Não há reconciliação real comprovada. |
| `SEND-RESPONSE-001` | UNVERIFIED: origem do ID observado pode ser `Location` ou `OriginalArrivalTime`; observação histórica não distinguiu as duas. | [send.rs](../crates/f365-teams/src/send.rs) e [matriz](CONTRATOS-FASE0.md): ID observado historicamente; testes de extração são sintéticos. | Parser prioriza ID numérico de `Location`, depois `OriginalArrivalTime`; se nenhum existir, recibo tem ID de servidor ausente. |
| `TROUTER-POLLING-001` | UNVERIFIED: cliente oficial faz polling antes do upgrade. | [matriz](CONTRATOS-FASE0.md#6--trouter); [#45](https://github.com/fabiomotta0311/fast365/pull/45) relata WS direto, não demonstra polling. | Cliente abre WebSocket diretamente após handshake e falha se transporte não for anunciado. |
| `TROUTER-SUBSCRIBE-001` | UNVERIFIED: cliente oficial envia primeiro frame de subscribe. | [mod.rs](../crates/f365-teams/src/realtime/mod.rs) espera confirmação, depois registra via HTTP; não comprova comportamento do cliente oficial. | Nenhum subscribe explícito é enviado nessa etapa. |
| `REALTIME-GAP-001` | UNVERIFIED: invalidações ocorridas durante queda se perdem mesmo após novo registro. | [machine.rs](../crates/f365-sync/src/machine.rs): hipótese e teste sintético; sem observação do replay real do serviço. | Após reconexão, o sync invalida e busca novamente dados conforme a máquina, sem garantir recuperação completa. |
| `TROUTER-RECONNECT-001` | UNVERIFIED: recuperação real após desconexão, incluindo lacunas e entrega. | [mod.rs](../crates/f365-teams/src/realtime/mod.rs) e [wiremock_e2e.rs](../crates/f365-teams/src/realtime/wiremock_e2e.rs) demonstram cenários sintéticos, não recuperação no serviço. | Após Ready, refaz conexão com backoff limitado; eventos são invalidação e não garantem entrega completa. |
| `TROUTER-HANDSHAKE-001` | UNVERIFIED: valores específicos de `tc` são os esperados; handshake e necessidade de `con_num` têm evidência histórica limitada. | [trouter.rs](../crates/f365-teams/src/realtime/trouter.rs), [matriz](CONTRATOS-FASE0.md) e [#45](https://github.com/fabiomotta0311/fast365/pull/45): não demonstram necessidade dos valores de `tc`. | Falha de handshake é propagada; não há `tc` alternativo validado. Parâmetros assinados não devem ir a logs. |

## Verificação anti-drift

Execute `python docs/test_catalogo_evidencias.py` na raiz; este comando é separado do `cargo test --workspace`; o job `catalogo` do CI o executa em pull requests. O teste percorre `.rs` em `crates/` e exige que cada marcador tenha ID na mesma linha ou no início do mesmo item de comentário de documentação Rust (`//!` ou `///`), contíguo e a no máximo três linhas acima; itens diferentes, linhas vazias e código delimitam o contexto. Todos os IDs identificados devem ter linha única no catálogo contendo UNVERIFIED. Não há associações fixas por arquivo. Um marcador sem ID nesse contexto falha e precisa ser corrigido no código. O teste não interpreta semanticamente os comentários, não verifica se evidências ou fallbacks continuam corretos, nem detecta hipóteses sem marcador ou marcadores fora do Rust em `crates/`; revise esses casos manualmente.

## Limites dos diagnósticos e perfil

O recorder operacional descrito abaixo possui bundle próprio com campos tipados. A cobertura
atual não demonstra segurança de integrações ou logs fora desse tipo.

O integrador pode carregar um arquivo explicitamente com `f365_auth::Profile::load(path)`
e aplicar o resultado a `TokenBroker::with_profile(profile)` antes de construir o executor.
O padrão continua embutido; `F365_PROTOCOL_PROFILE` seleciona explicitamente um arquivo
confiável na CLI, sem recarga automática; arquivo inválido aborta. A validação restringe
alterações à versão numérica no caminho authsvc. O teste sintético
`runtime_profile_changes_real_executor_authsvc_request` exercita o executor real.

`diagnostics::Recorder::new` envolve explicitamente o executor e `TeamsClient::with_recorder`
acrescenta eventos de protocolo e falhas WebSocket ao mesmo bundle limitado, com campos tipados,
sem corpos, URLs ou IDs. O JSON v2 distingue resultados HTTP de resultados de protocolo: HTTP
2xx não significa que a resposta passou na validação. `truncated_count` conta eventos expulsos
(saturando em u64); `truncated` indica que houve expulsão. Somente os 256 eventos mais recentes
são mantidos: um incidente também pode ser expulso após 256 eventos posteriores. A exportação
`bundle().export_json()` é opt-in, em memória; quem a chama é responsável por consentimento e
proteção do destino. Testes sintéticos cobrem REST, handshake e WebSocket local; não validam o serviço real.
**Lacuna:** a CLI não oferece ativação ou exportação deste bundle; apenas integradores da
biblioteca podem utilizá-lo. Não se garante segurança de logs ou de outros consumidores.
Authorize/PKCE e outros pontos sem captura permanecem UNVERIFIED; nenhuma sessão ou conta
real foi acessada.
