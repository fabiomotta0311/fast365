# Catálogo inicial de evidências — trabalho incompleto

Esta tabela registra um subconjunto dos contratos e não altera o status dos demais em
[CONTRATOS-FASE0.md](CONTRATOS-FASE0.md). Testes sintéticos não demonstram comportamento real.

| ID | Hipótese e comportamento dependente | Evidência | Fallback |
|---|---|---|---|
| SEND-RECONCILE-001 | UNVERIFIED: identificar envio anterior após perda de resposta via clientmessageid | [matriz](CONTRATOS-FASE0.md#5--rest), relato histórico #44 não prova reconciliação | Marcar incerto; nunca reenviar às cegas. |
| HISTORY-PAGE-001 | UNVERIFIED: backwardLink ou syncState indica próxima página | [history.rs](../crates/f365-teams/src/history.rs); fixtures sintéticas apenas | Rejeitar cursor inválido ou cíclico; não retornar sucesso parcial. |
| TROUTER-HANDSHAKE-001 | Handshake socket.io e con_num conforme implementação | [matriz](CONTRATOS-FASE0.md#6--trouter), relato histórico #45, limitado ao caso observado | Falhar a conexão; não divulgar URL assinada. |

O tipo `f365_teams::diagnostics::Bundle` serializa apenas eventos tipados, sem campos
textuais arbitrários; ele ainda não está conectado à coleta operacional, não tem
pseudonimização nem implementa captura de conteúdo. Campos numéricos são reservados a
medições verdadeiras; não devem ser usados para transportar dados arbitrários. A cobertura
atual não demonstra segurança de integrações ou logs fora desse tipo.

O profile de autenticação continua embutido na compilação, parcial e sem alteração. Os
campos de authorize/PKCE e os demais pontos não capturados continuam UNVERIFIED; não foi
feita captura de sessão ou teste com conta real. Harness wiremock, profile externo em
runtime, catálogo completo e integração operacional do diagnóstico continuam pendentes.
