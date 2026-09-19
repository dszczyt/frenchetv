//! Live-preview frames for guide rows.
//!
//! The seam between the guide and the platform. The guide asks this cache for
//! a channel's most recent frame and never learns how one is produced; each
//! front-end implements [`PreviewCapture`] over mpv or ExoPlayer.
//!
//! Frames cross that seam as raw RGBA (`egui::ColorImage`). `Context::load_texture`
//! is renderer-agnostic, so the same delivery path works for desktop's wgpu and
//! Android's glow — which is what makes one shared guide affordable instead of
//! two parallel ones.

use egui::{ColorImage, Context, TextureHandle, TextureOptions};
use frenchetv_core::Channel;
use std::collections::HashMap;
use std::time::Instant;

/// Frames are captured and stored at this size. Small enough to upload and
/// keep cheaply, large enough to read as live television in a row; downscaled
/// at capture time rather than at render time.
pub const PREVIEW_WIDTH: usize = 256;
pub const PREVIEW_HEIGHT: usize = 144;

/// How many channels' frames to keep. At 256x144 RGBA that is ~147KB each, so
/// ~6MB at the cap — affordable even on a 32-bit Fire TV.
const CAPACITY: usize = 40;

/// Produces preview frames. Implemented per platform; never blocks.
pub trait PreviewCapture: Send {
    /// Begin capturing one frame for `channel`. Returns without waiting.
    fn request(&mut self, channel: &Channel);
    /// Frames finished since the last call, as (channel id, frame).
    fn poll(&mut self) -> Vec<(String, ColorImage)>;
    /// Abandon anything in flight — the guide was left, or playback needs the
    /// decoder back.
    fn cancel(&mut self);
}

struct Entry {
    texture: TextureHandle,
    captured_at: Instant,
    /// Monotonic counter for LRU eviction; cheaper than touching timestamps.
    last_used: u64,
}

#[derive(Default)]
pub struct PreviewCache {
    entries: HashMap<String, Entry>,
    in_flight: Vec<String>,
    failures: HashMap<String, u32>,
    tick: u64,
}

impl PreviewCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Latest frame for `channel_id`, marking it as used for LRU purposes.
    pub fn get(&mut self, channel_id: &str) -> Option<TextureHandle> {
        self.tick += 1;
        let tick = self.tick;
        let entry = self.entries.get_mut(channel_id)?;
        entry.last_used = tick;
        Some(entry.texture.clone())
    }

    /// Frame for `channel_id` without disturbing LRU order — for painting a row
    /// that is merely visible rather than being interacted with.
    pub fn peek(&self, channel_id: &str) -> Option<&TextureHandle> {
        self.entries.get(channel_id).map(|e| &e.texture)
    }

    pub fn insert(&mut self, ctx: &Context, channel_id: &str, frame: ColorImage) {
        self.tick += 1;
        let texture = ctx.load_texture(
            format!("preview:{channel_id}"),
            frame,
            TextureOptions::LINEAR,
        );
        self.entries.insert(
            channel_id.to_string(),
            Entry {
                texture,
                captured_at: Instant::now(),
                last_used: self.tick,
            },
        );
        self.in_flight.retain(|id| id != channel_id);
        self.failures.remove(channel_id);
        self.evict_if_needed();
    }

    pub fn mark_requested(&mut self, channel_id: &str) {
        if !self.in_flight.iter().any(|id| id == channel_id) {
            self.in_flight.push(channel_id.to_string());
        }
    }

    pub fn mark_failed(&mut self, channel_id: &str) {
        self.in_flight.retain(|id| id != channel_id);
        *self.failures.entry(channel_id.to_string()).or_insert(0) += 1;
    }

    pub fn clear_in_flight(&mut self) {
        self.in_flight.clear();
    }

    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    fn evict_if_needed(&mut self) {
        while self.entries.len() > CAPACITY {
            let Some(victim) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.entries.remove(&victim);
        }
    }
}

/// Lets `core`'s scheduler read this cache without depending on egui.
impl frenchetv_core::preview::CacheView for PreviewCache {
    fn captured_at(&self, id: &str) -> Option<Instant> {
        self.entries.get(id).map(|e| e.captured_at)
    }

    fn in_flight(&self, id: &str) -> bool {
        self.in_flight.iter().any(|x| x == id)
    }

    fn failures(&self, id: &str) -> u32 {
        self.failures.get(id).copied().unwrap_or(0)
    }
}
