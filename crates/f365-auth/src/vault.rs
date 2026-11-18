//! Persistência da sessão no keyring (Windows Credential Manager, serviço `fast365`).
//!
//! Só o que precisa sobreviver ao processo: refresh token, cookies de sessão e a conta. Access/skype
//! tokens nunca passam por aqui (ficam em memória).
//!
//! **Gerações.** Cada gravação escreve uma geração nova e completa, sob chaves próprias
//! (`<upn>/<geração>/<campo>#i`), lê de volta para validar e só então publica o ponteiro `active`
//! (`<upn>\n<geração>`) — uma única credencial, escrita atômica no Windows. Até esse instante o leitor
//! continua vendo a geração anterior inteira; depois, a nova inteira; nunca uma mistura. A geração
//! antiga é apagada depois (melhor esforço). Gerações têm id único por escritor, então dois
//! `f365 auth` simultâneos não se misturam: vence o último ponteiro publicado, e a geração perdedora
//! fica órfã (sem ponteiro, nunca lida).
//!
//! **Pedaços.** Uma credencial do Windows guarda no máximo 2560 bytes e um RT + cookies passa disso,
//! então cada campo é gravado em pedaços `#0..N-1` com o total em `#n`.
//!
//! **Fronteira.** O backend bruto e a leitura de segredos são internos: fora de `f365-auth` só se
//! lê a conta (sem segredos).
//!
//! ```no_run
//! let conta = f365_auth::vault::Vault::keyring().active_account();
//! ```
//! ```compile_fail
//! // backend bruto não é público
//! let _ = f365_auth::vault::KeyringStore;
//! ```
//! ```compile_fail
//! // nem o acesso ao store a partir do Vault
//! let _ = f365_auth::vault::Vault::keyring().store();
//! ```
//! ```compile_fail
//! // nem a sessão com segredos
//! let _ = f365_auth::vault::Vault::keyring().load_session();
//! ```
//!
//! Fora do Windows não há keyring persistente: gravar falha com erro explícito e ler devolve "nunca
//! pareado" — de propósito, segredo da conta não vai para VPS/CI (`AGENTS.md`).

use crate::secret::Secret;
use crate::session::{AccountInfo, CapturedSession, RefreshToken, SessionCookie};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use zeroize::{Zeroize, Zeroizing};

