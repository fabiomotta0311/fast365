//! Sessão capturada do Teams Web e a leitura do cache MSAL — lógica pura, testável em qualquer SO.
//! A WebView (feature `webview`) só coleta as entradas de storage e os cookies; quem decide o que é
//! sessão é este módulo.
//!
//! Contratos de protocolo. Evidência = Checkpoint A manual na conta do dono, 2026-11-13 (#7):
//! - `AUTH-MSAL-CACHE-001` (VERIFIED): o Teams Web guarda o cache MSAL em claro no storage, uma
//!   entrada JSON por credencial/conta. Reconhecemos pelo **conteúdo** (`credentialType`,
//!   `authorityType`), não pelo formato da chave, que muda entre versões do MSAL.js.
//! - `AUTH-MSAL-CACHE-002` (UNVERIFIED, não ocorreu): msal-browser v4+ pode **criptografar** o
//!   cache (`{id, nonce, data}`) → erro explícito, nunca "captura vazia". Mantido como guarda.
//! - `AUTH-RT-SPA-001` (PARCIAL): VERIFIED que o RT é do cliente **web** (`5e3ce6c0-…`, sem
//!   `familyId`) — não do nativo `1fec8e78-…`. UNVERIFIED: validade fixa de ~24 h e vínculo à
//!   origem; medir na troca de tokens (#8). Risco ao Checkpoint F; mitigação = perfil persistente
//!   da WebView (cookies `ESTSAUTHPERSISTENT`) para re-login silencioso.
//! - `AUTH-COOKIES-001` (VERIFIED): `login.microsoftonline.com` entrega cookies de sessão do Entra
//!   (`ESTSAUTH*`/`buid`/`esctx`; 5 na captura, > 2560 B no total → exige o vault em pedaços).

use crate::secret::Secret;
use serde_json::Value;

/// Conta capturada (não secreta). `upn` é o `username` do MSAL (ex.: `fulano@empresa.com`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountInfo {
    pub upn: String,
    pub tenant_id: String,
    pub home_account_id: String,
    pub environment: String,
}

/// Refresh token e o cliente que o emitiu (necessário para a troca de tokens, #8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshToken {
    pub secret: Secret,
    pub client_id: String,
    /// `Some` se for um FOCI/family refresh token (pode servir a outros clientes da família).
    pub family_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCookie {
    pub name: String,
    pub value: Secret,
    pub domain: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedSession {
    pub account: AccountInfo,
    pub refresh_token: RefreshToken,
    pub cookies: Vec<SessionCookie>,
}

/// Resultado de uma leitura do storage da página.
#[derive(Debug, PartialEq, Eq)]
pub enum MsalProbe {
    /// Conta + refresh token encontrados.
    Found(AccountInfo, RefreshToken),
    /// Ainda não há sessão (usuário logando). `encrypted` = há entradas criptografadas.
    Pending { encrypted: bool },
}

/// Lê as entradas `(chave, valor)` coletadas do storage da página.
pub fn read_msal_cache<'a>(entries: impl IntoIterator<Item = (&'a str, &'a str)>) -> MsalProbe {
    let mut accounts = Vec::new();
    let mut tokens = Vec::new();
    let mut encrypted = false;

    for (_key, raw) in entries {
        let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(raw) else {
            continue;
        };
        let text = |field: &str| obj.get(field).and_then(Value::as_str).map(str::to_owned);

        if is_encrypted_entry(&obj) {
            encrypted = true;
        } else if text("credentialType").is_some_and(|t| t.eq_ignore_ascii_case("RefreshToken")) {
            if let (Some(secret), Some(home), Some(client_id)) =
                (text("secret"), text("homeAccountId"), text("clientId"))
            {
                if !secret.is_empty() {
                    let family_id = text("familyId").filter(|f| !f.is_empty());
                    tokens.push((
                        home,
                        RefreshToken {
                            secret: Secret::new(secret),
                            client_id,
                            family_id,
                        },
                    ));
                }
            }
        } else if text("authorityType").is_some() {
            if let (Some(upn), Some(home), Some(environment), Some(tenant_id)) = (
                text("username"),
                text("homeAccountId"),
                text("environment"),
                text("realm").or_else(|| text("tenantId")),
            ) {
                accounts.push(AccountInfo {
                    upn,
                    tenant_id,
                    home_account_id: home,
                    environment,
                });
            }
        }
    }

    // Prefere o family RT (mais útil para a troca de tokens); depois qualquer um com conta casada.
    tokens.sort_by_key(|(_, rt)| rt.family_id.is_none());
    for (home, rt) in tokens {
        if let Some(account) = accounts.iter().find(|a| a.home_account_id == home) {
            return MsalProbe::Found(account.clone(), rt);
        }
    }
    MsalProbe::Pending { encrypted }
}

