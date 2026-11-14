//! Protocol profile: os valores de protocolo do Teams Web como **dado versionado** (JSON), não código.
//! Mudança da Microsoft que couber aqui é edição de `profile/teams-web.json` (`docs/ARQUITETURA.md`,
//! "perfil declarativo × adaptador").
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