pub const SERVICE: &str = "fast365";
const ACTIVE_KEY: &str = "active";
/// Abaixo do limite de 2560 bytes do `CredWriteW`, com folga.
const CHUNK: usize = 2048;
/// Teto por campo (64 KiB): protege contra contagem corrompida/maliciosa no keyring.
const MAX_CHUNKS: usize = 32;
const FIELDS: [&str; 3] = ["rt", "cookies", "account"];

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("keyring: {0}")]
    Store(String),
    #[error("dados da sessão corrompidos no keyring ({0}); rode `f365 auth` de novo")]
    Corrupt(&'static str),
    #[error("valor grande demais para o keyring ({0})")]
    TooLarge(&'static str),
}

/// Armazenamento chave → bytes. Interno: abstraído só para testar o `Vault` (falhas injetadas).
pub(crate) trait SecretStore: Send + Sync {
    fn put(&self, key: &str, value: &[u8]) -> Result<(), VaultError>;
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, VaultError>;
    fn remove(&self, key: &str) -> Result<(), VaultError>;
}

/// Keyring do SO. No Windows: Credential Manager via `windows-native-keyring-store`, com a
/// configuração do keyring 3 (alvo `<chave>.<serviço>`, persistência `Enterprise`) — credenciais
/// gravadas antes da migração continuam legíveis (`KEYRING-COMPAT-001`, VERIFIED 2026-11-13:
/// sessão gravada pelo keyring 3 lida pelo código novo, `f365 whoami --offline` rodado pelo dono).
///
/// O store é passado explicitamente a cada credencial (sem store global de processo).
#[cfg(windows)]
pub(crate) struct KeyringStore {
    service: &'static str,
    store: Result<std::sync::Arc<windows_native_keyring_store::Store>, String>,
}

#[cfg(windows)]
impl KeyringStore {
    pub(crate) fn new(service: &'static str) -> Self {
        let store = windows_native_keyring_store::Store::new().map_err(|e| e.to_string());
        Self { service, store }
    }

    fn entry(&self, key: &str) -> Result<keyring_core::Entry, VaultError> {
        use keyring_core::api::CredentialStoreApi;
        let store = self
            .store
            .as_ref()
            .map_err(|e| VaultError::Store(e.clone()))?;
        store
            .build(self.service, key, None)
            .map_err(|e| VaultError::Store(e.to_string()))
    }
}

#[cfg(windows)]
impl SecretStore for KeyringStore {
    fn put(&self, key: &str, value: &[u8]) -> Result<(), VaultError> {
        self.entry(key)?
            .set_secret(value)
            .map_err(|e| VaultError::Store(e.to_string()))
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, VaultError> {
        match self.entry(key)?.get_secret() {
            Ok(v) => Ok(Some(v)),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(VaultError::Store(e.to_string())),
        }
    }

    fn remove(&self, key: &str) -> Result<(), VaultError> {
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(e) => Err(VaultError::Store(e.to_string())),
        }
    }
}

/// Fora do Windows não há keyring persistente **de propósito**: segredo da conta não vai para
/// VPS/CI (`AGENTS.md`). Leitura = nunca pareado; gravação = erro explícito (nunca "finge" salvar).
#[cfg(not(windows))]
pub(crate) struct KeyringStore;

#[cfg(not(windows))]
impl KeyringStore {
    pub(crate) fn new(_service: &'static str) -> Self {
        Self
    }
}

#[cfg(not(windows))]
impl SecretStore for KeyringStore {
    fn put(&self, _key: &str, _value: &[u8]) -> Result<(), VaultError> {
        Err(VaultError::Store(
            "não há keyring persistente fora do Windows: a sessão só é gravada na máquina do dono"
                .into(),
        ))
    }

    fn get(&self, _key: &str) -> Result<Option<Vec<u8>>, VaultError> {
        Ok(None)
    }

    fn remove(&self, _key: &str) -> Result<(), VaultError> {
        Ok(())
    }
}

/// Store em memória compartilhável (clones veem os mesmos dados) — testes de outros módulos.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct MemoryStore(
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>,
);

#[cfg(test)]
impl SecretStore for MemoryStore {
    fn put(&self, key: &str, value: &[u8]) -> Result<(), VaultError> {
        self.0.lock().unwrap().insert(key.into(), value.into());
        Ok(())
    }
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, VaultError> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }
    fn remove(&self, key: &str) -> Result<(), VaultError> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

/// Conta salva + metadados não secretos (o que `f365 whoami` mostra).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAccount {
    pub account: AccountInfo,
    pub client_id: String,
    pub family_id: Option<String>,
    /// Quando o login foi feito (não muda com a rotação do RT — é a base da validade do RT de SPA).
    pub captured_at: SystemTime,
    pub cookie_count: usize,
    /// Escopos dos access tokens que a própria página do Teams pediu (evidência de `TOKEN-SCOPE-001`).
    pub observed_scopes: Vec<String>,
    /// Último resgate bem-sucedido do RT (`None` = nunca resgatado desde a captura).
    pub last_refresh: Option<SystemTime>,
}

