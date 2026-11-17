//! Capacidade: listar conversas (Checkpoint C, `docs/ARQUITETURA.md` §2.3/§3.4).
//!
//! O provider só **descreve** a operação; quem põe credencial, valida o host e escolhe a rota é o
//! executor (`f365_auth::executor`).
//!
//! Contratos. Evidência = Checkpoint C na conta do dono, 2026-11-13 (#10), via executor (#9):
//! - `TEAMS-CSA-LIST-001` (VERIFIED): `GET https://chatsvcagg.teams.microsoft.com/api/v1/teams/users/me`
//!   com o token de `aud chatsvcagg` → HTTP 200 com as conversas do usuário.
//! - `TEAMS-CSA-SHAPE-001` (PARCIAL): VERIFIED `chats[]` com `id`, `title` (nulo em 1:1) e `chatType`
//!   — valores vistos: `meeting` e `chat` (este vale para 1:1 **e** grupo). O 1:1 se reconhece pelo
//!   id `…@unq.gbl.spaces`.
//!   UNVERIFIED: `teams[].channels[]` (não apareceu nos primeiros itens). Leitura tolerante: campo
//!   ausente vira valor padrão; só a falta das duas listas é tratada como contrato quebrado.

use crate::TeamsError;
use f365_auth::transport::HttpRequest;
use f365_auth::{AuthOp, Resource};
use serde_json::Value;
use std::time::Duration;

pub(crate) const LIST_URL: &str =
    "https://chatsvcagg.teams.microsoft.com/api/v1/teams/users/me?isPrefetch=false&enableMembershipSummary=true";
const TIMEOUT: Duration = Duration::from_secs(30);
const ONE_ON_ONE_SUFFIX: &str = "@unq.gbl.spaces";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationKind {
    /// Chat 1:1, em grupo ou de reunião (`chatType` como veio da API, se veio).
    Chat { chat_type: Option<String> },
    /// Canal de uma equipe.
    Channel { team: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub kind: ConversationKind,
}

/// Resultado da listagem: as conversas e, quando inferível, **quem sou eu**.
///
/// Um 1:1 não traz nome do outro lado no payload (`members[]` só tem `mri`/`objectId`). Mas quando
/// a última mensagem é minha, `lastMessage.imDisplayName` é o **meu** nome — e isso permite, junto
/// com o histórico da conversa, descobrir quem é a outra pessoa sem endpoint novo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub conversations: Vec<Conversation>,
    pub my_display_name: Option<String>,
}

impl Listing {
    /// Nome de exibição do próprio usuário, quando alguma conversa mostra a última mensagem dele.
    fn infer_my_name(value: &Value) -> Option<String> {
        value
            .get("chats")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|chat| {
                chat.get("isLastMessageFromMe")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            })
            .and_then(|chat| text(chat.get("lastMessage")?, "imDisplayName"))
            .map(str::to_owned)
    }
}

pub(crate) fn list_op() -> AuthOp {
    AuthOp {
        resource: Resource::ChatSvcAgg,
        request: HttpRequest {
            method: "GET".into(),
            url: LIST_URL.into(),
            headers: vec![("Accept".into(), "application/json".into())],
            body: None,
        },
        needs_page_context: false,
        idempotent: true,
        timeout: TIMEOUT,
    }
}

/// Conveniência dos testes; a produção usa [`parse_listing`], que também infere quem sou eu.
#[cfg(test)]
pub(crate) fn parse(body: &[u8]) -> Result<Vec<Conversation>, TeamsError> {
    parse_value(&decode(body)?)
}

/// Listagem + quem sou eu (ver [`Listing`]).
pub(crate) fn parse_listing(body: &[u8]) -> Result<Listing, TeamsError> {
    let value = decode(body)?;
    Ok(Listing {
        conversations: parse_value(&value)?,
        my_display_name: Listing::infer_my_name(&value),
    })
}

fn decode(body: &[u8]) -> Result<Value, TeamsError> {
    serde_json::from_slice(body).map_err(|_| TeamsError::Malformed("chatsvcagg: corpo não é JSON"))
}

