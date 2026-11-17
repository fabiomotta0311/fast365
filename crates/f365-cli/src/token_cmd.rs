//! `f365 token` (Checkpoint B): renova em silêncio e imprime **só claims/metadados** — o token não
//! sai de `f365-auth` nem aparece no terminal (`AGENTS.md`: nenhum segredo em saída/log).
//!
//! Opt-in: defina `F365_PROTOCOL_PROFILE` com o caminho de um JSON de perfil confiável antes de
//! executar o CLI. Ausente usa o perfil embutido; arquivo inválido ou inacessível aborta sem fallback.
//! O perfil só permite versões numéricas do caminho authsvc; destinos de credenciais não mudam.

use crate::session_cmd::human;
use f365_auth::tokens::{TokenBroker, TokenInfo, TokenResource};
use f365_auth::transport::RustTransport;
use f365_auth::vault::Vault;
use f365_auth::{Clock, SystemClock};
use std::sync::Arc;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Resource {
    /// Middle tier do Teams (aud `api.spaces.skype.com`).
    Teams,
    /// Agregador REST de conversas.
    Chatsvcagg,
    /// Skype token + host regional do chat (via authsvc).
    Skype,
}

pub(crate) fn base_broker() -> anyhow::Result<TokenBroker> {
    let path = std::env::var_os("F365_PROTOCOL_PROFILE").map(std::path::PathBuf::from);
    Ok(TokenBroker::new_with_profile_path(
        Vault::keyring(),
        Arc::new(RustTransport::new()?),
        Arc::new(SystemClock),
        path.as_deref(),
    )?)
}

pub fn broker() -> anyhow::Result<TokenBroker> {
    let broker = base_broker()?;
    // RT de SPA vence em ~24 h: com a WebView disponível, re-pareia sozinho (AUTH-RT-SPA-001).
    #[cfg(feature = "webview")]
    let broker = broker.with_repair(Arc::new(crate::session_cmd::SilentRepair));
    Ok(broker)
}

pub async fn token(resource: Resource) -> anyhow::Result<()> {
    let broker = broker()?;
    match resource {
        Resource::Teams => print_token(&broker.describe(TokenResource::Teams).await?),
        Resource::Chatsvcagg => print_token(&broker.describe(TokenResource::ChatSvcAgg).await?),
        Resource::Skype => {
            let ep = broker.chat_endpoint().await?;
            println!("Região:      {}", ep.region.as_deref().unwrap_or("?"));
            println!("ChatService: {}", ep.chat_service);
            println!("Expira em:   {}", until(ep.expires_at));
        }
    }
    println!("\n(sem WebView: só refresh silencioso — o token em si nunca é impresso)");
    Ok(())
}

fn print_token(info: &TokenInfo) {
    let c = &info.claims;
    let or = |v: &Option<String>| v.clone().unwrap_or_else(|| "?".into());
    println!("Recurso:   {:?}", info.resource);
    println!("aud:       {}", or(&c.audience));
    println!("tid:       {}", or(&c.tenant_id));
    println!("scp:       {}", or(&c.scopes));
    println!("appid:     {}", or(&c.app_id));
    println!("upn:       {}", or(&c.upn));
    println!("Expira em: {}", until(info.expires_at));
    if info.rotated_refresh_token {
        println!("RT:        rotacionado e regravado no keyring");
    }
}

fn until(t: SystemTime) -> String {
    match t.duration_since(SystemClock.utc_now()) {
        Ok(d) => human(d),
        Err(_) => "já expirado".into(),
    }
}
