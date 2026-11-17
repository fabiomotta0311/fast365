//! Troca de tokens (`docs/ARQUITETURA.md` §2.2, Checkpoint B): RT do keyring → access token do
//! recurso → skype token + host regional do chat. Tudo silencioso: a WebView não reabre.
//!
//! O token **não sai deste crate**: a API pública devolve só claims e metadados ([`TokenInfo`],
//! [`ChatEndpoint`]); os segredos ficam em `pub(crate)` para o executor (#9).
//!
//! Cada resgate rotaciona o RT; o novo vai para o keyring como geração nova (mesmas garantias do
//! vault), preservando o instante do login — base para medir a validade do RT de SPA
//! (`AUTH-RT-SPA-001`). Resgates são serializados no processo: dois em paralelo gastariam o mesmo RT.
//! Contratos de protocolo no módulo `profile` (protocol profile `profile/teams-web.json`).

use crate::clock::Clock;
use crate::profile::Profile;
use crate::secret::Secret;
use crate::transport::{HttpRequest, Transport, TransportError};
use crate::vault::{Vault, VaultError};
use base64::Engine;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use zeroize::{Zeroize, Zeroizing};

/// Renova antes de expirar, para uma operação não começar com token prestes a cair.
const REFRESH_MARGIN: Duration = Duration::from_secs(300);

/// Recursos com access token do Entra.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenResource {
    /// Middle tier do Teams (`api.spaces.skype.com`): troca pelo skype token.
    Teams,
    /// Agregador REST de conversas (`chatsvcagg`).
    ChatSvcAgg,
}

impl TokenResource {
    fn scope(self, profile: &Profile) -> &str {
        match self {
            Self::Teams => &profile.scopes.teams,
            Self::ChatSvcAgg => &profile.scopes.chatsvcagg,
        }
    }
}

/// Claims **não secretos** de um JWT (decodificados, não validados — quem valida é o servidor).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenClaims {
    pub audience: Option<String>,
    pub tenant_id: Option<String>,
    pub scopes: Option<String>,
    pub app_id: Option<String>,
    pub upn: Option<String>,
    pub expires_at: Option<SystemTime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenInfo {
    pub resource: TokenResource,
    pub claims: TokenClaims,
    pub expires_at: SystemTime,
    /// O resgate que produziu este token rotacionou o RT no keyring.
    pub rotated_refresh_token: bool,
}

/// Onde fica o serviço de chat da conta (descoberto no `authsvc`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatEndpoint {
    pub region: Option<String>,
    pub chat_service: String,
    pub expires_at: SystemTime,
}

#[derive(Debug, Clone)]
pub(crate) struct AccessToken {
    pub secret: Secret,
    valid_until: Instant,
    pub info: TokenInfo,
}

#[derive(Debug, Clone)]
pub(crate) struct SkypeToken {
    pub secret: Secret,
    valid_until: Instant,
    pub endpoint: ChatEndpoint,
}

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("nenhuma sessão salva neste usuário do Windows — rode `f365 auth`")]
    NotPaired,
    #[error(transparent)]
    Vault(#[from] VaultError),
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(
        "o Entra recusou o refresh token ({code}){}: rode `f365 auth` — o perfil da WebView costuma \
         re-logar sem pedir senha",
        if code == "AADSTS700084" { " — RT de SPA passou da validade fixa (AUTH-RT-SPA-001)" } else { "" }
    )]
    RefreshRejected { code: String },
    #[error(
        "o RT é de SPA e o Entra exige o cabeçalho Origin da página que o emitiu (AADSTS9002327, \
         TOKEN-SPA-ORIGIN-001): revise `spa_origin` no protocol profile"
    )]
    SpaOrigin,
    #[error("{service} respondeu HTTP {status}{}", code.as_deref().map(|c| format!(" ({c})")).unwrap_or_default())]
    Http {
        service: &'static str,
        status: u16,
        code: Option<String>,
    },
    #[error("resposta inesperada de {0} — o contrato de protocolo mudou?")]
    Malformed(&'static str),
    /// O RT venceu e o re-pareamento silencioso não conseguiu outro.
    #[error("a sessão expirou e o re-login automático falhou ({0}): rode `f365 auth`")]
    RepairFailed(String),
}