#[derive(Serialize, Deserialize)]
struct AccountRecord {
    upn: String,
    tenant_id: String,
    home_account_id: String,
    environment: String,
    client_id: String,
    family_id: Option<String>,
    captured_at_unix: u64,
    cookie_count: usize,
    // Campos novos com default: sessões gravadas antes deles continuam legíveis.
    #[serde(default)]
    observed_scopes: Vec<String>,
    #[serde(default)]
    last_refresh_unix: Option<u64>,
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

#[derive(Serialize, Deserialize)]
struct CookieRecord {
    name: String,
    value: String,
    domain: String,
    path: String,
}

impl Drop for CookieRecord {
    fn drop(&mut self) {
        self.value.zeroize();
    }
}

/// Ponteiro para a geração publicada. `generation = None` = layout antigo (sem gerações).
struct Pointer {
    upn: String,
    generation: Option<String>,
}

impl Pointer {
    fn prefix(&self) -> String {
        match &self.generation {
            Some(g) => format!("{}/{g}", self.upn),
            None => self.upn.clone(),
        }
    }
}

pub struct Vault {
    store: Box<dyn SecretStore>,
}

impl Vault {
    pub fn keyring() -> Self {
        Self::with_store(KeyringStore::new(SERVICE))
    }

    pub(crate) fn with_store(store: impl SecretStore + 'static) -> Self {
        Self {
            store: Box::new(store),
        }
    }

    /// Conta ativa, sem carregar segredos. `None` = nunca pareado (ou sessão em formato antigo).
    pub fn active_account(&self) -> Result<Option<StoredAccount>, VaultError> {
        let Some(ptr) = self.pointer()? else {
            return Ok(None);
        };
        if ptr.generation.is_none() {
            return Ok(None); // layout anterior às gerações: precisa parear de novo
        }
        self.read_account(&ptr.prefix()).map(Some)
    }

    /// Grava a sessão como nova geração e só então a publica. `captured_at` vem do `Clock`.
    pub(crate) fn save(
        &self,
        session: &CapturedSession,
        captured_at: SystemTime,
    ) -> Result<(), VaultError> {
        let previous = self.pointer().unwrap_or(None);
        let upn = &session.account.upn;
        let generation = fresh_generation(captured_at);
        let prefix = format!("{upn}/{generation}");

        if let Err(e) = self.write_generation(&prefix, session, captured_at) {
            self.remove_generation(&prefix);
            return Err(e);
        }
        let pointer = format!("{upn}\n{generation}");
        if let Err(e) = self.store.put(ACTIVE_KEY, pointer.as_bytes()) {
            self.remove_generation(&prefix);
            return Err(e);
        }
        if let Some(old) = previous {
            if old.prefix() != prefix {
                self.remove_generation(&old.prefix());
            }
        }
        Ok(())
    }

    /// Troca o RT pelo rotacionado no resgate, numa geração nova (mesmas garantias do `save`),
    /// preservando o instante do login.
    pub(crate) fn rotate_refresh_token(
        &self,
        new: Secret,
        refreshed_at: SystemTime,
    ) -> Result<(), VaultError> {
        let stored = self
            .active_account()?
            .ok_or(VaultError::Corrupt("sem sessão para rotacionar"))?;
        let mut session = self
            .load_session()?
            .ok_or(VaultError::Corrupt("sem sessão para rotacionar"))?;
        session.refresh_token.secret = new;
        // Persistido em segundos: truncar antes, senão a releitura de validação nunca bate.
        session.last_refresh = Some(UNIX_EPOCH + Duration::from_secs(unix(refreshed_at)));
        self.save(&session, stored.captured_at)
    }

    /// Apaga a sessão salva (RT, cookies e conta) — o "sair" do `f365 logout`.
    ///
    /// Devolve o UPN removido (não é segredo) ou `None` se não havia sessão. Ordem importa: o
    /// ponteiro da conta ativa só sai **depois** de a geração ter sumido por inteiro; se algo
    /// falhar no meio, o ponteiro continua e rodar de novo termina o serviço. Um ponteiro ilegível
    /// não impede o logout (é exatamente o estado que o usuário quer limpar).
    ///
    /// Limite conhecido: o keyring não é enumerável por aqui, então só a geração apontada é
    /// removida; restos de uma escrita interrompida (que já são removidos na falha) não são
    /// procurados.
    pub fn logout(&self) -> Result<Option<String>, VaultError> {
        let pointer = self.pointer().unwrap_or(None);
        if let Some(ptr) = &pointer {
            let prefix = ptr.prefix();
            self.remove_generation(&prefix);
            for field in FIELDS {
                if self.store.get(&format!("{prefix}/{field}#n"))?.is_some() {
                    return Err(VaultError::Store(
                        "parte da sessão continua no keyring; rode `f365 logout` de novo".into(),
                    ));
                }
            }
        }
        self.store.remove(ACTIVE_KEY)?;
        Ok(pointer.map(|ptr| ptr.upn))
    }

