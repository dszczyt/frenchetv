//! Row preview policy.
//!
//! Only the capture *policy* lives here. Capturing a frame needs a decoder, a
//! DRM proxy and a platform texture, none of which belong in core — but
//! deciding which channel to capture next is where this feature goes wrong,
//! and that decision needs none of them. Keeping it here makes it a table test
//! instead of a device session.

pub mod scheduler;

pub use scheduler::{CacheView, Scheduler, SchedulerConfig, ViewState};

/// A channel id, as carried by [`crate::channel::Channel::id`].
pub type ChannelId = String;
