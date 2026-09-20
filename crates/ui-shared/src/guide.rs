//! The channel guide: one row per channel, schedule along a time axis.
//!
//! Replaces the 4-column logo grid. Shared verbatim between desktop and
//! Android — the navigation is D-pad-shaped on both, which is also what
//! keyboard users get.

use crate::palette;
use crate::preview::{PreviewCache, PreviewCapture};
use crate::LogoCache;
use chrono::{DateTime, Duration, Timelike, Utc};
use egui::{Align2, Color32, FontId, Key, Rect, Sense, Stroke, Vec2};
use frenchetv_core::{Channel, ChannelCategory, EpgData};

#[derive(Debug)]
pub enum GuideAction {
    None,
    SelectChannel(Box<Channel>),
    /// Switch operator — back to the setup screen.
    ChangeProvider,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CategoryFilter {
    All,
    Category(ChannelCategory),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusLayer {
    FilterTabs,
    Rows,
}

/// Sizes for one viewport height.
///
/// A Fire TV Stick reports 1920x1080 at density 2.0, leaving egui ~540 logical
/// points. Comfortable sizing does not fit in that, and a guide that needs
/// scrolling to reach its own controls is not a guide.
struct Metrics {
    header: f32,
    ruler: f32,
    row_h: f32,
    logo_w: f32,
    preview_w: f32,
    left_block: f32,
    tab_font: f32,
    title_font: f32,
    meta_font: f32,
}

impl Metrics {
    fn for_height(h: f32) -> Self {
        if h < 620.0 {
            let preview_w = 112.0;
            Self {
                header: 40.0,
                ruler: 22.0,
                row_h: 60.0,
                logo_w: 48.0,
                preview_w,
                left_block: 48.0 + preview_w + 12.0,
                tab_font: 14.0,
                title_font: 14.0,
                meta_font: 11.0,
            }
        } else {
            let preview_w = 160.0;
            Self {
                header: 56.0,
                ruler: 28.0,
                row_h: 84.0,
                logo_w: 64.0,
                preview_w,
                left_block: 64.0 + preview_w + 16.0,
                tab_font: 17.0,
                title_font: 17.0,
                meta_font: 13.0,
            }
        }
    }
}

/// How much time the timeline shows at once, and how far one nudge moves it.
const WINDOW: i64 = 120;
const SCROLL_STEP: i64 = 30;

/// One frame of directional input.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NavInput {
    pub left: bool,
    pub right: bool,
    pub up: bool,
    pub down: bool,
    pub enter: bool,
}

/// What a navigation step decided. Kept separate from applying it so the
/// movement rules — the part with all the off-by-ones — can be tested without
/// an egui context, a font, or a screen.
#[derive(Debug, PartialEq, Eq)]
pub enum NavOutcome {
    None,
    /// Tune to this row index (into the filtered list).
    Select(usize),
    /// Apply the filter at this tab index.
    ApplyFilter(usize),
}

/// Scroll offset that keeps row `row` fully on screen, moving as little as
/// possible.
///
/// Minimal rather than centring: a guide that recentres on every D-pad press
/// makes the whole list lurch, and the rows around the focused one are the
/// context you are reading.
pub fn offset_keeping_row_visible(row: usize, row_h: f32, current: f32, viewport_h: f32) -> f32 {
    let top = row as f32 * row_h;
    let bottom = top + row_h;
    if top < current {
        top
    } else if bottom > current + viewport_h {
        (bottom - viewport_h).max(0.0)
    } else {
        current
    }
}

/// Largest rect of the given aspect ratio that fits inside `container`,
/// centred.
///
/// Channel logos are wide (360x90 is typical) and the row's logo slot is close
/// to square, so painting straight into the slot stretched every one of them.
pub fn fit_preserving_aspect(container: Rect, aspect: f32) -> Rect {
    if aspect <= 0.0 || !aspect.is_finite() || container.width() <= 0.0 || container.height() <= 0.0
    {
        return container;
    }
    let container_aspect = container.width() / container.height();
    let size = if container_aspect > aspect {
        // Container is wider than the image: height is the limit.
        Vec2::new(container.height() * aspect, container.height())
    } else {
        Vec2::new(container.width(), container.width() / aspect)
    };
    Rect::from_center_size(container.center(), size)
}

pub struct GuideScreen {
    channels: Vec<Channel>,
    filter: CategoryFilter,
    filter_focus_idx: usize,
    focus_layer: FocusLayer,
    focused_row: usize,
    focused_prog: usize,
    window_start: DateTime<Utc>,
    logos: LogoCache,
    epg: EpgData,
    show_locked: bool,
    /// Channel ids drawn last frame, in row order. The scroll area computes
    /// this to virtualize; the preview scheduler consumes the very same value
    /// rather than keeping a second, drifting notion of "on screen".
    visible: Vec<String>,
    visible_focus: Option<usize>,
    last_focus_row: usize,
    /// Set when focus moves, so the list scrolls to follow it. Without this the
    /// focused row walks off screen — and `visible_focus` then goes `None`,
    /// which also costs the scheduler its focus priority.
    scroll_pending: bool,
    scroll_offset: f32,
    viewport_h: f32,
    /// Live video for the focused row, when the platform can supply it.
    /// Desktop renders mpv into an egui texture so it can be painted anywhere;
    /// Android plays in a separate Activity and has none, so it stays `None`
    /// and the focused row shows its captured still like every other row.
    /// Live picture, tagged with the channel it belongs to. The tag matters:
    /// while a switch is settling the player is still rendering the previous
    /// channel, and an untagged texture gets painted into whichever row is
    /// focused now — showing the wrong channel's video.
    live_texture: Option<(String, egui::load::SizedTexture)>,
    /// Fades live video in over the still it replaces. Switching rows
    /// otherwise pops twice — live out, still in, live in — which reads as a
    /// flicker.
    live_fade: f32,
    fade_for_row: usize,
    /// Where the focused row's preview was painted last frame. The fullscreen
    /// transition zooms out of exactly this rectangle.
    focused_preview_rect: Option<Rect>,
    pub previews: PreviewCache,
}

impl GuideScreen {
    pub fn new(mut channels: Vec<Channel>, logos: LogoCache) -> Self {
        channels.sort_by_key(|c| c.number.unwrap_or(u32::MAX));
        Self {
            channels,
            filter: CategoryFilter::All,
            filter_focus_idx: 0,
            focus_layer: FocusLayer::Rows,
            focused_row: 0,
            focused_prog: 0,
            window_start: floor_to_half_hour(Utc::now()),
            logos,
            epg: EpgData::default(),
            show_locked: false,
            visible: Vec::new(),
            visible_focus: None,
            last_focus_row: 0,
            scroll_pending: false,
            scroll_offset: 0.0,
            viewport_h: 0.0,
            live_texture: None,
            live_fade: 0.0,
            fade_for_row: 0,
            focused_preview_rect: None,
            previews: PreviewCache::new(),
        }
    }

