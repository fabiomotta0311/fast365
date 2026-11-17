//! `f365-storage` — cache local como **fonte da verdade** (offline-first, render-before-network).
//! Fase 2, #14.
//!
//! SQLite em modo **WAL**, busca **FTS5** no mesmo arquivo e drafts que sobrevivem a restart. Este
//! crate é a fronteira **oposta** à do motor: aqui o I/O é o trabalho, e `f365-sync` nunca depende
//! dele — quem costura os dois é o driver.
//!
//! Nada aqui lê relógio: carimbos de tempo entram por parâmetro (`*_ms`, época Unix em ms), então as
//! consultas e os testes são determinísticos.
//!
//! O que este crate **não** faz: rede, token, WebView ou decisão de sincronização.

pub mod schema;

use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

/// Erro de armazenamento. Não vaza SQL nem dados de mensagem na mensagem de erro apresentada.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("entrada inválida: {0}")]
    Invalid(&'static str),
}

pub type Result<T> = std::result::Result<T, StorageError>;

/// Conversa ou canal, como o cache local a guarda.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub kind: String,
    /// Última atividade conhecida (ordena a lista); `None` quando o servidor não informou.
    pub last_activity_ms: Option<i64>,
    pub updated_ms: i64,
}

impl Conversation {
    pub fn new(id: impl Into<String>, title: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            kind: kind.into(),
            last_activity_ms: None,
            updated_ms: 0,
        }
    }

    pub fn at(mut self, last_activity_ms: i64) -> Self {
        self.last_activity_ms = Some(last_activity_ms);
        self
    }
}

/// Mensagem persistida. `client_message_id` é a chave de reconciliação do motor
/// (`SEND-RECONCILE-001`): é por ela que se reconhece um envio cuja resposta se perdeu.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Message {
    pub id: String,
    pub conversation_id: String,
    pub client_message_id: Option<String>,
    pub sender: Option<String>,
    pub body: String,
    pub message_type: String,
    pub created_ms: i64,
}

impl Message {
    pub fn text(
        conversation_id: impl Into<String>,
        id: impl Into<String>,
        body: impl Into<String>,
        created_ms: i64,
    ) -> Self {
        Self {
            id: id.into(),
            conversation_id: conversation_id.into(),
            client_message_id: None,
            sender: None,
            body: body.into(),
            message_type: "Text".to_string(),
            created_ms,
        }
    }

    pub fn with_client_id(mut self, client_message_id: impl Into<String>) -> Self {
        self.client_message_id = Some(client_message_id.into());
        self
    }
}