    /// Sessão completa (com segredos) da conta ativa — uso interno de `f365-auth`.
    pub(crate) fn load_session(&self) -> Result<Option<CapturedSession>, VaultError> {
        let Some(ptr) = self.pointer()? else {
            return Ok(None);
        };
        if ptr.generation.is_none() {
            return Ok(None);
        }
        self.read_generation(&ptr.prefix()).map(Some)
    }

    fn write_generation(
        &self,
        prefix: &str,
        session: &CapturedSession,
        captured_at: SystemTime,
    ) -> Result<(), VaultError> {
        let cookies: Vec<CookieRecord> = session
            .cookies
            .iter()
            .map(|c| CookieRecord {
                name: c.name.clone(),
                value: c.value.expose().to_owned(),
                domain: c.domain.clone(),
                path: c.path.clone(),
            })
            .collect();
        let cookies_json =
            Zeroizing::new(serde_json::to_vec(&cookies).expect("serializar cookies"));
        drop(cookies); // `CookieRecord` zera o valor no Drop
        let record = AccountRecord {
            upn: session.account.upn.clone(),
            tenant_id: session.account.tenant_id.clone(),
            home_account_id: session.account.home_account_id.clone(),
            environment: session.account.environment.clone(),
            client_id: session.refresh_token.client_id.clone(),
            family_id: session.refresh_token.family_id.clone(),
            captured_at_unix: unix(captured_at),
            cookie_count: session.cookies.len(),
            observed_scopes: session.observed_scopes.clone(),
            last_refresh_unix: session.last_refresh.map(unix),
        };

        self.put_chunked(
            &format!("{prefix}/rt"),
            session.refresh_token.secret.expose().as_bytes(),
        )?;
        self.put_chunked(&format!("{prefix}/cookies"), &cookies_json)?;
        self.put_chunked(
            &format!("{prefix}/account"),
            &serde_json::to_vec(&record).expect("conta"),
        )?;

        // Lê de volta antes de publicar: só um commit que relê idêntico vira a sessão ativa.
        let stored = self.read_generation(prefix)?;
        if stored != *session {
            return Err(VaultError::Corrupt("releitura diverge do que foi gravado"));
        }
        Ok(())
    }

    fn read_generation(&self, prefix: &str) -> Result<CapturedSession, VaultError> {
        let stored = self.read_account(prefix)?;
        let rt = self
            .get_chunked(&format!("{prefix}/rt"))?
            .ok_or(VaultError::Corrupt("refresh token"))?;
        let rt =
            Secret::new(String::from_utf8(rt).map_err(|_| VaultError::Corrupt("refresh token"))?);
        let raw = Zeroizing::new(
            self.get_chunked(&format!("{prefix}/cookies"))?
                .ok_or(VaultError::Corrupt("cookies"))?,
        );
        let cookies: Vec<CookieRecord> =
            serde_json::from_slice(&raw).map_err(|_| VaultError::Corrupt("cookies"))?;
        Ok(CapturedSession {
            account: stored.account,
            refresh_token: RefreshToken {
                secret: rt,
                client_id: stored.client_id,
                family_id: stored.family_id,
            },
            observed_scopes: stored.observed_scopes,
            last_refresh: stored.last_refresh,
            cookies: cookies
                .into_iter()
                .map(|mut c| SessionCookie {
                    name: std::mem::take(&mut c.name),
                    value: Secret::new(std::mem::take(&mut c.value)),
                    domain: std::mem::take(&mut c.domain),
                    path: std::mem::take(&mut c.path),
                })
                .collect(),
        })
    }

