//! Persistência da sessão no keyring (Windows Credential Manager, serviço `fast365`).
//!
//! Só o que precisa sobreviver ao processo: refresh token, cookies de sessão e a conta. Access/skype
//! tokens nunca passam por aqui (ficam em memória). Uma credencial do Windows guarda no máximo 2560
//! bytes e um RT + cookies passa disso — por isso cada valor é gravado em pedaços:
//! `<upn>/<campo>#0..N-1` e o total em `<upn>/<campo>#n`, escrito por último (commit).
//!
//! Fora do Windows o `keyring` usa um store em memória: nada persiste — de propósito, segredo da
//! conta não vai para VPS/CI (`AGENTS.md`).

use crate::secret::Secret;
use crate::session::{AccountInfo, CapturedSession, RefreshToken, SessionCookie};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use zeroize::{Zeroize, Zeroizing};

pub const SERVICE: &str = "fast365";
const ACTIVE_KEY: &str = "active";
/// Abaixo do limite de 2560 bytes do `CredWriteW`, com folga.
const CHUNK: usize = 2048;

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("keyring: {0}")]
    Store(String),
    #[error("dados da sessão corrompidos no keyring ({0}); rode `f365 auth` de novo")]
    Corrupt(&'static str),
}

/// Armazenamento chave → bytes. Abstraído só para testar o `Vault` sem tocar o keyring real.
pub trait SecretStore: Send + Sync {
    fn put(&self, key: &str, value: &[u8]) -> Result<(), VaultError>;
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, VaultError>;
    fn remove(&self, key: &str) -> Result<(), VaultError>;
}

/// Keyring do SO (`keyring` crate), serviço [`SERVICE`].
#[derive(Debug, Default)]
pub struct KeyringStore;

impl KeyringStore {
    fn entry(key: &str) -> Result<keyring::Entry, VaultError> {
        keyring::Entry::new(SERVICE, key).map_err(|e| VaultError::Store(e.to_string()))
    }
}

impl SecretStore for KeyringStore {
    fn put(&self, key: &str, value: &[u8]) -> Result<(), VaultError> {
        Self::entry(key)?
            .set_secret(value)
            .map_err(|e| VaultError::Store(e.to_string()))
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, VaultError> {
        match Self::entry(key)?.get_secret() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(VaultError::Store(e.to_string())),
        }
    }

    fn remove(&self, key: &str) -> Result<(), VaultError> {
        match Self::entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(VaultError::Store(e.to_string())),
        }
    }
}

/// Store em memória para testes.
#[derive(Debug, Default)]
pub struct MemoryStore(Mutex<HashMap<String, Vec<u8>>>);

impl MemoryStore {
    pub fn keys(&self) -> Vec<String> {
        let mut k: Vec<_> = self
            .0
            .lock()
            .expect("MemoryStore")
            .keys()
            .cloned()
            .collect();
        k.sort();
        k
    }

    pub fn max_value_len(&self) -> usize {
        self.0
            .lock()
            .expect("MemoryStore")
            .values()
            .map(Vec::len)
            .max()
            .unwrap_or(0)
    }
}

impl SecretStore for MemoryStore {
    fn put(&self, key: &str, value: &[u8]) -> Result<(), VaultError> {
        self.0
            .lock()
            .expect("MemoryStore")
            .insert(key.into(), value.into());
        Ok(())
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, VaultError> {
        Ok(self.0.lock().expect("MemoryStore").get(key).cloned())
    }

    fn remove(&self, key: &str) -> Result<(), VaultError> {
        self.0.lock().expect("MemoryStore").remove(key);
        Ok(())
    }
}

/// Conta salva + metadados não secretos (o que `f365 whoami` mostra).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAccount {
    pub account: AccountInfo,
    pub client_id: String,
    pub family_id: Option<String>,
    pub captured_at: SystemTime,
    pub cookie_count: usize,
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

pub struct Vault<S: SecretStore> {
    store: S,
}

impl Vault<KeyringStore> {
    pub fn keyring() -> Self {
        Self::new(KeyringStore)
    }
}

impl<S: SecretStore> Vault<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    /// Grava a sessão e a marca como ativa. `captured_at` vem do `Clock` do chamador.
    pub fn save(
        &self,
        session: &CapturedSession,
        captured_at: SystemTime,
    ) -> Result<(), VaultError> {
        let upn = &session.account.upn;
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
        self.put_chunked(
            &format!("{upn}/rt"),
            session.refresh_token.secret.expose().as_bytes(),
        )?;
        self.put_chunked(&format!("{upn}/cookies"), &cookies_json)?;

        let record = AccountRecord {
            upn: upn.clone(),
            tenant_id: session.account.tenant_id.clone(),
            home_account_id: session.account.home_account_id.clone(),
            environment: session.account.environment.clone(),
            client_id: session.refresh_token.client_id.clone(),
            family_id: session.refresh_token.family_id.clone(),
            captured_at_unix: captured_at
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            cookie_count: session.cookies.len(),
        };
        self.put_chunked(
            &format!("{upn}/account"),
            &serde_json::to_vec(&record).expect("conta"),
        )?;
        self.store.put(ACTIVE_KEY, upn.as_bytes())
    }

    /// Conta ativa, sem carregar segredos. `None` = nunca pareado.
    pub fn active_account(&self) -> Result<Option<StoredAccount>, VaultError> {
        let Some(upn) = self.store.get(ACTIVE_KEY)? else {
            return Ok(None);
        };
        let upn = String::from_utf8(upn).map_err(|_| VaultError::Corrupt("conta ativa"))?;
        let Some(raw) = self.get_chunked(&format!("{upn}/account"))? else {
            return Err(VaultError::Corrupt("conta"));
        };
        let r: AccountRecord =
            serde_json::from_slice(&raw).map_err(|_| VaultError::Corrupt("conta"))?;
        Ok(Some(StoredAccount {
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
        }))
    }

