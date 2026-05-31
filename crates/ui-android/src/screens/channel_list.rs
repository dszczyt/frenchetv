use crate::app::LogoCache;
use egui::{Color32, FontId, Key, RichText, Vec2};
use frenchetv_core::{Channel, ChannelCategory};

const LOGO_BYTES: &[u8] = include_bytes!("../../../../assets/logo.png");

const COLS: usize = 4;

#[derive(Debug, Clone, PartialEq)]
enum FocusLayer {
    FilterTabs,
    Grid,
}

#[derive(Debug, Clone, PartialEq)]
enum CategoryFilter {
    All,
    Category(ChannelCategory),
}

pub struct ChannelListScreen {
    channels: Vec<Channel>,
    filter: CategoryFilter,
    filter_focus_idx: usize,
    logos: LogoCache,
    focus_layer: FocusLayer,
    focused_row: usize,
    focused_col: usize,
}

#[derive(Debug)]
pub enum ChannelListAction {
    None,
    SelectChannel(Channel),
}

impl ChannelListScreen {
    pub fn new(mut channels: Vec<Channel>, logos: LogoCache) -> Self {
        channels.sort_by_key(|c| c.number.unwrap_or(u32::MAX));
        Self {
            channels,
            filter: CategoryFilter::All,
            filter_focus_idx: 0,
            logos,
            focus_layer: FocusLayer::Grid,
            focused_row: 0,
            focused_col: 0,
        }
    }

