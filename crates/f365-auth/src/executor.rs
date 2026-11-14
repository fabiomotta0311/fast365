//! Executor autenticado (`docs/ARQUITETURA.md` §1 e §2.1): o provider descreve a operação
//! ([`AuthOp`]); aqui ela ganha credencial, rota e prazo. O token nunca sai deste crate — o provider
//! recebe só a resposta.
//!
//! Política de rota:
//! - **Rust** (padrão): `reqwest` + token efêmero do [`TokenBroker`].
//! - **401** → o token em cache é descartado e a operação repete **uma** vez com token novo (seguro:
//!   401 = o servidor recusou antes de executar).
//! - **403** ou **desafio de Conditional Access** (401 com `claims`) → rota **InPage**, quando
//!   existir — reenviar é seguro pelo mesmo motivo. Sem InPage: erro de autenticação explícito.
//! - `needs_page_context` → direto para InPage (cookie-bound, WebSocket).
//! - **Erro de rede ou prazo estourado nunca é repetido aqui**: o efeito no servidor é desconhecido e
//!   quem decide é o motor (reconciliação de envio, `SEND-RECONCILE-001`).
//!
//! A credencial só vai para os hosts do recurso (HTTPS, host exato): um provider com bug ou URL
//! injetada nunca vaza token para outro destino.
//!
//! Contratos de protocolo:
//! - `EXEC-SCHEME-001` (PARCIAL): VERIFIED no Checkpoint C (#10) que o `chatsvcagg` aceita
//!   `Authorization: Bearer <aud chatsvcagg>`. UNVERIFIED: o chat regional aceitar
//!   `Authentication: skypetoken=<skype token>` — verificar no #11/#12.
//! - `EXEC-HOST-001` (VERIFIED): o agregador REST fica em `chatsvcagg.teams.microsoft.com` (Checkpoint
//!   C, #10); o chat regional, no host de `regionGtms.chatService` (#8: `amer.ng.msg.teams.microsoft.com`).
//! - `EXEC-CHALLENGE-001` (UNVERIFIED): desafio de Conditional Access chega como 401 com
//!   `WWW-Authenticate` contendo `claims`/`insufficient_claims`/`interaction_required`.

use crate::clock::Clock;
use crate::tokens::{TokenBroker, TokenError, TokenResource};
use crate::transport::{HttpRequest, HttpResponse, Transport};
use crate::{AuthOp, AuthenticatedExecutor, ExecError, Resource};
use async_trait::async_trait;
use std::sync::Arc;

const CHATSVCAGG_HOST: &str = "chatsvcagg.teams.microsoft.com";
/// Sufixo exigido do host regional devolvido pelo authsvc (defesa contra resposta inesperada).
const CHAT_SERVICE_SUFFIX: &str = ".teams.microsoft.com";
/// Cabeçalhos que só o executor põe; um `AuthOp` trazendo algum deles é bug do provider.
const CREDENTIAL_HEADERS: &[&str] = &["authorization", "authentication", "cookie", "x-skypetoken"];

pub struct SessionExecutor {
    broker: Arc<TokenBroker>,
    rust: Arc<dyn Transport>,
    /// Fetch dentro da WebView parqueada. `None` até o transporte InPage existir.
    in_page: Option<Arc<dyn Transport>>,
    clock: Arc<dyn Clock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Accepted,
    Unauthorized,
    Challenge,
    Forbidden,
}

fn classify(resp: &HttpResponse) -> Verdict {
    match resp.status {
        401 => {
            let www = resp
                .header("www-authenticate")
                .unwrap_or_default()
                .to_ascii_lowercase();
            if ["claims", "insufficient_claims", "interaction_required"]
                .iter()
                .any(|m| www.contains(m))
            {
                Verdict::Challenge
            } else {
                Verdict::Unauthorized
            }
        }
        403 => Verdict::Forbidden,
        _ => Verdict::Accepted,
    }
}

fn token_error(e: TokenError) -> ExecError {
    match e {
        TokenError::NotPaired => ExecError::NotPaired,
        TokenError::Transport(t) => ExecError::Transport(t.to_string()),
        other => ExecError::Auth(other.to_string()),
    }
}

