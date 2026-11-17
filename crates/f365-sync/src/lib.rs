//! `f365-sync` — fronteira do motor de sincronização reproduzível (`docs/ARQUITETURA.md`, Fase 2).
//!
//! O motor é uma FSM explícita e **sem I/O**: recebe acontecimentos ([`Event`]) e devolve decisões
//! ([`Decision`]). Quem executa as decisões é o *driver* (CLI/UI), com o [`AuthenticatedExecutor`] e
//! o [`Clock`] injetados — por isso o motor não depende de rede, relógio de parede, WebView nem UI,
//! e qualquer cenário (suspensão, evento duplicado/fora de ordem, resposta perdida) vira teste
//! determinístico. Fronteira garantida por: `Cargo.toml` sem dependências de I/O (teste
//! `boundary`) e `clippy.toml` proibindo ler o relógio/dormir.
//!
//! Fase 2 (#15): as transições estão em [`machine`] — FSM síncrona, sem relógio (`now` entra por
//! parâmetro). Invariante de envio (`SEND-RECONCILE-001`, UNVERIFIED): resposta perdida
//! **reconcilia** por `clientmessageid`; sem evidência o estado é [`SendState::Uncertain`] — nunca
//! "entregue".

pub mod machine;

pub use f365_auth::{AuthenticatedExecutor, Clock, ManualClock, SystemClock};
pub use machine::{
    Config, Connectivity, ConversationState, Realtime, SendState, SyncMachine, UncertainReason,
};

use std::time::Instant;

/// Identificador de conversa/canal no provider.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConversationId(pub String);

/// `clientmessageid` gerado localmente antes do envio — chave de reconciliação.
///
/// `SEND-RECONCILE-001` (UNVERIFIED): o servidor devolve o `clientmessageid` na mensagem criada, o
/// que permite reconhecer um envio cuja resposta se perdeu em vez de reenviá-lo.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientMessageId(pub String);

/// Correlaciona uma [`Decision`] que pede uma operação com o [`Event::OperationFinished`] que
/// traz o resultado dela.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpId(pub u64);

/// Resultado de uma operação, como o driver o observou.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Succeeded,
    Failed {
        retryable: bool,
    },
    /// A resposta não chegou (timeout, conexão caiu): o efeito no servidor é **desconhecido**.
    /// Para envio, o motor reconcilia ou marca como incerto — nunca presume entrega única.
    Lost,
}

/// Acontecimento que alimenta o motor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// O usuário pediu um envio (já gravado como pendente).
    SendRequested {
        conversation: ConversationId,
        client_id: ClientMessageId,
    },
    /// O canal de tempo real sinalizou mudança. Invalidação, não dado: o conteúdo vem via REST.
    Invalidated { conversation: ConversationId },
    /// O conteúdo buscado mostrou esta `clientmessageid` no servidor: prova de entrega.
    ///
    /// É o outro lado de `SEND-RECONCILE-001`: sem esse fato o motor **não** conclui entrega —
    /// [`Outcome::Succeeded`] só prova que o request foi aceito.
    MessageObserved {
        conversation: ConversationId,
        client_id: ClientMessageId,
    },
    /// Terminou uma operação pedida por uma [`Decision`].
    OperationFinished { op: OpId, outcome: Outcome },
    /// A conectividade caiu.
    ConnectionLost,
    /// A conectividade voltou.
    ConnectionRestored,
    /// Venceu o prazo pedido em [`Decision::ScheduleWake`].
    WakeUp,
}

/// O que o driver deve fazer. Operações levam um [`OpId`] para o resultado voltar correlacionado.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Gravar o estado da conversa no armazenamento local (fonte da verdade).
    Persist { conversation: ConversationId },
    /// Buscar conteúdo/histórico fresco via REST.
    FetchHistory {
        op: OpId,
        conversation: ConversationId,
    },
    /// Tentar (ou retentar) um envio.
    AttemptSend {
        op: OpId,
        conversation: ConversationId,
        client_id: ClientMessageId,
    },
    /// Reabrir o canal de tempo real.
    Reconnect { op: OpId },
    /// Entregar [`Event::WakeUp`] ao motor no instante `at` (no tempo do [`Clock`] do driver).
    ScheduleWake { at: Instant },
    /// Avisar a UI que a conversa mudou.
    Publish { conversation: ConversationId },
}

#[cfg(test)]
mod boundary {
    /// Crates que dão ao motor acesso direto a I/O, relógio assíncrono, WebView, UI ou storage.
    const FORBIDDEN: &[&str] = &[
        "reqwest",
        "hyper",
        "tokio",
        "tokio-tungstenite",
        "wry",
        "tao",
        "tauri",
        "rusqlite",
        "keyring",
    ];

    #[test]
    fn manifest_has_no_io_dependencies() {
        let manifest = include_str!("../Cargo.toml");
        let mut in_deps = false;
        for line in manifest.lines().map(str::trim) {
            if line.starts_with('[') {
                in_deps = line.ends_with("dependencies]") && !line.contains("dev-dependencies");
                continue;
            }
            if !in_deps || line.is_empty() || line.starts_with('#') {
                continue;
            }
            let name = line
                .split(['=', '.', ' '])
                .next()
                .unwrap_or_default()
                .trim_matches('"');
            assert!(
                !FORBIDDEN.contains(&name),
                "f365-sync não pode depender de `{name}` — o motor é sem I/O (ver lib.rs)"
            );
        }
    }
}
