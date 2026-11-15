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
//! - `AUTH-MSAL-ACTIVE-001` (UNVERIFIED): a conta escolhida na página fica em
//!   `msal.<clientId>.active-account-filters` (JSON `homeAccountId`/`localAccountId`/`tenantId`) ou,
//!   no MSAL v2, `msal.<clientId>.active-account` (`localAccountId` em texto).
//! - `AUTH-RT-SPA-001` (VERIFIED, 2026-11-15): o RT é do cliente **web** (`5e3ce6c0-…`, sem
//!   `familyId`) e tem **validade fixa**: login em 13/11 23:04 → recusado com `AADSTS700084` em
//!   15/11, mesmo com resgates bem-sucedidos no meio (rotação não estende). O `f365 auth` seguinte
//!   **re-logou sem senha** pelos cookies do perfil persistente da WebView → base do
//!   re-pareamento automático (`TokenBroker::with_repair` + captura `silent`), o mesmo caminho
//!   do prior art (rusty-teams recarrega a página e deixa o MSAL dela re-logar).
//! - `AUTH-COOKIES-001` (VERIFIED): `login.microsoftonline.com` entrega cookies de sessão do Entra
//!   (`ESTSAUTH*`/`buid`/`esctx`; 5 na captura, > 2560 B no total → exige o vault em pedaços).
//! - `AUTH-ORIGIN-001`: só aceitamos storage lido nas origens HTTPS exatas do Teams Web
//!   ([`TRUSTED_ORIGINS`]) — o login pode passar por outros hosts (federação), mas nenhum deles
//!   pode "entregar" uma sessão.
//!
//! Regra de identidade: a sessão é a da **conta ativa da página**. Sem indicação ativa, só se aceita
//! quando houver uma única identidade inequívoca; havendo dúvida, falha fechada — nunca escolher
//! outra conta por preferência de token.

use crate::secret::Secret;
use serde_json::{Map, Value};