/// HTTPS, host exato aceito por `allowed`, sem credencial embutida na URL e na porta padrão.
fn require_host(
    resource: Resource,
    url: &str,
    allowed: impl Fn(&str) -> bool,
) -> Result<(), ExecError> {
    let denied = || ExecError::HostNotAllowed {
        resource,
        url: redact_query(url),
    };
    let u = url::Url::parse(url).map_err(|_| denied())?;
    let ok = u.scheme() == "https"
        && u.username().is_empty()
        && u.password().is_none()
        && u.port().is_none()
        && u.host_str().is_some_and(&allowed);
    ok.then_some(()).ok_or_else(denied)
}

/// Para mensagens de erro: a query pode carregar ids ou tokens de terceiros.
fn redact_query(url: &str) -> String {
    url.split(['?', '#']).next().unwrap_or_default().to_owned()
}

impl SessionExecutor {
    pub fn new(
        broker: Arc<TokenBroker>,
        rust: Arc<dyn Transport>,
        in_page: Option<Arc<dyn Transport>>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            broker,
            rust,
            in_page,
            clock,
        }
    }

    async fn run(&self, op: &AuthOp) -> Result<HttpResponse, ExecError> {
        if let Some((h, _)) = op
            .request
            .headers
            .iter()
            .find(|(k, _)| CREDENTIAL_HEADERS.iter().any(|c| k.eq_ignore_ascii_case(c)))
        {
            return Err(ExecError::Auth(format!(
                "AuthOp não pode trazer credencial (cabeçalho `{h}`)"
            )));
        }
        if op.needs_page_context {
            return self.in_page(op).await;
        }

        let mut refreshed = false;
        loop {
            let req = self.authorize(op).await?;
            let resp = self
                .rust
                .send(req)
                .await
                .map_err(|e| ExecError::Transport(e.to_string()))?;
            match classify(&resp) {
                Verdict::Accepted => return Ok(resp),
                Verdict::Unauthorized if !refreshed => {
                    self.invalidate(op.resource).await;
                    refreshed = true;
                }
                verdict => return self.fallback(op, resp.status, verdict).await,
            }
        }
    }

    /// 401/403 = o servidor recusou antes de executar: reenviar pela página é seguro mesmo para
    /// operação não idempotente.
    async fn fallback(
        &self,
        op: &AuthOp,
        status: u16,
        verdict: Verdict,
    ) -> Result<HttpResponse, ExecError> {
        if self.in_page.is_some() {
            return self.in_page(op).await;
        }
        let why = match verdict {
            Verdict::Challenge => "desafio de Conditional Access",
            Verdict::Forbidden => "acesso negado",
            _ => "token recusado mesmo após renovar",
        };
        Err(ExecError::Auth(format!(
            "HTTP {status} — {why}; rota InPage indisponível"
        )))
    }

    async fn in_page(&self, op: &AuthOp) -> Result<HttpResponse, ExecError> {
        let transport = self.in_page.as_ref().ok_or(ExecError::RouteUnavailable(
            "InPage (fetch na WebView parqueada) ainda não implementada",
        ))?;
        self.check_host(op).await?;
        // Sem credencial nossa: dentro da página quem autentica é o contexto genuíno do cliente.
        let resp = transport
            .send(op.request.clone())
            .await
            .map_err(|e| ExecError::Transport(e.to_string()))?;
        match classify(&resp) {
            Verdict::Accepted => Ok(resp),
            _ => Err(ExecError::Auth(format!(
                "HTTP {} também pela página (InPage)",
                resp.status
            ))),
        }
    }

    async fn check_host(&self, op: &AuthOp) -> Result<(), ExecError> {
        match op.resource {
            Resource::ChatSvcAgg => {
                require_host(op.resource, &op.request.url, |h| h == CHATSVCAGG_HOST)
            }
            Resource::ChatService => {
                let skype = self.broker.skype_token().await.map_err(token_error)?;
                self.require_chat_host(op, &skype.endpoint.chat_service)
            }
            Resource::OutlookSubstrate => {
                Err(ExecError::RouteUnavailable("Outlook/Substrate (Fase 4)"))
            }
        }
    }

    fn require_chat_host(&self, op: &AuthOp, chat_service: &str) -> Result<(), ExecError> {
        let regional = url::Url::parse(chat_service)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .filter(|h| h.ends_with(CHAT_SERVICE_SUFFIX))
            .ok_or_else(|| ExecError::Auth("authsvc devolveu um chatService inesperado".into()))?;
        require_host(op.resource, &op.request.url, |h| h == regional)
    }

    /// Valida o destino ANTES de obter/anexar a credencial do recurso.
    async fn authorize(&self, op: &AuthOp) -> Result<HttpRequest, ExecError> {
        let mut req = op.request.clone();
        match op.resource {
            Resource::ChatSvcAgg => {
                require_host(op.resource, &req.url, |h| h == CHATSVCAGG_HOST)?;
                let token = self
                    .broker
                    .access_token(TokenResource::ChatSvcAgg)
                    .await
                    .map_err(token_error)?;
                req.headers.push((
                    "Authorization".into(),
                    format!("Bearer {}", token.secret.expose()),
                ));
            }
            Resource::ChatService => {
                let skype = self.broker.skype_token().await.map_err(token_error)?;
                self.require_chat_host(op, &skype.endpoint.chat_service)?;
                req.headers.push((
                    "Authentication".into(),
                    format!("skypetoken={}", skype.secret.expose()),
                ));
            }
            Resource::OutlookSubstrate => {
                return Err(ExecError::RouteUnavailable("Outlook/Substrate (Fase 4)"));
            }
        }
        Ok(req)
    }

    async fn invalidate(&self, resource: Resource) {
        match resource {
            Resource::ChatSvcAgg => {
                self.broker
                    .invalidate_access(TokenResource::ChatSvcAgg)
                    .await
            }
            Resource::ChatService => self.broker.invalidate_skype().await,
            Resource::OutlookSubstrate => {}
        }
    }
}

