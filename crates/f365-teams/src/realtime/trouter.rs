//! Operações do Trouter (`docs/ARQUITETURA.md` §3.5). Evidência = Checkpoint D na conta do dono,
//! 2026-11-14 (#11): os 4 passos passaram, a sessão ficou >18 min viva, e uma mensagem chegou ao
//! `listen` ~1,1 s depois de chegar ao servidor. O erro de cada passo cita o contrato que quebrou.
//!
//! - `TROUTER-CONNECT-001` (VERIFIED): `POST https://go.trouter.teams.microsoft.com/v4/a?epid=<uuid>`
//!   devolve `socketio` (`https://<nó>.trouter.teams.microsoft.com/`), `surl`, `ccid`,
//!   `registrarUrl` e `connectparams` assinados (`issuer, scae, se, sig, sp, sr, st`) — além de
//!   `curlb`, `healthUrl`, `id`, `ttl`, `url`, ainda sem uso.
//! - `TROUTER-HANDSHAKE-001` (VERIFIED): `GET <socketio>socket.io/1/?v=v4&<connectparams>&tc=…&
//!   timeout=40&epid=…&ccid=…&con_num=<ms>_<n>&auth=true`. **Sem `con_num` → HTTP 400** (corpo vazio).
//! - `TROUTER-WS-001` (VERIFIED): `wss://<nó>/socket.io/1/websocket/<sid>?<mesma query>` abre e
//!   confirma (`1::`) **sem** skype token — a autorização são os `connectparams` assinados.
//! - `TROUTER-REGISTRAR-001` (VERIFIED): o registrador vem em `registrarUrl`
//!   (`https://communications.svc.cloud.microsoft/registrar/<região>/v3/registrations`) e aceita o
//!   corpo do prior art (`clientDescription` + `transports.TROUTER[].path = surl`).
//! - `TROUTER-MSG-001` (VERIFIED): notificações chegam como mensagem (tipo 3), respondidas com
//!   `{id, status: 200}`; o id da conversa sai do `url`/`resourceLink` (`48:notes` visto). Parte das
//!   notificações vem sem id legível (corpo compactado) → invalidação genérica.
//! - Sinais vistos: `trouter.connected` e `trouter.message_loss` (avisos de possível lacuna — base
//!   do replay por REST da Fase 2).
//!
//! Os `connectparams` são uma credencial curta do socket: nunca vão para log nem mensagem de erro.

use crate::TeamsError;
use f365_auth::transport::HttpRequest;
use f365_auth::{AuthOp, Resource};
use serde_json::{json, Map, Value};
use std::time::Duration;

const CONNECT_URL: &str = "https://go.trouter.teams.microsoft.com/v4/a";
const REGISTRAR_URL: &str = "https://teams.microsoft.com/registrar/prod/V2/registrations";
const TROUTER_SUFFIX: &str = ".trouter.teams.microsoft.com";
/// Identificação de cliente que o Trouter espera em `tc` (valores do cliente web; UNVERIFIED).
const CLIENT_TC: &str = r#"{"cv":"2024.23.01.2","ua":"TeamsCDL","hr":"","v":"1415/24062722442"}"#;
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ConnectInfo {
    /// `https://<nó>.trouter.teams.microsoft.com/` (com barra final).
    pub socketio: String,
    pub surl: String,
    pub ccid: Option<String>,
    /// Registrador informado pelo próprio Trouter (`registrarUrl`); o executor ainda trava o host.
    pub registrar: Option<String>,
    /// Número da conexão `<timestamp_ms>_<n>` (`TROUTER-HANDSHAKE-001`, prior art purple-teams).
    pub con_num: Option<String>,
    /// Parâmetros assinados do socket — credencial: `Debug` não os mostra.
    params: Vec<(String, String)>,
}

impl std::fmt::Debug for ConnectInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectInfo")
            .field("socketio", &self.socketio)
            .field("connectparams", &format!("<{} ocultos>", self.params.len()))
            .finish_non_exhaustive()
    }
}

fn op(method: &str, url: String, body: Option<Value>) -> AuthOp {
    let mut headers = vec![("Accept".into(), "application/json".into())];
    if body.is_some() {
        headers.push(("Content-Type".into(), "application/json".into()));
    }
    AuthOp {
        resource: Resource::Trouter,
        request: HttpRequest {
            method: method.into(),
            url,
            headers,
            body: body.map(|b| b.to_string().into_bytes()),
        },
        needs_page_context: false,
        idempotent: true,
        timeout: TIMEOUT,
    }
}

