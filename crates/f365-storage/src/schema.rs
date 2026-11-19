//! Esquema do cache local (Fase 2, #14).
//!
//! Um esquema = um `PRAGMA user_version`. Todo o DDL mora aqui para o diff de esquema ser revisável
//! num lugar só; nenhuma consulta fica neste módulo.
//!
//! Duas escolhas que não são óbvias:
//! - **`messages_fts` é external-content** (`content='messages'`): o índice não duplica o corpus e
//!   não há sincronização de índice para esquecer. Os três triggers mantêm o FTS fiel a `messages`.
//! - **`messages.client_message_id` tem índice**: é por ele que o motor reconcilia um envio cuja
//!   resposta se perdeu (`SEND-RECONCILE-001`) — a busca tem de ser barata, não um scan.

/// Versão do esquema. Subir junto com um bloco novo em `Store::migrate`.
pub const VERSION: i64 = 3;

/// DDL da versão 1. Idempotente (`IF NOT EXISTS`) para poder rodar em bancos já criados.
pub const DDL_V1: &str = r#"
CREATE TABLE IF NOT EXISTS conversations (
    id               TEXT PRIMARY KEY,
    title            TEXT NOT NULL,
    kind             TEXT NOT NULL,
    last_activity_ms INTEGER,
    updated_ms       INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS conversations_by_activity
    ON conversations(last_activity_ms DESC);

CREATE TABLE IF NOT EXISTS messages (
    id                TEXT PRIMARY KEY,
    conversation_id   TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    client_message_id TEXT,
    sender            TEXT,
    body              TEXT NOT NULL,
    message_type      TEXT NOT NULL,
    created_ms        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS messages_by_conversation
    ON messages(conversation_id, created_ms DESC);
CREATE INDEX IF NOT EXISTS messages_by_client_id
    ON messages(client_message_id);

CREATE TABLE IF NOT EXISTS accounts (
    upn          TEXT PRIMARY KEY,
    tenant_id    TEXT,
    client_id    TEXT,
    captured_ms  INTEGER,
    updated_ms   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sync_state (
    scope      TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS drafts (
    conversation_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
    body            TEXT NOT NULL,
    updated_ms      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS attachments_cache (
    id         TEXT PRIMARY KEY,
    message_id TEXT,
    path       TEXT NOT NULL,
    size_bytes INTEGER,
    updated_ms INTEGER NOT NULL
);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    body,
    content='messages',
    content_rowid='rowid'
);

CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, body) VALUES (new.rowid, new.body);
END;

CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, body) VALUES ('delete', old.rowid, old.body);
END;

CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, body) VALUES ('delete', old.rowid, old.body);
    INSERT INTO messages_fts(rowid, body) VALUES (new.rowid, new.body);
END;
"#;

/// DDL da versão 2 (#22): fatos × interpretações (`docs/ARQUITETURA.md` §6).
///
/// - **Fato:** `messages` guarda a revisão corrente; a anterior vai para `message_revisions` quando
///   o corpo muda. O original nunca é perdido nem substituído por conteúdo derivado.
/// - **Derivado:** `derivatives` é imutável (triggers) e referencia o fato por
///   `derivative_sources(message_id, message_revision)`. Não há FK para `messages`: se a mensagem
///   some ou muda, o derivado fica **obsoleto** em vez de ser apagado em silêncio.
/// - Aprovar fixa o texto exato (`approved_ms` só sai de `NULL` uma vez).
///
/// Não é idempotente (`ALTER TABLE`): só roda quando `user_version < 2`.
pub const DDL_V2: &str = r#"
ALTER TABLE messages ADD COLUMN revision INTEGER NOT NULL DEFAULT 1;

CREATE TABLE IF NOT EXISTS message_revisions (
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    revision   INTEGER NOT NULL,
    body       TEXT NOT NULL,
    PRIMARY KEY (message_id, revision)
);

CREATE TABLE IF NOT EXISTS derivatives (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    kind        TEXT NOT NULL,
    model       TEXT NOT NULL,
    lang        TEXT,
    body        TEXT NOT NULL,
    created_ms  INTEGER NOT NULL,
    approved_ms INTEGER
);

CREATE TABLE IF NOT EXISTS derivative_sources (
    derivative_id    INTEGER NOT NULL REFERENCES derivatives(id) ON DELETE CASCADE,
    message_id       TEXT NOT NULL,
    message_revision INTEGER NOT NULL,
    PRIMARY KEY (derivative_id, message_id)
);
CREATE INDEX IF NOT EXISTS derivative_sources_by_message
    ON derivative_sources(message_id);

CREATE TRIGGER IF NOT EXISTS derivatives_immutable
BEFORE UPDATE OF kind, model, lang, body, created_ms ON derivatives
BEGIN
    SELECT RAISE(ABORT, 'derivado é imutável: grave uma nova versão');
END;

CREATE TRIGGER IF NOT EXISTS derivatives_approve_once
BEFORE UPDATE OF approved_ms ON derivatives
WHEN old.approved_ms IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'derivado já aprovado');
END;
"#;

/// DDL da versão 3 (#23, calendário): eventos como **fato** do cache, lidos sem rede.
///
/// Tempo em época Unix (ms, UTC); fuso e recorrência são problema de quem escreve (o provider
/// expande as ocorrências), então a tela só consulta uma janela `[from, to)`. `join_url` fica no
/// cache para a UI nunca receber URL de reunião de fora do banco (o clique passa pelo id).
pub const DDL_V3: &str = r#"
CREATE TABLE IF NOT EXISTS calendar_events (
    id         TEXT PRIMARY KEY,
    subject    TEXT NOT NULL,
    start_ms   INTEGER NOT NULL,
    end_ms     INTEGER NOT NULL CHECK (end_ms >= start_ms),
    all_day    INTEGER NOT NULL DEFAULT 0,
    location   TEXT,
    organizer  TEXT,
    join_url   TEXT,
    updated_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS calendar_events_by_start ON calendar_events(start_ms);
"#;
