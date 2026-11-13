//! `f365 auth` (Checkpoint A) e `f365 whoami`. Só dados não secretos chegam aqui: o refresh token e
//! os cookies ficam dentro de `f365-auth` (`AGENTS.md`).

use anyhow::bail;
use f365_auth::vault::{StoredAccount, Vault};
use f365_auth::{Clock, SystemClock};
use std::time::Duration;

#[cfg(feature = "webview")]
pub fn auth() -> anyhow::Result<()> {
    use anyhow::Context;
    use f365_auth::capture::{pair, CaptureOptions};
    use f365_auth::vault::SERVICE;

    let local = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA não definido")?;
    let opts = CaptureOptions::new(std::path::Path::new(&local).join("fast365").join("webview"));
    println!("Abrindo o Teams Web — entre com a sua conta (senha + MFA) na janela.");
    let account = pair(&opts, &SystemClock, &Vault::keyring())?;
    println!(
        "Sessão capturada. Refresh token + {} cookie(s) de sessão gravados no keyring (serviço `{SERVICE}`).\n",
        account.cookie_count
    );
    print_account(&account, &SystemClock);
    Ok(())
}

#[cfg(not(feature = "webview"))]
pub fn auth() -> anyhow::Result<()> {
    bail!("`f365 auth` precisa da WebView: rode no Windows com `cargo run -p f365-cli --features webview -- auth`")
}

pub fn whoami() -> anyhow::Result<()> {
    match Vault::keyring().active_account()? {
        Some(account) => {
            print_account(&account, &SystemClock);
            Ok(())
        }
        None => bail!("nenhuma sessão salva neste usuário do Windows — rode `f365 auth`"),
    }
}

fn print_account(stored: &StoredAccount, clock: &dyn Clock) {
    let age = clock
        .utc_now()
        .duration_since(stored.captured_at)
        .unwrap_or_default();
    let family = if stored.family_id.is_some() {
        " (family RT)"
    } else {
        ""
    };
    println!("UPN:      {}", stored.account.upn);
    println!("Tenant:   {}", stored.account.tenant_id);
    println!("Cliente:  {}{family}", stored.client_id);
    println!("Sessão:   capturada há {}", human(age));
    println!("Cookies:  {} de sessão (ESTSAUTH*)", stored.cookie_count);
}

fn human(d: Duration) -> String {
    let mins = d.as_secs() / 60;
    match (mins / 60, mins % 60) {
        (0, 0) => "menos de 1 min".into(),
        (0, m) => format!("{m} min"),
        (h, m) => format!("{h} h {m:02} min"),
    }
}

#[cfg(test)]
mod tests {
    use super::human;
    use std::time::Duration;

    #[test]
    fn human_durations() {
        assert_eq!(human(Duration::from_secs(30)), "menos de 1 min");
        assert_eq!(human(Duration::from_secs(5 * 60)), "5 min");
        assert_eq!(human(Duration::from_secs(2 * 3600 + 7 * 60)), "2 h 07 min");
    }
}