fn is_encrypted_entry(obj: &serde_json::Map<String, Value>) -> bool {
    ["id", "nonce", "data"]
        .iter()
        .all(|k| obj.get(*k).is_some_and(Value::is_string))
        && !obj.contains_key("credentialType")
}

/// Mantém só os cookies de sessão do Entra (`AUTH-COOKIES-001`).
pub fn is_session_cookie(name: &str) -> bool {
    name.starts_with("ESTSAUTH") || name == "buid" || name == "esctx"
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "oid-123.tid-456";

    fn account(home: &str, upn: &str) -> String {
        serde_json::json!({
            "homeAccountId": home, "environment": "login.windows.net", "realm": "tid-456",
            "localAccountId": "oid-123", "username": upn, "authorityType": "MSSTS", "name": "Fulano"
        })
        .to_string()
    }

    fn rt(home: &str, client: &str, family: Option<&str>, secret: &str) -> String {
        let mut v = serde_json::json!({
            "credentialType": "RefreshToken", "homeAccountId": home,
            "environment": "login.windows.net", "clientId": client, "secret": secret
        });
        if let Some(f) = family {
            v["familyId"] = f.into();
        }
        v.to_string()
    }

    fn probe(entries: &[(&str, String)]) -> MsalProbe {
        read_msal_cache(entries.iter().map(|(k, v)| (*k, v.as_str())))
    }

    #[test]
    fn finds_account_and_refresh_token_by_content() {
        let got = probe(&[
            ("qualquer-chave-1", account(HOME, "fulano@empresa.com")),
            (
                "qualquer-chave-2",
                rt(HOME, "client-web", None, "rt-secreto"),
            ),
            ("lixo", "não é json".into()),
        ]);
        let MsalProbe::Found(acc, token) = got else {
            panic!("esperava sessão, veio {got:?}")
        };
        assert_eq!(acc.upn, "fulano@empresa.com");
        assert_eq!(acc.tenant_id, "tid-456");
        assert_eq!(token.secret.expose(), "rt-secreto");
        assert_eq!(token.client_id, "client-web");
    }

    #[test]
    fn prefers_family_refresh_token() {
        let got = probe(&[
            ("a", account(HOME, "fulano@empresa.com")),
            ("b", rt(HOME, "client-a", None, "rt-comum")),
            ("c", rt(HOME, "client-b", Some("1"), "rt-familia")),
        ]);
        let MsalProbe::Found(_, token) = got else {
            panic!()
        };
        assert_eq!(token.secret.expose(), "rt-familia");
        assert_eq!(token.family_id.as_deref(), Some("1"));
    }

    #[test]
    fn token_without_matching_account_is_not_a_session() {
        let got = probe(&[
            ("a", account("outra-conta", "x@y.com")),
            ("b", rt(HOME, "client", None, "rt")),
        ]);
        assert_eq!(got, MsalProbe::Pending { encrypted: false });
    }

    #[test]
    fn empty_secret_is_ignored() {
        let got = probe(&[
            ("a", account(HOME, "f@e.com")),
            ("b", rt(HOME, "c", None, "")),
        ]);
        assert_eq!(got, MsalProbe::Pending { encrypted: false });
    }

    #[test]
    fn detects_encrypted_cache() {
        let enc = serde_json::json!({"id": "k1", "nonce": "n", "data": "base64...", "lastUpdatedAt": "1"});
        let got = probe(&[
            ("msal.2|x", enc.to_string()),
            ("a", account(HOME, "f@e.com")),
        ]);
        assert_eq!(got, MsalProbe::Pending { encrypted: true });
    }

    #[test]
    fn session_cookie_filter() {
        for ok in [
            "ESTSAUTH",
            "ESTSAUTHPERSISTENT",
            "ESTSAUTHLIGHT",
            "buid",
            "esctx",
        ] {
            assert!(is_session_cookie(ok), "{ok}");
        }
        for no in ["MUID", "fpc", "x-ms-gateway-slice", "estsauth"] {
            assert!(!is_session_cookie(no), "{no}");
        }
    }
}
