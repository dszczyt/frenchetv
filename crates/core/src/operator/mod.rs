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
