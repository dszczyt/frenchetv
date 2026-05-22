/// Widevine DRM pipeline: CDM host → license exchange → CENC decryption → HTTP proxy.
///
/// Architecture:
/// ```
/// mpv → http://localhost:PORT → DrmProxy
///         ├── /manifest.mpd  → rewrites real MPD (strips ContentProtection, rewrites CDN URLs)
///         └── /cdn/<url>     → fetches from CDN, CENC-decrypts via CDM, returns plain fMP4
/// ```
pub mod cdm;
pub mod fmp4;
pub mod license;
pub mod proxy;

pub use proxy::DrmProxy;