    fn all_filter_labels() -> Vec<(&'static str, Option<ChannelCategory>)> {
        let mut labels: Vec<(&'static str, Option<ChannelCategory>)> = vec![("Tout", None)];
        for cat in ChannelCategory::fixed() {
            labels.push((cat.label(), Some(cat.clone())));
        }
        labels
    }

    pub fn show(&mut self, ctx: &egui::Context) -> ChannelListAction {
        let mut action = ChannelListAction::None;

        let visible: Vec<&Channel> = self
            .channels
            .iter()
            .filter(|c| match &self.filter {
                CategoryFilter::All => true,
                CategoryFilter::Category(cat) => &c.category == cat,
            })
            .collect();

        let row_count = if visible.is_empty() {
            0
        } else {
            (visible.len() + COLS - 1) / COLS
        };

        let filter_labels = Self::all_filter_labels();
        let filter_count = filter_labels.len();

        // Process D-pad navigation
        let (left, right, up, down, enter) = ctx.input(|i| {
            (
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::Enter),
            )
        });

        match self.focus_layer {
            FocusLayer::FilterTabs => {
                if left && self.filter_focus_idx > 0 {
                    self.filter_focus_idx -= 1;
                }
                if right && self.filter_focus_idx + 1 < filter_count {
                    self.filter_focus_idx += 1;
                }
                if down {
                    self.focus_layer = FocusLayer::Grid;
                    self.focused_row = 0;
                }
                if enter {
                    let (_, cat_opt) = &filter_labels[self.filter_focus_idx];
                    self.filter = match cat_opt {
                        None => CategoryFilter::All,
                        Some(cat) => CategoryFilter::Category(cat.clone()),
                    };
                    // Clamp focus after filter change
                    let new_visible_count = self
                        .channels
                        .iter()
                        .filter(|c| match &self.filter {
                            CategoryFilter::All => true,
                            CategoryFilter::Category(cat) => &c.category == cat,
                        })
                        .count();
                    let new_row_count = if new_visible_count == 0 {
                        0
                    } else {
                        (new_visible_count + COLS - 1) / COLS
                    };
                    if self.focused_row >= new_row_count && new_row_count > 0 {
                        self.focused_row = new_row_count - 1;
                    }
                    let row_start = self.focused_row * COLS;
                    let row_len = (new_visible_count - row_start).min(COLS);
                    if self.focused_col >= row_len && row_len > 0 {
                        self.focused_col = row_len - 1;
                    }
                }
            }
            FocusLayer::Grid => {
                if up {
                    if self.focused_row == 0 {
                        self.focus_layer = FocusLayer::FilterTabs;
                    } else {
                        self.focused_row -= 1;
                    }
                }
                if down && row_count > 0 && self.focused_row + 1 < row_count {
                    self.focused_row += 1;
                }
                if left && self.focused_col > 0 {
                    self.focused_col -= 1;
                }
                if right {
                    let row_start = self.focused_row * COLS;
                    let remaining = visible.len().saturating_sub(row_start);
                    let cols_in_row = remaining.min(COLS);
                    if self.focused_col + 1 < cols_in_row {
                        self.focused_col += 1;
                    }
                }
                if enter {
                    let idx = self.focused_row * COLS + self.focused_col;
                    if let Some(channel) = visible.get(idx) {
                        if !channel.locked {
                            action = ChannelListAction::SelectChannel((*channel).clone());
                        }
                    }
                }
            }
        }

        let focused_idx = self.focused_row * COLS + self.focused_col;

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::from_rgb(13, 15, 20)))
            .show(ctx, |ui| {
                ui.add_space(24.0);

                // Title bar
                ui.horizontal(|ui| {
                    ui.add_space(24.0);
                    ui.add(
                        egui::Image::from_bytes("bytes://frenchetv-logo.png", LOGO_BYTES)
                            .max_size(egui::vec2(216.0, 54.0))
                            .maintain_aspect_ratio(true),
                    );
                });

                ui.add_space(16.0);

                // Filter tabs
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    for (tab_idx, (label, cat_opt)) in filter_labels.iter().enumerate() {
                        let is_focused = self.focus_layer == FocusLayer::FilterTabs
                            && self.filter_focus_idx == tab_idx;
                        let is_active = match (&self.filter, cat_opt) {
                            (CategoryFilter::All, None) => true,
                            (CategoryFilter::Category(a), Some(b)) => a == b,
                            _ => false,
                        };
                        let text_color = if is_focused {
                            Color32::from_rgb(80, 180, 255)
                        } else if is_active {
                            Color32::from_rgb(10, 132, 255)
                        } else {
                            Color32::from_rgb(160, 160, 170)
                        };
                        let border_color = if is_focused {
                            Color32::from_rgb(10, 132, 255)
                        } else {
                            Color32::TRANSPARENT
                        };
                        egui::Frame::none()
                            .stroke(egui::Stroke::new(2.0, border_color))
                            .rounding(6.0)
                            .inner_margin(egui::Margin::symmetric(12.0, 6.0))
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new(*label)
                                        .font(FontId::proportional(24.0))
                                        .color(text_color),
                                );
                            });
                        ui.add_space(8.0);
                    }
                });

                ui.add_space(8.0);
                ui.separator();
                ui.add_space(16.0);

                // Channel grid
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let available_width = (ui.available_width() - 32.0).max(0.0);
                    let tile_width = (available_width / COLS as f32 - 12.0).max(180.0);
                    let tile_height = 120.0_f32;
                    let logo_size = Vec2::new(tile_width - 24.0, 60.0);
                    let placeholder_color = Color32::from_rgb(40, 42, 50);

                    egui::Grid::new("channel_grid")
                        .num_columns(COLS)
                        .spacing([12.0, 16.0])
                        .show(ui, |ui| {
                            for (i, channel) in visible.iter().enumerate() {
                                let is_focused =
                                    self.focus_layer == FocusLayer::Grid && i == focused_idx;
                                let is_locked = channel.locked;

                                let bg_color = if is_locked {
                                    Color32::from_rgb(20, 20, 25)
                                } else {
                                    Color32::from_rgb(25, 27, 34)
                                };
                                let (border_color, stroke_width) = if is_focused {
                                    (Color32::from_rgb(10, 132, 255), 3.0)
                                } else {
                                    (Color32::from_rgb(50, 50, 60), 1.0)
                                };
                                let text_color = if is_locked {
                                    Color32::from_rgb(100, 100, 110)
                                } else if is_focused {
                                    Color32::WHITE
                                } else {
                                    Color32::from_rgb(200, 200, 210)
                                };

                                ui.push_id(i, |ui| {
                                    egui::Frame::none()
                                        .fill(bg_color)
                                        .stroke(egui::Stroke::new(stroke_width, border_color))
                                        .rounding(10.0)
                                        .inner_margin(12.0)
                                        .show(ui, |ui| {
                                            ui.set_min_size(Vec2::new(tile_width, tile_height));
                                            ui.vertical(|ui| {
                                                // Logo
                                                let cached_texture =
                                                    channel.logo_url.as_ref().and_then(|url| {
                                                        self.logos.lock().ok().and_then(|m| {
                                                            m.get(url.as_str()).cloned()
                                                        })
                                                    });
                                                if let Some(texture) = cached_texture {
                                                    ui.add(
                                                        egui::Image::from_texture(
                                                            egui::load::SizedTexture::from_handle(
                                                                &texture,
                                                            ),
                                                        )
                                                        .max_size(logo_size)
                                                        .maintain_aspect_ratio(true)
                                                        .sense(egui::Sense::hover()),
                                                    );
                                                } else {
                                                    let (rect, _) = ui.allocate_exact_size(
                                                        logo_size,
                                                        egui::Sense::hover(),
                                                    );
                                                    ui.painter().rect_filled(
                                                        rect,
                                                        4.0,
                                                        placeholder_color,
                                                    );
                                                    if channel.logo_url.is_some() {
                                                        let spin_rect =
                                                            egui::Rect::from_center_size(
                                                                rect.center(),
                                                                egui::Vec2::splat(28.0),
                                                            );
                                                        ui.put(
                                                            spin_rect,
                                                            egui::Spinner::new().size(20.0),
                                                        );
                                                    }
                                                }

                                                ui.add_space(6.0);

                                                // Name
                                                let num_prefix = channel
                                                    .number
                                                    .map(|n| format!("{} · ", n))
                                                    .unwrap_or_default();
                                                ui.label(
                                                    RichText::new(format!(
                                                        "{}{}",
                                                        num_prefix, channel.name
                                                    ))
                                                    .font(FontId::proportional(18.0))
                                                    .color(text_color),
                                                );
                                            });
                                        });
                                });

                                if (i + 1) % COLS == 0 {
                                    ui.end_row();
                                }
                            }
                            if !visible.is_empty() && visible.len() % COLS != 0 {
                                ui.end_row();
                            }
                        });
                });
            });

        action
    }
}
