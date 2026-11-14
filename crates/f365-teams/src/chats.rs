//! Capacidade: listar conversas (Checkpoint C, `docs/ARQUITETURA.md` §2.3/§3.4).
//!
//! O provider só **descreve** a operação; quem põe credencial, valida o host e escolhe a rota é o
//! executor (`f365_auth::executor`).
//!
//! Contratos de protocolo (UNVERIFIED até o Checkpoint C rodar na conta do dono — ver #5):
//! - `TEAMS-CSA-LIST-001`: `GET https://chatsvcagg.teams.microsoft.com/api/v1/teams/users/me` com o
//!   token de `aud chatsvcagg` lista o que o usuário vê.
//! - `TEAMS-CSA-SHAPE-001`: a resposta traz `chats[]` (`id`, `title` — nulo em 1:1 —, `chatType`) e
//!   `teams[]` (`displayName`, `channels[]` com `id`/`displayName`). Leitura tolerante: campo ausente
//!   vira valor padrão; só a falta das duas listas é tratada como contrato quebrado.

use crate::TeamsError;
use f365_auth::transport::HttpRequest;
use f365_auth::{AuthOp, Resource};
use serde_json::Value;
use std::time::Duration;

pub(crate) const LIST_URL: &str =
    "https://chatsvcagg.teams.microsoft.com/api/v1/teams/users/me?isPrefetch=false&enableMembershipSummary=true";
const TIMEOUT: Duration = Duration::from_secs(30);

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

pub(crate) fn parse(body: &[u8]) -> Result<Vec<Conversation>, TeamsError> {
    let v: Value = serde_json::from_slice(body)
        .map_err(|_| TeamsError::Malformed("chatsvcagg: corpo não é JSON"))?;
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
        let title =
            text(chat, "title")
                .map(str::to_owned)
                .unwrap_or_else(|| match chat_type.as_deref() {
                    Some("oneOnOne") => "(conversa 1:1 sem título)".into(),
                    _ => "(chat sem título)".into(),
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
        assert_eq!(got[1].title, "(conversa 1:1 sem título)");
        assert_eq!(got[2].title, "Engenharia › Geral");
        assert_eq!(
            got[2].kind,
            ConversationKind::Channel {
                team: "Engenharia".into()
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