    /// Supply live video for the focused row. `None` falls back to the still.
    pub fn set_live_texture(&mut self, texture: Option<(String, egui::load::SizedTexture)>) {
        self.live_texture = texture;
    }

    /// The channel the cursor is on, if any.
    pub fn focused_channel(&self) -> Option<Channel> {
        self.filtered().get(self.focused_row).map(|c| (*c).clone())
    }

    /// Where the focused row's preview sits on screen, for the zoom-to-
    /// fullscreen transition to start from.
    pub fn focused_preview_rect(&self) -> Option<Rect> {
        self.focused_preview_rect
    }

    pub fn set_epg(&mut self, epg: EpgData) {
        self.epg = epg;
    }

    /// Advance the preview pipeline by one frame.
    ///
    /// Call once per update, after `show`, with whatever capture backend this
    /// platform has. Collects finished frames, then asks the scheduler what to
    /// capture next — the scheduler being a pure function in `core`, tested
    /// without a decoder anywhere near it.
    ///
    /// The visible range it reasons over is the one the scroll area computed
    /// while painting, not a second guess at what is on screen.
    pub fn drive_previews(
        &mut self,
        ctx: &egui::Context,
        capture: &mut dyn PreviewCapture,
        scheduler: &mut frenchetv_core::preview::Scheduler,
        now: std::time::Instant,
    ) {
        for (channel_id, frame) in capture.poll() {
            self.previews.insert(ctx, &channel_id, frame);
        }
        // Must happen every tick: an unreported failure leaves the channel in
        // flight, and the scheduler then skips it for good.
        for channel_id in capture.poll_failures() {
            self.previews.mark_failed(&channel_id);
        }

        // Restart the settle timer whenever focus moved. Without this a held
        // D-pad cancels and restarts capture forever and nothing ever
        // completes — during exactly the interaction previews exist for.
        if self.focused_row != self.last_focus_row {
            self.last_focus_row = self.focused_row;
            scheduler.note_focus_change(now);
        }

        if self.visible.is_empty() {
            return;
        }

        let view = frenchetv_core::preview::ViewState {
            visible: self.visible.clone(),
            focused: self.visible_focus,
        };
        if let Some(target) = scheduler.next_target(&view, &self.previews, now) {
            if let Some(channel) = self.channels.iter().find(|c| c.id == target) {
                self.previews.mark_requested(&target);
                capture.request(channel);
            }
        }
    }