fn parse_value(v: &Value) -> Result<Vec<Conversation>, TeamsError> {
    let chats = v.get("chats").and_then(Value::as_array);
    let teams = v.get("teams").and_then(Value::as_array);
    if chats.is_none() && teams.is_none() {
        return Err(TeamsError::Malformed(
            "chatsvcagg: sem `chats` nem `teams` (TEAMS-CSA-SHAPE-001)",
        ));
    }

    let mut out = Vec::new();
    for chat in chats.into_iter().flatten() {
        let Some(id) = text(chat, "id") else { continue };
        let chat_type = text(chat, "chatType").map(str::to_owned);
        // 1:1 se reconhece pelo id **ou** pelo `isOneOnOne`; `chatType` vem `chat` também em grupo.
        let one_on_one = id.ends_with(ONE_ON_ONE_SUFFIX)
            || chat
                .get("isOneOnOne")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        let title = text(chat, "title")
            .map(str::to_owned)
            .or_else(|| {
                if one_on_one {
                    counterpart_name(chat)
                } else {
                    None
                }
            })
            .unwrap_or_else(|| {
                if one_on_one {
                    // Sem nome disponível: um sufixo estável distingue as conversas entre si
                    // (antes todas caíam no mesmo texto fixo).
                    format!("1:1 · {}", short_suffix(id))
                } else {
                    "(chat sem título)".to_string()
                }
            });
        out.push(Conversation {
            id: id.to_owned(),
            title,
            kind: ConversationKind::Chat { chat_type },
        });
    }
    for team in teams.into_iter().flatten() {
        let team_name = text(team, "displayName")
            .unwrap_or("(equipe sem nome)")
            .to_owned();
        for channel in team
            .get("channels")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(id) = text(channel, "id") else {
                continue;
            };
            let title = format!(
                "{team_name} › {}",
                text(channel, "displayName").unwrap_or("(canal sem nome)")
            );
            out.push(Conversation {
                id: id.to_owned(),
                title,
                kind: ConversationKind::Channel {
                    team: team_name.clone(),
                },
            });
        }
    }
    Ok(out)
}

/// Texto não vazio (aparado) de um campo do JSON.
fn text<'a>(o: &'a Value, k: &str) -> Option<&'a str> {
    o.get(k)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Nome do outro lado num 1:1 — **só** quando a última mensagem não é minha.
///
/// `title` vem vazio e `members[]` traz apenas `mri`/`objectId`, sem nome (`TEAMS-CSA-SHAPE-001`).
/// O único nome no payload é `lastMessage.imDisplayName`, que é do **remetente**: serve quando o
/// remetente é a outra pessoa e mentiria quando sou eu — daí o `isLastMessageFromMe`.
fn counterpart_name(chat: &Value) -> Option<String> {
    let mine = chat
        .get("isLastMessageFromMe")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if mine {
        return None;
    }
    text(chat.get("lastMessage")?, "imDisplayName").map(str::to_owned)
}

/// Sufixo curto e estável do id, para distinguir 1:1 sem nome conhecido.
fn short_suffix(id: &str) -> String {
    let hex = id.trim_start_matches("19:").split('@').next().unwrap_or(id);
    hex.chars().take(6).collect()
}

