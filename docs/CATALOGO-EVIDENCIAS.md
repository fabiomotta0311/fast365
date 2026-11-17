# Catálogo inicial de evidências — trabalho incompleto

Esta tabela registra um subconjunto dos contratos e não altera o status dos demais em
[CONTRATOS-FASE0.md](CONTRATOS-FASE0.md). Testes sintéticos não demonstram comportamento real.

| ID | Hipótese e comportamento dependente | Evidência | Fallback |
|---|---|---|---|
| SEND-RECONCILE-001 | UNVERIFIED: identificar envio anterior após perda de resposta via clientmessageid | [matriz](CONTRATOS-FASE0.md#5--rest), relato histórico #44 não prova reconciliação | Marcar incerto; nunca reenviar às cegas. |
| HISTORY-PAGE-001 | UNVERIFIED: backwardLink ou syncState indica próxima página | [history.rs](../crates/f365-teams/src/history.rs); fixtures sintéticas apenas | Rejeitar cursor inválido ou cíclico; não retornar sucesso parcial. |
| TROUTER-HANDSHAKE-001 | Handshake socket.io e con_num conforme implementação | [matriz](CONTRATOS-FASE0.md#6--trouter), relato histórico #45, limitado ao caso observado | Falhar a conexão; não divulgar URL assinada. |

O recorder operacional descrito abaixo possui bundle próprio com campos tipados. A cobertura
atual não demonstra segurança de integrações ou logs fora desse tipo.

O integrador pode carregar um arquivo explicitamente com `f365_auth::Profile::load(path)`
e aplicar o resultado a `TokenBroker::with_profile(profile)` antes de construir o executor.
O padrão continua embutido; não há seleção pela CLI nem recarga automática. A validação
restringe alterações à versão numérica no caminho authsvc. O teste sintético
`runtime_profile_changes_real_executor_authsvc_request` exercita o executor real.

`diagnostics::Recorder::new` envolve explicitamente o executor e `bundle()` retorna somente
eventos HTTP tipados, sem corpos, URLs ou IDs. O teste `wiremock_incident` cobre REST e falha
de handshake sintéticos, mas não WebSocket local nem suas falhas. A segurança de logs e
outros consumidores não é garantida pelo bundle. O catálogo completo e a integração de
WebSocket permanecem pendentes. Authorize/PKCE e outros pontos sem captura permanecem
UNVERIFIED; nenhuma sessão ou conta real foi acessada.
