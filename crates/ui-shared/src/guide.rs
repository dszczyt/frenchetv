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
            previews: PreviewCache::new(),
        }
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
                    }
                }
                if input.down && self.focused_row + 1 < rows {
                    self.focused_row += 1;
                    self.focused_prog = 0;
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

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show_rows(ui, m.row_h, filtered.len(), |ui, range| {
                        for idx in range.clone() {
                            let ch = &filtered[idx];
                            if idx == focused_row {
                                visible_focus = Some(visible.len());
                            }
                            visible.push(ch.id.clone());

                            let is_focused =
                                self.focus_layer == FocusLayer::Rows && idx == focused_row;
                            self.paint_row(ui, m, ch, is_focused, timeline_w, window_end);
                        }
                    });

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
                painter.image(
                    tex.id(),
                    logo_rect,
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
        match self.previews.peek(&ch.id) {
            Some(tex) => {
                painter.image(
                    tex.id(),
                    preview_rect,
                    Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            None => {
                painter.text(
                    preview_rect.center(),
                    Align2::CENTER_CENTER,
                    &ch.name,
                    FontId::proportional(m.meta_font),
                    palette::TEXT_FAINT,
                );
            }
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