    fn read_account(&self, prefix: &str) -> Result<StoredAccount, VaultError> {
        let raw = self
            .get_chunked(&format!("{prefix}/account"))?
            .ok_or(VaultError::Corrupt("conta"))?;
        let r: AccountRecord =
            serde_json::from_slice(&raw).map_err(|_| VaultError::Corrupt("conta"))?;
        Ok(StoredAccount {
            account: AccountInfo {
                upn: r.upn,
                tenant_id: r.tenant_id,
                home_account_id: r.home_account_id,
                environment: r.environment,
            },
            client_id: r.client_id,
            family_id: r.family_id,
            captured_at: UNIX_EPOCH + Duration::from_secs(r.captured_at_unix),
            cookie_count: r.cookie_count,
            observed_scopes: r.observed_scopes,
            last_refresh: r
                .last_refresh_unix
                .map(|s| UNIX_EPOCH + Duration::from_secs(s)),
        })
    }

    fn pointer(&self) -> Result<Option<Pointer>, VaultError> {
        let Some(raw) = self.store.get(ACTIVE_KEY)? else {
            return Ok(None);
        };
        let text = String::from_utf8(raw).map_err(|_| VaultError::Corrupt("conta ativa"))?;
        Ok(Some(match text.split_once('\n') {
            Some((upn, generation)) => Pointer {
                upn: upn.into(),
                generation: Some(generation.into()),
            },
            None => Pointer {
                upn: text,
                generation: None,
            },
        }))
    }

    /// Melhor esforço: falha aqui não desfaz um commit já publicado.
    fn remove_generation(&self, prefix: &str) {
        for field in FIELDS {
            let key = format!("{prefix}/{field}");
            let n = self.chunk_count(&key).ok().flatten().unwrap_or(MAX_CHUNKS);
            let _ = self.store.remove(&format!("{key}#n"));
            for i in 0..n {
                let _ = self.store.remove(&format!("{key}#{i}"));
            }
        }
    }

    fn put_chunked(&self, key: &str, value: &[u8]) -> Result<(), VaultError> {
        let chunks: Vec<&[u8]> = value.chunks(CHUNK).collect();
        if chunks.len() > MAX_CHUNKS {
            return Err(VaultError::TooLarge("campo da sessão"));
        }
        for (i, chunk) in chunks.iter().enumerate() {
            self.store.put(&format!("{key}#{i}"), chunk)?;
        }
        self.store
            .put(&format!("{key}#n"), chunks.len().to_string().as_bytes())
    }

    fn get_chunked(&self, key: &str) -> Result<Option<Vec<u8>>, VaultError> {
        let Some(n) = self.chunk_count(key)? else {
            return Ok(None);
        };
        let mut out = Vec::new();
        for i in 0..n {
            let part = self
                .store
                .get(&format!("{key}#{i}"))?
                .ok_or(VaultError::Corrupt("pedaço ausente"))?;
            out.extend_from_slice(&part);
        }
        Ok(Some(out))
    }

    fn chunk_count(&self, key: &str) -> Result<Option<usize>, VaultError> {
        let Some(raw) = self.store.get(&format!("{key}#n"))? else {
            return Ok(None);
        };
        let n: Option<usize> = std::str::from_utf8(&raw).ok().and_then(|s| s.parse().ok());
        match n {
            Some(n) if n <= MAX_CHUNKS => Ok(Some(n)),
            _ => Err(VaultError::Corrupt("contagem de pedaços")),
        }
    }
}

/// Id único por escritor: instante da captura + processo + contador local.
fn fresh_generation(captured_at: SystemTime) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = captured_at
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "g{nanos:x}-{:x}-{:x}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};

    /// Store em memória com falha injetável na N-ésima escrita/remoção.
    #[derive(Clone, Default)]
    struct TestStore {
        data: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        puts: Arc<AtomicUsize>,
        fail_put_at: Arc<Mutex<Option<usize>>>,
        fail_removes: Arc<Mutex<bool>>,
    }

