//! Capacidade: enviar mensagem (Checkpoint E, `docs/ARQUITETURA.md` §2.3/§3.4).
//!
//! Vai para o **serviço de chat regional** (skype token), não para o agregador: o provider manda só o
//! caminho e o executor completa com o host descoberto no authsvc (`EXEC-HOST-001`).
//!
//! O `clientmessageid` é gerado por **quem chama**, antes do envio: se a resposta se perder (rede,
//! prazo), quem chamou ainda tem a chave para reconciliar sem reenviar (`SEND-RECONCILE-001`). O
//! envio **não** é idempotente — o executor nunca o repete.
//!
//! Contratos. Evidência = Checkpoint E na conta do dono, 2026-11-13 23:55 (#12): mensagem enviada
//! pelo `f365 send` apareceu **uma vez** no Teams oficial.
//! - `SEND-PATH-001` (VERIFIED): `POST /v1/users/ME/conversations/{id}/messages` no chat regional.
//!   O chat do usuário consigo mesmo tem id `48:notes` (não aparece no agregador).
//! - `SEND-BODY-001` (VERIFIED): corpo `{content, messagetype: "Text", contenttype: "text",
//!   clientmessageid}`. Texto puro de propósito: `RichText/Html` exigiria escapar HTML (injeção).
//! - `SEND-CLIENTMSGID-001` (VERIFIED): `clientmessageid` numérico de 19 dígitos aceito.
//! - `SEND-RESPONSE-001` (PARCIAL): VERIFIED que vem um id numérico = horário de chegada em ms
//!   (bateu com o horário exibido no Teams). UNVERIFIED se veio do `Location` ou do corpo — o
//!   parser aceita os dois.

use crate::TeamsError;
use f365_auth::transport::{HttpRequest, HttpResponse};
use f365_auth::{AuthOp, Resource};
use serde_json::{json, Value};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);
/// Limite generoso abaixo do que o Teams aceita; mensagem maior é erro de uso, não de rede.
const MAX_TEXT_CHARS: usize = 20_000;
const MAX_CHAT_ID_LEN: usize = 512;

/// Resultado de um envio aceito pelo servidor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendReceipt {
    pub client_message_id: String,
    /// Id atribuído pelo servidor, quando a resposta o informa.
    pub server_message_id: Option<String>,
}

/// Novo `clientmessageid` (numérico, aleatório) — gere ANTES de enviar e guarde.
pub fn new_client_message_id() -> String {
    let (hi, _) = uuid::Uuid::new_v4().as_u64_pair();
    // Sem zeros à esquerda e sempre com 19 dígitos: mesmo formato do cliente web.
    (hi % 9_000_000_000_000_000_000 + 1_000_000_000_000_000_000).to_string()
}

/// O id entra no caminho da URL: só caracteres que não mudam a rota.
pub(crate) fn validate_chat_id(chat_id: &str) -> Result<(), TeamsError> {
    let ok = !chat_id.is_empty()
        && chat_id != "."
        && chat_id != ".."
        && chat_id.len() <= MAX_CHAT_ID_LEN
        && chat_id.chars().all(|c| {
            !c.is_whitespace() && !c.is_control() && !matches!(c, '/' | '?' | '#' | '%' | '\\')
        });
    ok.then_some(()).ok_or(TeamsError::InvalidInput(
        "id de conversa inválido (use o id que `f365 chats` mostra)",
    ))
}

pub(crate) fn send_op(
    chat_id: &str,
    text: &str,
    client_message_id: &str,
) -> Result<AuthOp, TeamsError> {
    validate_chat_id(chat_id)?;
    if text.trim().is_empty() {
        return Err(TeamsError::InvalidInput("mensagem vazia"));
    }
    if text.chars().count() > MAX_TEXT_CHARS {
        return Err(TeamsError::InvalidInput("mensagem longa demais"));
    }
    if client_message_id.is_empty() || !client_message_id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(TeamsError::InvalidInput(
            "clientmessageid deve ser numérico",
        ));
    }
    let body = json!({
        "content": text,
        "messagetype": "Text",
        "contenttype": "text",
        "clientmessageid": client_message_id,
    });
    Ok(AuthOp {
        resource: Resource::ChatService,
        request: HttpRequest {
            method: "POST".into(),
            url: format!("/v1/users/ME/conversations/{chat_id}/messages"),
            headers: vec![
                ("Content-Type".into(), "application/json".into()),
                ("Accept".into(), "application/json".into()),
            ],
            body: Some(body.to_string().into_bytes()),
        },
        needs_page_context: false,
        idempotent: false,
        timeout: TIMEOUT,
    })
}

