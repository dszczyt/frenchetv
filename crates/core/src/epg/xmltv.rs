use super::EpgData;
use crate::error::EpgError;

pub fn parse_xmltv(_xml_bytes: &[u8]) -> Result<EpgData, EpgError> {
    Ok(EpgData::default())
}
