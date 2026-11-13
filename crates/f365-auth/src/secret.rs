//! Segredo em memória: nunca aparece em `Debug`/logs e é zerado ao sair de escopo.

use zeroize::Zeroizing;

#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Zeroizing<String>);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into()))
    }

    /// Acesso explícito ao valor — cada chamada é um ponto auditável de uso do segredo.
    /// `pub(crate)`: o token nunca cruza a fronteira de `f365-auth` (`AGENTS.md`).
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret(<{} bytes ocultos>)", self.0.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_the_value() {
        let s = Secret::new("eyJ0eXAi.super-secreto");
        let shown = format!("{s:?} {:?}", vec![s.clone()]);
        assert!(!shown.contains("super-secreto"));
        assert!(shown.contains("bytes ocultos"));
    }
}