pub(crate) fn connect_op(epid: &str) -> AuthOp {
    let mut o = op("POST", format!("{CONNECT_URL}?epid={epid}"), None);
    o.request.body = Some(Vec::new());
    o
}

pub(crate) fn parse_connect(body: &[u8]) -> Result<ConnectInfo, TeamsError> {
    let broken =
        || TeamsError::Malformed("Trouter: resposta de conexão inesperada (TROUTER-CONNECT-001)");
    let v: Value = serde_json::from_slice(body).map_err(|_| broken())?;
    let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    let socketio = text("socketio").ok_or_else(broken)?;
    let host_ok = url::Url::parse(&socketio)
        .ok()
        .filter(|u| u.scheme() == "https")
        .and_then(|u| u.host_str().map(|h| h.ends_with(TROUTER_SUFFIX)))
        .unwrap_or(false);
    if !host_ok {
        return Err(TeamsError::Malformed(
            "Trouter: `socketio` fora de *.trouter.teams.microsoft.com",
        ));
    }
    let surl = text("surl").ok_or_else(broken)?;
    let params = match v.get("connectparams") {
        Some(Value::Object(m)) => m
            .iter()
            .filter_map(|(k, v)| scalar(v).map(|s| (k.clone(), s)))
            .collect(),
        _ => return Err(broken()),
    };
    let socketio = if socketio.ends_with('/') {
        socketio
    } else {
        format!("{socketio}/")
    };
    Ok(ConnectInfo {
        socketio,
        surl,
        ccid: text("ccid"),
        registrar: text("registrarUrl"),
        con_num: None,
        params,
    })
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Só os NOMES dos campos (e dos `connectparams`) — nunca os valores, que incluem assinatura.
pub(crate) fn shape(body: &[u8]) -> Vec<String> {
    let Ok(Value::Object(top)) = serde_json::from_slice::<Value>(body) else {
        return vec!["<não é objeto JSON>".into()];
    };
    let mut out: Vec<String> = top.keys().cloned().collect();
    if let Some(Value::Object(cp)) = top.get("connectparams") {
        out.extend(
            cp.iter()
                .map(|(k, v)| format!("connectparams.{k}:{}", kind(v))),
        );
    }
    out
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::String(_) => "str",
        Value::Number(_) => "num",
        Value::Bool(_) => "bool",
        Value::Object(_) => "obj",
        Value::Array(_) => "arr",
        Value::Null => "null",
    }
}

/// Query comum ao handshake e ao WebSocket.
pub(crate) fn query(info: &ConnectInfo, epid: &str) -> String {
    let mut q = url::form_urlencoded::Serializer::new(String::new());
    q.append_pair("v", "v4");
    for (k, v) in &info.params {
        q.append_pair(k, v);
    }
    q.append_pair("tc", CLIENT_TC);
    q.append_pair("timeout", "40");
    q.append_pair("epid", epid);
    if let Some(ccid) = &info.ccid {
        q.append_pair("ccid", ccid);
    }
    if let Some(n) = &info.con_num {
        q.append_pair("con_num", n);
    }
    q.append_pair("auth", "true");
    q.finish()
}

pub(crate) fn handshake_op(info: &ConnectInfo, epid: &str) -> AuthOp {
    let mut o = op(
        "GET",
        format!("{}socket.io/1/?{}", info.socketio, query(info, epid)),
        None,
    );
    o.request.headers = vec![("Accept".into(), "*/*".into())];
    o
}

/// `wss://<nó>/socket.io/1/websocket/<sid>?<query>`. O nó vem de `parse_connect`, já validado.
pub(crate) fn ws_url(info: &ConnectInfo, session_id: &str, epid: &str) -> String {
    let base = info.socketio.replacen("https://", "wss://", 1);
    format!(
        "{base}socket.io/1/websocket/{session_id}?{}",
        query(info, epid)
    )
}

pub(crate) fn registrar_op(info: &ConnectInfo, epid: &str) -> AuthOp {
    let body = json!({
        "clientDescription": {
            "appId": "TeamsCDLWebWorker",
            "aesKey": "",
            "languageId": "en-US",
            "platform": "edge",
            "templateKey": "TeamsCDLWebWorker_2.1",
            "platformUIVersion": "1415/24062722442",
        },
        "registrationId": epid,
        "nodeId": "",
        "transports": {"TROUTER": [{"context": "", "path": info.surl, "ttl": 86_400}]},
    });
    let url = info
        .registrar
        .clone()
        .unwrap_or_else(|| REGISTRAR_URL.into());
    op("POST", url, Some(body))
}