#[async_trait]
impl AuthenticatedExecutor for SessionExecutor {
    async fn execute(&self, op: AuthOp) -> Result<HttpResponse, ExecError> {
        let timeout = op.timeout;
        tokio::select! {
            result = self.run(&op) => result,
            () = self.clock.sleep(timeout) => Err(ExecError::TimedOut(timeout)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;
    use crate::secret::Secret;
    use crate::session::{AccountInfo, CapturedSession, RefreshToken};
    use crate::transport::TransportError;
    use crate::vault::{MemoryStore, Vault};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    const AGG: &str = "https://chatsvcagg.teams.microsoft.com/api/v1/chats";
    const CHAT: &str = "https://amer.ng.msg.teams.microsoft.com/v1/users/ME/conversations";

    /// Entra + authsvc falsos: cada resgate devolve um access token novo (`at-1`, `at-2`, …).
    #[derive(Default)]
    struct FakeIdentity {
        redeemed: AtomicUsize,
        skype_issued: AtomicUsize,
    }

    #[async_trait]
    impl Transport for FakeIdentity {
        async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportError> {
            let body = if req.url.contains("/oauth2/v2.0/token") {
                let n = self.redeemed.fetch_add(1, Ordering::SeqCst) + 1;
                json!({"access_token": format!("at-{n}"), "expires_in": 3600, "refresh_token": format!("rt-{n}")})
            } else if req.url.contains("/authsvc/") {
                let n = self.skype_issued.fetch_add(1, Ordering::SeqCst) + 1;
                json!({"tokens": {"skypeToken": format!("skype-{n}"), "expiresIn": 86_400},
                       "region": "amer", "regionGtms": {"chatService": "https://amer.ng.msg.teams.microsoft.com"}})
            } else {
                panic!("identidade falsa recebeu {req:?}");
            };
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: body.to_string().into_bytes(),
            })
        }
    }

    enum Step {
        Status(u16, Vec<(&'static str, &'static str)>),
        NetworkError,
        Hang,
    }

    /// API falsa (rota Rust ou InPage): responde na ordem programada e grava o que recebeu.
    #[derive(Default)]
    struct FakeApi {
        steps: Mutex<Vec<Step>>,
        seen: Mutex<Vec<HttpRequest>>,
    }

    impl FakeApi {
        fn script(steps: Vec<Step>) -> Arc<Self> {
            Arc::new(Self {
                steps: Mutex::new(steps),
                ..Default::default()
            })
        }
        fn seen(&self) -> Vec<HttpRequest> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Transport for FakeApi {
        async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportError> {
            self.seen.lock().unwrap().push(req);
            let step = {
                let mut s = self.steps.lock().unwrap();
                assert!(!s.is_empty(), "request além do roteiro");
                s.remove(0)
            };
            match step {
                Step::Status(status, headers) => Ok(HttpResponse {
                    status,
                    headers: headers
                        .into_iter()
                        .map(|(k, v)| (k.into(), v.into()))
                        .collect(),
                    body: b"{}".to_vec(),
                }),
                Step::NetworkError => Err(TransportError::Other("conexão caiu".into())),
                Step::Hang => std::future::pending().await,
            }
        }
    }

    struct Harness {
        exec: SessionExecutor,
        identity: Arc<FakeIdentity>,
        clock: ManualClock,
    }

    fn harness(rust: Arc<FakeApi>, in_page: Option<Arc<FakeApi>>) -> Harness {
        let store = MemoryStore::default();
        let vault = Vault::with_store(store);
        let session = CapturedSession {
            account: AccountInfo {
                upn: "a@empresa.com".into(),
                tenant_id: "tid".into(),
                home_account_id: "oid.tid".into(),
                environment: "login.windows.net".into(),
            },
            refresh_token: RefreshToken {
                secret: Secret::new("rt-0"),
                client_id: "client-web".into(),
                family_id: None,
            },
            cookies: vec![],
            observed_scopes: vec![],
            last_refresh: None,
        };
        vault
            .save(&session, UNIX_EPOCH + Duration::from_secs(1_700_000_000))
            .unwrap();
        let identity = Arc::new(FakeIdentity::default());
        let clock =
            ManualClock::starting_at(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_100));
        let broker = Arc::new(TokenBroker::new(
            vault,
            identity.clone(),
            Arc::new(clock.clone()),
        ));
        let in_page = in_page.map(|t| t as Arc<dyn Transport>);
        let exec = SessionExecutor::new(broker, rust, in_page, Arc::new(clock.clone()));
        Harness {
            exec,
            identity,
            clock,
        }
    }

    fn op(resource: Resource, url: &str) -> AuthOp {
        AuthOp {
            resource,
            request: HttpRequest {
                method: "GET".into(),
                url: url.into(),
                headers: vec![("Accept".into(), "application/json".into())],
                body: None,
            },
            needs_page_context: false,
            idempotent: true,
            timeout: Duration::from_secs(30),
        }
    }

    fn header<'a>(req: &'a HttpRequest, name: &str) -> Option<&'a str> {
        req.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    #[tokio::test]
    async fn rust_route_attaches_bearer_for_chatsvcagg() {
        let api = FakeApi::script(vec![Step::Status(200, vec![])]);
        let h = harness(api.clone(), None);

        let resp = h.exec.execute(op(Resource::ChatSvcAgg, AGG)).await.unwrap();

        assert_eq!(resp.status, 200);
        let sent = api.seen();
        assert_eq!(header(&sent[0], "Authorization"), Some("Bearer at-1"));
        assert_eq!(
            header(&sent[0], "Accept"),
            Some("application/json"),
            "headers do provider preservados"
        );
    }