    /// Abandon in-flight capture — the guide was left, or playback wants the
    /// decoder. Capture always yields to playback.
    pub fn cancel_previews(&mut self, capture: &mut dyn PreviewCapture) {
        capture.cancel();
        self.previews.clear_in_flight();
    }

    pub fn channels(&self) -> &[Channel] {
        &self.channels
    }

    /// Channel ids currently on screen, in row order — the scheduler's
    /// `ViewState.visible`.
    pub fn visible_channel_ids(&self) -> &[String] {
        &self.visible
    }

    /// Index into [`visible_channel_ids`] of the focused row.
    pub fn visible_focus(&self) -> Option<usize> {
        self.visible_focus
    }

    fn filter_labels() -> Vec<(&'static str, Option<ChannelCategory>)> {
        let mut labels: Vec<(&'static str, Option<ChannelCategory>)> = vec![("Tout", None)];
        for cat in ChannelCategory::fixed() {
            labels.push((cat.label(), Some(cat.clone())));
        }
        labels
    }

    fn filtered(&self) -> Vec<&Channel> {
        self.channels
            .iter()
            .filter(|c| self.show_locked || !c.locked)
            .filter(|c| match &self.filter {
                CategoryFilter::All => true,
                CategoryFilter::Category(cat) => &c.category == cat,
            })
            .collect()
    }

    pub fn show(&mut self, ctx: &egui::Context) -> GuideAction {
        let m = Metrics::for_height(ctx.screen_rect().height());

        // Live video fades in over whatever the row was already showing.
        const FADE_SECS: f32 = 0.22;
        if self.focused_row != self.fade_for_row {
            self.fade_for_row = self.focused_row;
            self.live_fade = 0.0;
        }
        match self.live_texture {
            // Reset while there is no live picture, so whenever one arrives —
            // a row change, or the stream reconnecting on the same row — it
            // fades in over the still rather than snapping on.
            None => self.live_fade = 0.0,
            Some(_) if self.live_fade < 1.0 => {
                self.live_fade =
                    (self.live_fade + ctx.input(|i| i.unstable_dt) / FADE_SECS).min(1.0);
                ctx.request_repaint();
            }
            Some(_) => {}
        }

        let input = ctx.input(|i| NavInput {
            left: i.key_pressed(Key::ArrowLeft),
            right: i.key_pressed(Key::ArrowRight),
            up: i.key_pressed(Key::ArrowUp),
            down: i.key_pressed(Key::ArrowDown),
            enter: i.key_pressed(Key::Enter),
        });

        let labels = Self::filter_labels();
        let rows = self.filtered().len();
        let progs = self.focused_row_programs();

        let action = match self.apply_nav(input, rows, labels.len(), progs) {
            NavOutcome::Select(idx) => match self.filtered().get(idx) {
                Some(ch) if !ch.locked => GuideAction::SelectChannel(Box::new((*ch).clone())),
                _ => GuideAction::None,
            },
            NavOutcome::ApplyFilter(idx) => {
                self.filter = match &labels[idx].1 {
                    None => CategoryFilter::All,
                    Some(cat) => CategoryFilter::Category(cat.clone()),
                };
                self.focused_row = 0;
                self.focused_prog = 0;
                self.scroll_pending = true;
                GuideAction::None
            }
            NavOutcome::None => GuideAction::None,
        };

        self.paint(ctx, &m, &labels);
        action
    }

    /// Pure movement rules. No egui, no clock, no I/O.
    ///
    /// `rows` and `tabs` are the current counts; `programs` is how many
    /// programmes the focused row shows in the visible window, which is what
    /// bounds the horizontal cursor before the timeline scrolls.
    pub fn apply_nav(
        &mut self,
        input: NavInput,
        rows: usize,
        tabs: usize,
        programs: usize,
    ) -> NavOutcome {
        match self.focus_layer {
            FocusLayer::FilterTabs => {
                if input.left && self.filter_focus_idx > 0 {
                    self.filter_focus_idx -= 1;
                }
                if input.right && self.filter_focus_idx + 1 < tabs {
                    self.filter_focus_idx += 1;
                }
                if input.down {
                    self.focus_layer = FocusLayer::Rows;
                }
                if input.enter {
                    return NavOutcome::ApplyFilter(self.filter_focus_idx);
                }
                NavOutcome::None
            }
            FocusLayer::Rows => {
                if input.up {
                    if self.focused_row == 0 {
                        self.focus_layer = FocusLayer::FilterTabs;
                    } else {
                        self.focused_row -= 1;
                        self.focused_prog = 0;
                        self.scroll_pending = true;
                    }
                }
                if input.down && self.focused_row + 1 < rows {
                    self.focused_row += 1;
                    self.focused_prog = 0;
                    self.scroll_pending = true;
                }
                // Left/Right walk programmes within the row; the window scrolls
                // once the cursor reaches an edge. Positional, so it stays
                // predictable — which is what a guide needs.
                if input.right {
                    if self.focused_prog + 1 < programs {
                        self.focused_prog += 1;
                    } else {
                        self.window_start += Duration::minutes(SCROLL_STEP);
                    }
                }
                if input.left {
                    if self.focused_prog > 0 {
                        self.focused_prog -= 1;
                    } else {
                        self.window_start -= Duration::minutes(SCROLL_STEP);
                    }
                }
                if input.enter && self.focused_row < rows {
                    // Enter always tunes to the row's channel, whichever
                    // programme column is highlighted. Giving a future
                    // programme its own meaning implies reminders, which this
                    // does not need.
                    return NavOutcome::Select(self.focused_row);
                }
                NavOutcome::None
            }
        }
    }

    fn focused_row_programs(&self) -> usize {
        let filtered = self.filtered();
        let Some(ch) = filtered.get(self.focused_row) else {
            return 0;
        };
        self.epg
            .programs_in_window(
                &ch.id,
                self.window_start,
                self.window_start + Duration::minutes(WINDOW),
            )
            .len()
    }

    fn paint(
        &mut self,
        ctx: &egui::Context,
        m: &Metrics,
        labels: &[(&'static str, Option<ChannelCategory>)],
    ) {
        let window_end = self.window_start + Duration::minutes(WINDOW);
        let filtered: Vec<Channel> = self.filtered().into_iter().cloned().collect();

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(palette::BACKGROUND))
            .show(ctx, |ui| {
                self.paint_header(ui, m, labels);

                let timeline_x = ui.min_rect().left() + m.left_block;
                let timeline_w = (ui.available_width() - m.left_block).max(80.0);
                self.paint_ruler(ui, m, timeline_x, timeline_w);

                if filtered.is_empty() {
                    ui.add_space(24.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new("Aucune chaîne dans cette catégorie")
                                .font(FontId::proportional(m.title_font))
                                .color(palette::TEXT_MUTED),
                        );
                    });
                    self.visible.clear();
                    self.visible_focus = None;
                    return;
                }

                let focused_row = self.focused_row.min(filtered.len() - 1);
                let mut visible = Vec::new();
                let mut visible_focus = None;

                let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
                if self.scroll_pending {
                    let target = offset_keeping_row_visible(
                        focused_row,
                        m.row_h,
                        self.scroll_offset,
                        self.viewport_h,
                    );
                    area = area.vertical_scroll_offset(target);
                    self.scroll_pending = false;
                }

                let out = area.show_rows(ui, m.row_h, filtered.len(), |ui, range| {
                    for idx in range.clone() {
                        let ch = &filtered[idx];
                        if idx == focused_row {
                            visible_focus = Some(visible.len());
                        }
                        visible.push(ch.id.clone());

                        let is_focused = self.focus_layer == FocusLayer::Rows && idx == focused_row;
                        self.paint_row(ui, m, ch, is_focused, timeline_w, window_end);
                    }
                });

                // Remembered for the next focus move: the offset maths needs
                // to know where the list actually sits and how tall it is.
                self.scroll_offset = out.state.offset.y;
                self.viewport_h = out.inner_rect.height();

                self.visible = visible;
                self.visible_focus = visible_focus;
            });
    }