pub(crate) fn receipt(resp: &HttpResponse, client_message_id: &str) -> SendReceipt {
    let from_location = resp
        .header("location")
        .and_then(|l| l.split(['?', '#']).next())
        .and_then(|l| l.trim_end_matches('/').rsplit('/').next())
        .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
        .map(str::to_owned);
    let from_body = || {
        serde_json::from_slice::<Value>(&resp.body)
            .ok()
            .and_then(|v| match v.get("OriginalArrivalTime") {
                Some(Value::Number(n)) => Some(n.to_string()),
                Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
                _ => None,
            })
    };
    SendReceipt {
        client_message_id: client_message_id.to_owned(),
        server_message_id: from_location.or_else(from_body),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_is_a_non_idempotent_post_to_the_regional_chat_by_path() {
        let op = send_op("19:abc@thread.v2", "ping do f365", "1234567890123456789").unwrap();
        assert_eq!(op.resource, Resource::ChatService);
        assert_eq!(op.request.method, "POST");
        assert_eq!(
            op.request.url,
            "/v1/users/ME/conversations/19:abc@thread.v2/messages"
        );
        assert!(!op.idempotent, "envio nunca pode ser repetido às cegas");
        assert!(op
            .request
            .headers
            .iter()
            .all(|(k, _)| !k.eq_ignore_ascii_case("authentication")));

        let body: Value = serde_json::from_slice(op.request.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["content"], "ping do f365");
        assert_eq!(body["messagetype"], "Text");
        assert_eq!(body["contenttype"], "text");
        assert_eq!(body["clientmessageid"], "1234567890123456789");
    }

    #[test]
    fn text_is_sent_verbatim_as_json_not_as_html() {
        let op = send_op("19:a@thread.v2", "<b>oi</b> & \"aspas\"", "1").unwrap();
        let body: Value = serde_json::from_slice(op.request.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["content"], "<b>oi</b> & \"aspas\"");
        assert_eq!(
            body["messagetype"], "Text",
            "nunca RichText/Html com texto do usuário"
        );
    }

    #[test]
    fn chat_id_cannot_change_the_route() {
        for bad in [
            "",
            "19:a/../../evil",
            "19:a?x=1",
            "19:a#frag",
            "19:a%2F",
            "19:a b",
            "19:a\\b",
            "19:a\n",
        ] {
            assert!(
                matches!(send_op(bad, "oi", "1"), Err(TeamsError::InvalidInput(_))),
                "{bad:?}"
            );
        }
        assert!(validate_chat_id("19:meeting_NWYz@thread.v2").is_ok());
        assert!(validate_chat_id("19:a-b_c@unq.gbl.spaces").is_ok());
        assert!(validate_chat_id(&"x".repeat(MAX_CHAT_ID_LEN + 1)).is_err());
    }

    #[test]
    fn rejects_empty_or_huge_text_and_bad_client_id() {
        assert!(send_op("19:a@thread.v2", "   ", "1").is_err());
        assert!(send_op("19:a@thread.v2", &"a".repeat(MAX_TEXT_CHARS + 1), "1").is_err());
        assert!(send_op("19:a@thread.v2", "oi", "").is_err());
        assert!(send_op("19:a@thread.v2", "oi", "12ab").is_err());
    }

    #[test]
    fn client_message_ids_are_19_digit_and_unique() {
        let a = new_client_message_id();
        let b = new_client_message_id();
        assert_eq!(a.len(), 19);
        assert!(a.bytes().all(|c| c.is_ascii_digit()) && !a.starts_with('0'));
        assert_ne!(a, b);
    }

    fn resp(status: u16, headers: Vec<(&str, &str)>, body: &str) -> HttpResponse {
        HttpResponse {
            status,
            headers: headers
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn server_id_from_location_or_arrival_time() {
        let r = resp(201, vec![("Location", "https://amer.ng.msg.teams.microsoft.com/v1/users/ME/conversations/19:a/messages/1731500000000")], "");
        assert_eq!(
            receipt(&r, "7").server_message_id.as_deref(),
            Some("1731500000000")
        );

        let r = resp(201, vec![], r#"{"OriginalArrivalTime": 1731500000001}"#);
        assert_eq!(
            receipt(&r, "7").server_message_id.as_deref(),
            Some("1731500000001")
        );

        let r = resp(201, vec![], "");
        let rc = receipt(&r, "7");
        assert_eq!(
            (rc.client_message_id.as_str(), rc.server_message_id),
            ("7", None)
        );
    }
}
