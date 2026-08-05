use egui::{FontId, Key, RichText, ScrollArea, TextEdit, Vec2};

const LOGO_BYTES: &[u8] = include_bytes!("../../../../assets/logo.png");
use crate::app::LogoCache;
use crate::theme::{color, space, text};
use crate::widgets::{focus_ring, hover_card};
use egui_phosphor::regular as icon;
use frenchetv_core::{Channel, ChannelCategory};

/// Target tile width the responsive grid aims for — shared between the
/// keyboard-nav row/col math below and the actual tile layout further down,
/// so the two can never disagree about how many columns are on screen.
const TARGET_TILE_W: f32 = 220.0;

/// Which group of on-screen elements arrow keys currently move focus
/// within. Mirrors `ui-android`'s D-pad focus model — desktop keyboard
/// users get the same navigable highlight, not just mouse hover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusLayer {
    FilterTabs,
    Grid,
}

pub struct ChannelListScreen {
    channels: Vec<Channel>,
    filter: CategoryFilter,
    search: String,
    show_locked: bool,
    logos: LogoCache,
    focus_layer: FocusLayer,
    filter_focus_idx: usize,
    focused_row: usize,
    focused_col: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CategoryFilter {
    All,
    Category(ChannelCategory),
}

#[derive(Debug)]
pub enum ChannelListAction {
    None,
    SelectChannel(Box<Channel>),
    /// User wants to switch operator — return to the Setup screen.
    ChangeProvider,
}

impl ChannelListScreen {
    pub fn new(mut channels: Vec<Channel>, logos: LogoCache) -> Self {
        channels.sort_by_key(|c| c.number.unwrap_or(u32::MAX));
        Self {
            channels,
            filter: CategoryFilter::All,
            search: String::new(),
            show_locked: false,
            logos,
            focus_layer: FocusLayer::Grid,
            filter_focus_idx: 0,
            focused_row: 0,
            focused_col: 0,
        }
    }