/// Re-pareamento automático da sessão (`AUTH-RT-SPA-001`): o RT do cliente web vale ~24 h fixas;
/// quando o Entra o recusa, quem implementa isto recaptura a sessão (ex.: WebView escondida com o
/// perfil persistente) e grava no vault. O broker chama no máximo **uma vez** por processo.
#[async_trait::async_trait]
pub trait SessionRepair: Send + Sync {
    async fn repair(&self) -> Result<(), String>;
}

#[derive(Default)]
struct Cache {
    access: HashMap<TokenResource, AccessToken>,
    skype: Option<SkypeToken>,
}

pub struct TokenBroker {
    vault: Vault,
    transport: Arc<dyn Transport>,
    clock: Arc<dyn Clock>,
    profile: Profile,
    cache: Mutex<Cache>,
    repair: Option<Arc<dyn SessionRepair>>,
    repaired: AtomicBool,
}

impl TokenBroker {
    pub fn new(vault: Vault, transport: Arc<dyn Transport>, clock: Arc<dyn Clock>) -> Self {
        Self {
            vault,
            transport,
            clock,
            profile: Profile::teams_web(),
            cache: Mutex::default(),
            repair: None,
            repaired: AtomicBool::new(false),
        }
    }

    /// Usa um perfil externo validado, antes de qualquer operação ou preenchimento do cache.
    pub fn with_profile(mut self, profile: Profile) -> Result<Self, crate::profile::ProfileError> {
        profile.validate()?;
        self.profile = profile;
        self.cache = Mutex::default();
        Ok(self)
    }

    /// Liga o re-pareamento automático quando o Entra recusar o RT.
    pub fn with_repair(mut self, repair: Arc<dyn SessionRepair>) -> Self {
        self.repair = Some(repair);
        self
    }

    /// Garante um access token válido para `resource` e devolve só os claims dele.
    pub async fn describe(&self, resource: TokenResource) -> Result<TokenInfo, TokenError> {
        Ok(self.access_token(resource).await?.info)
    }

    /// Garante um skype token válido e devolve só onde fica o serviço de chat.
    pub async fn chat_endpoint(&self) -> Result<ChatEndpoint, TokenError> {
        Ok(self.skype_token().await?.endpoint)
    }

    pub(crate) async fn access_token(
        &self,
        resource: TokenResource,
    ) -> Result<AccessToken, TokenError> {
        // O lock cobre o resgate inteiro: serializa o uso (e a rotação) do RT.
        let mut cache = self.cache.lock().await;
        if let Some(t) = cache.access.get(&resource) {
            if self.clock.now() + REFRESH_MARGIN < t.valid_until {
                return Ok(t.clone());
            }
        }
        let token = match self.redeem(resource).await {
            Err(TokenError::RefreshRejected { code }) => {
                self.repair_once(code).await?;
                // Sessão nova no vault: o skype token antigo também é de outra geração.
                cache.skype = None;
                self.redeem(resource).await?
            }
            other => other?,
        };
        cache.access.insert(resource, token.clone());
        Ok(token)
    }

    /// Uma tentativa de re-pareamento por processo; sem gancho (ou já tentado), o erro original sobe.
    async fn repair_once(&self, code: String) -> Result<(), TokenError> {
        let Some(repair) = &self.repair else {
            return Err(TokenError::RefreshRejected { code });
        };
        if self.repaired.swap(true, Ordering::SeqCst) {
            return Err(TokenError::RefreshRejected { code });
        }
        repair.repair().await.map_err(TokenError::RepairFailed)
    }

    /// Descarta o access token em cache (o servidor o recusou com 401): o próximo uso resgata outro.
    pub(crate) async fn invalidate_access(&self, resource: TokenResource) {
        self.cache.lock().await.access.remove(&resource);
    }

    /// Descarta o skype token em cache (recusado pelo serviço de chat).
    pub(crate) async fn invalidate_skype(&self) {
        self.cache.lock().await.skype = None;
    }