/// Resposta obrigatória a uma mensagem (tipo 3) do Trouter (`TROUTER-MSG-001`).
pub(crate) fn response_for(message: &Value) -> Option<String> {
    let id = message.get("id")?;
    let reply = json!({"id": id, "status": 200, "headers": {}, "body": ""});
    Some(format!("3:::{reply}"))
}

/// Só o id da conversa alterada — o conteúdo da mensagem nunca é lido (evento = invalidação).
pub(crate) fn conversation_of(message: &Value) -> Option<String> {
    let body = message.get("body").and_then(|v| match v {
        Value::String(s) => serde_json::from_str::<Value>(s).ok(),
        Value::Object(_) => Some(v.clone()),
        _ => None,
    });
    let haystacks = [
        message.get("url"),
        body.as_ref().and_then(|b| b.get("resourceLink")),
    ];
    for text in haystacks.into_iter().flatten().filter_map(Value::as_str) {
        let text = text.replace("\\/", "/");
        for (i, _) in text.match_indices("/conversations/") {
            let rest = &text[i + "/conversations/".len()..];
            let id: String = rest
                .chars()
                .take_while(|c| !matches!(c, '/' | '"' | '?' | '#' | '\\' | ' ' | ',' | '}'))
                .collect();
            let id = url::form_urlencoded::parse(format!("x={id}").as_bytes())
                .next()
                .map(|(_, v)| v.into_owned())
                .unwrap_or(id);
            if id.starts_with("19:") || id.starts_with("48:") {
                return Some(id);
            }
        }
    }
    None
}

