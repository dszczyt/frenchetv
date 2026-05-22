use async_trait::async_trait;
use crate::channel::Channel;
use crate::epg::EpgData;
use crate::stream::StreamUrl;
use crate::error::OperatorError;

pub type Result<T> = std::result::Result<T, OperatorError>;

/// Which credential step follows the initial login submission.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthPhase {
    /// Classic flow: proceed with `complete_auth_password`.
    Password,
    /// Out-of-band flow: a push notification was sent to the user's phone.
    /// Call `wait_for_push_auth` and show a "please approve on your phone" UI.
    Push,
}

#[async_trait]
pub trait Operator: Send + Sync {
    fn name(&self) -> &'static str;
    fn requires_auth(&self) -> bool;

    /// Single-call auth for operators that do not require out-of-band factors.
    async fn authenticate(&mut self, username: &str, password: &str) -> Result<()>;

    /// Returns true when this operator may require an out-of-band factor
    /// (e.g., mobile push). When true, use the phased-auth methods instead
    /// of (or in addition to) `authenticate`.
    fn uses_phased_auth(&self) -> bool {
        false
    }

    /// Phase 1: submit the username and detect what credential step comes next.
    async fn begin_auth(&mut self, _username: &str) -> Result<AuthPhase> {
        Err(OperatorError::AuthFailed(
            "phased auth not supported by this operator".into(),
        ))
    }

    /// Phase 2a (password path): submit the password and complete authentication.
    async fn complete_auth_password(&mut self, _password: &str) -> Result<()> {
        Err(OperatorError::AuthFailed(
            "phased auth not supported by this operator".into(),
        ))
    }

    /// Phase 2b (push path): poll until the user approves the push notification
    /// or the attempt times out. The password is forwarded in case account
    /// selection leads back to a `/api/password` step (e.g. Orange remoteAccounts).
    async fn wait_for_push_auth(&mut self, _password: &str) -> Result<()> {
        Err(OperatorError::AuthFailed(
            "phased auth not supported by this operator".into(),
        ))
    }

    async fn fetch_channels(&self) -> Result<Vec<Channel>>;
    async fn resolve_stream(&self, channel: &Channel) -> Result<StreamUrl>;
    async fn fetch_epg(&self, hours: u8) -> Result<Option<EpgData>>;
}