    fn paint_header(
        &self,
        ui: &mut egui::Ui,
        m: &Metrics,
        labels: &[(&'static str, Option<ChannelCategory>)],
    ) {
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            for (idx, (label, cat)) in labels.iter().enumerate() {
                let focused =
                    self.focus_layer == FocusLayer::FilterTabs && self.filter_focus_idx == idx;
                let active = match (&self.filter, cat) {
                    (CategoryFilter::All, None) => true,
                    (CategoryFilter::Category(a), Some(b)) => a == b,
                    _ => false,
                };
                let color = if focused {
                    palette::TEXT
                } else if active {
                    palette::ACCENT
                } else {
                    palette::TEXT_MUTED
                };
                let galley = ui.painter().layout_no_wrap(
                    (*label).to_string(),
                    FontId::proportional(m.tab_font),
                    color,
                );
                let (rect, _) = ui.allocate_exact_size(
                    Vec2::new(galley.size().x + 16.0, m.header),
                    Sense::hover(),
                );
                if focused {
                    ui.painter().rect_stroke(
                        rect.shrink2(Vec2::new(2.0, 8.0)),
                        6.0,
                        Stroke::new(2.0_f32, palette::ACCENT),
                    );
                }
                ui.painter()
                    .galley(rect.center() - galley.size() / 2.0, galley, color);
            }
            // Clock, right-aligned: a guide is about time.
            let now = chrono::Local::now().format("%H:%M").to_string();
            let avail = ui.available_width();
            let (rect, _) = ui.allocate_exact_size(Vec2::new(avail, m.header), Sense::hover());
            ui.painter().text(
                rect.right_center() - Vec2::new(12.0, 0.0),
                Align2::RIGHT_CENTER,
                now,
                FontId::proportional(m.tab_font),
                palette::TEXT_MUTED,
            );
        });
    }