/// Nome de um evento socket.io (tipo 5), ex.: `trouter.connected`.
pub(crate) fn event_name(data: &str) -> Option<String> {
    let v: Map<String, Value> = serde_json::from_str(data).ok()?;
    v.get("name").and_then(Value::as_str).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connect_body() -> Vec<u8> {
        json!({
            "socketio": "https://pub-ent-brso-07-t.trouter.teams.microsoft.com:443/",
            "surl": "https://pub-ent-brso-07-t.trouter.teams.microsoft.com:443/v4/f/abc/",
            "ccid": "cc1",
            "connectparams": {"sr": "x", "issuer": "prod-2", "sp": "connect", "se": 1731600000000u64, "st": 1731500000000u64, "sig": "SEGREDO=="}
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn connect_op_is_a_trouter_post_without_credential() {
        let o = connect_op("ep-1");
        assert_eq!(o.resource, Resource::Trouter);
        assert_eq!(
            o.request.url,
            "https://go.trouter.teams.microsoft.com/v4/a?epid=ep-1"
        );
        assert!(o
            .request
            .headers
            .iter()
            .all(|(k, _)| !k.eq_ignore_ascii_case("x-skypetoken")));
    }

    #[test]
    fn parses_connect_info_and_hides_signature_from_debug() {
        let info = parse_connect(&connect_body()).unwrap();
        assert_eq!(
            info.socketio,
            "https://pub-ent-brso-07-t.trouter.teams.microsoft.com:443/"
        );
        assert_eq!(info.ccid.as_deref(), Some("cc1"));
        assert!(!format!("{info:?}").contains("SEGREDO"));
    }

    #[test]
    fn connect_rejects_foreign_socket_host_or_missing_fields() {
        let foreign =
            json!({"socketio": "https://evil.example/", "surl": "x", "connectparams": {}});
        assert!(
            matches!(parse_connect(foreign.to_string().as_bytes()), Err(TeamsError::Malformed(m)) if m.contains("fora de"))
        );
        let partial = json!({"socketio": "https://a.trouter.teams.microsoft.com/"});
        assert!(
            matches!(parse_connect(partial.to_string().as_bytes()), Err(TeamsError::Malformed(m)) if m.contains("TROUTER-CONNECT-001"))
        );
    }

    #[test]
    fn handshake_and_ws_urls_carry_params_on_the_validated_node() {
        let info = parse_connect(&connect_body()).unwrap();
        let hs = handshake_op(&info, "ep-1");
        assert!(hs.request.url.starts_with(
            "https://pub-ent-brso-07-t.trouter.teams.microsoft.com:443/socket.io/1/?v=v4&"
        ));
        assert!(
            hs.request.url.contains("sig=SEGREDO%3D%3D") && hs.request.url.contains("epid=ep-1")
        );
        assert!(hs.request.url.contains("auth=true") && hs.request.url.contains("ccid=cc1"));
        let ws = ws_url(&info, "SID9", "ep-1");
        assert!(ws.starts_with("wss://pub-ent-brso-07-t.trouter.teams.microsoft.com:443/socket.io/1/websocket/SID9?v=v4&"));
    }

    /// Achado do Checkpoint D: sem `con_num` o handshake dá HTTP 400.
    #[test]
    fn con_num_goes_on_handshake_and_websocket_query() {
        let mut info = parse_connect(&connect_body()).unwrap();
        info.con_num = Some("1731600000000_1".into());
        assert!(handshake_op(&info, "ep")
            .request
            .url
            .contains("con_num=1731600000000_1"));
        assert!(ws_url(&info, "S", "ep").contains("con_num=1731600000000_1"));
    }

    /// Achado do Checkpoint D: o registrador atual vem em `registrarUrl` (v3).
    #[test]
    fn registrar_url_from_trouter_wins_over_the_legacy_default() {
        let mut body: Value = serde_json::from_slice(&connect_body()).unwrap();
        body["registrarUrl"] =
            "https://communications.svc.cloud.microsoft/registrar/noam/v3/registrations".into();
        let info = parse_connect(body.to_string().as_bytes()).unwrap();
        assert_eq!(
            registrar_op(&info, "ep").request.url,
            "https://communications.svc.cloud.microsoft/registrar/noam/v3/registrations"
        );
    }

    #[test]
    fn registrar_registers_the_surl_for_this_endpoint() {
        let info = parse_connect(&connect_body()).unwrap();
        let o = registrar_op(&info, "ep-1");
        assert_eq!(
            o.request.url,
            "https://teams.microsoft.com/registrar/prod/V2/registrations"
        );
        let body: Value = serde_json::from_slice(o.request.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["registrationId"], "ep-1");
        assert_eq!(body["transports"]["TROUTER"][0]["path"], info.surl);
    }

    #[test]
    fn every_message_gets_a_200_response_with_its_id() {
        let msg = json!({"id": 1234, "method": "POST", "url": "/v4/f/abc/messaging", "body": "…"});
        assert_eq!(
            response_for(&msg).unwrap(),
            r#"3:::{"body":"","headers":{},"id":1234,"status":200}"#
        );
        assert_eq!(response_for(&json!({"x": 1})), None);
    }

    #[test]
    fn ignores_conversation_paths_in_message_content() {
        let msg = json!({"body": json!({"content": "See /conversations/19:fake@thread.v2/messages/1"}).to_string()});
        assert_eq!(conversation_of(&msg), None);
    }

    #[test]
    fn extracts_only_the_conversation_id() {
        let in_body = json!({"id": 1, "url": "/v4/f/a/messaging",
            "body": r#"{"resourceLink":"https:\/\/amer.ng.msg.teams.microsoft.com\/v1\/users\/ME\/conversations\/19:abc@thread.v2\/messages\/1731","content":"texto privado"}"#});
        assert_eq!(
            conversation_of(&in_body).as_deref(),
            Some("19:abc@thread.v2")
        );

        let encoded =
            json!({"url": "/v1/users/ME/conversations/19%3Ameeting_x%40thread.v2/properties"});
        assert_eq!(
            conversation_of(&encoded).as_deref(),
            Some("19:meeting_x@thread.v2")
        );

        let compressed = json!({"id": 2, "body": "H4sIAAAAAAAA/6tWKkktLlGyUlAqS8wpTVWqBQAm"});
        assert_eq!(
            conversation_of(&compressed),
            None,
            "sem id legível: invalidação genérica"
        );
    }

    #[test]
    fn diagnostics_show_field_names_never_values() {
        let shape = shape(&connect_body());
        assert!(shape.contains(&"socketio".to_owned()));
        assert!(shape.contains(&"connectparams.sig:str".to_owned()));
        assert!(!format!("{shape:?}").contains("SEGREDO"));
    }

    #[test]
    fn event_names() {
        assert_eq!(
            event_name(r#"{"name":"trouter.connected","args":[]}"#).as_deref(),
            Some("trouter.connected")
        );
        assert_eq!(event_name("x"), None);
    }
}