    /// Sessão completa (com segredos) da conta ativa — uso interno do executor (#8).
    #[allow(dead_code)] // consumidor: troca de tokens (#8)
    pub(crate) fn load_session(&self) -> Result<Option<CapturedSession>, VaultError> {
        let Some(stored) = self.active_account()? else {
            return Ok(None);
        };
        let upn = &stored.account.upn;
        let rt = self
            .get_chunked(&format!("{upn}/rt"))?
            .ok_or(VaultError::Corrupt("refresh token"))?;
        let rt = String::from_utf8(rt).map_err(|_| VaultError::Corrupt("refresh token"))?;
        let raw = Zeroizing::new(
            self.get_chunked(&format!("{upn}/cookies"))?
                .ok_or(VaultError::Corrupt("cookies"))?,
        );
        let cookies: Vec<CookieRecord> =
            serde_json::from_slice(&raw).map_err(|_| VaultError::Corrupt("cookies"))?;
        Ok(Some(CapturedSession {
            account: stored.account,
            refresh_token: RefreshToken {
                secret: Secret::new(rt),
                client_id: stored.client_id,
                family_id: stored.family_id,
            },
            cookies: cookies
                .into_iter()
                .map(|mut c| SessionCookie {
                    name: std::mem::take(&mut c.name),
                    value: Secret::new(std::mem::take(&mut c.value)),
                    domain: std::mem::take(&mut c.domain),
                    path: std::mem::take(&mut c.path),
                })
                .collect(),
        }))
    }

    fn put_chunked(&self, key: &str, value: &[u8]) -> Result<(), VaultError> {
        let previous = self.chunk_count(key)?.unwrap_or(0);
        let chunks: Vec<&[u8]> = value.chunks(CHUNK).collect();
        for (i, chunk) in chunks.iter().enumerate() {
            self.store.put(&format!("{key}#{i}"), chunk)?;
        }
        self.store
            .put(&format!("{key}#n"), chunks.len().to_string().as_bytes())?;
        for stale in chunks.len()..previous {
            self.store.remove(&format!("{key}#{stale}"))?;
        }
        Ok(())
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
        let n = std::str::from_utf8(&raw).ok().and_then(|s| s.parse().ok());
        n.map(Some)
            .ok_or(VaultError::Corrupt("contagem de pedaços"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(rt_len: usize, cookie_len: usize) -> CapturedSession {
        CapturedSession {
            account: AccountInfo {
                upn: "fulano@empresa.com".into(),
                tenant_id: "tid".into(),
                home_account_id: "oid.tid".into(),
                environment: "login.windows.net".into(),
            },
            refresh_token: RefreshToken {
                secret: Secret::new("r".repeat(rt_len)),
                client_id: "client-web".into(),
                family_id: Some("1".into()),
            },
            cookies: vec![SessionCookie {
                name: "ESTSAUTHPERSISTENT".into(),
                value: Secret::new("c".repeat(cookie_len)),
                domain: ".login.microsoftonline.com".into(),
                path: "/".into(),
            }],
        }
    }

    #[test]
    fn roundtrip_large_session_respects_credential_size_limit() {
        let vault = Vault::new(MemoryStore::default());
        let original = session(5_000, 3_000);
        let at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);

        vault.save(&original, at).unwrap();

        assert!(
            vault.store().max_value_len() <= 2560,
            "credencial acima do limite do Windows"
        );
        assert_eq!(vault.load_session().unwrap(), Some(original));
        let stored = vault.active_account().unwrap().unwrap();
        assert_eq!(stored.captured_at, at);
        assert_eq!(stored.cookie_count, 1);
        assert_eq!(stored.family_id.as_deref(), Some("1"));
    }

    #[test]
    fn shorter_resave_removes_stale_chunks() {
        let vault = Vault::new(MemoryStore::default());
        vault.save(&session(9_000, 10), UNIX_EPOCH).unwrap();
        vault.save(&session(100, 10), UNIX_EPOCH).unwrap();

        assert!(!vault
            .store()
            .keys()
            .iter()
            .any(|k| k == "fulano@empresa.com/rt#1"));
        let rt = vault.load_session().unwrap().unwrap().refresh_token.secret;
        assert_eq!(rt.len(), 100);
    }

    /// Manual, no Windows: `cargo test -p f365-auth -- --ignored keyring_selftest` e depois
    /// `cmdkey /list:selftest.fast365` em OUTRO processo — prova que não é o store em memória.
    #[test]
    #[ignore = "toca o Credential Manager real"]
    fn keyring_selftest() {
        let store = KeyringStore;
        let big = vec![b'x'; CHUNK];
        store.put("selftest", &big).unwrap();
        assert_eq!(store.get("selftest").unwrap(), Some(big));
    }

    #[test]
    fn never_paired_is_none() {
        let vault = Vault::new(MemoryStore::default());
        assert_eq!(vault.active_account().unwrap(), None);
        assert_eq!(vault.load_session().unwrap(), None);
    }

    #[test]
    fn missing_chunk_is_reported_as_corrupt() {
        let vault = Vault::new(MemoryStore::default());
        vault.save(&session(5_000, 10), UNIX_EPOCH).unwrap();
        vault.store().remove("fulano@empresa.com/rt#1").unwrap();
        assert!(matches!(vault.load_session(), Err(VaultError::Corrupt(_))));
    }
}