/// Nomes de campo do primeiro item de `chats[]` (e de `lastMessage`/`members`), **sem valores**.
///
/// Diagnóstico de `TEAMS-CSA-SHAPE-001`: um 1:1 costuma vir sem `title`, e o nome da pessoa está
/// em algum campo que ainda não mapeamos. Nomes de campo não são dado de tenant — valores sim, e
/// eles nunca saem daqui.
pub fn field_names(body: &[u8]) -> Vec<String> {
    let mut names = std::collections::BTreeSet::new();
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return Vec::new();
    };
    let Some(chat) = value
        .get("chats")
        .and_then(Value::as_array)
        .and_then(|chats| chats.first())
    else {
        return Vec::new();
    };
    if let Some(object) = chat.as_object() {
        names.extend(object.keys().cloned());
    }
    for nested in ["lastMessage", "members"] {
        match chat.get(nested) {
            Some(Value::Array(items)) => {
                if let Some(first) = items.first().and_then(Value::as_object) {
                    names.extend(first.keys().map(|key| format!("{nested}[].{key}")));
                }
            }
            Some(Value::Object(object)) => {
                names.extend(object.keys().map(|key| format!("{nested}.{key}")));
            }
            _ => {}
        }
    }
    names.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(v: Value) -> Vec<u8> {
        v.to_string().into_bytes()
    }

    #[test]
    fn op_is_a_credential_free_idempotent_get_on_the_aggregator() {
        let op = list_op();
        assert_eq!(op.resource, Resource::ChatSvcAgg);
        assert_eq!(op.request.method, "GET");
        assert!(op.idempotent && !op.needs_page_context);
        let url = url::Url::parse(&op.request.url).unwrap();
        assert_eq!(url.host_str(), Some("chatsvcagg.teams.microsoft.com"));
        assert!(op
            .request
            .headers
            .iter()
            .all(|(k, _)| !k.eq_ignore_ascii_case("authorization")));
    }

    /// Diagnóstico de forma: mostra caminho de campo e **nunca** o valor.
    #[test]
    fn field_names_never_leak_values() {
        let names = field_names(&body(json!({
            "chats": [{
                "id": "19:dm@unq.gbl.spaces",
                "title": null,
                "chatType": "oneOnOne",
                "lastMessage": {"imdisplayname": "Pessoa Secreta", "content": "texto privado"},
                "members": [{"mri": "8:orgid:x", "friendlyName": "Pessoa Secreta"}]
            }]
        })));
        let joined = names.join(",");
        assert!(names.contains(&"id".to_string()));
        assert!(names.contains(&"lastMessage.imdisplayname".to_string()));
        assert!(names.contains(&"members[].friendlyName".to_string()));
        assert!(!joined.contains("Pessoa Secreta"), "{joined}");
        assert!(!joined.contains("texto privado"), "{joined}");
    }

    #[test]
    fn parses_chats_and_channels() {
        let got = parse(&body(json!({
            "chats": [
                {"id": "19:abc@thread.v2", "title": "Projeto X", "chatType": "group"},
                {"id": "19:dm@unq.gbl.spaces", "title": null, "chatType": "oneOnOne"},
            ],
            "teams": [
                {"displayName": "Engenharia", "channels": [
                    {"id": "19:geral@thread.tacv2", "displayName": "Geral"}
                ]}
            ]
        })))
        .unwrap();

        assert_eq!(got.len(), 3);
        assert_eq!(got[0].title, "Projeto X");
        assert_eq!(got[1].title, "1:1 · dm", "sem nome: sufixo estável");
        assert_eq!(got[2].title, "Engenharia › Geral");
        assert_eq!(
            got[2].kind,
            ConversationKind::Channel {
                team: "Engenharia".into()
            }
        );
    }

    /// O único nome no payload é o do remetente da última mensagem — serve só quando não sou eu.
    #[test]
    fn one_on_one_uses_the_counterpart_from_the_last_message() {
        let got = parse(&body(json!({"chats": [
            {
                "id": "19:dm1@unq.gbl.spaces",
                "title": null,
                "isOneOnOne": true,
                "isLastMessageFromMe": false,
                "lastMessage": {"imDisplayName": "Ana Souza", "content": "oi"}
            },
            {
                "id": "19:dm2@unq.gbl.spaces",
                "title": null,
                "isOneOnOne": true,
                "isLastMessageFromMe": true,
                "lastMessage": {"imDisplayName": "Eu Mesmo", "content": "tchau"}
            },
            {
                "id": "19:dm3@unq.gbl.spaces",
                "title": "",
                "isOneOnOne": true,
                "members": [{"mri": "8:orgid:x", "objectId": "00000000-0000"}]
            }
        ]})))
        .unwrap();

        assert_eq!(got[0].title, "Ana Souza");
        // Nunca usar o meu próprio nome como título da conversa.
        assert_eq!(got[1].title, "1:1 · dm2");
        // `members[]` não traz nome: cai no sufixo, mas cada conversa fica distinguível.
        assert_eq!(got[2].title, "1:1 · dm3");
    }

    /// Visto no Checkpoint C: `chatType` é `chat` em 1:1 e em grupo; o 1:1 sem título se reconhece
    /// pelo id.
    #[test]
    fn one_on_one_is_recognized_by_id_not_chat_type() {
        let got = parse(&body(json!({"chats": [
            {"id": "19:a_b@unq.gbl.spaces", "title": null, "chatType": "chat"},
            {"id": "19:grupo@thread.v2", "title": null, "chatType": "chat"},
            {"id": "19:meeting_x@thread.v2", "title": "Reunião", "chatType": "meeting"}
        ]})))
        .unwrap();
        assert_eq!(got[0].title, "1:1 · a_b");
        assert_eq!(got[1].title, "(chat sem título)");
        assert_eq!(
            got[0].kind,
            ConversationKind::Chat {
                chat_type: Some("chat".into())
            }
        );
        assert_eq!(
            got[2].kind,
            ConversationKind::Chat {
                chat_type: Some("meeting".into())
            }
        );
    }

    #[test]
    fn tolerates_missing_fields_and_skips_entries_without_id() {
        let got = parse(&body(json!({
            "chats": [{"title": "sem id"}, {"id": "19:x", "title": "   "}],
            "teams": [{"channels": [{"displayName": "sem id"}, {"id": "19:c"}]}]
        })))
        .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].title, "(chat sem título)");
        assert_eq!(got[1].title, "(equipe sem nome) › (canal sem nome)");
    }

    #[test]
    fn only_chats_or_only_teams_is_fine() {
        assert_eq!(parse(&body(json!({"chats": []}))).unwrap(), vec![]);
        assert_eq!(parse(&body(json!({"teams": []}))).unwrap(), vec![]);
    }

    #[test]
    fn unexpected_shape_is_a_broken_contract() {
        assert!(
            matches!(parse(&body(json!({"value": []}))), Err(TeamsError::Malformed(m)) if m.contains("TEAMS-CSA-SHAPE-001"))
        );
        assert!(matches!(parse(b"<html>"), Err(TeamsError::Malformed(_))));
    }
}