    impl TestStore {
        fn fail_put_at(&self, n: Option<usize>) {
            self.puts.store(0, Ordering::SeqCst);
            *self.fail_put_at.lock().unwrap() = n;
        }
        fn max_value_len(&self) -> usize {
            self.data
                .lock()
                .unwrap()
                .values()
                .map(Vec::len)
                .max()
                .unwrap_or(0)
        }
        fn raw(&self, key: &str) -> Option<Vec<u8>> {
            self.data.lock().unwrap().get(key).cloned()
        }
    }

    impl SecretStore for TestStore {
        fn put(&self, key: &str, value: &[u8]) -> Result<(), VaultError> {
            let n = self.puts.fetch_add(1, Ordering::SeqCst);
            if *self.fail_put_at.lock().unwrap() == Some(n) {
                return Err(VaultError::Store(format!("falha injetada na escrita {n}")));
            }
            self.data.lock().unwrap().insert(key.into(), value.into());
            Ok(())
        }
        fn get(&self, key: &str) -> Result<Option<Vec<u8>>, VaultError> {
            Ok(self.data.lock().unwrap().get(key).cloned())
        }
        fn remove(&self, key: &str) -> Result<(), VaultError> {
            if *self.fail_removes.lock().unwrap() {
                return Err(VaultError::Store("falha injetada na remoção".into()));
            }
            self.data.lock().unwrap().remove(key);
            Ok(())
        }
    }

    fn session(rt: char, rt_len: usize, cookie_len: usize) -> CapturedSession {
        CapturedSession {
            account: AccountInfo {
                upn: "fulano@empresa.com".into(),
                tenant_id: "tid".into(),
                home_account_id: "oid.tid".into(),
                environment: "login.windows.net".into(),
            },
            refresh_token: RefreshToken {
                secret: Secret::new(rt.to_string().repeat(rt_len)),
                client_id: "client-web".into(),
                family_id: Some("1".into()),
            },
            observed_scopes: vec!["https://api.spaces.skype.com/.default".into()],
            last_refresh: None,
            cookies: vec![SessionCookie {
                name: "ESTSAUTHPERSISTENT".into(),
                value: Secret::new(rt.to_string().repeat(cookie_len)),
                domain: ".login.microsoftonline.com".into(),
                path: "/".into(),
            }],
        }
    }

    fn vault(store: &TestStore) -> Vault {
        Vault::with_store(store.clone())
    }

    #[test]
    fn roundtrip_large_session_respects_credential_size_limit() {
        let store = TestStore::default();
        let v = vault(&store);
        let original = session('r', 5_000, 3_000);
        let at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);

        v.save(&original, at).unwrap();

