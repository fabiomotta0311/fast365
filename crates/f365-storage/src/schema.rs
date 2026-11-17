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
pub const VERSION: i64 = 1;

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
