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

/// Fato citado por um derivado: a mensagem **e a revisão** que o derivado interpretou.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRef {
    pub message_id: String,
    pub message_revision: i64,
}

/// Conteúdo derivado (tradução, resumo, ata, ação proposta). Nunca substitui a mensagem original.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derivative {
    pub id: i64,
    pub kind: String,
    /// Modelo/versão que produziu o texto — permite trocar de modelo sem reescrever histórico.
    pub model: String,
    pub lang: Option<String>,
    pub body: String,
    pub created_ms: i64,
    pub approved_ms: Option<i64>,
    pub sources: Vec<SourceRef>,
    /// `true` quando alguma fonte mudou de revisão ou foi apagada: não vale mais como tradução/ata
    /// daquele texto e não pode ser aprovado nem enviado.
    pub stale: bool,
}

/// Evento de calendário como o cache o guarda (já expandido pelo provider; sem recorrência).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarEvent {
    pub id: String,
    pub subject: String,
    /// Início/fim em época Unix (ms, UTC). `end_ms >= start_ms`.
    pub start_ms: i64,
    pub end_ms: i64,
    pub all_day: bool,
    pub location: Option<String>,
    pub organizer: Option<String>,
    /// Link da reunião online. Abre no navegador padrão (sem WebRTC nativo).
    pub join_url: Option<String>,
}

/// Cache local. Um `Store` = uma conexão SQLite já migrada.
pub struct Store {
    conn: Connection,
}

fn event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CalendarEvent> {
    Ok(CalendarEvent {
        id: row.get(0)?,
        subject: row.get(1)?,
        start_ms: row.get(2)?,
        end_ms: row.get(3)?,
        all_day: row.get(4)?,
        location: row.get(5)?,
        organizer: row.get(6)?,
        join_url: row.get(7)?,
    })
}