    #[tokio::test]
    async fn rust_route_attaches_skypetoken_for_regional_chat() {
        let api = FakeApi::script(vec![Step::Status(200, vec![])]);
        let h = harness(api.clone(), None);
        h.exec
            .execute(op(Resource::ChatService, CHAT))
            .await
            .unwrap();
        let sent = api.seen();
        assert_eq!(
            header(&sent[0], "Authentication"),
            Some("skypetoken=skype-1")
        );
        assert_eq!(header(&sent[0], "Authorization"), None);
    }

    /// O token nunca vai para fora dos hosts do recurso — nem um request é feito.
    #[tokio::test]
    async fn credential_never_leaves_allowed_hosts() {
        for (resource, url) in [
            (Resource::ChatSvcAgg, "https://evil.example/api"),
            (
                Resource::ChatSvcAgg,
                "http://chatsvcagg.teams.microsoft.com/api",
            ),
            (
                Resource::ChatSvcAgg,
                "https://chatsvcagg.teams.microsoft.com.evil.com/api",
            ),
            (
                Resource::ChatSvcAgg,
                "https://chatsvcagg.teams.microsoft.com:8443/api",
            ),
            (
                Resource::ChatSvcAgg,
                "https://user@chatsvcagg.teams.microsoft.com/api",
            ),
            (
                Resource::ChatService,
                "https://emea.ng.msg.teams.microsoft.com/v1/x",
            ), // outra região
            (Resource::ChatService, AGG),
        ] {
            let api = FakeApi::script(vec![]);
            let h = harness(api.clone(), None);
            let err = h.exec.execute(op(resource, url)).await.unwrap_err();
            assert!(
                matches!(err, ExecError::HostNotAllowed { .. }),
                "{url}: {err:?}"
            );
            assert!(api.seen().is_empty(), "{url}: nada pode ter sido enviado");
        }
    }