        assert!(
            store.max_value_len() <= 2560,
            "credencial acima do limite do Windows"
        );
        assert_eq!(v.load_session().unwrap(), Some(original));
        let stored = v.active_account().unwrap().unwrap();
        assert_eq!(stored.captured_at, at);
        assert_eq!(stored.cookie_count, 1);
        assert_eq!(stored.family_id.as_deref(), Some("1"));
    }

    /// Achado do review: falha em QUALQUER escrita de uma regravação deixa a geração anterior
    /// inteira; sucesso deixa a nova inteira. Nunca mistura — inclusive tamanho igual e menor.
    #[test]
    fn interrupted_resave_never_mixes_generations() {
        for (old_len, new_len) in [(5_000, 5_000), (5_000, 100), (100, 9_000), (9_000, 9_000)] {
            let old = session('A', old_len, 3_000);
            let new = session('B', new_len, 10);
            let mut n = 0;
            loop {
                let store = TestStore::default();
                let v = vault(&store);
                v.save(&old, UNIX_EPOCH).unwrap();
                store.fail_put_at(Some(n));
                let result = v.save(&new, UNIX_EPOCH + Duration::from_secs(60));
                store.fail_put_at(None);
                let loaded = v.load_session().unwrap().expect("sempre há uma sessão");
                match result {
                    Err(_) => {
                        assert_eq!(
                            loaded, old,
                            "falha na escrita {n} ({old_len}->{new_len}) corrompeu a anterior"
                        );
                    }
                    Ok(()) => {
                        assert_eq!(loaded, new);
                        break;
                    }
                }
                n += 1;
            }
            assert!(
                n >= 6,
                "o teste precisa ter exercitado várias escritas (foram {n})"
            );
        }
    }

    #[test]
    fn failed_resave_leaves_no_orphan_generation() {
        let store = TestStore::default();
        let v = vault(&store);
        v.save(&session('A', 5_000, 10), UNIX_EPOCH).unwrap();
        let keys_before = store.data.lock().unwrap().len();
        store.fail_put_at(Some(4)); // no meio dos pedaços dos cookies
        assert!(v.save(&session('B', 5_000, 10), UNIX_EPOCH).is_err());
        assert_eq!(store.data.lock().unwrap().len(), keys_before);
    }

    #[test]
    fn successful_resave_removes_previous_generation() {
        let store = TestStore::default();
        let v = vault(&store);
        v.save(&session('A', 9_000, 10), UNIX_EPOCH).unwrap();
        let keys_one_gen = store.data.lock().unwrap().len();
        v.save(&session('B', 9_000, 10), UNIX_EPOCH).unwrap();
        assert_eq!(store.data.lock().unwrap().len(), keys_one_gen);
        assert_eq!(v.load_session().unwrap(), Some(session('B', 9_000, 10)));
    }

    #[test]
    fn cleanup_failure_does_not_undo_the_commit() {
        let store = TestStore::default();
        let v = vault(&store);
        v.save(&session('A', 100, 10), UNIX_EPOCH).unwrap();
        *store.fail_removes.lock().unwrap() = true;
        v.save(&session('B', 100, 10), UNIX_EPOCH).unwrap();
        assert_eq!(v.load_session().unwrap(), Some(session('B', 100, 10)));
    }

    #[test]
    fn never_paired_and_legacy_layout_read_as_none() {
        let store = TestStore::default();
        let v = vault(&store);
        assert_eq!(v.active_account().unwrap(), None);
        store.put(ACTIVE_KEY, b"fulano@empresa.com").unwrap(); // formato anterior às gerações
        assert_eq!(v.active_account().unwrap(), None);
        assert_eq!(v.load_session().unwrap(), None);
    }

    #[test]
    fn save_over_legacy_layout_removes_legacy_keys() {
        let store = TestStore::default();
        store.put(ACTIVE_KEY, b"fulano@empresa.com").unwrap();
        store.put("fulano@empresa.com/rt#n", b"1").unwrap();
        store.put("fulano@empresa.com/rt#0", b"rt-antigo").unwrap();
        vault(&store)
            .save(&session('B', 10, 10), UNIX_EPOCH)
            .unwrap();
        assert_eq!(store.raw("fulano@empresa.com/rt#0"), None);
        assert_eq!(store.raw("fulano@empresa.com/rt#n"), None);
    }

    #[test]
    fn missing_chunk_and_absurd_count_are_corrupt() {
        let store = TestStore::default();
        let v = vault(&store);
        v.save(&session('A', 5_000, 10), UNIX_EPOCH).unwrap();
        let prefix = v.pointer().unwrap().unwrap().prefix();

        store.data.lock().unwrap().remove(&format!("{prefix}/rt#1"));
        assert!(matches!(v.load_session(), Err(VaultError::Corrupt(_))));

        store
            .put(&format!("{prefix}/account#n"), b"999999")
            .unwrap();
        assert!(matches!(v.active_account(), Err(VaultError::Corrupt(_))));
    }

    #[test]
    fn oversized_field_is_rejected_before_writing() {
        let store = TestStore::default();
        let v = vault(&store);
        let huge = session('A', CHUNK * MAX_CHUNKS + 1, 10);
        assert!(matches!(
            v.save(&huge, UNIX_EPOCH),
            Err(VaultError::TooLarge(_))
        ));
        assert_eq!(v.active_account().unwrap(), None);
    }

    /// Serviço só de teste: os selftests NUNCA tocam nas credenciais `fast365` do dono (AGENTS.md).
    #[cfg(windows)]
    const SELFTEST_SERVICE: &str = "fast365-selftest";

    /// Manual, no Windows, em DOIS processos (prova persistência real, não store em memória):
    /// 1. `cargo test -p f365-auth -- --ignored keyring_selftest_write`
    /// 2. `cmdkey /list:selftest.fast365-selftest` (outro programa vê a credencial)
    /// 3. `cargo test -p f365-auth -- --ignored keyring_selftest_read_and_delete`
    #[cfg(windows)]
    #[test]
    #[ignore = "toca o Credential Manager real"]
    fn keyring_selftest_write() {
        let store = KeyringStore::new(SELFTEST_SERVICE);
        store.put("selftest", &selftest_blob()).unwrap();
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "toca o Credential Manager real; rodar depois de keyring_selftest_write"]
    fn keyring_selftest_read_and_delete() {
        let store = KeyringStore::new(SELFTEST_SERVICE);
        assert_eq!(
            store.get("selftest").unwrap(),
            Some(selftest_blob()),
            "gravado por outro processo"
        );
        store.remove("selftest").unwrap();
        assert_eq!(store.get("selftest").unwrap(), None);
    }

    /// 2048 bytes binários (todos os valores de byte): pega truncamento e conversão UTF-16.
    #[cfg(windows)]
    fn selftest_blob() -> Vec<u8> {
        (0..CHUNK).map(|i| (i % 256) as u8).collect()
    }

    #[cfg(not(windows))]
    #[test]
    fn no_persistent_keyring_outside_windows() {
        let v = Vault::keyring();
        assert_eq!(v.active_account().unwrap(), None);
        assert!(matches!(
            KeyringStore::new(SERVICE).put("k", b"v"),
            Err(VaultError::Store(_))
        ));
    }

    #[test]
    fn logout_removes_the_whole_session_and_reports_the_upn() {
        let store = TestStore::default();
        let v = vault(&store);
        v.save(&session('r', 5_000, 3_000), UNIX_EPOCH).unwrap();
        assert!(v.active_account().unwrap().is_some());

        let removed = v.logout().unwrap();

        assert_eq!(removed.as_deref(), Some("fulano@empresa.com"));
        assert_eq!(v.active_account().unwrap(), None);
        assert_eq!(v.load_session().unwrap(), None);
        assert!(
            store.data.lock().unwrap().is_empty(),
            "não pode sobrar nenhuma credencial no keyring"
        );
    }

    #[test]
    fn logout_without_session_is_a_noop() {
        let store = TestStore::default();
        assert_eq!(vault(&store).logout().unwrap(), None);
    }

    #[test]
    fn failed_logout_keeps_the_pointer_so_it_can_be_retried() {
        let store = TestStore::default();
        let v = vault(&store);
        v.save(&session('r', 100, 100), UNIX_EPOCH).unwrap();

        *store.fail_removes.lock().unwrap() = true;
        assert!(
            v.logout().is_err(),
            "remoção falhou: não pode fingir que saiu"
        );
        assert!(
            store.raw(ACTIVE_KEY).is_some(),
            "o ponteiro fica para a segunda tentativa"
        );

        *store.fail_removes.lock().unwrap() = false;
        assert_eq!(v.logout().unwrap().as_deref(), Some("fulano@empresa.com"));
        assert!(store.data.lock().unwrap().is_empty());
    }

    #[test]
    fn logout_clears_a_corrupt_pointer() {
        let store = TestStore::default();
        store
            .data
            .lock()
            .unwrap()
            .insert(ACTIVE_KEY.into(), vec![0xff, 0xfe]);
        let v = vault(&store);

        assert_eq!(v.logout().unwrap(), None);
        assert!(store.raw(ACTIVE_KEY).is_none());
    }
}
