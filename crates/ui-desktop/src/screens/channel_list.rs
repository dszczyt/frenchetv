use egui::{FontId, RichText, ScrollArea, TextEdit, Vec2};

const LOGO_BYTES: &[u8] = include_bytes!("../../../../assets/logo.png");
use crate::app::LogoCache;
use crate::theme::{color, space, text};
use crate::widgets::hover_card;
use egui_phosphor::regular as icon;
use frenchetv_core::{Channel, ChannelCategory};

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
        }
    }

    pub fn show(&mut self, ctx: &egui::Context) -> ChannelListAction {
        let mut action = ChannelListAction::None;

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
                ui.horizontal(|ui| {
                    ui.add_space(space::SM + space::XS);

                    let is_all = self.filter == CategoryFilter::All;
                    if ui
                        .selectable_label(
                            is_all,
                            RichText::new("Tout").font(FontId::proportional(text::BODY - 1.0)),
                        )
                        .clicked()
                    {
                        self.filter = CategoryFilter::All;
                    }

                    for cat in ChannelCategory::fixed() {
                        let is_active = self.filter == CategoryFilter::Category(cat.clone());
                        if ui
                            .selectable_label(
                                is_active,
                                RichText::new(cat.label())
                                    .font(FontId::proportional(text::BODY - 1.0)),
                            )
                            .clicked()
                        {
                            self.filter = CategoryFilter::Category(cat.clone());
                        }
                    }
                });

                ui.separator();

                // ── Filtering ────────────────────────────────────────────────
                let search_lower = self.search.to_lowercase();
                let visible: Vec<&Channel> = self
                    .channels
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
                    .collect();

                // ── Grid ─────────────────────────────────────────────────────
                ScrollArea::vertical().show(ui, |ui| {
                    if visible.is_empty() {
                        empty_state(ui, &self.search);
                        return;
                    }

                    ui.spacing_mut().item_spacing = Vec2::new(space::MD, space::MD);

                    // Column count derived from available width instead of a
                    // fixed 4 — `horizontal_wrapped` wraps to a new row on its
                    // own once tiles no longer fit, so there's no manual
                    // `end_row()` bookkeeping to keep in sync with a column count.
                    const TARGET_TILE_W: f32 = 220.0;
                    let gap = space::MD;
                    // Reserve a little slack (scrollbar width + rounding) so the
                    // row never computes to *exactly* the available width — that
                    // would wrap one column early and leave an empty gutter.
                    let avail_w = (ui.available_width() - space::MD).max(0.0);
                    let cols = ((avail_w + gap) / (TARGET_TILE_W + gap)).floor().max(1.0);
                    let tile_width = ((avail_w - gap * (cols - 1.0)) / cols).floor().max(150.0);
                    let tile_height = 108.0_f32;
                    let logo_size = Vec2::new(tile_width - space::MD * 2.0, 50.0);

                    ui.horizontal_wrapped(|ui| {
                        for &channel in &visible {
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
                                false,
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

                            if !is_locked && resp.clicked() {
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
