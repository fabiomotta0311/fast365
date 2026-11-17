//! Protocol profile: os valores de protocolo do Teams Web como **dado versionado** (JSON), não código.
//! O perfil padrão é embutido; `Profile::load` permite carregar JSON externo explicitamente e
//! `TokenBroker::with_profile` o aplica ao executor. O chamador escolhe o arquivo na inicialização;
//! não há recarga automática. Somente versões numéricas do caminho authsvc são configuráveis
//! atualmente; destinos de credenciais, scopes e demais valores têm allowlist estrita.
//! Transformação de payload e framing continuam no adaptador (`docs/ARQUITETURA.md`, Fase 2).
//!
//! Contratos. Evidência = Checkpoint B na conta do dono, 2026-11-13 (#8), via `RustTransport`:
//! - `TOKEN-ENDPOINT-001` (documentado + VERIFIED): RT → access token no endpoint v2.0 do tenant;
//!   cada resgate devolve RT novo (rotação).
//! - `TOKEN-SPA-ORIGIN-001` (PARCIAL): VERIFIED que o RT do cliente web (`5e3ce6c0-…`) é resgatado
//!   **com** `Origin: https://teams.cloud.microsoft`. UNVERIFIED que seja obrigatório (não testamos
//!   sem); sem ele o Entra responderia `AADSTS9002327`, que já é diagnosticado.
//! - `TOKEN-SCOPE-001` (VERIFIED): `api.spaces.skype.com/.default` → `aud https://api.spaces.skype.com`
//!   (`scp` Authorization.ReadWrite Region.ReadWrite user_impersonation);
//!   `chatsvcagg.teams.microsoft.com/.default` → `aud https://chatsvcagg.teams.microsoft.com`.
//! - `TOKEN-AUTHSVC-001` (VERIFIED): `POST authsvc/v1.0/authz` com Bearer do escopo `teams` devolve
//!   `tokens.skypeToken`, `tokens.expiresIn` (~2,5 h observado) e `regionGtms.chatService`
//!   (`https://<região>.ng.msg.teams.microsoft.com`).

use serde::Deserialize;

const TEAMS_WEB: &str = include_str!("profile/teams-web.json");

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    schema_version: u32,
    #[serde(rename = "_comment")]
    _comment: Option<String>,
    token_endpoint: String,
    pub(crate) spa_origin: String,
    pub(crate) scopes: Scopes,
    pub(crate) authsvc_url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Scopes {
    pub teams: String,
    pub chatsvcagg: String,
}

#[derive(Debug, thiserror::Error)]
#[error("perfil de protocolo inválido ou indisponível")]
pub struct ProfileError;

impl Profile {
    /// Carrega explicitamente um arquivo externo. Erros não expõem seu conteúdo ou caminho.
    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self, ProfileError> {
        use std::io::Read;
        let file = std::fs::File::open(path).map_err(|_| ProfileError)?;
        let mut bytes = Vec::new();
        file.take(65537)
            .read_to_end(&mut bytes)
            .map_err(|_| ProfileError)?;
        if bytes.len() > 65536 {
            return Err(ProfileError);
        }
        let profile: Self = serde_json::from_slice(&bytes).map_err(|_| ProfileError)?;
        profile.validate()?;
        Ok(profile)
    }

    pub(crate) fn validate(&self) -> Result<(), ProfileError> {
        let bundled: Self = serde_json::from_str(TEAMS_WEB).map_err(|_| ProfileError)?;
        if self.schema_version != 1
            || self.token_endpoint != bundled.token_endpoint
            || self.spa_origin != bundled.spa_origin
            || self.scopes.teams != bundled.scopes.teams
            || self.scopes.chatsvcagg != bundled.scopes.chatsvcagg
        {
            return Err(ProfileError);
        }
        // Somente a versão numérica do caminho conhecido pode variar; host e demais
        // componentes são literais para impedir redirecionar credenciais.
        let Some(version) = self
            .authsvc_url
            .strip_prefix("https://teams.microsoft.com/api/authsvc/v")
            .and_then(|s| s.strip_suffix("/authz"))
        else {
            return Err(ProfileError);
        };
        let Some((major, minor)) = version.split_once('.') else {
            return Err(ProfileError);
        };
        if major.is_empty()
            || minor.is_empty()
            || major.len() > 3
            || minor.len() > 3
            || !major.bytes().all(|b| b.is_ascii_digit())
            || !minor.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(ProfileError);
        }
        Ok(())
    }

    pub fn teams_web() -> Self {
        let profile: Self = serde_json::from_str(TEAMS_WEB)
            .expect("profile/teams-web.json inválido (coberto por teste)");
        profile.validate().expect("profile/teams-web.json inseguro");
        profile
    }

    pub fn token_url(&self, tenant_id: &str) -> Option<String> {
        if tenant_id.is_empty()
            || tenant_id.len() > 255
            || !tenant_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.')
            || tenant_id == "."
            || tenant_id == ".."
        {
            return None;
        }
        Some(self.token_endpoint.replace("{tenant}", tenant_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_untrusted_configuration_and_tenant() {
        let original: serde_json::Value = serde_json::from_str(TEAMS_WEB).unwrap();
        for (key, value) in [
            ("schema_version", serde_json::json!(2)),
            (
                "token_endpoint",
                serde_json::json!("https://evil.example/{tenant}/oauth2/v2.0/token"),
            ),
            ("spa_origin", serde_json::json!("https://evil.example")),
            (
                "authsvc_url",
                serde_json::json!(
                    "https://teams.microsoft.com.evil.example/api/authsvc/v2.0/authz"
                ),
            ),
            (
                "authsvc_url",
                serde_json::json!("https://teams.microsoft.com/api/authsvc/v2.0/authz?x=secret"),
            ),
            (
                "authsvc_url",
                serde_json::json!("https://teams.microsoft.com/api/authsvc/v2.0/../authz"),
            ),
            ("unknown", serde_json::json!("secret")),
        ] {
            let mut input = original.clone();
            input[key] = value;
            let parsed = serde_json::from_value::<Profile>(input);
            assert!(parsed.map_or(true, |p| p.validate().is_err()), "{key}");
        }
        let mut input = original;
        input["scopes"]["teams"] = serde_json::json!("https://evil.example/.default");
        assert!(serde_json::from_value::<Profile>(input)
            .unwrap()
            .validate()
            .is_err());
        let profile = Profile::teams_web();
        for tenant in ["../evil", "..", "evil?x", "a/b", "a%2fb", "", "a\\b"] {
            assert!(profile.token_url(tenant).is_none(), "{tenant}");
        }
    }

    #[test]
    fn bundled_profile_parses_and_is_https() {
        let p = Profile::teams_web();
        for url in [&p.token_url("tid").unwrap(), &p.spa_origin, &p.authsvc_url] {
            assert!(url.starts_with("https://"), "{url}");
        }
        assert_eq!(
            p.token_url("9d63").unwrap(),
            "https://login.microsoftonline.com/9d63/oauth2/v2.0/token"
        );
        assert!(
            p.scopes.teams.contains("offline_access"),
            "sem offline_access não há RT rotacionado"
        );
        assert!(p.scopes.chatsvcagg.contains("offline_access"));
    }
}
