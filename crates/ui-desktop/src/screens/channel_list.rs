use egui::{Color32, FontId, RichText, ScrollArea, TextEdit, Vec2};
use frenchetv_core::{Channel, ChannelCategory};
use crate::app::LogoCache;

pub struct ChannelListScreen {
    channels: Vec<Channel>,
    filter: CategoryFilter,
    search: String,
    show_locked: bool,
    logos: LogoCache,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CategoryFilter {
    All,
    Category(ChannelCategory),
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
            search: String::new(),
            show_locked: false,
            logos,
        }
    }

    pub fn show(&mut self, ctx: &egui::Context) -> ChannelListAction {
        let mut action = ChannelListAction::None;

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::from_rgb(13, 15, 20)))
            .show(ctx, |ui| {
                // ── Top bar ──────────────────────────────────────────────────
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    ui.label(
                        RichText::new("FrenchTV")
                            .font(FontId::proportional(22.0))
                            .color(Color32::WHITE),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(16.0);
                        ui.add(
                            TextEdit::singleline(&mut self.search)
                                .hint_text("🔍 Rechercher…")
                                .desired_width(200.0),
                        );
                        ui.add_space(12.0);
                        let locked_count = self.channels.iter().filter(|c| c.locked).count();
                        if locked_count > 0 {
                            let lock_label = if self.show_locked {
                                format!("🔒 Masquer ({} canal(s))", locked_count)
                            } else {
                                format!("🔒 Afficher ({} canal(s))", locked_count)
                            };
                            if ui.selectable_label(
                                self.show_locked,
                                RichText::new(lock_label)
                                    .font(FontId::proportional(13.0))
                                    .color(Color32::from_rgb(180, 180, 180)),
                            ).clicked() {
                                self.show_locked = !self.show_locked;
                            }
                        }
                    });
                });

                ui.add_space(8.0);

                // ── Filter tabs ──────────────────────────────────────────────
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    let selected_color = Color32::from_rgb(10, 132, 255);
                    let normal_color   = Color32::from_rgb(180, 180, 180);

                    let is_all = self.filter == CategoryFilter::All;
                    if ui.button(
                        RichText::new("Tout")
                            .color(if is_all { selected_color } else { normal_color })
                            .font(FontId::proportional(15.0)),
                    ).clicked() {
                        self.filter = CategoryFilter::All;
                    }

                    for cat in ChannelCategory::fixed() {
                        let is_active = self.filter == CategoryFilter::Category(cat.clone());
                        if ui.button(
                            RichText::new(cat.label())
                                .color(if is_active { selected_color } else { normal_color })
                                .font(FontId::proportional(15.0)),
                        ).clicked() {
                            self.filter = CategoryFilter::Category(cat.clone());
                        }
                    }
                });

                ui.separator();

                // ── Filtering ────────────────────────────────────────────────
                let search_lower = self.search.to_lowercase();
                let visible: Vec<&Channel> = self.channels.iter().filter(|c| {
                    let matches_filter = match &self.filter {
                        CategoryFilter::All => true,
                        CategoryFilter::Category(cat) => &c.category == cat,
                    };
                    let matches_search = search_lower.is_empty()
                        || c.name.to_lowercase().contains(&search_lower)
                        || c.number.map_or(false, |n| n.to_string().contains(&search_lower));
                    let matches_locked = self.show_locked || !c.locked;
                    matches_filter && matches_search && matches_locked
                }).collect();

                // ── Grid ─────────────────────────────────────────────────────
                ScrollArea::vertical().show(ui, |ui| {
                    let available_width = (ui.available_width() - 16.0).max(0.0);
                    const COLS: usize = 4;
                    let tile_width  = (available_width / COLS as f32 - 12.0).max(160.0);
                    let tile_height = 100.0f32;
                    let logo_size   = Vec2::new(tile_width - 20.0, 50.0);
                    let placeholder_color = Color32::from_rgb(40, 42, 50);

                    egui::Grid::new("channel_grid")
                        .num_columns(COLS)
                        .spacing([12.0, 12.0])
                        .show(ui, |ui| {
                            for (i, channel) in visible.iter().enumerate() {
                                let is_locked = channel.locked;
                                let bg_color = if is_locked {
                                    Color32::from_rgb(20, 20, 25)
                                } else {
                                    Color32::from_rgb(25, 27, 34)
                                };
                                let text_color = if is_locked {
                                    Color32::from_rgb(100, 100, 110)
                                } else {
                                    Color32::WHITE
                                };

                                // push_id ensures each tile gets a unique ID in egui's
                                // interaction system — prevents Frame ID collisions.
                                let tile_resp = ui.push_id(i, |ui| {
                                    egui::Frame::none()
                                        .fill(bg_color)
                                        .stroke(egui::Stroke::new(
                                            1.0,
                                            Color32::from_rgb(50, 50, 60),
                                        ))
                                        .rounding(8.0)
                                        .inner_margin(10.0)
                                        .show(ui, |ui| {
                                            ui.set_min_size(Vec2::new(tile_width, tile_height));
                                            ui.vertical(|ui| {
                                                // ── Logo ────────────────────
                                                let cached_texture = channel.logo_url.as_ref()
                                                    .and_then(|url| {
                                                        self.logos.lock().ok()
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
                                                }

                                                ui.add_space(4.0);

                                                // ── Name + number ────────────
                                                let num_prefix = channel.number
                                                    .map(|n| format!("{} · ", n))
                                                    .unwrap_or_default();
                                                let lock_suffix = if is_locked { " 🔒" } else { "" };
                                                ui.label(
                                                    RichText::new(format!(
                                                        "{}{}{}",
                                                        num_prefix, channel.name, lock_suffix
                                                    ))
                                                    .font(FontId::proportional(12.0))
                                                    .color(text_color),
                                                );
                                            });
                                        })
                                });

                                // Detect click on the frame via the response returned from push_id.
                                // Calling .interact() on a Response registers a click zone without
                                // adding a new Grid cell.
                                let clicked = tile_resp
                                    .inner
                                    .response
                                    .interact(egui::Sense::click())
                                    .clicked();

                                if !is_locked && clicked {
                                    action = ChannelListAction::SelectChannel((*channel).clone());
                                }

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