    fn filter_labels() -> Vec<(&'static str, Option<ChannelCategory>)> {
        let mut labels: Vec<(&'static str, Option<ChannelCategory>)> = vec![("Tout", None)];
        for cat in ChannelCategory::fixed() {
            labels.push((cat.label(), Some(cat.clone())));
        }
        labels
    }

    /// Cloned rather than borrowed — the result outlives the per-frame nav
    /// bookkeeping below, which needs `&mut self` (to move keyboard focus)
    /// while `visible` is still in scope; borrowing `self.channels` would
    /// make that a conflicting borrow.
    fn visible_channels(&self) -> Vec<Channel> {
        let search_lower = self.search.to_lowercase();
        self.channels
            .iter()
            .filter(|c| {
                let matches_filter = match &self.filter {
                    CategoryFilter::All => true,
                    CategoryFilter::Category(cat) => &c.category == cat,
                };
                let matches_search = search_lower.is_empty()
                    || c.name.to_lowercase().contains(&search_lower)
                    || c.number
                        .is_some_and(|n| n.to_string().contains(&search_lower));
                let matches_locked = self.show_locked || !c.locked;
                matches_filter && matches_search && matches_locked
            })
            .cloned()
            .collect()
    }

    pub fn show(&mut self, ctx: &egui::Context) -> ChannelListAction {
        let mut action = ChannelListAction::None;

        // Snapshot once so a single keypress can only ever drive ONE layer's
        // handler below, even though that handler runs at two different
        // points in this function (FilterTabs before the tabs are painted,
        // Grid once the column count is known) — without the snapshot, a
        // key that flips `self.focus_layer` from FilterTabs to Grid earlier
        // in the frame would then also be re-read by the Grid handler later
        // in the *same* frame, double-moving focus on one keypress.
        let starting_layer = self.focus_layer;
        let (left, right, up, down, enter) = ctx.input(|i| {
            (
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::Enter),
            )
        });
        // Don't hijack arrow/enter while a real widget (search box, a
        // button) holds keyboard focus — e.g. typing in the search field.
        let nav_active = ctx.memory(|m| m.focused().is_none());

        if nav_active && starting_layer == FocusLayer::FilterTabs {
            let labels = Self::filter_labels();
            if left && self.filter_focus_idx > 0 {
                self.filter_focus_idx -= 1;
            }
            if right && self.filter_focus_idx + 1 < labels.len() {
                self.filter_focus_idx += 1;
            }
            if down {
                self.focus_layer = FocusLayer::Grid;
                self.focused_row = 0;
                self.focused_col = 0;
            }
            if enter {
                let (_, cat_opt) = &labels[self.filter_focus_idx];
                self.filter = match cat_opt {
                    None => CategoryFilter::All,
                    Some(cat) => CategoryFilter::Category(cat.clone()),
                };
                let new_count = self.visible_channels().len();
                let cols = ((ctx.screen_rect().width() - space::MD).max(0.0) + space::MD)
                    / (TARGET_TILE_W + space::MD);
                let cols = (cols.floor().max(1.0)) as usize;
                let new_rows = new_count.div_ceil(cols).max(1);
                self.focused_row = self.focused_row.min(new_rows - 1);
                let row_start = self.focused_row * cols;
                let row_len = new_count.saturating_sub(row_start).min(cols).max(1);
                self.focused_col = self.focused_col.min(row_len - 1);
            }
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(color::BG))
            .show(ctx, |ui| {
                // ── Top bar ──────────────────────────────────────────────────
                ui.horizontal(|ui| {
                    ui.add_space(space::MD);
                    ui.add(
                        egui::Image::from_bytes("bytes://frenchetv-logo.png", LOGO_BYTES)
                            .max_size(egui::vec2(144.0, 36.0))
                            .maintain_aspect_ratio(true),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(space::MD);
                        ui.add(
                            TextEdit::singleline(&mut self.search)
                                .hint_text(format!("{}  Rechercher…", icon::MAGNIFYING_GLASS))
                                .desired_width(200.0),
                        );
                        ui.add_space(space::SM + space::XS);
                        if ui
                            .button(
                                RichText::new(format!(
                                    "{}  Changer d'opérateur",
                                    icon::ARROWS_LEFT_RIGHT
                                ))
                                .font(FontId::proportional(text::SMALL)),
                            )
                            .clicked()
                        {
                            action = ChannelListAction::ChangeProvider;
                        }
                        ui.add_space(space::SM + space::XS);
                        let locked_count = self.channels.iter().filter(|c| c.locked).count();
                        if locked_count > 0 {
                            let lock_label = if self.show_locked {
                                format!(
                                    "{}  Masquer ({} canal(s))",
                                    icon::LOCK_SIMPLE,
                                    locked_count
                                )
                            } else {
                                format!(
                                    "{}  Afficher ({} canal(s))",
                                    icon::LOCK_SIMPLE,
                                    locked_count
                                )
                            };
                            if ui
                                .selectable_label(
                                    self.show_locked,
                                    RichText::new(lock_label)
                                        .font(FontId::proportional(text::SMALL)),
                                )
                                .clicked()
                            {
                                self.show_locked = !self.show_locked;
                            }
                        }
                    });
                });

                ui.add_space(space::SM);

                // ── Filter tabs ──────────────────────────────────────────────
                // `selectable_label` gives the active tab a filled background
                // (not just a color swap), so selection reads even without color.
                // A thin accent ring is layered on top to show keyboard focus,
                // independent of which tab is actually the active filter.
                ui.horizontal(|ui| {
                    ui.add_space(space::SM + space::XS);

                    for (tab_idx, (label, cat_opt)) in Self::filter_labels().iter().enumerate() {
                        let is_kbd_focused = self.focus_layer == FocusLayer::FilterTabs
                            && self.filter_focus_idx == tab_idx;
                        let is_active = match (&self.filter, cat_opt) {
                            (CategoryFilter::All, None) => true,
                            (CategoryFilter::Category(a), Some(b)) => a == b,
                            _ => false,
                        };
                        let clicked = focus_ring(ui, is_kbd_focused, |ui| {
                            ui.selectable_label(
                                is_active,
                                RichText::new(*label).font(FontId::proportional(text::BODY - 1.0)),
                            )
                            .clicked()
                        });
                        if clicked {
                            self.filter = match cat_opt {
                                None => CategoryFilter::All,
                                Some(cat) => CategoryFilter::Category(cat.clone()),
                            };
                            self.focus_layer = FocusLayer::FilterTabs;
                            self.filter_focus_idx = tab_idx;
                        }
                    }
                });

                ui.separator();

                // ── Filtering ────────────────────────────────────────────────
                let visible = self.visible_channels();

                // ── Grid ─────────────────────────────────────────────────────
                ScrollArea::vertical().show(ui, |ui| {
                    // Column count derived from available width instead of a
                    // fixed 4 — `horizontal_wrapped` wraps to a new row on its
                    // own once tiles no longer fit, so there's no manual
                    // `end_row()` bookkeeping to keep in sync with a column count.
                    let gap = space::MD;
                    // Reserve a little slack (scrollbar width + rounding) so the
                    // row never computes to *exactly* the available width — that
                    // would wrap one column early and leave an empty gutter.
                    let avail_w = (ui.available_width() - space::MD).max(0.0);
                    let cols_f = ((avail_w + gap) / (TARGET_TILE_W + gap)).floor().max(1.0);
                    let tile_width = ((avail_w - gap * (cols_f - 1.0)) / cols_f)
                        .floor()
                        .max(150.0);
                    let tile_height = 108.0_f32;
                    let logo_size = Vec2::new(tile_width - space::MD * 2.0, 50.0);
                    let cols = cols_f as usize;

                    // Computed even when `visible` is empty (row_count then
                    // clamps to 1 via `.max(1)`) so Up still escapes an
                    // empty grid back to the filter tabs instead of getting
                    // keyboard-nav stuck on a "no results" screen.
                    //
                    // Same single-snapshot guard as the FilterTabs handler
                    // above — only the layer active at the *start* of this
                    // frame reacts, so a layer-switching key can't also be
                    // replayed against the layer it just switched into.
                    if nav_active && starting_layer == FocusLayer::Grid {
                        let row_count = visible.len().div_ceil(cols).max(1);
                        if up {
                            if self.focused_row == 0 {
                                self.focus_layer = FocusLayer::FilterTabs;
                            } else {
                                self.focused_row -= 1;
                            }
                        }
                        if down && self.focused_row + 1 < row_count {
                            self.focused_row += 1;
                        }
                        if left && self.focused_col > 0 {
                            self.focused_col -= 1;
                        }
                        if right {
                            let row_start = self.focused_row * cols;
                            let cols_in_row = visible.len().saturating_sub(row_start).min(cols);
                            if self.focused_col + 1 < cols_in_row {
                                self.focused_col += 1;
                            }
                        }
                        if enter {
                            let idx = self.focused_row * cols + self.focused_col;
                            if let Some(channel) = visible.get(idx) {
                                if !channel.locked {
                                    action = ChannelListAction::SelectChannel(Box::new(
                                        (*channel).clone(),
                                    ));
                                }
                            }
                        }
                    }
                    let focused_idx = self.focused_row * cols + self.focused_col;

                    if visible.is_empty() {
                        empty_state(ui, &self.search);
                        return;
                    }

                    ui.spacing_mut().item_spacing = Vec2::new(space::MD, space::MD);

                    ui.horizontal_wrapped(|ui| {
                        for (i, channel) in visible.iter().enumerate() {
                            let is_kbd_focused =
                                self.focus_layer == FocusLayer::Grid && i == focused_idx;
                            let is_locked = channel.locked;
                            let text_color = if is_locked {
                                color::TEXT_DISABLED
                            } else {
                                color::TEXT
                            };
                            let id = ui.make_persistent_id(("channel_tile", &channel.id));

                            let resp = hover_card(
                                ui,
                                id,
                                Vec2::new(tile_width, tile_height),
                                is_kbd_focused,
                                !is_locked,
                                |ui| {
                                    ui.vertical(|ui| {
                                        // ── Logo ────────────────────
                                        let cached_texture =
                                            channel.logo_url.as_ref().and_then(|url| {
                                                self.logos
                                                    .lock()
                                                    .ok()
                                                    .and_then(|m| m.get(url.as_str()).cloned())
                                            });
                                        if let Some(texture) = cached_texture {
                                            ui.add(
                                                egui::Image::from_texture(
                                                    egui::load::SizedTexture::from_handle(&texture),
                                                )
                                                .max_size(logo_size)
                                                .maintain_aspect_ratio(true)
                                                .sense(egui::Sense::hover()),
                                            );
                                        } else if channel.logo_url.is_some() {
                                            // URL known but fetch not yet complete — spinner
                                            let (rect, _) = ui.allocate_exact_size(
                                                logo_size,
                                                egui::Sense::hover(),
                                            );
                                            ui.painter().rect_filled(
                                                rect,
                                                4.0,
                                                color::SURFACE_HOVER,
                                            );
                                            let spin_rect = egui::Rect::from_center_size(
                                                rect.center(),
                                                egui::Vec2::splat(24.0),
                                            );
                                            ui.put(spin_rect, egui::Spinner::new().size(16.0));
                                        } else {
                                            // No logo for this channel
                                            let (rect, _) = ui.allocate_exact_size(
                                                logo_size,
                                                egui::Sense::hover(),
                                            );
                                            ui.painter().rect_filled(
                                                rect,
                                                4.0,
                                                color::SURFACE_HOVER,
                                            );
                                        }

                                        ui.add_space(space::XS);

                                        // ── Name + number ────────────
                                        let num_prefix = channel
                                            .number
                                            .map(|n| format!("{} · ", n))
                                            .unwrap_or_default();
                                        let label = if is_locked {
                                            format!(
                                                "{}{}  {}",
                                                num_prefix,
                                                channel.name,
                                                icon::LOCK_SIMPLE
                                            )
                                        } else {
                                            format!("{}{}", num_prefix, channel.name)
                                        };
                                        ui.label(
                                            RichText::new(label)
                                                .font(FontId::proportional(text::SMALL - 1.0))
                                                .color(text_color),
                                        );
                                    });
                                },
                            );

                            // Keep the keyboard-focused tile in view — only on
                            // the frame a nav key actually moved it, so this
                            // doesn't fight a manual scroll-wheel browse of an
                            // otherwise-idle focused tile.
                            if is_kbd_focused && nav_active && (left || right || up || down) {
                                resp.scroll_to_me(Some(egui::Align::Center));
                            }

                            if !is_locked && resp.clicked() {
                                self.focus_layer = FocusLayer::Grid;
                                self.focused_row = i / cols;
                                self.focused_col = i % cols;
                                action =
                                    ChannelListAction::SelectChannel(Box::new((*channel).clone()));
                            }
                        }
                    });
                });
            });

        action
    }
}

/// Shown instead of a blank grid when no channel matches the current
/// filter/search — a silent empty grid reads as "broken", not "no results".
fn empty_state(ui: &mut egui::Ui, search: &str) {
    ui.add_space(space::XXL);
    ui.vertical_centered(|ui| {
        ui.label(
            RichText::new(icon::MAGNIFYING_GLASS)
                .size(40.0)
                .color(color::TEXT_MUTED),
        );
        ui.add_space(space::SM);
        let message = if search.is_empty() {
            "Aucune chaîne dans cette catégorie.".to_string()
        } else {
            format!("Aucun résultat pour « {search} ».")
        };
        ui.label(
            RichText::new(message)
                .size(text::BODY)
                .color(color::TEXT_MUTED),
        );
    });
}
