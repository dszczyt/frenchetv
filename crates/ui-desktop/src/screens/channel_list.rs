use egui::{Color32, FontId, RichText, ScrollArea, TextEdit, Vec2};
use frenchetv_core::{Channel, ChannelCategory};

pub struct ChannelListScreen {
    channels: Vec<Channel>,
    filter: CategoryFilter,
    search: String,
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
    pub fn new(channels: Vec<Channel>) -> Self {
        Self {
            channels,
            filter: CategoryFilter::All,
            search: String::new(),
        }
    }

    pub fn show(&mut self, ctx: &egui::Context) -> ChannelListAction {
        let mut action = ChannelListAction::None;

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::from_rgb(13, 15, 20)))
            .show(ctx, |ui| {
                // Top bar: title + search
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
                    });
                });

                ui.add_space(8.0);

                // Filter tabs
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    let selected_tab_color = Color32::from_rgb(10, 132, 255);
                    let normal_color = Color32::from_rgb(180, 180, 180);

                    // "All" tab
                    let is_all = self.filter == CategoryFilter::All;
                    let all_label = RichText::new("Tout")
                        .color(if is_all { selected_tab_color } else { normal_color })
                        .font(FontId::proportional(15.0));
                    if ui.button(all_label).clicked() {
                        self.filter = CategoryFilter::All;
                    }

                    // Fixed category tabs
                    for cat in ChannelCategory::fixed() {
                        let is_active = self.filter == CategoryFilter::Category(cat.clone());
                        let label = RichText::new(cat.label())
                            .color(if is_active { selected_tab_color } else { normal_color })
                            .font(FontId::proportional(15.0));
                        if ui.button(label).clicked() {
                            self.filter = CategoryFilter::Category(cat.clone());
                        }
                    }
                });

                ui.separator();

                // Filtered + searched channel list
                let search_lower = self.search.to_lowercase();
                let visible: Vec<&Channel> = self.channels.iter().filter(|c| {
                    let matches_filter = match &self.filter {
                        CategoryFilter::All => true,
                        CategoryFilter::Category(cat) => &c.category == cat,
                    };
                    let matches_search = search_lower.is_empty()
                        || c.name.to_lowercase().contains(&search_lower)
                        || c.number.map_or(false, |n| n.to_string().contains(&search_lower));
                    matches_filter && matches_search
                }).collect();

                ScrollArea::vertical().show(ui, |ui| {
                    // 4-column grid
                    let available_width = ui.available_width();
                    let tile_width = (available_width / 4.0 - 12.0).max(160.0);
                    let tile_height = 80.0;

                    egui::Grid::new("channel_grid")
                        .num_columns(4)
                        .spacing([12.0, 12.0])
                        .show(ui, |ui| {
                            for (i, channel) in visible.iter().enumerate() {
                                let resp = egui::Frame::none()
                                    .fill(Color32::from_rgb(25, 27, 34))
                                    .stroke(egui::Stroke::new(1.0, Color32::from_rgb(50, 50, 60)))
                                    .rounding(8.0)
                                    .inner_margin(10.0)
                                    .show(ui, |ui| {
                                        ui.set_min_size(Vec2::new(tile_width, tile_height));
                                        ui.vertical(|ui| {
                                            // Channel number badge
                                            if let Some(num) = channel.number {
                                                ui.label(
                                                    RichText::new(format!("{}", num))
                                                        .font(FontId::proportional(11.0))
                                                        .color(Color32::from_rgb(120, 120, 140)),
                                                );
                                            }
                                            ui.label(
                                                RichText::new(&channel.name)
                                                    .font(FontId::proportional(16.0))
                                                    .color(Color32::WHITE),
                                            );
                                        });
                                    });

                                if resp.response.interact(egui::Sense::click()).clicked() {
                                    action = ChannelListAction::SelectChannel((*channel).clone());
                                }

                                if (i + 1) % 4 == 0 {
                                    ui.end_row();
                                }
                            }
                        });
                });
            });

        action
    }
}
