//! Protocol profile: os valores de protocolo do Teams Web como **dado versionado** (JSON), não código.
//! Mudança da Microsoft que couber aqui é edição de `profile/teams-web.json` (`docs/ARQUITETURA.md`,
//! "perfil declarativo × adaptador").
//!
//! Contratos:
//! - `TOKEN-ENDPOINT-001` (documentado pela Microsoft): RT → access token no endpoint v2.0 do tenant.
//! - `TOKEN-SPA-ORIGIN-001` (UNVERIFIED): o RT do cliente web é de SPA e só é resgatado com
//!   `Origin` igual à origem onde foi emitido (`AADSTS9002327` sem ela). Capturado em
//!   `teams.cloud.microsoft` (Checkpoint A).
//! - `TOKEN-SCOPE-001` (UNVERIFIED): `api.spaces.skype.com` é o `aud` do middle tier do Teams (troca
//!   pelo skype token) e `chatsvcagg.teams.microsoft.com` o do agregador REST. Conferir contra os
//!   escopos observados na captura (`f365 whoami` lista).
//! - `TOKEN-AUTHSVC-001` (UNVERIFIED): `POST authsvc/v1.0/authz` com Bearer do escopo `teams`
//!   devolve `tokens.skypeToken`, `tokens.expiresIn` e `regionGtms.chatService`.

use serde::Deserialize;

const TEAMS_WEB: &str = include_str!("profile/teams-web.json");

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Profile {
    token_endpoint: String,
    pub spa_origin: String,
    pub scopes: Scopes,
    pub authsvc_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Scopes {
    pub teams: String,
    pub chatsvcagg: String,
}

impl Profile {
    pub fn teams_web() -> Self {
        serde_json::from_str(TEAMS_WEB)
            .expect("profile/teams-web.json inválido (coberto por teste)")
    }

    pub fn token_url(&self, tenant_id: &str) -> String {
        self.token_endpoint.replace("{tenant}", tenant_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_profile_parses_and_is_https() {
        let p = Profile::teams_web();
        for url in [&p.token_url("tid"), &p.spa_origin, &p.authsvc_url] {
            assert!(url.starts_with("https://"), "{url}");
        }
        assert_eq!(
            p.token_url("9d63"),
            "https://login.microsoftonline.com/9d63/oauth2/v2.0/token"
        );
        assert!(
            p.scopes.teams.contains("offline_access"),
            "sem offline_access não há RT rotacionado"
        );
        assert!(p.scopes.chatsvcagg.contains("offline_access"));
    }
}
