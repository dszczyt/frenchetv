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
            "orange"   => Some(Self::Orange),
            "bouygues" => Some(Self::Bouygues),
            _ => None,
        }
    }
    pub fn requires_auth(&self) -> bool { true }
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
