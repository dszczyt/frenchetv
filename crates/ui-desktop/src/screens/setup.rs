use egui::{Align, Color32, FontId, Layout, RichText, Vec2};

const LOGO_BYTES: &[u8] = include_bytes!("../../../../assets/logo.png");
use frenchetv_core::{OperatorKind, OperatorRegistry};

pub struct SetupScreen {
    selected_operator: Option<OperatorKind>,
    username: String,
    password: String,
    /// Operator-specific extra credential (e.g. Bouygues "Nom de famille").
    extra: String,
    error_message: Option<String>,
    loading: bool,
}

#[derive(Debug)]
pub enum SetupAction {
    None,
    /// User pressed "Watch TV"; caller must authenticate then fetch channels.
    StartAuth {
        operator: OperatorKind,
        username: String,
        password: String,
        /// Operator-specific extra credential, if the operator requires one.
        extra: Option<String>,
    },
}

impl SetupScreen {
    pub fn new() -> Self {
        Self {
            selected_operator: None,
            username: String::new(),
            password: String::new(),
            extra: String::new(),
            error_message: None,
            loading: false,
        }
    }

    /// Display an inline error (call after a failed authentication).
    pub fn set_error(&mut self, msg: impl Into<String>) {
        self.loading = false;
        self.error_message = Some(msg.into());
    }

    pub fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
        if loading {
            self.error_message = None;
        }
    }

    pub fn show(&mut self, ctx: &egui::Context) -> SetupAction {
        let mut action = SetupAction::None;

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::from_rgb(13, 15, 20)))
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(40.0);

                    // Logo
                    ui.add(
                        egui::Image::from_bytes("bytes://frenchetv-logo.png", LOGO_BYTES)
                            .max_size(egui::vec2(270.0, 67.0))
                            .maintain_aspect_ratio(true),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("Choisissez votre opérateur")
                            .font(FontId::proportional(16.0))
                            .color(Color32::from_rgb(180, 180, 180)),
                    );
                    ui.add_space(32.0);

                    // Operator cards — allocate a row of the exact total width and
                    // let the surrounding `vertical_centered` center it (same idiom
                    // as the credentials form below). Each card is 160 min width +
                    // 2×20 inner margin = 200 wide, separated by 16px.
                    const CARD_W: f32 = 200.0;
                    const CARD_GAP: f32 = 16.0;
                    let n = OperatorRegistry::all().len() as f32;
                    let row_w = n * CARD_W + (n - 1.0).max(0.0) * CARD_GAP;
                    ui.allocate_ui_with_layout(
                        Vec2::new(row_w, 130.0),
                        Layout::left_to_right(Align::Center),
                        |ui| {
                            for (i, kind) in OperatorRegistry::all().iter().enumerate() {
                                let selected = self.selected_operator.as_ref() == Some(kind);
                                let (border_color, bg_color) = if selected {
                                    (
                                        Color32::from_rgb(10, 132, 255),
                                        Color32::from_rgb(20, 40, 70),
                                    )
                                } else {
                                    (Color32::from_rgb(60, 60, 60), Color32::from_rgb(25, 27, 34))
                                };

                                let card = egui::Frame::none()
                                    .fill(bg_color)
                                    .stroke(egui::Stroke::new(
                                        if selected { 3.0_f32 } else { 1.5_f32 },
                                        border_color,
                                    ))
                                    .rounding(12.0)
                                    .inner_margin(20.0);

                                // push_id gives each card a unique interaction ID;
                                // without it both Frames share an auto-ID and only the
                                // first card registers clicks (see channel_list grid).
                                let resp = ui.push_id(i, |ui| {
                                    card.show(ui, |ui| {
                                        ui.set_min_size(Vec2::new(160.0, 80.0));
                                        ui.vertical_centered(|ui| {
                                            ui.label(
                                                RichText::new(kind.display_name())
                                                    .font(FontId::proportional(18.0))
                                                    .color(Color32::WHITE),
                                            );
                                        });
                                    })
                                });

                                if resp.inner.response.interact(egui::Sense::click()).clicked() {
                                    self.selected_operator = Some(kind.clone());
                                    self.error_message = None;
                                }

                                ui.add_space(16.0);
                            }
                        },
                    );

                    ui.add_space(32.0);

                    // Credentials form (only if operator selected)
                    if let Some(op) = &self.selected_operator {
                        if op.requires_auth() {
                            let width = 320.0_f32.min(ui.available_width() - 40.0);
                            ui.allocate_ui_with_layout(
                                Vec2::new(width, 0.0),
                                Layout::top_down(Align::Center),
                                |ui| {
                                    // Operator-specific extra field (e.g. Bouygues last name).
                                    if let Some(label) = op.extra_credential_label() {
                                        ui.label(
                                            RichText::new(label)
                                                .color(Color32::from_rgb(180, 180, 180)),
                                        );
                                        ui.add(
                                            egui::TextEdit::singleline(&mut self.extra)
                                                .desired_width(f32::INFINITY)
                                                .font(FontId::proportional(16.0)),
                                        );
                                        ui.add_space(8.0);
                                    }
                                    ui.label(
                                        RichText::new("Identifiant")
                                            .color(Color32::from_rgb(180, 180, 180)),
                                    );
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.username)
                                            .hint_text("email@example.com")
                                            .desired_width(f32::INFINITY)
                                            .font(FontId::proportional(16.0)),
                                    );
                                    ui.add_space(8.0);
                                    ui.label(
                                        RichText::new("Mot de passe")
                                            .color(Color32::from_rgb(180, 180, 180)),
                                    );
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.password)
                                            .password(true)
                                            .hint_text("••••••••")
                                            .desired_width(f32::INFINITY)
                                            .font(FontId::proportional(16.0)),
                                    );
                                },
                            );

                            ui.add_space(24.0);

                            // Error message
                            if let Some(err) = &self.error_message {
                                ui.label(
                                    RichText::new(err)
                                        .color(Color32::from_rgb(255, 80, 80))
                                        .font(FontId::proportional(14.0)),
                                );
                                ui.add_space(8.0);
                            }

                            // Watch TV button
                            let btn_label = if self.loading {
                                "Connexion…"
                            } else {
                                "Regarder la TV"
                            };
                            let btn = egui::Button::new(
                                RichText::new(btn_label)
                                    .font(FontId::proportional(18.0))
                                    .color(Color32::WHITE),
                            )
                            .fill(Color32::from_rgb(10, 132, 255))
                            .rounding(8.0)
                            .min_size(Vec2::new(200.0, 48.0));

                            let extra_ok =
                                op.extra_credential_label().is_none() || !self.extra.is_empty();
                            if ui.add_enabled(!self.loading, btn).clicked()
                                && !self.username.is_empty()
                                && !self.password.is_empty()
                                && extra_ok
                            {
                                let op_kind = self.selected_operator.clone().unwrap();
                                let extra = if op_kind.extra_credential_label().is_some() {
                                    Some(self.extra.clone())
                                } else {
                                    None
                                };
                                action = SetupAction::StartAuth {
                                    operator: op_kind,
                                    username: self.username.clone(),
                                    password: self.password.clone(),
                                    extra,
                                };
                                self.set_loading(true);
                            }
                        }
                    }
                });
            });

        action
    }
}

impl Default for SetupScreen {
    fn default() -> Self {
        Self::new()
    }
}