/// Escopo reservado em `sync_state` para o nome do próprio usuário.
///
/// Ids de conversa começam com `19:`/`48:`, então `__self__` não colide.
const MY_NAME_SCOPE: &str = "__self__";

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
        if current < 1 {
            self.conn.execute_batch(schema::DDL_V1)?;
        }
        if current < 2 {
            self.conn.execute_batch(schema::DDL_V2)?;
        }
        if current < 3 {
            self.conn.execute_batch(schema::DDL_V3)?;
        }
        self.conn
            .pragma_update(None, "user_version", schema::VERSION)?;
        Ok(())
    }

    /// Grava eventos (id novo insere, id existente atualiza). Fim antes do início é recusado.
    pub fn upsert_events(&mut self, events: &[CalendarEvent], now_ms: i64) -> Result<usize> {
        if events.iter().any(|event| event.end_ms < event.start_ms) {
            return Err(StorageError::Invalid("evento termina antes de começar"));
        }
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO calendar_events
                    (id, subject, start_ms, end_ms, all_day, location, organizer, join_url, updated_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(id) DO UPDATE SET
                    subject = excluded.subject, start_ms = excluded.start_ms,
                    end_ms = excluded.end_ms, all_day = excluded.all_day,
                    location = excluded.location, organizer = excluded.organizer,
                    join_url = excluded.join_url, updated_ms = excluded.updated_ms",
            )?;
            for event in events {
                stmt.execute(params![
                    event.id,
                    event.subject,
                    event.start_ms,
                    event.end_ms,
                    event.all_day,
                    event.location,
                    event.organizer,
                    event.join_url,
                    now_ms
                ])?;
            }
        }
        tx.commit()?;
        Ok(events.len())
    }

    /// Eventos que tocam a janela `[from_ms, to_ms)`, em ordem de início. Um evento que termina
    /// exatamente em `from_ms` não entra; um de duração zero entra se começar dentro da janela.
    pub fn events_between(&self, from_ms: i64, to_ms: i64) -> Result<Vec<CalendarEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, subject, start_ms, end_ms, all_day, location, organizer, join_url
             FROM calendar_events
             WHERE start_ms < ?2 AND (end_ms > ?1 OR (end_ms = start_ms AND start_ms >= ?1))
             ORDER BY start_ms, end_ms, id",
        )?;
        let rows = stmt.query_map(params![from_ms, to_ms], event_from_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Um evento pelo id (a UI pede o link da reunião por aqui, nunca manda URL).
    pub fn event(&self, id: &str) -> Result<Option<CalendarEvent>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, subject, start_ms, end_ms, all_day, location, organizer, join_url
                 FROM calendar_events WHERE id = ?1",
                params![id],
                event_from_row,
            )
            .optional()?)
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

    /// Uma conversa do cache, se existir.
    pub fn conversation(&self, id: &str) -> Result<Option<Conversation>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, kind, last_activity_ms, updated_ms FROM conversations WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], |row| {
            Ok(Conversation {
                id: row.get(0)?,
                title: row.get(1)?,
                kind: row.get(2)?,
                last_activity_ms: row.get(3)?,
                updated_ms: row.get(4)?,
            })
        })?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// Remove a conversa e, por cascata, suas mensagens (e o índice FTS, via trigger).
    pub fn delete_conversation(&self, id: &str) -> Result<usize> {
        Ok(self
            .conn
            .execute("DELETE FROM conversations WHERE id = ?1", params![id])?)
    }

    // ---- mensagens ----

    /// Garante a linha da conversa sem sobrescrever o que já se sabe dela.
    ///
    /// Uma mensagem chega antes de a conversa ser listada (ex.: invalidação de um canal que o
    /// usuário ainda não abriu); a FK não pode recusar por causa disso, e o título real não pode
    /// ser apagado por um placeholder.
    pub fn ensure_conversation(&self, id: &str, now_ms: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO conversations (id, title, kind, last_activity_ms, updated_ms)
             VALUES (?1, '', 'unknown', NULL, ?2)
             ON CONFLICT(id) DO NOTHING",
            params![id, now_ms],
        )?;
        Ok(())
    }

    /// Grava um lote de forma atômica e idempotente (chave = `id`).
    ///
    /// Reaplicar o mesmo lote não duplica nada — é o que permite repetir uma sincronização
    /// interrompida sem inventar mensagens.
    pub fn upsert_messages(&mut self, messages: &[Message]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut written = 0;
        {
            let mut previous = tx.prepare("SELECT body, revision FROM messages WHERE id = ?1")?;
            let mut keep_revision = tx.prepare(
                "INSERT OR IGNORE INTO message_revisions (message_id, revision, body)
                 VALUES (?1, ?2, ?3)",
            )?;
            let mut stmt = tx.prepare(
                "INSERT INTO messages
                     (id, conversation_id, client_message_id, sender, body, message_type, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET
                     revision = CASE WHEN excluded.body <> messages.body
                                     THEN messages.revision + 1 ELSE messages.revision END,
                     body = excluded.body,
                     message_type = excluded.message_type,
                     client_message_id = COALESCE(excluded.client_message_id, messages.client_message_id),
                     sender = COALESCE(excluded.sender, messages.sender)",
            )?;
            for message in messages {
                // Corpo que muda = nova revisão do fato; a anterior fica guardada.
                let existing: Option<(String, i64)> = previous
                    .query_row(params![message.id], |row| Ok((row.get(0)?, row.get(1)?)))
                    .optional()?;
                if let Some((old_body, old_revision)) = existing {
                    if old_body != message.body {
                        keep_revision.execute(params![message.id, old_revision, old_body])?;
                    }
                }
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

    /// Nome de exibição do próprio usuário, aprendido da listagem.
    ///
    /// Mora em `sync_state` com escopo reservado (`__self__` não colide com id de conversa, que
    /// começa com `19:`/`48:`). Serve para descobrir a outra pessoa de um 1:1 sem endpoint novo.
    pub fn my_display_name(&self) -> Result<Option<String>> {
        self.sync_token(MY_NAME_SCOPE)
    }

    pub fn set_my_display_name(&self, name: &str, now_ms: i64) -> Result<()> {
        self.set_sync_token(MY_NAME_SCOPE, name, now_ms)
    }

    /// Remetentes de uma conversa, do mais frequente para o menos (empate pelo nome).
    ///
    /// É o que revela a outra pessoa de um 1:1: excluindo quem sou eu, sobra o outro lado.
    pub fn senders(&self, conversation_id: &str) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT sender, COUNT(*) AS n FROM messages
             WHERE conversation_id = ?1 AND sender IS NOT NULL
             GROUP BY sender
             ORDER BY n DESC, sender",
        )?;
        let rows = stmt.query_map(params![conversation_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
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

    // ---- fatos × interpretações (#22) ----

    /// Revisão corrente do fato (1 = como chegou). `None` se a mensagem não existe.
    pub fn message_revision(&self, message_id: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT revision FROM messages WHERE id = ?1",
                params![message_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Revisões **anteriores** do corpo (a corrente está em `messages`), da mais antiga à mais nova.
    pub fn message_history(&self, message_id: &str) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT revision, body FROM message_revisions WHERE message_id = ?1 ORDER BY revision",
        )?;
        let rows = stmt.query_map(params![message_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Grava um derivado novo, preso às revisões **atuais** das mensagens-fonte.
    ///
    /// Derivado é imutável: refazer a tradução/ata é gravar outro (o anterior fica de histórico).
    /// Recusa fonte inexistente ou lista vazia — derivado sem fato não é rastreável.
    pub fn add_derivative(
        &mut self,
        kind: &str,
        model: &str,
        lang: Option<&str>,
        body: &str,
        source_message_ids: &[&str],
        now_ms: i64,
    ) -> Result<i64> {
        if kind.is_empty() || model.is_empty() {
            return Err(StorageError::Invalid("derivado sem kind/modelo"));
        }
        if source_message_ids.is_empty() {
            return Err(StorageError::Invalid("derivado sem mensagem-fonte"));
        }
        let tx = self.conn.transaction()?;
        let mut sources = Vec::with_capacity(source_message_ids.len());
        for message_id in source_message_ids {
            let revision: Option<i64> = tx
                .query_row(
                    "SELECT revision FROM messages WHERE id = ?1",
                    params![message_id],
                    |row| row.get(0),
                )
                .optional()?;
            let revision = revision.ok_or(StorageError::Invalid("mensagem-fonte inexistente"))?;
            sources.push((*message_id, revision));
        }
        tx.execute(
            "INSERT INTO derivatives (kind, model, lang, body, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![kind, model, lang, body, now_ms],
        )?;
        let id = tx.last_insert_rowid();
        for (message_id, revision) in sources {
            tx.execute(
                "INSERT OR REPLACE INTO derivative_sources
                     (derivative_id, message_id, message_revision) VALUES (?1, ?2, ?3)",
                params![id, message_id, revision],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// Derivados que citam a mensagem (do mais novo ao mais antigo), com a marca de obsolescência.
    pub fn derivatives_for(&self, message_id: &str) -> Result<Vec<Derivative>> {
        let mut stmt = self.conn.prepare(
            "SELECT d.id FROM derivatives d
               JOIN derivative_sources s ON s.derivative_id = d.id
              WHERE s.message_id = ?1
              ORDER BY d.id DESC",
        )?;
        let ids: Vec<i64> = stmt
            .query_map(params![message_id], |row| row.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        ids.into_iter()
            .filter_map(|id| self.derivative(id).transpose())
            .collect()
    }

    /// Um derivado pelo id, com fontes e obsolescência calculadas agora.
    pub fn derivative(&self, id: i64) -> Result<Option<Derivative>> {
        let row = self
            .conn
            .query_row(
                "SELECT kind, model, lang, body, created_ms, approved_ms
                   FROM derivatives WHERE id = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((kind, model, lang, body, created_ms, approved_ms)) = row else {
            return Ok(None);
        };
        let mut stmt = self.conn.prepare(
            "SELECT s.message_id, s.message_revision, m.revision
               FROM derivative_sources s LEFT JOIN messages m ON m.id = s.message_id
              WHERE s.derivative_id = ?1 ORDER BY s.message_id",
        )?;
        let mut sources = Vec::new();
        let mut stale = false;
        let rows = stmt.query_map(params![id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        })?;
        for row in rows {
            let (message_id, message_revision, current) = row?;
            stale |= current != Some(message_revision);
            sources.push(SourceRef {
                message_id,
                message_revision,
            });
        }
        Ok(Some(Derivative {
            id,
            kind,
            model,
            lang,
            body,
            created_ms,
            approved_ms,
            sources,
            stale,
        }))
    }

    /// O usuário aprova **este** texto. Devolve exatamente o que foi aprovado.
    ///
    /// Recusa derivado inexistente ou obsoleto (o fato mudou desde que foi gerado). Aprovar de novo
    /// um já aprovado é idempotente e não muda o carimbo.
    pub fn approve_derivative(&self, id: i64, now_ms: i64) -> Result<String> {
        let derivative = self
            .derivative(id)?
            .ok_or(StorageError::Invalid("derivado inexistente"))?;
        if derivative.stale {
            return Err(StorageError::Invalid("derivado obsoleto: o original mudou"));
        }
        if derivative.approved_ms.is_none() {
            self.conn.execute(
                "UPDATE derivatives SET approved_ms = ?2 WHERE id = ?1",
                params![id, now_ms],
            )?;
        }
        Ok(derivative.body)
    }

    /// Texto que **pode** ser enviado: aprovado e ainda válido. Qualquer outra coisa é `None`.
    pub fn approved_text(&self, id: i64) -> Result<Option<String>> {
        Ok(self
            .derivative(id)?
            .filter(|d| d.approved_ms.is_some() && !d.stale)
            .map(|d| d.body))
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
    fn my_name_and_senders_survive_a_restart() {
        let mut store = store();
        assert_eq!(store.my_display_name().expect("vazio"), None);
        store.set_my_display_name("Fabio Motta", 10).expect("grava");
        assert_eq!(
            store.my_display_name().expect("lê").as_deref(),
            Some("Fabio Motta"),
            "o nome tem de sobreviver ao restart, senão o 1:1 volta a ficar sem nome"
        );

        store
            .upsert_conversation(&Conversation::new("19:a@thread.v2", "1:1 · abc", "chat"))
            .expect("conversa");
        let batch = vec![
            Message {
                sender: Some("Ana".into()),
                ..Message::text("19:a@thread.v2", "m1", "oi", 1)
            },
            Message {
                sender: Some("Ana".into()),
                ..Message::text("19:a@thread.v2", "m2", "oi 2", 2)
            },
            Message {
                sender: Some("Fabio Motta".into()),
                ..Message::text("19:a@thread.v2", "m3", "meu", 3)
            },
        ];
        store.upsert_messages(&batch).expect("mensagens");

        // Mais frequente primeiro: é assim que o driver acha o outro lado do 1:1.
        let senders = store.senders("19:a@thread.v2").expect("remetentes");
        assert_eq!(
            senders,
            vec![("Ana".to_string(), 2), ("Fabio Motta".to_string(), 1)]
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

    // ---- fatos × interpretações (#22) ----

    fn with_message(body: &str) -> Store {
        let mut store = store();
        seed(&store, "19:a@thread.v2");
        store
            .upsert_messages(&[Message::text("19:a@thread.v2", "m1", body, 10)])
            .expect("lote");
        store
    }

    #[test]
    fn edited_body_bumps_revision_and_keeps_the_previous_one() {
        let mut store = with_message("versão 1");
        assert_eq!(store.message_revision("m1").unwrap(), Some(1));

        store
            .upsert_messages(&[Message::text("19:a@thread.v2", "m1", "versão 2", 10)])
            .expect("edição");
        assert_eq!(store.message_revision("m1").unwrap(), Some(2));
        assert_eq!(
            store.message_history("m1").unwrap(),
            vec![(1, "versão 1".to_string())]
        );

        // Reaplicar o mesmo corpo não cria revisão (idempotência da sincronização).
        store
            .upsert_messages(&[Message::text("19:a@thread.v2", "m1", "versão 2", 10)])
            .expect("repetição");
        assert_eq!(store.message_revision("m1").unwrap(), Some(2));
        assert_eq!(store.message_history("m1").unwrap().len(), 1);
    }

    #[test]
    fn derivative_is_traceable_and_never_touches_the_original() {
        let mut store = with_message("bom dia");
        let id = store
            .add_derivative(
                "translation",
                "modelo-x@1",
                Some("en"),
                "good morning",
                &["m1"],
                50,
            )
            .expect("derivado");

        let derivative = store.derivative(id).unwrap().expect("existe");
        assert_eq!(derivative.body, "good morning");
        assert_eq!(derivative.lang.as_deref(), Some("en"));
        assert_eq!(derivative.model, "modelo-x@1");
        assert_eq!(
            derivative.sources,
            vec![SourceRef {
                message_id: "m1".into(),
                message_revision: 1
            }]
        );
        assert!(!derivative.stale);
        assert_eq!(store.derivatives_for("m1").unwrap().len(), 1);

        // O fato segue idêntico: a tradução não entrou em `messages` nem no FTS.
        let page = store.page_messages("19:a@thread.v2", None, 10).unwrap();
        assert_eq!(page[0].body, "bom dia");
        assert!(store.search("morning", 10).unwrap().is_empty());
    }

    #[test]
    fn schema_refuses_to_rewrite_a_derivative() {
        let mut store = with_message("oi");
        let id = store
            .add_derivative("summary", "m@1", None, "resumo", &["m1"], 1)
            .unwrap();
        let error = store
            .conn
            .execute(
                "UPDATE derivatives SET body = 'outro' WHERE id = ?1",
                params![id],
            )
            .expect_err("imutável");
        assert!(error.to_string().contains("imutável"));
        assert_eq!(store.derivative(id).unwrap().unwrap().body, "resumo");
    }

    #[test]
    fn editing_the_fact_makes_derivatives_stale_and_blocks_approval() {
        let mut store = with_message("preço: 10");
        let id = store
            .add_derivative("translation", "m@1", Some("en"), "price: 10", &["m1"], 1)
            .unwrap();

        store
            .upsert_messages(&[Message::text("19:a@thread.v2", "m1", "preço: 20", 10)])
            .unwrap();
        assert!(store.derivative(id).unwrap().unwrap().stale);
        assert!(store.approve_derivative(id, 2).is_err());
        assert_eq!(store.approved_text(id).unwrap(), None);

        // Nova versão sobre a revisão corrente é aprovável; a antiga continua de histórico.
        let fresh = store
            .add_derivative("translation", "m@1", Some("en"), "price: 20", &["m1"], 3)
            .unwrap();
        assert_eq!(store.approve_derivative(fresh, 4).unwrap(), "price: 20");
        assert_eq!(store.derivatives_for("m1").unwrap().len(), 2);
    }

    #[test]
    fn approval_pins_the_exact_text_and_is_idempotent() {
        let mut store = with_message("oi");
        let id = store
            .add_derivative("proposed_action", "m@1", None, "responder: ok", &["m1"], 1)
            .unwrap();
        assert_eq!(store.approved_text(id).unwrap(), None, "não aprovado ainda");

        assert_eq!(store.approve_derivative(id, 5).unwrap(), "responder: ok");
        assert_eq!(store.approve_derivative(id, 9).unwrap(), "responder: ok");
        let derivative = store.derivative(id).unwrap().unwrap();
        assert_eq!(derivative.approved_ms, Some(5), "carimbo não muda");
        assert_eq!(
            store.approved_text(id).unwrap().as_deref(),
            Some("responder: ok")
        );
    }

    #[test]
    fn deleted_source_makes_the_derivative_stale_instead_of_losing_it() {
        let mut store = with_message("ata base");
        let id = store
            .add_derivative("minutes", "m@1", None, "ata", &["m1"], 1)
            .unwrap();
        store.delete_conversation("19:a@thread.v2").unwrap();
        let derivative = store.derivative(id).unwrap().expect("não some em silêncio");
        assert!(derivative.stale);
    }

    #[test]
    fn derivative_needs_an_existing_source() {
        let mut store = with_message("oi");
        assert!(store
            .add_derivative("summary", "m@1", None, "x", &[], 1)
            .is_err());
        assert!(store
            .add_derivative("summary", "m@1", None, "x", &["nao-existe"], 1)
            .is_err());
        assert!(store
            .add_derivative("", "m@1", None, "x", &["m1"], 1)
            .is_err());
    }

    #[test]
    fn summary_can_cite_several_messages() {
        let mut store = with_message("a");
        store
            .upsert_messages(&[Message::text("19:a@thread.v2", "m2", "b", 11)])
            .unwrap();
        let id = store
            .add_derivative("summary", "m@1", None, "a+b", &["m1", "m2"], 1)
            .unwrap();
        assert_eq!(store.derivative(id).unwrap().unwrap().sources.len(), 2);
        assert_eq!(store.derivatives_for("m2").unwrap().len(), 1);

        store
            .upsert_messages(&[Message::text("19:a@thread.v2", "m2", "b editado", 11)])
            .unwrap();
        assert!(
            store.derivative(id).unwrap().unwrap().stale,
            "qualquer fonte editada invalida"
        );
    }

    #[test]
    fn v1_database_migrates_to_v2_keeping_messages() {
        let conn = Connection::open_in_memory().unwrap();
        {
            conn.execute_batch(schema::DDL_V1).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
            conn.execute(
                "INSERT INTO conversations (id, title, kind, updated_ms) VALUES ('c','t','chat',0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO messages (id, conversation_id, body, message_type, created_ms)
                 VALUES ('m','c','antiga','Text',1)",
                [],
            )
            .unwrap();
        }
        let store = Store::init(conn).expect("migra");
        assert_eq!(store.schema_version().unwrap(), schema::VERSION);
        assert_eq!(store.message_revision("m").unwrap(), Some(1));
    }

    fn event(id: &str, start: i64, end: i64) -> CalendarEvent {
        CalendarEvent {
            id: id.into(),
            subject: format!("evento {id}"),
            start_ms: start,
            end_ms: end,
            all_day: false,
            location: None,
            organizer: None,
            join_url: None,
        }
    }

    #[test]
    fn events_between_uses_a_half_open_window_ordered_by_start() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_events(
                &[
                    event("tarde", 300, 400),
                    event("antes", 0, 100),
                    event("cedo", 100, 200),
                    event("pontual", 150, 150),
                    event("depois", 500, 600),
                    event("atravessa", 50, 250),
                ],
                1,
            )
            .unwrap();
        let ids: Vec<_> = store
            .events_between(100, 500)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids, ["atravessa", "cedo", "pontual", "tarde"]);
    }

    #[test]
    fn upsert_events_updates_by_id_and_rejects_inverted_ranges() {
        let mut store = Store::open_in_memory().unwrap();
        store.upsert_events(&[event("a", 10, 20)], 1).unwrap();
        let mut changed = event("a", 30, 40);
        changed.join_url = Some("https://teams.example/join/1".into());
        store.upsert_events(&[changed], 2).unwrap();
        let got = store.event("a").unwrap().unwrap();
        assert_eq!((got.start_ms, got.end_ms), (30, 40));
        assert_eq!(
            got.join_url.as_deref(),
            Some("https://teams.example/join/1")
        );
        assert_eq!(store.events_between(0, 100).unwrap().len(), 1);

        assert!(store.upsert_events(&[event("b", 9, 1)], 3).is_err());
        assert!(store.event("b").unwrap().is_none());
        assert!(store.event("nao-existe").unwrap().is_none());
    }
}
