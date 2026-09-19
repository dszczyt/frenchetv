pub mod bouygues;
pub mod orange;
pub mod traits;

pub use traits::{AuthPhase, Operator};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperatorKind {
    Orange,
    Bouygues,
}

impl OperatorKind {
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Orange => "Orange TV",
            Self::Bouygues => "Bouygues Bbox",
        }
    }
    /// The stable config/keyring identifier string.
    pub fn config_str(&self) -> &'static str {
        match self {
            Self::Orange => "orange",
            Self::Bouygues => "bouygues",
        }
    }
    pub fn from_config_str(s: &str) -> Option<Self> {
        match s {
            "orange" => Some(Self::Orange),
            "bouygues" => Some(Self::Bouygues),
            _ => None,
        }
    }
    pub fn requires_auth(&self) -> bool {
        true
    }

    /// Whether this operator can authenticate through its mobile app instead
    /// of a password.
    ///
    /// Orange routes app-capable accounts to "Orange et Moi" itself — the
    /// client cannot request it, since `/api/access` takes no parameter for
    /// it. This only says the flow is reachable, so the setup screen can stop
    /// demanding a password the user may not need.
    pub fn supports_app_auth(&self) -> bool {
        matches!(self, Self::Orange)
    }

    /// Label for an operator-specific extra credential field shown on the setup
    /// screen, or `None` when only username + password are required. Bouygues
    /// requires the account holder's last name in its CAS login form.
    pub fn extra_credential_label(&self) -> Option<&'static str> {
        match self {
            Self::Bouygues => Some("Nom de famille"),
            Self::Orange => None,
        }
    }
}

pub struct OperatorRegistry;

impl OperatorRegistry {
    pub fn all() -> &'static [OperatorKind] {
        &[OperatorKind::Orange, OperatorKind::Bouygues]
    }
    pub fn build(kind: &OperatorKind) -> Box<dyn Operator> {
        match kind {
            OperatorKind::Orange => Box::new(orange::OrangeOperator::new()),
            OperatorKind::Bouygues => Box::new(bouygues::BouyguesOperator::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_orange_advertises_app_auth() {
        assert!(OperatorKind::Orange.supports_app_auth());
        assert!(!OperatorKind::Bouygues.supports_app_auth());
    }

    #[test]
    fn every_registered_operator_builds_and_round_trips_its_config_key() {
        for kind in OperatorRegistry::all() {
            let key = kind.config_str();
            assert_eq!(
                OperatorKind::from_config_str(key).as_ref(),
                Some(kind),
                "config_str is the keyring and config key; a kind that does not \
                 round-trip would orphan saved sessions"
            );
            let _ = OperatorRegistry::build(kind);
        }
    }
}