/// Origens (esquema + host, sem barra final) em que o storage pode virar sessão.
pub(crate) const TRUSTED_ORIGINS: &[&str] = &[
    "https://teams.cloud.microsoft",
    "https://teams.microsoft.com",
];

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
pub(crate) struct RefreshToken {
    pub secret: Secret,
    pub client_id: String,
    /// `Some` se for um FOCI/family refresh token (pode servir a outros clientes da família).
    pub family_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionCookie {
    pub name: String,
    pub value: Secret,
    pub domain: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapturedSession {
    pub account: AccountInfo,
    pub refresh_token: RefreshToken,
    pub cookies: Vec<SessionCookie>,
    /// Escopos (`target`) dos access tokens que a própria página pediu — só metadado, sem token.
    pub observed_scopes: Vec<String>,
    /// Último resgate bem-sucedido do RT.
    pub last_refresh: Option<std::time::SystemTime>,
}

/// O que impede concluir a captura (se persistir, vira erro explícito).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Blocker {
    /// Cache MSAL criptografado (`AUTH-MSAL-CACHE-002`).
    Encrypted,
    /// Mais de uma identidade possível e nenhuma indicação inequívoca de qual é a ativa.
    AmbiguousAccount,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MsalProbe {
    /// Conta ativa + refresh token dela + escopos observados nos access tokens da página.
    Found(AccountInfo, RefreshToken, Vec<String>),
    /// Ainda não há sessão utilizável.
    Pending(Option<Blocker>),
}

#[derive(Debug, Clone)]
struct Account {
    info: AccountInfo,
    local_account_id: Option<String>,
}

#[derive(Debug, Default)]
struct ActiveHint {
    home: Option<String>,
    local: Option<String>,
    tenant: Option<String>,
}

impl ActiveHint {
    fn matches(&self, a: &Account) -> bool {
        let id = match (&self.home, &self.local) {
            (Some(h), _) => *h == a.info.home_account_id,
            (None, Some(l)) => a.local_account_id.as_deref() == Some(l.as_str()),
            (None, None) => false,
        };
        id && self.tenant.as_ref().is_none_or(|t| *t == a.info.tenant_id)
    }
}

/// Lê as entradas `(chave, valor)` coletadas do storage da página.
pub(crate) fn read_msal_cache<'a>(
    entries: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> MsalProbe {
    let mut accounts: Vec<Account> = Vec::new();
    let mut tokens: Vec<(String, String, RefreshToken)> = Vec::new(); // (home, environment, rt)
    let mut hints: Vec<ActiveHint> = Vec::new();
    let mut targets: Vec<(String, String, String)> = Vec::new(); // (home, environment, escopos)
    let mut encrypted = false;

    for (key, raw) in entries {
        let parsed = serde_json::from_str::<Value>(raw).ok();
        if key.contains("active-account") {
            hints.extend(active_hint(key, raw, parsed.as_ref()));
            continue;
        }
        let Some(Value::Object(obj)) = parsed else {
            continue;
        };
        let text = |field: &str| obj.get(field).and_then(Value::as_str).map(str::to_owned);

        if is_encrypted_entry(&obj) {
            encrypted = true;
        } else if text("credentialType").is_some_and(|t| t.eq_ignore_ascii_case("RefreshToken")) {
            if let (Some(secret), Some(home), Some(env), Some(client_id)) = (
                text("secret"),
                text("homeAccountId"),
                text("environment"),
                text("clientId"),
            ) {
                if !secret.is_empty() {
                    let family_id = text("familyId").filter(|f| !f.is_empty());
                    let rt = RefreshToken {
                        secret: Secret::new(secret),
                        client_id,
                        family_id,
                    };
                    tokens.push((home, env, rt));
                }
            }
        } else if text("credentialType")
            .is_some_and(|t| t.to_ascii_lowercase().starts_with("accesstoken"))
        {
            // Só o `target` (escopos): o `secret` do access token nunca é lido.
            if let (Some(home), Some(env), Some(target)) =
                (text("homeAccountId"), text("environment"), text("target"))
            {
                targets.push((home, env, target));
            }
        } else if text("authorityType").is_some() {
            if let (Some(upn), Some(home), Some(environment), Some(tenant_id)) = (
                text("username"),
                text("homeAccountId"),
                text("environment"),
                text("realm").or_else(|| text("tenantId")),
            ) {
                let info = AccountInfo {
                    upn,
                    tenant_id,
                    home_account_id: home,
                    environment,
                };
                if !accounts.iter().any(|a| a.info == info) {
                    accounts.push(Account {
                        info,
                        local_account_id: text("localAccountId"),
                    });
                }
            }
        }
    }

    let account = match select_account(&accounts, &hints) {
        Selection::One(a) => a,
        Selection::None => return MsalProbe::Pending(encrypted.then_some(Blocker::Encrypted)),
        Selection::Ambiguous => return MsalProbe::Pending(Some(Blocker::AmbiguousAccount)),
    };

    // Só RTs da MESMA identidade e ambiente; entre eles, o family RT é o mais útil (#8).
    let mut compatible: Vec<RefreshToken> = tokens
        .into_iter()
        .filter(|(home, env, _)| *home == account.home_account_id && *env == account.environment)
        .map(|(_, _, rt)| rt)
        .collect();
    compatible.sort_by(|a, b| {
        (a.family_id.is_none(), &a.client_id).cmp(&(b.family_id.is_none(), &b.client_id))
    });
    let mut scopes: Vec<String> = targets
        .into_iter()
        .filter(|(home, env, _)| *home == account.home_account_id && *env == account.environment)
        .map(|(_, _, t)| t)
        .collect();
    scopes.sort();
    scopes.dedup();
    match compatible.into_iter().next() {
        Some(rt) => MsalProbe::Found(account, rt, scopes),
        None => MsalProbe::Pending(encrypted.then_some(Blocker::Encrypted)),
    }
}

enum Selection {
    One(AccountInfo),
    None,
    Ambiguous,
}

fn select_account(accounts: &[Account], hints: &[ActiveHint]) -> Selection {
    let candidates: Vec<&Account> = if hints.is_empty() {
        accounts.iter().collect()
    } else {
        accounts
            .iter()
            .filter(|a| hints.iter().any(|h| h.matches(a)))
            .collect()
    };
    let Some(first) = candidates.first() else {
        return Selection::None;
    };

    if candidates
        .iter()
        .any(|a| a.info.home_account_id != first.info.home_account_id)
    {
        return Selection::Ambiguous; // identidades diferentes
    }
    if candidates.len() == 1 {
        return Selection::One(first.info.clone());
    }
    // Mesma identidade em vários tenants (convidado) e sem tenant indicado: só o tenant de origem
    // (`homeAccountId` = `<oid>.<tid>`) é inequívoco.
    let home_tid = first
        .info
        .home_account_id
        .rsplit('.')
        .next()
        .unwrap_or_default();
    let mut home_realm = candidates.iter().filter(|a| a.info.tenant_id == home_tid);
    match (home_realm.next(), home_realm.next()) {
        (Some(a), None) => Selection::One(a.info.clone()),
        _ => Selection::Ambiguous,
    }
}

fn active_hint(key: &str, raw: &str, parsed: Option<&Value>) -> Option<ActiveHint> {
    let text = |obj: &Map<String, Value>, f: &str| {
        obj.get(f)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    match parsed {
        Some(Value::Object(obj)) => Some(ActiveHint {
            home: text(obj, "homeAccountId"),
            local: text(obj, "localAccountId"),
            tenant: text(obj, "tenantId").or_else(|| text(obj, "realm")),
        })
        .filter(|h| h.home.is_some() || h.local.is_some()),
        // MSAL v2: o valor é o `localAccountId` em texto (às vezes entre aspas JSON).
        Some(Value::String(s)) if !s.is_empty() && key.ends_with("active-account") => {
            Some(ActiveHint {
                local: Some(s.clone()),
                ..Default::default()
            })
        }
        None if !raw.is_empty() && key.ends_with("active-account") => Some(ActiveHint {
            local: Some(raw.to_owned()),
            ..Default::default()
        }),
        _ => None,
    }
}

fn is_encrypted_entry(obj: &Map<String, Value>) -> bool {
    ["id", "nonce", "data"]
        .iter()
        .all(|k| obj.get(*k).is_some_and(Value::is_string))
        && !obj.contains_key("credentialType")
}

/// Mantém só os cookies de sessão do Entra (`AUTH-COOKIES-001`).
pub(crate) fn is_session_cookie(name: &str) -> bool {
    name.starts_with("ESTSAUTH") || name == "buid" || name == "esctx"
}

/// Origem (`https://host[:porta]`) de uma URL absoluta, no formato de `location.origin`.
pub(crate) fn origin_of(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let origin = u.origin();
    origin.is_tuple().then(|| origin.ascii_serialization())
}

/// `AUTH-ORIGIN-001`: comparação exata — subdomínio, outro esquema ou porta não passam.
pub(crate) fn is_trusted_origin(origin: &str) -> bool {
    TRUSTED_ORIGINS.contains(&origin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HOME_A: &str = "oid-a.tid-a";
    const HOME_B: &str = "oid-b.tid-b";
    const ENV: &str = "login.windows.net";

    fn account(home: &str, realm: &str, upn: &str) -> String {
        let local = home.split('.').next().unwrap();
        json!({"homeAccountId": home, "environment": ENV, "realm": realm, "localAccountId": local,
               "username": upn, "authorityType": "MSSTS"})
        .to_string()
    }

    fn rt(home: &str, client: &str, family: Option<&str>, secret: &str) -> String {
        let mut v = json!({"credentialType": "RefreshToken", "homeAccountId": home,
                           "environment": ENV, "clientId": client, "secret": secret});
        if let Some(f) = family {
            v["familyId"] = f.into();
        }
        v.to_string()
    }

    fn active(home: &str, tenant: Option<&str>) -> (&'static str, String) {
        let local = home.split('.').next().unwrap();
        let mut v = json!({"homeAccountId": home, "localAccountId": local});
        if let Some(t) = tenant {
            v["tenantId"] = t.into();
        }
        ("msal.client-web.active-account-filters", v.to_string())
    }

    fn probe(entries: &[(&str, String)]) -> MsalProbe {
        read_msal_cache(entries.iter().map(|(k, v)| (*k, v.as_str())))
    }

    fn found_upn(p: MsalProbe) -> (String, String) {
        match p {
            MsalProbe::Found(a, t, _) => (a.upn, t.secret.expose().to_owned()),
            other => panic!("esperava sessão, veio {other:?}"),
        }
    }

    #[test]
    fn single_identity_without_hint_is_accepted() {
        let got = probe(&[
            ("k1", account(HOME_A, "tid-a", "a@empresa.com")),
            ("k2", rt(HOME_A, "client-web", None, "rt-a")),
            ("lixo", "não é json".into()),
        ]);
        assert_eq!(found_upn(got), ("a@empresa.com".into(), "rt-a".into()));
    }

    /// Achado do review: conta antiga com family RT não pode vencer a conta ativa.
    #[test]
    fn active_account_wins_over_stale_family_token() {
        let (k, v) = active(HOME_B, None);
        let mut entries = vec![
            ("a", account(HOME_A, "tid-a", "antiga@empresa.com")),
            ("b", account(HOME_B, "tid-b", "nova@empresa.com")),
            (
                "rt-a",
                rt(HOME_A, "client-web", Some("1"), "rt-antiga-familia"),
            ),
            ("rt-b", rt(HOME_B, "client-web", None, "rt-nova")),
            (k, v),
        ];
        assert_eq!(
            found_upn(probe(&entries)),
            ("nova@empresa.com".into(), "rt-nova".into())
        );
        entries.reverse(); // ordem do storage não importa
        assert_eq!(
            found_upn(probe(&entries)),
            ("nova@empresa.com".into(), "rt-nova".into())
        );
    }

    #[test]
    fn active_account_without_its_own_token_stays_pending() {
        let (k, v) = active(HOME_B, None);
        let got = probe(&[
            ("a", account(HOME_A, "tid-a", "antiga@empresa.com")),
            ("b", account(HOME_B, "tid-b", "nova@empresa.com")),
            ("rt-a", rt(HOME_A, "client-web", Some("1"), "rt-antiga")),
            (k, v),
        ]);
        assert_eq!(got, MsalProbe::Pending(None));
    }

    #[test]
    fn two_identities_without_hint_fail_closed() {
        let got = probe(&[
            ("a", account(HOME_A, "tid-a", "a@empresa.com")),
            ("b", account(HOME_B, "tid-b", "b@empresa.com")),
            ("rt-a", rt(HOME_A, "c", None, "rt-a")),
            ("rt-b", rt(HOME_B, "c", None, "rt-b")),
        ]);
        assert_eq!(got, MsalProbe::Pending(Some(Blocker::AmbiguousAccount)));
    }

    #[test]
    fn guest_tenants_resolve_to_home_realm_or_to_hinted_tenant() {
        let base = vec![
            ("home", account(HOME_A, "tid-a", "a@empresa.com")),
            ("guest", account(HOME_A, "tid-guest", "a@empresa.com")),
            ("rt", rt(HOME_A, "client-web", None, "rt-a")),
        ];
        let MsalProbe::Found(acc, _, _) = probe(&base) else {
            panic!()
        };
        assert_eq!(acc.tenant_id, "tid-a");

        let mut hinted = base.clone();
        let (k, v) = active(HOME_A, Some("tid-guest"));
        hinted.push((k, v));
        let MsalProbe::Found(acc, _, _) = probe(&hinted) else {
            panic!()
        };
        assert_eq!(acc.tenant_id, "tid-guest");
    }

    #[test]
    fn msal_v2_plain_text_active_account() {
        let got = probe(&[
            ("a", account(HOME_A, "tid-a", "a@empresa.com")),
            ("b", account(HOME_B, "tid-b", "b@empresa.com")),
            ("rt-a", rt(HOME_A, "c", Some("1"), "rt-a")),
            ("rt-b", rt(HOME_B, "c", None, "rt-b")),
            ("msal.client-web.active-account", "oid-b".into()),
        ]);
        assert_eq!(found_upn(got).0, "b@empresa.com");
    }

    #[test]
    fn token_from_other_environment_is_not_used() {
        let mut other_env: Value =
            serde_json::from_str(&rt(HOME_A, "c", None, "rt-sovereign")).unwrap();
        other_env["environment"] = "login.microsoftonline.us".into();
        let got = probe(&[
            ("a", account(HOME_A, "tid-a", "a@empresa.com")),
            ("rt", other_env.to_string()),
        ]);
        assert_eq!(got, MsalProbe::Pending(None));
    }

    #[test]
    fn prefers_family_token_of_the_same_identity() {
        let got = probe(&[
            ("a", account(HOME_A, "tid-a", "a@empresa.com")),
            ("rt1", rt(HOME_A, "client-a", None, "rt-comum")),
            ("rt2", rt(HOME_A, "client-b", Some("1"), "rt-familia")),
        ]);
        assert_eq!(found_upn(got).1, "rt-familia");
    }

    #[test]
    fn empty_secret_is_ignored() {
        let got = probe(&[
            ("a", account(HOME_A, "tid-a", "a@e.com")),
            ("b", rt(HOME_A, "c", None, "")),
        ]);
        assert_eq!(got, MsalProbe::Pending(None));
    }

    #[test]
    fn detects_encrypted_cache() {
        let enc = json!({"id": "k1", "nonce": "n", "data": "base64...", "lastUpdatedAt": "1"});
        let got = probe(&[
            ("msal.2|x", enc.to_string()),
            ("a", account(HOME_A, "tid-a", "a@e.com")),
        ]);
        assert_eq!(got, MsalProbe::Pending(Some(Blocker::Encrypted)));
    }

    #[test]
    fn records_scopes_of_page_access_tokens_without_reading_them() {
        let at = |home: &str, target: &str| {
            json!({"credentialType": "AccessToken", "homeAccountId": home, "environment": ENV,
                   "clientId": "client-web", "target": target, "secret": "at-segredo"})
            .to_string()
        };
        let got = probe(&[
            ("a", account(HOME_A, "tid-a", "a@empresa.com")),
            ("rt", rt(HOME_A, "client-web", None, "rt-a")),
            ("at1", at(HOME_A, "https://api.spaces.skype.com/.default")),
            ("at2", at(HOME_A, "https://api.spaces.skype.com/.default")),
            (
                "at3",
                at(HOME_A, "https://chatsvcagg.teams.microsoft.com/.default"),
            ),
            ("outra", at(HOME_B, "https://outlook.office.com/.default")),
        ]);
        let MsalProbe::Found(_, _, scopes) = got else {
            panic!("{got:?}")
        };
        assert_eq!(
            scopes,
            [
                "https://api.spaces.skype.com/.default",
                "https://chatsvcagg.teams.microsoft.com/.default"
            ]
        );
        assert!(!format!("{scopes:?}").contains("segredo"));
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

    #[test]
    fn origin_policy_is_exact() {
        for ok in [
            "https://teams.cloud.microsoft/v2/",
            "https://teams.microsoft.com/_#/",
        ] {
            assert!(is_trusted_origin(&origin_of(ok).unwrap()), "{ok}");
        }
        for no in [
            "http://teams.cloud.microsoft/",
            "https://evil.teams.cloud.microsoft/",
            "https://teams.cloud.microsoft.evil.com/",
            "https://teams.microsoft.com:8443/",
            "https://login.microsoftonline.com/",
            "about:blank",
        ] {
            assert!(
                !origin_of(no).is_some_and(|o| is_trusted_origin(&o)),
                "{no}"
            );
        }
    }
}
