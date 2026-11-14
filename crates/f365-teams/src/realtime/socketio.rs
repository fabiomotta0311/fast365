//! Framing socket.io **v1** (o que o Trouter fala): `tipo:id:endpoint[:dados]`.
//!
//! `TROUTER-SIO-001` (VERIFIED no Checkpoint D, #11): tipos 0 desconectar, 1 conectar, 2 heartbeat,
//! 3 mensagem, 4 json, 5 evento, 6 ack, 7 erro, 8 noop; `id` com `+` pede ack.
//! Handshake HTTP devolve `sessionid:heartbeat:closetimeout:transportes`.

use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Frame {
    Disconnect,
    Connect,
    Heartbeat,
    /// Mensagem (tipo 3). No Trouter, `data` é um JSON de "request" que precisa de resposta.
    Message {
        id: Option<String>,
        data: String,
    },
    Json {
        id: Option<String>,
        data: String,
    },
    /// Evento nomeado (tipo 5): `{"name": …, "args": […]}`.
    Event {
        id: Option<String>,
        data: String,
    },
    Ack {
        data: String,
    },
    Error {
        data: String,
    },
    Noop,
}

pub(crate) fn parse(raw: &str) -> Option<Frame> {
    let mut parts = raw.splitn(4, ':');
    let kind = parts.next()?;
    let id = parts.next().filter(|s| !s.is_empty()).map(str::to_owned);
    let _endpoint = parts.next();
    let data = parts.next().unwrap_or_default().to_owned();
    Some(match kind {
        "0" => Frame::Disconnect,
        "1" => Frame::Connect,
        "2" => Frame::Heartbeat,
        "3" => Frame::Message { id, data },
        "4" => Frame::Json { id, data },
        "5" => Frame::Event { id, data },
        "6" => Frame::Ack { data },
        "7" => Frame::Error { data },
        "8" => Frame::Noop,
        _ => return None,
    })
}

pub(crate) fn encode(frame: &Frame) -> String {
    let with = |kind: u8, id: &Option<String>, data: &str| {
        format!("{kind}:{}::{data}", id.as_deref().unwrap_or(""))
    };
    match frame {
        Frame::Disconnect => "0::".into(),
        Frame::Connect => "1::".into(),
        Frame::Heartbeat => "2::".into(),
        Frame::Message { id, data } => with(3, id, data),
        Frame::Json { id, data } => with(4, id, data),
        Frame::Event { id, data } => with(5, id, data),
        Frame::Ack { data } => format!("6:::{data}"),
        Frame::Error { data } => format!("7:::{data}"),
        Frame::Noop => "8::".into(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Handshake {
    pub session_id: String,
    pub heartbeat: Option<Duration>,
    pub transports: Vec<String>,
}

pub(crate) fn parse_handshake(body: &str) -> Option<Handshake> {
    let mut parts = body.trim().splitn(4, ':');
    let session_id = parts
        .next()
        .filter(|s| !s.is_empty() && !s.contains('/'))?
        .to_owned();
    let heartbeat = parts
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(Duration::from_secs);
    let _close = parts.next();
    let transports = parts
        .next()
        .map(|t| t.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    Some(Handshake {
        session_id,
        heartbeat,
        transports,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_data_may_contain_colons() {
        let f = Frame::Message {
            id: None,
            data: r#"{"url":"https://x:443/a","n":1}"#.into(),
        };
        let raw = encode(&f);
        assert_eq!(raw, r#"3:::{"url":"https://x:443/a","n":1}"#);
        assert_eq!(parse(&raw), Some(f));
        assert_eq!(parse("2::"), Some(Frame::Heartbeat));
        assert_eq!(parse("1::"), Some(Frame::Connect));
        assert_eq!(
            parse("2"),
            Some(Frame::Heartbeat),
            "servidor às vezes manda sem os dois-pontos"
        );
    }

    #[test]
    fn event_with_ack_request_keeps_the_plus() {
        let f = parse(r#"5:3+::{"name":"trouter.connected"}"#).unwrap();
        assert_eq!(
            f,
            Frame::Event {
                id: Some("3+".into()),
                data: r#"{"name":"trouter.connected"}"#.into()
            }
        );
        assert_eq!(encode(&f), r#"5:3+::{"name":"trouter.connected"}"#);
    }

    #[test]
    fn unknown_kind_is_none() {
        assert_eq!(parse("9::x"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn handshake_body() {
        let h = parse_handshake("abcDEF123:40:60:websocket,xhr-polling\n").unwrap();
        assert_eq!(h.session_id, "abcDEF123");
        assert_eq!(h.heartbeat, Some(Duration::from_secs(40)));
        assert_eq!(h.transports, ["websocket", "xhr-polling"]);
        assert_eq!(parse_handshake(""), None);
        assert_eq!(
            parse_handshake("../x:1:2:ws"),
            None,
            "session id não pode virar caminho"
        );
    }
}