    #[tokio::test]
    async fn host_error_message_drops_query() {
        let h = harness(FakeApi::script(vec![]), None);
        let err = h
            .exec
            .execute(op(
                Resource::ChatSvcAgg,
                "https://evil.example/x?token=segredo",
            ))
            .await
            .unwrap_err();
        assert!(!err.to_string().contains("segredo"), "{err}");
    }

    #[tokio::test]
    async fn provider_cannot_smuggle_credentials() {
        let api = FakeApi::script(vec![]);
        let h = harness(api.clone(), None);
        let mut o = op(Resource::ChatSvcAgg, AGG);
        o.request
            .headers
            .push(("Authorization".into(), "Bearer de-outro-lugar".into()));
        assert!(matches!(h.exec.execute(o).await, Err(ExecError::Auth(_))));
        assert!(api.seen().is_empty());
    }

    #[tokio::test]
    async fn unauthorized_refreshes_once_then_succeeds() {
        let api = FakeApi::script(vec![Step::Status(401, vec![]), Step::Status(200, vec![])]);
        let h = harness(api.clone(), None);

        let resp = h.exec.execute(op(Resource::ChatSvcAgg, AGG)).await.unwrap();

        assert_eq!(resp.status, 200);
        let sent = api.seen();
        assert_eq!(header(&sent[0], "Authorization"), Some("Bearer at-1"));
        assert_eq!(
            header(&sent[1], "Authorization"),
            Some("Bearer at-2"),
            "token renovado após o 401"
        );
        assert_eq!(h.identity.redeemed.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn second_unauthorized_without_in_page_is_auth_error() {
        let api = FakeApi::script(vec![Step::Status(401, vec![]), Step::Status(401, vec![])]);
        let h = harness(api.clone(), None);
        let err = h
            .exec
            .execute(op(Resource::ChatSvcAgg, AGG))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ExecError::Auth(m) if m.contains("InPage")),
            "{err:?}"
        );
        assert_eq!(api.seen().len(), 2, "renova uma vez só");
    }

    /// Critério de aceite do #9: cai para InPage sob bloqueio.
    #[tokio::test]
    async fn forbidden_falls_back_to_in_page_without_our_credential() {
        let api = FakeApi::script(vec![Step::Status(403, vec![])]);
        let page = FakeApi::script(vec![Step::Status(200, vec![])]);
        let h = harness(api.clone(), Some(page.clone()));

        let mut o = op(Resource::ChatSvcAgg, AGG);
        o.idempotent = false; // 403 = não executado: seguro reenviar mesmo assim
        let resp = h.exec.execute(o).await.unwrap();

        assert_eq!(resp.status, 200);
        assert_eq!(api.seen().len(), 1);
        let via_page = page.seen();
        assert_eq!(via_page.len(), 1);
        assert_eq!(
            header(&via_page[0], "Authorization"),
            None,
            "na página quem autentica é a página"
        );
    }