    pub(crate) async fn skype_token(&self) -> Result<SkypeToken, TokenError> {
        if let Some(t) = &self.cache.lock().await.skype {
            if self.clock.now() + REFRESH_MARGIN < t.valid_until {
                return Ok(t.clone());
            }
        }
        let teams = self.access_token(TokenResource::Teams).await?;
        let req = HttpRequest {
            method: "POST".into(),
            url: self.profile.authsvc_url.clone(),
            headers: vec![
                (
                    "Authorization".into(),
                    format!("Bearer {}", teams.secret.expose()),
                ),
                ("Content-Type".into(), "application/json".into()),
                ("Accept".into(), "application/json".into()),
                ("Origin".into(), self.profile.spa_origin.clone()),
            ],
            body: Some(b"{}".to_vec()),
        };
        let resp = self.transport.send(req).await?;
        let body = Zeroizing::new(resp.body);
        if resp.status != 200 {
            return Err(TokenError::Http {
                service: "authsvc",
                status: resp.status,
                code: None,
            });
        }
        let parsed: AuthzResponse =
            serde_json::from_slice(&body).map_err(|_| TokenError::Malformed("authsvc"))?;
        let expires_in = Duration::from_secs(parsed.tokens.expires_in);
        let token = SkypeToken {
            secret: Secret::new(parsed.tokens.skype_token.as_str()),
            valid_until: self.clock.now() + expires_in,
            endpoint: ChatEndpoint {
                region: parsed.region.clone(),
                chat_service: parsed.region_gtms.chat_service.clone(),
                expires_at: self.clock.utc_now() + expires_in,
            },
        };
        self.cache.lock().await.skype = Some(token.clone());
        Ok(token)
    }

    async fn redeem(&self, resource: TokenResource) -> Result<AccessToken, TokenError> {
        let session = self.vault.load_session()?.ok_or(TokenError::NotPaired)?;
        let form = Zeroizing::new(
            url::form_urlencoded::Serializer::new(String::new())
                .append_pair("client_id", &session.refresh_token.client_id)
                .append_pair("grant_type", "refresh_token")
                .append_pair("refresh_token", session.refresh_token.secret.expose())
                .append_pair("scope", resource.scope(&self.profile))
                .finish(),
        );
        let req = HttpRequest {
            method: "POST".into(),
            url: self
                .profile
                .token_url(&session.account.tenant_id)
                .ok_or(TokenError::Malformed("tenant"))?,
            headers: vec![
                (
                    "Content-Type".into(),
                    "application/x-www-form-urlencoded".into(),
                ),
                ("Accept".into(), "application/json".into()),
                ("Origin".into(), self.profile.spa_origin.clone()),
            ],
            body: Some(form.as_bytes().to_vec()),
        };
        let resp = self.transport.send(req).await?;
        let body = Zeroizing::new(resp.body);
        if resp.status != 200 {
            return Err(entra_error(resp.status, &body));
        }
        let parsed: TokenResponse =
            serde_json::from_slice(&body).map_err(|_| TokenError::Malformed("token"))?;
        let expires_in = Duration::from_secs(
            parsed
                .expires_in
                .seconds()
                .ok_or(TokenError::Malformed("token"))?,
        );
        let claims = decode_claims(&parsed.access_token);
        let expires_at = claims
            .expires_at
            .unwrap_or_else(|| self.clock.utc_now() + expires_in);

        let rotated = match parsed.refresh_token.as_deref() {
            Some(new) if !new.is_empty() && new != session.refresh_token.secret.expose() => {
                self.vault
                    .rotate_refresh_token(Secret::new(new), self.clock.utc_now())?;
                true
            }
            _ => false,
        };
        Ok(AccessToken {
            secret: Secret::new(parsed.access_token.as_str()),
            valid_until: self.clock.now() + expires_in,
            info: TokenInfo {
                resource,
                claims,
                expires_at,
                rotated_refresh_token: rotated,
            },
        })
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Seconds,
    refresh_token: Option<String>,
}

impl Drop for TokenResponse {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
    }
}

/// O Entra já devolveu `expires_in` como número e como texto.
#[derive(Deserialize)]
#[serde(untagged)]
enum Seconds {
    Number(u64),
    Text(String),
}

impl Seconds {
    fn seconds(&self) -> Option<u64> {
        match self {
            Self::Number(n) => Some(*n),
            Self::Text(s) => s.parse().ok(),
        }
    }
}

#[derive(Deserialize)]
struct AuthzResponse {
    tokens: SkypeTokens,
    region: Option<String>,
    #[serde(rename = "regionGtms")]
    region_gtms: RegionGtms,
}

#[derive(Deserialize)]
struct SkypeTokens {
    #[serde(rename = "skypeToken")]
    skype_token: String,
    #[serde(rename = "expiresIn")]
    expires_in: u64,
}

impl Drop for SkypeTokens {
    fn drop(&mut self) {
        self.skype_token.zeroize();
    }
}

