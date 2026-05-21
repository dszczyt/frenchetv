use async_trait::async_trait;
use crate::channel::Channel;
use crate::epg::EpgData;
use crate::stream::StreamUrl;
use super::traits::{Operator, Result};

pub struct OrangeOperator {
    pub(crate) access_token: Option<String>,
}

impl OrangeOperator {
    pub fn new() -> Self { Self { access_token: None } }
}

impl Default for OrangeOperator {
    fn default() -> Self { Self::new() }
}

#[async_trait]
impl Operator for OrangeOperator {
    fn name(&self) -> &'static str { "Orange TV" }
    fn requires_auth(&self) -> bool { true }
    async fn authenticate(&mut self, _u: &str, _p: &str) -> Result<()> { Ok(()) }
    async fn fetch_channels(&self) -> Result<Vec<Channel>> { Ok(vec![]) }
    async fn resolve_stream(&self, _c: &Channel) -> Result<StreamUrl> {
        Ok(StreamUrl::direct(url::Url::parse("http://localhost").unwrap()))
    }
    async fn fetch_epg(&self, _hours: u8) -> Result<Option<EpgData>> { Ok(None) }
}