    #[tokio::test]
    async fn conditional_access_challenge_skips_refresh_and_goes_in_page() {
        let api = FakeApi::script(vec![Step::Status(
            401,
            vec![(
                "WWW-Authenticate",
                r#"Bearer error="insufficient_claims", claims="eyJ...""#,
            )],
        )]);
        let page = FakeApi::script(vec![Step::Status(200, vec![])]);
        let h = harness(api.clone(), Some(page.clone()));
        h.exec.execute(op(Resource::ChatSvcAgg, AGG)).await.unwrap();
        assert_eq!(api.seen().len(), 1, "renovar não satisfaz claims de CA");
        assert_eq!(h.identity.redeemed.load(Ordering::SeqCst), 1);
        assert_eq!(page.seen().len(), 1);
    }

    #[tokio::test]
    async fn page_context_ops_go_straight_in_page_or_fail_clearly() {
        let mut o = op(Resource::ChatSvcAgg, AGG);
        o.needs_page_context = true;

        let page = FakeApi::script(vec![Step::Status(200, vec![])]);
        let api = FakeApi::script(vec![]);
        let h = harness(api.clone(), Some(page.clone()));
        h.exec.execute(o.clone()).await.unwrap();
        assert!(api.seen().is_empty());
        assert_eq!(page.seen().len(), 1);

        let h = harness(FakeApi::script(vec![]), None);
        assert!(matches!(
            h.exec.execute(o).await,
            Err(ExecError::RouteUnavailable(_))
        ));
    }

    #[tokio::test]
    async fn in_page_also_respects_allowed_hosts() {
        let page = FakeApi::script(vec![]);
        let h = harness(FakeApi::script(vec![]), Some(page.clone()));
        let mut o = op(Resource::ChatSvcAgg, "https://evil.example/x");
        o.needs_page_context = true;
        assert!(matches!(
            h.exec.execute(o).await,
            Err(ExecError::HostNotAllowed { .. })
        ));
        assert!(page.seen().is_empty());
    }

    /// Envio cuja resposta se perdeu: nunca reenviado aqui (`SEND-RECONCILE-001` é do motor).
    #[tokio::test]
    async fn network_error_is_never_retried() {
        let api = FakeApi::script(vec![Step::NetworkError]);
        let h = harness(api.clone(), None);
        let mut o = op(Resource::ChatSvcAgg, AGG);
        o.request.method = "POST".into();
        o.idempotent = false;
        assert!(matches!(
            h.exec.execute(o).await,
            Err(ExecError::Transport(_))
        ));
        assert_eq!(api.seen().len(), 1);
    }

    #[tokio::test]
    async fn deadline_comes_from_the_injected_clock() {
        let api = FakeApi::script(vec![Step::Hang]);
        let h = harness(api.clone(), None);
        let mut o = op(Resource::ChatSvcAgg, AGG);
        o.timeout = Duration::from_secs(10);
        let exec = Arc::new(h.exec);
        let task = tokio::spawn({
            let exec = exec.clone();
            async move { exec.execute(o).await }
        });
        while api.seen().is_empty() {
            tokio::task::yield_now().await;
        }
        h.clock.advance(Duration::from_secs(10));
        let result = task.await.unwrap();
        assert!(matches!(result, Err(ExecError::TimedOut(d)) if d == Duration::from_secs(10)));
    }

    #[tokio::test]
    async fn non_auth_statuses_are_returned_to_the_provider() {
        let api = FakeApi::script(vec![Step::Status(404, vec![])]);
        let h = harness(api, None);
        assert_eq!(
            h.exec
                .execute(op(Resource::ChatSvcAgg, AGG))
                .await
                .unwrap()
                .status,
            404
        );
    }

    #[tokio::test]
    async fn outlook_is_not_routed_yet() {
        let h = harness(FakeApi::script(vec![]), None);
        let o = op(Resource::OutlookSubstrate, "https://substrate.office.com/x");
        assert!(matches!(
            h.exec.execute(o).await,
            Err(ExecError::RouteUnavailable(_))
        ));
    }
}
