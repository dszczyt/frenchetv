use crate::channel::Channel;
use crate::epg::EpgData;
use crate::error::OperatorError;
use crate::stream::StreamUrl;
use async_trait::async_trait;

pub type Result<T> = std::result::Result<T, OperatorError>;

/// Which credential step follows the initial login submission. Returned by each
/// phased-auth method to tell the caller what to do next; the driver loops until
/// it sees `Done`.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthPhase {
    /// Classic flow: proceed with `complete_auth_password`.
    Password,
    /// Out-of-band flow: a push notification was sent to the user's phone.
    /// Call `wait_for_push_auth` and show a "please approve on your phone" UI.
    Push,
    /// A one-time code was sent to the user (SMS/app); show an entry field and
    /// call `submit_otp` with the code (e.g. Bouygues `mfa-otp-bytel`).
    Otp,
    /// Authentication is complete; the session is established.
    Done,
}

#[async_trait]
pub trait Operator: Send + Sync {
    fn name(&self) -> &'static str;
    fn requires_auth(&self) -> bool;

    /// Single-call auth for operators that do not require out-of-band factors.
    async fn authenticate(&mut self, username: &str, password: &str) -> Result<()>;

    /// Provide an operator-specific extra credential gathered at setup time
    /// (e.g. Bouygues requires the account holder's last name in its CAS login
    /// form). Default is a no-op for operators that need only username/password.
    fn set_extra_credential(&mut self, _value: &str) {}

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

    /// Phase 2a (password path): submit the password. Returns the next phase —
    /// `Done` when auth is complete, or `Otp` when a one-time code is required.
    async fn complete_auth_password(&mut self, _password: &str) -> Result<AuthPhase> {
        Err(OperatorError::AuthFailed(
            "phased auth not supported by this operator".into(),
        ))
    }

    /// Phase 2b (push path): poll until the user approves the push notification
    /// or the attempt times out. The password is forwarded in case account
    /// selection leads back to a `/api/password` step (e.g. Orange remoteAccounts).
    async fn wait_for_push_auth(&mut self, _password: &str) -> Result<AuthPhase> {
        Err(OperatorError::AuthFailed(
            "phased auth not supported by this operator".into(),
        ))
    }

    /// Phase 2c (OTP path): submit the one-time code the user received. Returns
    /// `Done` on success.
    async fn submit_otp(&mut self, _code: &str) -> Result<AuthPhase> {
        Err(OperatorError::AuthFailed(
            "OTP auth not supported by this operator".into(),
        ))
    }

    async fn fetch_channels(&self) -> Result<Vec<Channel>>;
    async fn resolve_stream(&self, channel: &Channel) -> Result<StreamUrl>;
    async fn fetch_epg(&self, hours: u8) -> Result<Option<EpgData>>;

    /// Returns the current session token (e.g. `wassup` cookie) if authenticated.
    /// Used to persist the session across app restarts.
    fn session_token(&self) -> Option<&str> {
        None
    }

    /// Restore a previously saved session token and verify it is still valid
    /// by fetching the tv_token.  Returns Ok if the session is usable.
    async fn restore_session(&mut self, _token: &str) -> Result<()> {
        Err(OperatorError::AuthFailed(
            "session restore not supported by this operator".into(),
        ))
    }
}
