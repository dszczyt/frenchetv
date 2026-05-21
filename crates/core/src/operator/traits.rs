use async_trait::async_trait;
use crate::channel::Channel;
use crate::epg::EpgData;
use crate::stream::StreamUrl;
use crate::error::OperatorError;

pub type Result<T> = std::result::Result<T, OperatorError>;

#[async_trait]
pub trait Operator: Send + Sync {
    fn name(&self) -> &'static str;
    fn requires_auth(&self) -> bool;
    async fn authenticate(&mut self, username: &str, password: &str) -> Result<()>;
    async fn fetch_channels(&self) -> Result<Vec<Channel>>;
    async fn resolve_stream(&self, channel: &Channel) -> Result<StreamUrl>;
    async fn fetch_epg(&self, hours: u8) -> Result<Option<EpgData>>;
}