#[derive(Deserialize)]
struct RegionGtms {
    #[serde(rename = "chatService")]
    chat_service: String,
}

/// Erro do Entra → erro acionável. Só código e nome do erro saem daqui (a descrição traz ids de
/// correlação e pode citar a conta).
fn entra_error(status: u16, body: &[u8]) -> TokenError {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let code = v["error_codes"]
        .as_array()
        .and_then(|c| c.first())
        .and_then(Value::as_u64);
    let aadsts = code.map(|c| format!("AADSTS{c}"));
    match (code, v["error"].as_str()) {
        (Some(9_002_327), _) => TokenError::SpaOrigin,
        (_, Some("invalid_grant")) => TokenError::RefreshRejected {
            code: aadsts.unwrap_or_else(|| "invalid_grant".into()),
        },
        (_, error) => TokenError::Http {
            service: "token",
            status,
            code: aadsts.or(error.map(str::to_owned)),
        },
    }
}

/// Lê os claims do payload de um JWT. Não valida assinatura — só informação para diagnóstico.
pub(crate) fn decode_claims(jwt: &str) -> TokenClaims {
    let Some(payload) = jwt.split('.').nth(1) else {
        return TokenClaims::default();
    };
    let Ok(bytes) =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('='))
    else {
        return TokenClaims::default();
    };
    let Ok(Value::Object(c)) = serde_json::from_slice::<Value>(&bytes) else {
        return TokenClaims::default();
    };
    let text = |k: &str| c.get(k).and_then(Value::as_str).map(str::to_owned);
    TokenClaims {
        audience: match c.get("aud") {
            Some(Value::String(a)) => Some(a.clone()),
            Some(Value::Array(a)) => Some(
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            _ => None,
        },
        tenant_id: text("tid"),
        scopes: text("scp"),
        app_id: text("appid").or_else(|| text("azp")),
        upn: text("upn")
            .or_else(|| text("preferred_username"))
            .or_else(|| text("unique_name")),
        expires_at: c
            .get("exp")
            .and_then(Value::as_u64)
            .map(|s| UNIX_EPOCH + Duration::from_secs(s)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;
    use crate::session::{AccountInfo, CapturedSession, RefreshToken};
    use crate::transport::HttpResponse;
    use crate::vault::MemoryStore;
    use async_trait::async_trait;
    use serde_json::json;

    const TENANT: &str = "tid-123";

    fn jwt(claims: Value) -> String {
        let b64 =
            |v: &Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
        format!(
            "{}.{}.assinatura",
            b64(&json!({"alg": "RS256"})),
            b64(&claims)
        )
    }

    #[derive(Default)]
    struct FakeTransport {
        requests: std::sync::Mutex<Vec<HttpRequest>>,
        responses: std::sync::Mutex<Vec<(u16, Value)>>,
    }

    impl FakeTransport {
        fn reply(&self, status: u16, body: Value) {
            self.responses.lock().unwrap().push((status, body));
        }
        fn sent(&self) -> Vec<HttpRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Transport for FakeTransport {
        async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportError> {
            self.requests.lock().unwrap().push(req);
            let mut queue = self.responses.lock().unwrap();
            assert!(!queue.is_empty(), "request sem resposta programada");
            let (status, body) = queue.remove(0);
            Ok(HttpResponse {
                status,
                headers: vec![],
                body: body.to_string().into_bytes(),
            })
        }
    }

    fn header<'a>(req: &'a HttpRequest, name: &str) -> Option<&'a str> {
        req.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn form(req: &HttpRequest) -> HashMap<String, String> {
        url::form_urlencoded::parse(req.body.as_deref().unwrap())
            .into_owned()
            .collect()
    }

    struct Harness {
        broker: TokenBroker,
        transport: Arc<FakeTransport>,
        clock: ManualClock,
        vault: Vault,
        store: MemoryStore,
    }

    const CAPTURED: u64 = 1_700_000_000;

    fn harness(paired: bool) -> Harness {
        let store = MemoryStore::default();
        let seed = Vault::with_store(store.clone());
        if paired {
            let session = CapturedSession {
                account: AccountInfo {
                    upn: "a@empresa.com".into(),
                    tenant_id: TENANT.into(),
                    home_account_id: "oid.tid-123".into(),
                    environment: "login.windows.net".into(),
                },
                refresh_token: RefreshToken {
                    secret: Secret::new("rt-antigo"),
                    client_id: "client-web".into(),
                    family_id: None,
                },
                cookies: vec![],
                observed_scopes: vec![],
                last_refresh: None,
            };
            seed.save(&session, UNIX_EPOCH + Duration::from_secs(CAPTURED))
                .unwrap();
        }
        let transport = Arc::new(FakeTransport::default());
        let clock = ManualClock::starting_at(UNIX_EPOCH + Duration::from_secs(CAPTURED + 3_600));
        let broker = TokenBroker::new(
            Vault::with_store(store.clone()),
            transport.clone(),
            Arc::new(clock.clone()),
        );
        Harness {
            broker,
            transport,
            clock,
            vault: Vault::with_store(store.clone()),
            store,
        }
    }

    fn token_ok(aud: &str, exp_offset: u64, rt: Option<&str>) -> Value {
        let mut v = json!({
            "token_type": "Bearer",
            "expires_in": 3600,
            "access_token": jwt(json!({"aud": aud, "tid": TENANT, "scp": "user_impersonation",
                                       "upn": "a@empresa.com", "appid": "client-web",
                                       "exp": CAPTURED + 3_600 + exp_offset})),
        });
        if let Some(rt) = rt {
            v["refresh_token"] = rt.into();
        }
        v
    }

    /// Checkpoint B (unidade): RT do keyring → access token, request de SPA, RT rotacionado salvo.
    #[tokio::test]
    async fn redeems_refresh_token_as_spa_and_persists_rotation() {
        let h = harness(true);
        h.transport.reply(
            200,
            token_ok("https://api.spaces.skype.com", 3_600, Some("rt-novo")),
        );

        let info = h.broker.describe(TokenResource::Teams).await.unwrap();

        let sent = h.transport.sent();
        assert_eq!(sent.len(), 1);
        let req = &sent[0];
        assert_eq!(req.method, "POST");
        assert_eq!(
            req.url,
            format!("https://login.microsoftonline.com/{TENANT}/oauth2/v2.0/token")
        );
        assert_eq!(header(req, "Origin"), Some("https://teams.cloud.microsoft"));
        let f = form(req);
        assert_eq!(f["grant_type"], "refresh_token");
        assert_eq!(f["client_id"], "client-web");
        assert_eq!(f["refresh_token"], "rt-antigo");
        assert!(f["scope"].starts_with("https://api.spaces.skype.com/.default"));

        assert_eq!(
            info.claims.audience.as_deref(),
            Some("https://api.spaces.skype.com")
        );
        assert_eq!(info.claims.tenant_id.as_deref(), Some(TENANT));
        assert!(info.rotated_refresh_token);

        let stored = h.vault.load_session().unwrap().unwrap();
        assert_eq!(stored.refresh_token.secret.expose(), "rt-novo");
        let acc = h.vault.active_account().unwrap().unwrap();
        assert_eq!(
            acc.captured_at,
            UNIX_EPOCH + Duration::from_secs(CAPTURED),
            "login preservado"
        );
        assert_eq!(
            acc.last_refresh,
            Some(UNIX_EPOCH + Duration::from_secs(CAPTURED + 3_600))
        );
    }

    #[tokio::test]
    async fn cached_until_refresh_margin_then_uses_rotated_rt() {
        let h = harness(true);
        h.transport
            .reply(200, token_ok("aud-1", 3_600, Some("rt-2")));
        h.broker.describe(TokenResource::Teams).await.unwrap();
        h.broker.describe(TokenResource::Teams).await.unwrap();
        assert_eq!(h.transport.sent().len(), 1, "segundo pedido vem do cache");

        h.clock.advance(Duration::from_secs(3_600) - REFRESH_MARGIN); // entra na margem
        h.transport
            .reply(200, token_ok("aud-1", 7_200, Some("rt-3")));
        h.broker.describe(TokenResource::Teams).await.unwrap();
        let sent = h.transport.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(
            form(&sent[1])["refresh_token"],
            "rt-2",
            "usa o RT rotacionado, não o original"
        );
    }

    #[tokio::test]
    async fn resources_are_cached_separately() {
        let h = harness(true);
        h.transport.reply(200, token_ok("teams", 3_600, None));
        h.transport.reply(200, token_ok("chatsvcagg", 3_600, None));
        h.broker.describe(TokenResource::Teams).await.unwrap();
        let info = h.broker.describe(TokenResource::ChatSvcAgg).await.unwrap();
        assert_eq!(info.claims.audience.as_deref(), Some("chatsvcagg"));
        assert!(form(&h.transport.sent()[1])["scope"]
            .starts_with("https://chatsvcagg.teams.microsoft.com/"));
        assert!(
            !info.rotated_refresh_token,
            "sem refresh_token na resposta, nada rotaciona"
        );
    }

    #[tokio::test]
    async fn spa_lifetime_expiry_is_actionable_and_keeps_vault() {
        let h = harness(true);
        h.transport.reply(
            400,
            json!({"error": "invalid_grant", "error_codes": [700084],
            "error_description": "AADSTS700084: ... conta a@empresa.com ..."}),
        );
        let err = h.broker.describe(TokenResource::Teams).await.unwrap_err();
        assert!(matches!(&err, TokenError::RefreshRejected { code } if code == "AADSTS700084"));
        let msg = err.to_string();
        assert!(
            msg.contains("AUTH-RT-SPA-001") && msg.contains("f365 auth"),
            "{msg}"
        );
        assert!(
            !msg.contains("a@empresa.com"),
            "descrição do Entra não vaza na mensagem"
        );
        assert_eq!(
            h.vault
                .load_session()
                .unwrap()
                .unwrap()
                .refresh_token
                .secret
                .expose(),
            "rt-antigo"
        );
    }

    #[tokio::test]
    async fn missing_origin_is_diagnosed() {
        let h = harness(true);
        h.transport.reply(
            400,
            json!({"error": "invalid_request", "error_codes": [9002327]}),
        );
        assert!(matches!(
            h.broker.describe(TokenResource::Teams).await,
            Err(TokenError::SpaOrigin)
        ));
    }

    #[tokio::test]
    async fn not_paired_makes_no_request() {
        let h = harness(false);
        assert!(matches!(
            h.broker.describe(TokenResource::Teams).await,
            Err(TokenError::NotPaired)
        ));
        assert!(h.transport.sent().is_empty());
    }

    #[tokio::test]
    async fn skype_token_uses_teams_bearer_and_discovers_region() {
        let h = harness(true);
        let teams = token_ok("https://api.spaces.skype.com", 3_600, None);
        let teams_at = teams["access_token"].as_str().unwrap().to_owned();
        h.transport.reply(200, teams);
        h.transport.reply(200, json!({
            "tokens": {"skypeToken": "skype-segredo", "expiresIn": 86_400, "tokenType": "SkypeToken"},
            "region": "amer",
            "regionGtms": {"chatService": "https://amer.ng.msg.teams.microsoft.com"}
        }));

        let ep = h.broker.chat_endpoint().await.unwrap();

        assert_eq!(ep.region.as_deref(), Some("amer"));
        assert_eq!(ep.chat_service, "https://amer.ng.msg.teams.microsoft.com");
        let authz = &h.transport.sent()[1];
        assert_eq!(
            authz.url,
            "https://teams.microsoft.com/api/authsvc/v1.0/authz"
        );
        assert_eq!(
            header(authz, "Authorization"),
            Some(format!("Bearer {teams_at}").as_str())
        );
        assert!(!format!("{ep:?}").contains("skype-segredo"));

        h.broker.chat_endpoint().await.unwrap();
        assert_eq!(h.transport.sent().len(), 2, "skype token vem do cache");
    }

    #[tokio::test]
    async fn unexpected_authsvc_shape_is_malformed() {
        let h = harness(true);
        h.transport.reply(200, token_ok("teams", 3_600, None));
        h.transport.reply(200, json!({"tokens": {}}));
        assert!(matches!(
            h.broker.chat_endpoint().await,
            Err(TokenError::Malformed("authsvc"))
        ));
    }

    #[test]
    fn claims_decode_tolerates_shapes() {
        let c = decode_claims(&jwt(
            json!({"aud": ["a", "b"], "azp": "app", "preferred_username": "x@y"}),
        ));
        assert_eq!(c.audience.as_deref(), Some("a b"));
        assert_eq!(c.app_id.as_deref(), Some("app"));
        assert_eq!(c.upn.as_deref(), Some("x@y"));
        assert_eq!(c.expires_at, None);
        assert_eq!(decode_claims("não-é-jwt"), TokenClaims::default());
        assert_eq!(decode_claims("a.%%%.c"), TokenClaims::default());
    }

    #[test]
    fn expires_in_as_text_is_accepted() {
        let r: TokenResponse =
            serde_json::from_value(json!({"access_token": "x", "expires_in": "3599"})).unwrap();
        assert_eq!(r.expires_in.seconds(), Some(3599));
    }

    /// Re-pareamento falso: grava uma sessão nova (RT `rt-reparado`) como a WebView faria.
    struct FakeRepair {
        store: MemoryStore,
        calls: std::sync::atomic::AtomicUsize,
        fail: bool,
    }

    #[async_trait]
    impl SessionRepair for FakeRepair {
        async fn repair(&self) -> Result<(), String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail {
                return Err("janela fechada".into());
            }
            let vault = Vault::with_store(self.store.clone());
            let mut session = vault.load_session().unwrap().unwrap();
            session.refresh_token.secret = Secret::new("rt-reparado");
            vault
                .save(
                    &session,
                    UNIX_EPOCH + Duration::from_secs(CAPTURED + 90_000),
                )
                .unwrap();
            Ok(())
        }
    }

    fn with_fake_repair(
        h: Harness,
        fail: bool,
    ) -> (TokenBroker, Arc<FakeTransport>, Arc<FakeRepair>, Vault) {
        let repair = Arc::new(FakeRepair {
            store: h.store.clone(),
            calls: Default::default(),
            fail,
        });
        (
            h.broker.with_repair(repair.clone()),
            h.transport,
            repair,
            h.vault,
        )
    }

    fn spa_expired() -> Value {
        json!({"error": "invalid_grant", "error_codes": [700084]})
    }

    /// AUTH-RT-SPA-001: RT de 24 h vencido → re-pareia uma vez e segue com o RT novo.
    #[tokio::test]
    async fn expired_spa_token_triggers_one_silent_repair_and_retries() {
        let (broker, transport, repair, vault) = with_fake_repair(harness(true), false);
        transport.reply(400, spa_expired());
        transport.reply(
            200,
            token_ok("https://api.spaces.skype.com", 3_600, Some("rt-novo")),
        );

        let info = broker.describe(TokenResource::Teams).await.unwrap();

        assert_eq!(
            info.claims.audience.as_deref(),
            Some("https://api.spaces.skype.com")
        );
        assert_eq!(repair.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let sent = transport.sent();
        assert_eq!(form(&sent[0])["refresh_token"], "rt-antigo");
        assert_eq!(
            form(&sent[1])["refresh_token"],
            "rt-reparado",
            "retentativa usa a sessão recapturada"
        );
        assert_eq!(
            vault
                .load_session()
                .unwrap()
                .unwrap()
                .refresh_token
                .secret
                .expose(),
            "rt-novo"
        );
    }

    #[tokio::test]
    async fn failed_repair_is_reported_once() {
        let (broker, transport, repair, _) = with_fake_repair(harness(true), true);
        transport.reply(400, spa_expired());
        let err = broker.describe(TokenResource::Teams).await.unwrap_err();
        assert!(
            matches!(&err, TokenError::RepairFailed(m) if m.contains("janela fechada")),
            "{err:?}"
        );
        assert!(err.to_string().contains("f365 auth"));
        assert_eq!(repair.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Nunca entra em loop: recusado de novo depois do reparo → erro, e não repara outra vez.
    #[tokio::test]
    async fn repair_runs_at_most_once_per_process() {
        let (broker, transport, repair, _) = with_fake_repair(harness(true), false);
        transport.reply(400, spa_expired());
        transport.reply(400, spa_expired());
        assert!(matches!(
            broker.describe(TokenResource::Teams).await,
            Err(TokenError::RefreshRejected { .. })
        ));
        transport.reply(400, spa_expired());
        assert!(matches!(
            broker.describe(TokenResource::Teams).await,
            Err(TokenError::RefreshRejected { .. })
        ));
        assert_eq!(repair.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn other_errors_do_not_trigger_repair() {
        let (broker, transport, repair, _) = with_fake_repair(harness(true), false);
        transport.reply(
            400,
            json!({"error": "invalid_request", "error_codes": [9002327]}),
        );
        assert!(matches!(
            broker.describe(TokenResource::Teams).await,
            Err(TokenError::SpaOrigin)
        ));
        assert_eq!(repair.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