    fn paint_ruler(&self, ui: &mut egui::Ui, m: &Metrics, x0: f32, width: f32) {
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), m.ruler), Sense::hover());
        let painter = ui.painter();
        let mut t = self.window_start;
        while t < self.window_start + Duration::minutes(WINDOW) {
            let frac = (t - self.window_start).num_minutes() as f32 / WINDOW as f32;
            let x = x0 + frac * width;
            painter.text(
                egui::pos2(x + 4.0, rect.center().y),
                Align2::LEFT_CENTER,
                chrono::DateTime::<chrono::Local>::from(t)
                    .format("%H:%M")
                    .to_string(),
                FontId::proportional(m.meta_font),
                palette::TEXT_FAINT,
            );
            painter.line_segment(
                [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                Stroke::new(1.0_f32, palette::BORDER),
            );
            t += Duration::minutes(SCROLL_STEP);
        }
    }

    fn paint_row(
        &mut self,
        ui: &mut egui::Ui,
        m: &Metrics,
        ch: &Channel,
        focused: bool,
        timeline_w: f32,
        window_end: DateTime<Utc>,
    ) {
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), m.row_h), Sense::hover());
        let painter = ui.painter();

        painter.rect_filled(
            rect.shrink2(Vec2::new(0.0, 1.0)),
            4.0,
            if focused {
                palette::SURFACE_SELECTED
            } else {
                palette::SURFACE_ALT
            },
        );
        if focused {
            painter.rect_stroke(
                rect.shrink2(Vec2::new(0.0, 1.0)),
                4.0,
                Stroke::new(2.0_f32, palette::ACCENT),
            );
        }

        // ── left block: logo, then the live preview ──────────────────────
        let logo_rect = Rect::from_min_size(
            rect.left_top() + Vec2::new(4.0, 4.0),
            Vec2::new(m.logo_w - 8.0, m.row_h - 8.0),
        );
        let logo = ch
            .logo_url
            .as_ref()
            .and_then(|u| self.logos.lock().ok().and_then(|m| m.get(u).cloned()));
        match logo {
            Some(tex) => {
                let size = tex.size_vec2();
                painter.image(
                    tex.id(),
                    fit_preserving_aspect(logo_rect, size.x / size.y),
                    Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            None => {
                painter.text(
                    logo_rect.center(),
                    Align2::CENTER_CENTER,
                    ch.name.chars().take(3).collect::<String>(),
                    FontId::proportional(m.meta_font),
                    palette::TEXT_MUTED,
                );
            }
        }

        let preview_rect = Rect::from_min_size(
            rect.left_top() + Vec2::new(m.logo_w, 4.0),
            Vec2::new(m.preview_w, m.row_h - 8.0),
        );
        // Never a blank hole: a row with no captured frame yet shows the
        // channel name on the preview's dark fill.
        painter.rect_filled(preview_rect, 3.0, palette::SURFACE);
        if focused {
            self.focused_preview_rect = Some(preview_rect);
        }
        // Live video for the focused row when the platform supplies it,
        // otherwise the most recent captured still, and failing that the
        // channel's name — a row is never a blank hole.
        let uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        let still = self.previews.peek(&ch.id).map(|t| (t.id(), t.size_vec2()));
        // Only when it is this channel's picture: see `live_texture`.
        let live = match (focused, self.live_texture.as_ref()) {
            (true, Some((id, tex))) if *id == ch.id => Some((tex.id, tex.size)),
            _ => None,
        };

        // The still stays underneath while live fades in, so a row change
        // never blanks: the picture already there is replaced, not removed.
        if let Some((id, size)) = still {
            painter.image(
                id,
                fit_preserving_aspect(preview_rect, size.x / size.y),
                uv,
                Color32::WHITE,
            );
        }
        match live {
            Some((id, size)) => {
                let alpha = if still.is_some() { self.live_fade } else { 1.0 };
                painter.image(
                    id,
                    fit_preserving_aspect(preview_rect, size.x / size.y),
                    uv,
                    Color32::WHITE.gamma_multiply(alpha),
                );
            }
            None if still.is_none() => {
                painter.text(
                    preview_rect.center(),
                    Align2::CENTER_CENTER,
                    &ch.name,
                    FontId::proportional(m.meta_font),
                    palette::TEXT_FAINT,
                );
            }
            None => {}
        }

        // ── timeline ─────────────────────────────────────────────────────
        let x0 = rect.left() + m.left_block;
        let programs = self
            .epg
            .programs_in_window(&ch.id, self.window_start, window_end);

        if programs.is_empty() {
            painter.text(
                egui::pos2(x0 + 8.0, rect.center().y),
                Align2::LEFT_CENTER,
                "Programme non disponible",
                FontId::proportional(m.meta_font),
                palette::TEXT_FAINT,
            );
        } else {
            let per_min = timeline_w / WINDOW as f32;
            for p in programs {
                // Clip to the window: a programme that began earlier still
                // needs its title visible at the left edge.
                let start = p.start.max(self.window_start);
                let stop = p.stop.min(window_end);
                let x = x0 + (start - self.window_start).num_minutes() as f32 * per_min;
                let w = ((stop - start).num_minutes() as f32 * per_min - 2.0).max(6.0);
                let block = Rect::from_min_size(
                    egui::pos2(x, rect.top() + 6.0),
                    Vec2::new(w, m.row_h - 12.0),
                );
                painter.rect_filled(block, 3.0, palette::SURFACE);
                if w > 24.0 {
                    let text = painter.layout(
                        p.title.clone(),
                        FontId::proportional(m.title_font),
                        palette::TEXT,
                        w - 10.0,
                    );
                    painter.galley(
                        egui::pos2(block.left() + 6.0, block.center().y - text.size().y / 2.0),
                        text,
                        palette::TEXT,
                    );
                }
            }
        }

        // ── now-line ─────────────────────────────────────────────────────
        let now = Utc::now();
        if now >= self.window_start && now <= window_end {
            let x =
                x0 + (now - self.window_start).num_minutes() as f32 * (timeline_w / WINDOW as f32);
            painter.line_segment(
                [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                Stroke::new(2.0_f32, palette::NOW_LINE),
            );
        }
    }
}

/// Round down to the previous :00 or :30 — the timeline should start on a
/// boundary a viewer recognises, not on whatever minute it happens to be.
fn floor_to_half_hour(t: DateTime<Utc>) -> DateTime<Utc> {
    let minute = if t.minute() >= 30 { 30 } else { 0 };
    t.with_minute(minute)
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    use frenchetv_core::{ChannelCategory, StreamTemplate};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    fn chan(id: &str, name: &str, number: u32) -> Channel {
        Channel {
            id: id.into(),
            name: name.into(),
            logo_url: None,
            number: Some(number),
            category: ChannelCategory::Generalist,
            stream_template: StreamTemplate::Direct(
                "https://example.invalid/s.m3u8".parse().unwrap(),
            ),
            locked: false,
        }
    }

    fn screen(n: usize) -> GuideScreen {
        let channels = (0..n)
            .map(|i| chan(&format!("c{i}"), &format!("Chaine {i}"), i as u32))
            .collect();
        GuideScreen::new(channels, Arc::new(Mutex::new(HashMap::new())))
    }

    fn key(f: impl FnOnce(&mut NavInput)) -> NavInput {
        let mut i = NavInput::default();
        f(&mut i);
        i
    }

    #[test]
    fn down_moves_through_rows_and_stops_at_the_last() {
        let mut g = screen(3);
        for expected in [1, 2, 2] {
            g.apply_nav(key(|i| i.down = true), 3, 4, 0);
            assert_eq!(g.focused_row, expected);
        }
    }

    #[test]
    fn up_from_the_first_row_reaches_the_filter_tabs() {
        let mut g = screen(3);
        g.apply_nav(key(|i| i.down = true), 3, 4, 0);
        assert_eq!(g.focus_layer, FocusLayer::Rows);

        g.apply_nav(key(|i| i.up = true), 3, 4, 0);
        assert_eq!(g.focused_row, 0);
        // Only from row 0 does Up leave the rows.
        assert_eq!(g.focus_layer, FocusLayer::Rows);

        g.apply_nav(key(|i| i.up = true), 3, 4, 0);
        assert_eq!(g.focus_layer, FocusLayer::FilterTabs);
    }

    #[test]
    fn right_walks_programmes_then_scrolls_the_window() {
        let mut g = screen(2);
        let start = g.window_start;

        // Three programmes in the row: two moves stay put, the third scrolls.
        g.apply_nav(key(|i| i.right = true), 2, 4, 3);
        assert_eq!(g.focused_prog, 1);
        g.apply_nav(key(|i| i.right = true), 2, 4, 3);
        assert_eq!(g.focused_prog, 2);
        assert_eq!(g.window_start, start, "no scroll while cursor can advance");

        g.apply_nav(key(|i| i.right = true), 2, 4, 3);
        assert_eq!(g.focused_prog, 2, "cursor holds at the last programme");
        assert_eq!(g.window_start, start + Duration::minutes(SCROLL_STEP));
    }

    #[test]
    fn left_at_the_first_programme_scrolls_backwards() {
        let mut g = screen(2);
        let start = g.window_start;
        g.apply_nav(key(|i| i.left = true), 2, 4, 3);
        assert_eq!(g.focused_prog, 0);
        assert_eq!(g.window_start, start - Duration::minutes(SCROLL_STEP));
    }

    #[test]
    fn changing_row_resets_the_programme_cursor() {
        let mut g = screen(3);
        g.apply_nav(key(|i| i.right = true), 3, 4, 5);
        assert_eq!(g.focused_prog, 1);
        g.apply_nav(key(|i| i.down = true), 3, 4, 5);
        assert_eq!(g.focused_prog, 0, "a new row starts at its first programme");
    }

    #[test]
    fn enter_selects_the_row_whatever_the_programme_cursor() {
        let mut g = screen(3);
        g.apply_nav(key(|i| i.down = true), 3, 4, 5);
        g.apply_nav(key(|i| i.right = true), 3, 4, 5);
        assert_eq!(g.focused_prog, 1);
        assert_eq!(
            g.apply_nav(key(|i| i.enter = true), 3, 4, 5),
            NavOutcome::Select(1),
            "Enter tunes the row, not the highlighted programme"
        );
    }

    #[test]
    fn enter_on_an_empty_list_selects_nothing() {
        let mut g = screen(0);
        assert_eq!(
            g.apply_nav(key(|i| i.enter = true), 0, 4, 0),
            NavOutcome::None
        );
    }

    #[test]
    fn filter_tabs_clamp_at_both_ends_and_apply_on_enter() {
        let mut g = screen(2);
        g.apply_nav(key(|i| i.up = true), 2, 4, 0);
        assert_eq!(g.focus_layer, FocusLayer::FilterTabs);

        g.apply_nav(key(|i| i.left = true), 2, 4, 0);
        assert_eq!(g.filter_focus_idx, 0, "clamps at the first tab");

        for expected in [1, 2, 3, 3] {
            g.apply_nav(key(|i| i.right = true), 2, 4, 0);
            assert_eq!(g.filter_focus_idx, expected);
        }

        assert_eq!(
            g.apply_nav(key(|i| i.enter = true), 2, 4, 0),
            NavOutcome::ApplyFilter(3)
        );
    }

    #[test]
    fn down_from_the_tabs_returns_to_the_rows() {
        let mut g = screen(2);
        g.apply_nav(key(|i| i.up = true), 2, 4, 0);
        g.apply_nav(key(|i| i.down = true), 2, 4, 0);
        assert_eq!(g.focus_layer, FocusLayer::Rows);
    }

    #[test]
    fn logos_keep_their_aspect_ratio_inside_the_slot() {
        // A 360x90 logo (the shape most channels ship) in a near-square slot:
        // painting straight into the slot stretched it 4x vertically.
        let slot = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(60.0, 50.0));
        let fitted = fit_preserving_aspect(slot, 360.0 / 90.0);
        assert!((fitted.width() / fitted.height() - 4.0).abs() < 0.001);
        assert!(fitted.width() <= slot.width() + 0.001);
        assert!(fitted.height() <= slot.height() + 0.001);
        // Centred in the slot it was given.
        assert!((fitted.center().x - slot.center().x).abs() < 0.001);
        assert!((fitted.center().y - slot.center().y).abs() < 0.001);
    }

    #[test]
    fn fit_uses_whichever_dimension_binds() {
        let slot = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(100.0, 100.0));
        // Wide image: width fills, height shrinks.
        let wide = fit_preserving_aspect(slot, 2.0);
        assert!((wide.width() - 100.0).abs() < 0.001);
        assert!((wide.height() - 50.0).abs() < 0.001);
        // Tall image: height fills, width shrinks.
        let tall = fit_preserving_aspect(slot, 0.5);
        assert!((tall.height() - 100.0).abs() < 0.001);
        assert!((tall.width() - 50.0).abs() < 0.001);
    }

    #[test]
    fn fit_is_safe_on_degenerate_input() {
        let slot = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(10.0, 10.0));
        // A texture reporting zero height would otherwise divide by zero.
        assert_eq!(fit_preserving_aspect(slot, 0.0), slot);
        assert_eq!(fit_preserving_aspect(slot, f32::NAN), slot);
        let empty = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::ZERO);
        assert_eq!(fit_preserving_aspect(empty, 1.78), empty);
    }

    #[test]
    fn scrolling_follows_focus_by_the_smallest_move() {
        let row_h = 60.0;
        let viewport = 300.0; // five rows

        // Already visible: do not move. Recentring on every press would make
        // the list lurch and throw away the context around the focused row.
        assert_eq!(offset_keeping_row_visible(2, row_h, 0.0, viewport), 0.0);
        assert_eq!(offset_keeping_row_visible(4, row_h, 0.0, viewport), 0.0);

        // Just below the fold: scroll exactly enough to show it.
        assert_eq!(offset_keeping_row_visible(5, row_h, 0.0, viewport), 60.0);
        assert_eq!(offset_keeping_row_visible(6, row_h, 0.0, viewport), 120.0);

        // Above the fold: align its top.
        assert_eq!(offset_keeping_row_visible(3, row_h, 240.0, viewport), 180.0);
        assert_eq!(offset_keeping_row_visible(0, row_h, 240.0, viewport), 0.0);
    }

    #[test]
    fn scroll_offset_never_goes_negative() {
        // A viewport taller than the whole list must not scroll backwards past
        // the top.
        assert_eq!(offset_keeping_row_visible(0, 60.0, 0.0, 1000.0), 0.0);
        assert_eq!(offset_keeping_row_visible(1, 60.0, 0.0, 1000.0), 0.0);
    }

    #[test]
    fn moving_focus_requests_a_scroll() {
        let mut g = screen(20);
        assert!(!g.scroll_pending);
        g.apply_nav(key(|i| i.down = true), 20, 4, 0);
        assert!(g.scroll_pending, "a row move must ask the list to follow");

        g.scroll_pending = false;
        // Left/Right walk programmes, not rows — no scroll needed.
        g.apply_nav(key(|i| i.right = true), 20, 4, 3);
        assert!(!g.scroll_pending);
    }

    #[test]
    fn floors_to_the_previous_half_hour() {
        let t = Utc.with_ymd_and_hms(2026, 9, 19, 20, 47, 13).unwrap();
        assert_eq!(
            floor_to_half_hour(t),
            Utc.with_ymd_and_hms(2026, 9, 19, 20, 30, 0).unwrap()
        );
        let t = Utc.with_ymd_and_hms(2026, 9, 19, 20, 12, 59).unwrap();
        assert_eq!(
            floor_to_half_hour(t),
            Utc.with_ymd_and_hms(2026, 9, 19, 20, 0, 0).unwrap()
        );
        // Already on a boundary: unchanged.
        let t = Utc.with_ymd_and_hms(2026, 9, 19, 21, 0, 0).unwrap();
        assert_eq!(floor_to_half_hour(t), t);
    }

    #[test]
    fn metrics_shrink_for_a_short_viewport() {
        let tv = Metrics::for_height(540.0);
        let desktop = Metrics::for_height(900.0);
        assert!(tv.row_h < desktop.row_h);
        // Five rows must fit the Fire TV's ~540pt budget alongside the header
        // and ruler; that row count is what bounds the preview working set.
        assert!(tv.header + tv.ruler + 5.0 * tv.row_h < 540.0);
    }
}