/// Cache local. Um `Store` = uma conexão SQLite já migrada.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Abre (ou cria) o banco, liga o WAL/migrações e devolve o cache pronto.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    /// Banco efêmero para teste e para o modo "sem disco".
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // WAL só vale para arquivo; em memória o SQLite responde "memory" e não é erro.
        let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;\nPRAGMA synchronous = NORMAL;\nPRAGMA busy_timeout = 5000;",
        )?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Aplica o DDL quando o banco está atrás da versão do código.
    fn migrate(&self) -> Result<()> {
        let current: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if current >= schema::VERSION {
            return Ok(());
        }
        self.conn.execute_batch(schema::DDL_V1)?;
        self.conn
            .pragma_update(None, "user_version", schema::VERSION)?;
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    /// Modo de journal em uso (`wal` num banco de arquivo). Útil para diagnóstico e teste.
    pub fn journal_mode(&self) -> Result<String> {
        Ok(self
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))?)
    }

    // ---- conversas ----

    /// Insere ou atualiza. `last_activity_ms` ausente **não** apaga a atividade já conhecida.
    pub fn upsert_conversation(&self, conversation: &Conversation) -> Result<()> {
        self.conn.execute(
            "INSERT INTO conversations (id, title, kind, last_activity_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
                 title = excluded.title,
                 kind = excluded.kind,
                 last_activity_ms = COALESCE(excluded.last_activity_ms, conversations.last_activity_ms),
                 updated_ms = excluded.updated_ms",
            params![
                conversation.id,
                conversation.title,
                conversation.kind,
                conversation.last_activity_ms,
                conversation.updated_ms,
            ],
        )?;
        Ok(())
    }

    /// Lista por atividade recente. A UI abre **do cache**, sem rede.
    pub fn list_conversations(&self, limit: usize) -> Result<Vec<Conversation>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, kind, last_activity_ms, updated_ms
             FROM conversations
             ORDER BY COALESCE(last_activity_ms, 0) DESC, id
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(Conversation {
                id: row.get(0)?,
                title: row.get(1)?,
                kind: row.get(2)?,
                last_activity_ms: row.get(3)?,
                updated_ms: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Remove a conversa e, por cascata, suas mensagens (e o índice FTS, via trigger).
    pub fn delete_conversation(&self, id: &str) -> Result<usize> {
        Ok(self
            .conn
            .execute("DELETE FROM conversations WHERE id = ?1", params![id])?)
    }

    // ---- mensagens ----

    /// Grava um lote de forma atômica e idempotente (chave = `id`).
    ///
    /// Reaplicar o mesmo lote não duplica nada — é o que permite repetir uma sincronização
    /// interrompida sem inventar mensagens.
    pub fn upsert_messages(&mut self, messages: &[Message]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut written = 0;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO messages
                     (id, conversation_id, client_message_id, sender, body, message_type, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET
                     body = excluded.body,
                     message_type = excluded.message_type,
                     client_message_id = COALESCE(excluded.client_message_id, messages.client_message_id),
                     sender = COALESCE(excluded.sender, messages.sender)",
            )?;
            for message in messages {
                written += stmt.execute(params![
                    message.id,
                    message.conversation_id,
                    message.client_message_id,
                    message.sender,
                    message.body,
                    message.message_type,
                    message.created_ms,
                ])?;
            }
        }
        tx.commit()?;
        Ok(written)
    }

    /// Página de histórico, da mais nova para a mais antiga (keyset por `created_ms`).
    pub fn page_messages(
        &self,
        conversation_id: &str,
        before_ms: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, conversation_id, client_message_id, sender, body, message_type, created_ms
             FROM messages
             WHERE conversation_id = ?1 AND (?2 IS NULL OR created_ms < ?2)
             ORDER BY created_ms DESC, id
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![conversation_id, before_ms, limit as i64], |row| {
            Ok(Message {
                id: row.get(0)?,
                conversation_id: row.get(1)?,
                client_message_id: row.get(2)?,
                sender: row.get(3)?,
                body: row.get(4)?,
                message_type: row.get(5)?,
                created_ms: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Acha a mensagem por `clientmessageid` — o lado local de `SEND-RECONCILE-001`.
    pub fn find_by_client_message_id(&self, client_message_id: &str) -> Result<Option<Message>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, conversation_id, client_message_id, sender, body, message_type, created_ms
             FROM messages WHERE client_message_id = ?1 LIMIT 1",
        )?;
        let found = stmt
            .query_row(params![client_message_id], |row| {
                Ok(Message {
                    id: row.get(0)?,
                    conversation_id: row.get(1)?,
                    client_message_id: row.get(2)?,
                    sender: row.get(3)?,
                    body: row.get(4)?,
                    message_type: row.get(5)?,
                    created_ms: row.get(6)?,
                })
            })
            .optional()?;
        Ok(found)
    }

    // ---- busca ----

    /// Busca no texto local (FTS5), da mensagem mais nova para a mais antiga.
    ///
    /// O texto do usuário entra como **frase**: sintaxe de FTS (`"`, `*`, `AND`) não vaza para o
    /// parser, então entrada estranha não vira erro. Ranking bm25 fica para a Fase 4 (§4).
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Message>> {
        let phrase = format!("\"{}\"", query.replace('"', " "));
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.conversation_id, m.client_message_id, m.sender, m.body,
                    m.message_type, m.created_ms
             FROM messages m
             WHERE m.rowid IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?1)
             ORDER BY m.created_ms DESC, m.id
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![phrase, limit as i64], |row| {
            Ok(Message {
                id: row.get(0)?,
                conversation_id: row.get(1)?,
                client_message_id: row.get(2)?,
                sender: row.get(3)?,
                body: row.get(4)?,
                message_type: row.get(5)?,
                created_ms: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // ---- estado de sincronização ----

    /// `lastSyncToken`/cursor por escopo (conversa ou "tudo").
    pub fn sync_token(&self, scope: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM sync_state WHERE scope = ?1",
                params![scope],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn set_sync_token(&self, scope: &str, value: &str, now_ms: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sync_state (scope, value, updated_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT(scope) DO UPDATE SET value = excluded.value, updated_ms = excluded.updated_ms",
            params![scope, value, now_ms],
        )?;
        Ok(())
    }

    // ---- drafts ----

    /// Draft por conversa. Sobrevive a restart porque mora no banco, não na memória da UI.
    pub fn put_draft(&self, conversation_id: &str, body: &str, now_ms: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO drafts (conversation_id, body, updated_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT(conversation_id) DO UPDATE SET body = excluded.body, updated_ms = excluded.updated_ms",
            params![conversation_id, body, now_ms],
        )?;
        Ok(())
    }

    pub fn draft(&self, conversation_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT body FROM drafts WHERE conversation_id = ?1",
                params![conversation_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn delete_draft(&self, conversation_id: &str) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM drafts WHERE conversation_id = ?1",
            params![conversation_id],
        )?)
    }

    // ---- anexos ----

    /// Só o **caminho** do arquivo em cache entra no banco; o conteúdo vive no disco.
    pub fn cache_attachment(
        &self,
        id: &str,
        message_id: Option<&str>,
        path: &str,
        size_bytes: Option<i64>,
        now_ms: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO attachments_cache (id, message_id, path, size_bytes, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
                 message_id = COALESCE(excluded.message_id, attachments_cache.message_id),
                 path = excluded.path,
                 size_bytes = excluded.size_bytes,
                 updated_ms = excluded.updated_ms",
            params![id, message_id, path, size_bytes, now_ms],
        )?;
        Ok(())
    }

    pub fn attachment_path(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT path FROM attachments_cache WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?)
    }

    // ---- contas ----

    /// Registro da conta pareada. **Sem token**: credencial é assunto do keyring (`AGENTS.md`).
    pub fn upsert_account(
        &self,
        upn: &str,
        tenant_id: Option<&str>,
        client_id: Option<&str>,
        captured_ms: Option<i64>,
        now_ms: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO accounts (upn, tenant_id, client_id, captured_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(upn) DO UPDATE SET
                 tenant_id = COALESCE(excluded.tenant_id, accounts.tenant_id),
                 client_id = COALESCE(excluded.client_id, accounts.client_id),
                 captured_ms = COALESCE(excluded.captured_ms, accounts.captured_ms),
                 updated_ms = excluded.updated_ms",
            params![upn, tenant_id, client_id, captured_ms, now_ms],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Banco de arquivo com limpeza garantida (inclusive dos `-wal`/`-shm`).
    struct TempDb {
        path: PathBuf,
    }

    impl TempDb {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("f365-storage-{}-{name}.db", std::process::id()));
            Self::clean(&path);
            Self { path }
        }

        fn clean(path: &Path) {
            let _ = std::fs::remove_file(path);
            for suffix in ["-wal", "-shm"] {
                let mut side = path.as_os_str().to_owned();
                side.push(suffix);
                let _ = std::fs::remove_file(PathBuf::from(side));
            }
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            Self::clean(&self.path);
        }
    }

    fn store() -> Store {
        Store::open_in_memory().expect("abre o banco em memória")
    }

    fn seed(store: &Store, conversation_id: &str) {
        store
            .upsert_conversation(&Conversation::new(conversation_id, "Time", "chat"))
            .expect("conversa");
    }

    #[test]
    fn opening_applies_the_current_schema_version() {
        let store = store();
        assert_eq!(store.schema_version().expect("versão"), schema::VERSION);
    }

    #[test]
    fn file_database_runs_in_wal_mode() {
        let db = TempDb::new("wal");
        let store = Store::open(&db.path).expect("abre");
        assert_eq!(store.journal_mode().expect("modo").to_lowercase(), "wal");
    }

    #[test]
    fn conversation_upsert_is_idempotent_and_keeps_known_activity() {
        let store = store();
        let mut conversation = Conversation::new("19:a@thread.v2", "Time", "chat");
        conversation.last_activity_ms = Some(1_000);
        conversation.updated_ms = 1_000;
        store.upsert_conversation(&conversation).expect("primeira");

        // Reaplicar sem atividade não pode apagar a que já se sabia.
        let mut again = conversation.clone();
        again.title = "Time (renomeado)".into();
        again.last_activity_ms = None;
        again.updated_ms = 2_000;
        store.upsert_conversation(&again).expect("segunda");

        let listed = store.list_conversations(10).expect("lista");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "Time (renomeado)");
        assert_eq!(listed[0].last_activity_ms, Some(1_000));
    }

    #[test]
    fn conversations_are_listed_by_recent_activity() {
        let store = store();
        for (id, activity) in [("a", 10), ("b", 30), ("c", 20)] {
            let mut conversation = Conversation::new(id, id, "chat");
            conversation.last_activity_ms = Some(activity);
            store.upsert_conversation(&conversation).expect("grava");
        }

        let ids: Vec<String> = store
            .list_conversations(10)
            .expect("lista")
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, ["b", "c", "a"]);
    }

    #[test]
    fn messages_are_written_atomically_and_idempotently() {
        let mut store = store();
        seed(&store, "19:a@thread.v2");
        let message = Message::text("19:a@thread.v2", "m1", "primeira", 1_000)
            .with_client_id("1700000000000000001");

        let one = std::slice::from_ref(&message);
        assert_eq!(store.upsert_messages(one).expect("lote"), 1);
        assert_eq!(store.upsert_messages(one).expect("repetido"), 1);

        let page = store
            .page_messages("19:a@thread.v2", None, 10)
            .expect("página");
        assert_eq!(page.len(), 1, "reaplicar o lote não duplica");
        assert_eq!(
            page[0].client_message_id.as_deref(),
            Some("1700000000000000001")
        );
    }

    #[test]
    fn history_is_paginated_by_keyset() {
        let mut store = store();
        seed(&store, "19:a@thread.v2");
        let batch: Vec<Message> = (1..=5)
            .map(|i| {
                Message::text(
                    "19:a@thread.v2",
                    format!("m{i}"),
                    format!("corpo {i}"),
                    i * 10,
                )
            })
            .collect();
        store.upsert_messages(&batch).expect("lote");

        let first = store.page_messages("19:a@thread.v2", None, 2).expect("p1");
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].id, "m5");

        let next = store
            .page_messages("19:a@thread.v2", Some(first[1].created_ms), 2)
            .expect("p2");
        assert_eq!(
            next.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["m3", "m2"]
        );
    }

    #[test]
    fn search_works_on_the_local_file_without_network() {
        let mut store = store();
        seed(&store, "19:a@thread.v2");
        store
            .upsert_messages(&[
                Message::text("19:a@thread.v2", "m1", "relatório trimestral fechado", 10),
                Message::text("19:a@thread.v2", "m2", "outro assunto", 20),
            ])
            .expect("lote");

        let found = store.search("relatório", 10).expect("busca");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "m1");
    }

    #[test]
    fn search_follows_message_updates_and_deletes() {
        let mut store = store();
        seed(&store, "19:a@thread.v2");
        store
            .upsert_messages(&[Message::text("19:a@thread.v2", "m1", "alfa", 10)])
            .expect("lote");

        store
            .upsert_messages(&[Message::text("19:a@thread.v2", "m1", "beta", 10)])
            .expect("atualiza");
        assert!(store.search("alfa", 10).expect("busca").is_empty());
        assert_eq!(store.search("beta", 10).expect("busca").len(), 1);

        store.delete_conversation("19:a@thread.v2").expect("remove");
        assert!(
            store.search("beta", 10).expect("busca").is_empty(),
            "cascata + trigger mantêm o FTS fiel"
        );
    }

    #[test]
    fn user_text_never_breaks_the_parser() {
        let store = store();
        // Sintaxe de FTS na entrada do usuário não pode virar erro de parse.
        for query in ["\"aspas", "AND OR NOT", "a*", "(", "-"] {
            store.search(query, 5).expect("consulta sempre válida");
        }
    }

    #[test]
    fn drafts_survive_reopening_the_database() {
        let db = TempDb::new("draft");
        {
            let store = Store::open(&db.path).expect("abre");
            seed(&store, "19:a@thread.v2");
            store
                .put_draft("19:a@thread.v2", "rascunho não enviado", 5_000)
                .expect("grava draft");
        }
        let reopened = Store::open(&db.path).expect("reabre");
        assert_eq!(
            reopened.draft("19:a@thread.v2").expect("lê").as_deref(),
            Some("rascunho não enviado")
        );
    }

    #[test]
    fn sync_token_roundtrips() {
        let store = store();
        assert_eq!(store.sync_token("19:a@thread.v2").expect("vazio"), None);
        store
            .set_sync_token("19:a@thread.v2", "token-1", 100)
            .expect("grava");
        store
            .set_sync_token("19:a@thread.v2", "token-2", 200)
            .expect("atualiza");
        assert_eq!(
            store.sync_token("19:a@thread.v2").expect("lê").as_deref(),
            Some("token-2")
        );
    }

    #[test]
    fn attachment_cache_keeps_only_the_path() {
        let store = store();
        store
            .cache_attachment("a1", Some("m1"), "cache/a1.png", Some(2_048), 10)
            .expect("grava");
        assert_eq!(
            store.attachment_path("a1").expect("lê").as_deref(),
            Some("cache/a1.png")
        );
    }

    #[test]
    fn message_requires_a_known_conversation() {
        let mut store = store();
        let error = store
            .upsert_messages(&[Message::text("19:desconhecida", "m1", "oi", 10)])
            .expect_err("sem conversa, a FK recusa");
        assert!(matches!(error, StorageError::Sqlite(_)));
    }

    #[test]
    fn reconciliation_lookup_finds_the_message_by_client_id() {
        let mut store = store();
        seed(&store, "19:a@thread.v2");
        store
            .upsert_messages(&[Message::text("19:a@thread.v2", "m1", "ping", 10)
                .with_client_id("1700000000000000001")])
            .expect("lote");

        let found = store
            .find_by_client_message_id("1700000000000000001")
            .expect("busca")
            .expect("achou");
        assert_eq!(found.id, "m1");
        assert!(store
            .find_by_client_message_id("inexistente")
            .expect("busca")
            .is_none());
    }
}
