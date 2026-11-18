//! `f365 logout` — apaga a sessão local para trocar de conta.
//!
//! Roda **só** pelo dono, num terminal normal: mexe no keyring e no perfil da WebView (`AGENTS.md`).
//! O que apaga, sempre dentro de `%LOCALAPPDATA%\\fast365\\`:
//! - a sessão no keyring (RT, cookies e conta) — `Vault::logout`;
//! - o perfil da WebView (`webview\\`), que guarda os cookies do login Microsoft;
//! - com `--cache`, o banco do cache local (mensagens da conta antiga).
//!
//! Nada é apagado fora dessas rotas, e nunca se segue link simbólico/junção.

use anyhow::{bail, Context};
use f365_auth::vault::Vault;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

const APP_DIR: &str = "fast365";
const PROFILE_DIR: &str = "webview";
const CACHE_FILES: [&str; 3] = ["f365-cache.db", "f365-cache.db-wal", "f365-cache.db-shm"];

/// O que o `logout` vai apagar, calculado antes de tocar em qualquer coisa.
#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    root: PathBuf,
    profile: Option<PathBuf>,
    cache: Vec<PathBuf>,
}

/// Monta o plano a partir da raiz (`%LOCALAPPDATA%\\fast365`). Só inclui o que existe.
pub fn plan(root: &Path, with_cache: bool) -> anyhow::Result<Plan> {
    let profile = root.join(PROFILE_DIR);
    let profile = match std::fs::symlink_metadata(&profile) {
        Ok(meta) if meta.file_type().is_symlink() => {
            bail!(
                "{} é um link/junção; não vou apagar (remova você mesmo se for intencional)",
                profile.display()
            )
        }
        Ok(meta) if meta.is_dir() => Some(profile),
        Ok(_) => bail!("{} existe mas não é uma pasta", profile.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context(format!("lendo {}", profile.display())),
    };
    let cache = if with_cache {
        CACHE_FILES
            .iter()
            .map(|name| root.join(name))
            .filter(|path| std::fs::symlink_metadata(path).is_ok())
            .collect()
    } else {
        Vec::new()
    };
    Ok(Plan {
        root: root.to_path_buf(),
        profile,
        cache,
    })
}

/// Apaga o perfil e o cache do plano. A raiz só é removida se ficar vazia.
pub fn apply(plan: &Plan) -> anyhow::Result<()> {
    if let Some(profile) = &plan.profile {
        std::fs::remove_dir_all(profile).with_context(|| {
            format!(
                "não consegui apagar {} — feche o f365 e qualquer janela/processo do WebView2 e tente de novo",
                profile.display()
            )
        })?;
    }
    for file in &plan.cache {
        std::fs::remove_file(file)
            .with_context(|| format!("não consegui apagar {}", file.display()))?;
    }
    // `remove_dir` recusa pasta não vazia: nunca leva junto o que não é nosso.
    let _ = std::fs::remove_dir(&plan.root);
    Ok(())
}

fn confirmed(answer: &str) -> bool {
    matches!(
        answer.trim().to_lowercase().as_str(),
        "s" | "sim" | "y" | "yes"
    )
}

pub fn logout(with_cache: bool, yes: bool) -> anyhow::Result<()> {
    let local = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA não definido")?;
    let root = Path::new(&local).join(APP_DIR);
    let plan = plan(&root, with_cache)?;

    println!("Vou apagar, só neste usuário do Windows:");
    println!("  - a sessão `fast365` no keyring (Credential Manager)");
    match &plan.profile {
        Some(path) => println!("  - o perfil da WebView: {}", path.display()),
        None => println!("  - (perfil da WebView: não existe)"),
    }
    if with_cache {
        println!("  - o cache local ({} arquivo(s))", plan.cache.len());
    } else {
        println!("  (o cache local de mensagens fica; use --cache para apagá-lo também)");
    }

    if !yes {
        if !std::io::stdin().is_terminal() {
            bail!("sem terminal para confirmar: rode de novo com --yes");
        }
        print!("Continuar? [s/N] ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        if !confirmed(&line) {
            println!("Cancelado: nada foi apagado.");
            return Ok(());
        }
    }

    match Vault::keyring().logout()? {
        Some(upn) => println!("Sessão de {upn} removida do keyring."),
        None => println!("Não havia sessão no keyring."),
    }
    apply(&plan)?;
    println!("Pronto. Para entrar com outra conta: `f365 auth` (num terminal normal).");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pasta de teste própria (sem depender do temp do sistema nos nomes): limpa ao fim.
    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("f365-logout-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("cria pasta de teste");
            Self(path)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn plan_only_lists_what_exists_and_cache_is_opt_in() {
        let dir = Dir::new("plan");
        std::fs::create_dir_all(dir.0.join(PROFILE_DIR)).unwrap();
        std::fs::write(dir.0.join("f365-cache.db"), b"x").unwrap();

        let without = plan(&dir.0, false).unwrap();
        assert_eq!(without.profile, Some(dir.0.join(PROFILE_DIR)));
        assert!(without.cache.is_empty(), "cache só com --cache");

        let with = plan(&dir.0, true).unwrap();
        assert_eq!(with.cache, vec![dir.0.join("f365-cache.db")]);
    }

    #[test]
    fn apply_removes_profile_and_cache_but_keeps_foreign_files() {
        let dir = Dir::new("apply");
        let profile = dir.0.join(PROFILE_DIR);
        std::fs::create_dir_all(profile.join("EBWebView")).unwrap();
        std::fs::write(profile.join("EBWebView").join("Cookies"), b"x").unwrap();
        std::fs::write(dir.0.join("f365-cache.db"), b"x").unwrap();
        std::fs::write(dir.0.join("f365-cache.db-wal"), b"x").unwrap();
        std::fs::write(dir.0.join("nao-e-nosso.txt"), b"fica").unwrap();

        apply(&plan(&dir.0, true).unwrap()).unwrap();

        assert!(!profile.exists());
        assert!(!dir.0.join("f365-cache.db").exists());
        assert!(!dir.0.join("f365-cache.db-wal").exists());
        assert!(
            dir.0.join("nao-e-nosso.txt").exists(),
            "o que não é nosso fica"
        );
        assert!(dir.0.exists(), "raiz não vazia não é removida");
    }

    #[test]
    fn apply_removes_an_empty_root_and_is_idempotent() {
        let dir = Dir::new("empty-root");
        std::fs::create_dir_all(dir.0.join(PROFILE_DIR)).unwrap();

        let planned = plan(&dir.0, false).unwrap();
        apply(&planned).unwrap();
        assert!(!dir.0.exists(), "raiz vazia some junto");

        let again = plan(&dir.0, false).unwrap();
        assert_eq!(again.profile, None);
        apply(&again).unwrap();
    }

    #[test]
    fn missing_profile_is_not_an_error() {
        let dir = Dir::new("missing");
        let planned = plan(&dir.0, true).unwrap();
        assert_eq!(planned.profile, None);
        assert!(planned.cache.is_empty());
    }

    #[test]
    fn a_file_named_like_the_profile_is_refused() {
        let dir = Dir::new("not-a-dir");
        std::fs::write(dir.0.join(PROFILE_DIR), b"x").unwrap();
        assert!(plan(&dir.0, false).is_err());
    }

    #[test]
    fn only_explicit_yes_confirms() {
        assert!(confirmed("s\n") && confirmed(" SIM ") && confirmed("y") && confirmed("Yes"));
        assert!(!confirmed("") && !confirmed("n") && !confirmed("talvez") && !confirmed("\n"));
    }
}
