use egui::{Color32, FontId, Key, RichText, Vec2};

const LOGO_BYTES: &[u8] = include_bytes!("../../../../assets/logo.png");
use frenchetv_core::{OperatorKind, OperatorRegistry};

#[derive(Debug, Clone, PartialEq)]
enum FieldFocus {
    OperatorCards,
    Username,
    Password,
    SubmitButton,
}

pub struct SetupScreen {
    selected_op_idx: usize,
    field_focus: FieldFocus,
    username: String,
    password: String,
    error_message: Option<String>,
    loading: bool,
}

#[derive(Debug)]
pub enum SetupAction {
    None,
    StartAuth {
        operator: OperatorKind,
        username: String,
        password: String,
    },
}

impl SetupScreen {
    pub fn new() -> Self {
        Self {
            selected_op_idx: 0,
            field_focus: FieldFocus::OperatorCards,
            username: String::new(),
            password: String::new(),
            error_message: None,
            loading: false,
        }
    }

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
        let operators = OperatorRegistry::all();
        let op_count = operators.len();

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

        match self.field_focus {
            FieldFocus::OperatorCards => {
                if right && self.selected_op_idx + 1 < op_count {
                    self.selected_op_idx += 1;
                }
                if left && self.selected_op_idx > 0 {
                    self.selected_op_idx -= 1;
                }
                if down {
                    self.field_focus = FieldFocus::Username;
                }
                if enter {
                    // Confirm operator selection — stay on cards but acknowledge
                    self.error_message = None;
                }
            }
            FieldFocus::Username => {
                if up {
                    self.field_focus = FieldFocus::OperatorCards;
                }
                if down {
                    self.field_focus = FieldFocus::Password;
                }
            }
            FieldFocus::Password => {
                if up {
                    self.field_focus = FieldFocus::Username;
                }
                if down {
                    self.field_focus = FieldFocus::SubmitButton;
                }
            }
            FieldFocus::SubmitButton => {
                if up {
                    self.field_focus = FieldFocus::Password;
                }
                if enter && !self.loading {
                    let op = &operators[self.selected_op_idx];
                    if !self.username.is_empty() && !self.password.is_empty() {
                        action = SetupAction::StartAuth {
                            operator: op.clone(),
                            username: self.username.clone(),
                            password: self.password.clone(),
                        };
                        self.set_loading(true);
                    }
                }
            }
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::from_rgb(13, 15, 20)))
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(60.0);

                    // Logo
                    ui.add(
                        egui::Image::from_bytes("bytes://frenchetv-logo.png", LOGO_BYTES)
                            .max_size(egui::vec2(360.0, 90.0))
                            .maintain_aspect_ratio(true),
                    );
                    ui.add_space(12.0);
                    ui.label(
                        RichText::new("Choisissez votre opérateur")
                            .font(FontId::proportional(24.0))
                            .color(Color32::from_rgb(180, 180, 180)),
                    );
                    ui.add_space(48.0);

                    // Operator cards
                    ui.horizontal(|ui| {
                        let total_cards = op_count as f32 * 216.0; // 200 card + 16 gap
                        let offset = (ui.available_width() - total_cards).max(0.0) / 2.0;
                        ui.add_space(offset);

                        for (idx, kind) in operators.iter().enumerate() {
                            let is_focused = self.field_focus == FieldFocus::OperatorCards
                                && self.selected_op_idx == idx;
                            let (border_color, bg_color, stroke_width) = if is_focused {
                                (
                                    Color32::from_rgb(10, 132, 255),
                                    Color32::from_rgb(20, 40, 70),
                                    3.0,
                                )
                            } else {
                                (
                                    Color32::from_rgb(60, 60, 70),
                                    Color32::from_rgb(25, 27, 34),
                                    1.5,
                                )
                            };

                            let card = egui::Frame::none()
                                .fill(bg_color)
                                .stroke(egui::Stroke::new(stroke_width, border_color))
                                .rounding(16.0)
                                .inner_margin(24.0);

                            card.show(ui, |ui| {
                                ui.set_min_size(Vec2::new(200.0, 120.0));
                                ui.vertical_centered(|ui| {
                                    ui.add_space(20.0);
                                    ui.label(
                                        RichText::new(kind.display_name())
                                            .font(FontId::proportional(28.0))
                                            .color(Color32::WHITE),
                                    );
                                });
                            });

                            ui.add_space(16.0);
                        }
                    });

                    ui.add_space(48.0);

                    // Credentials form
                    let current_op = &operators[self.selected_op_idx];
                    if current_op.requires_auth() {
                        let width = 400.0_f32.min(ui.available_width() - 80.0);

                        // Username field
                        let username_focused = self.field_focus == FieldFocus::Username;
                        let username_border = if username_focused {
                            Color32::from_rgb(10, 132, 255)
                        } else {
                            Color32::from_rgb(60, 60, 70)
                        };
                        ui.label(
                            RichText::new("Identifiant")
                                .font(FontId::proportional(22.0))
                                .color(Color32::from_rgb(180, 180, 180)),
                        );
                        ui.add_space(8.0);
                        egui::Frame::none()
                            .stroke(egui::Stroke::new(2.0_f32, username_border))
                            .rounding(8.0)
                            .inner_margin(8.0)
                            .show(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.username)
                                        .hint_text("email@example.com")
                                        .desired_width(width)
                                        .font(FontId::proportional(24.0)),
                                );
                            });

                        ui.add_space(16.0);

                        // Password field
                        let password_focused = self.field_focus == FieldFocus::Password;
                        let password_border = if password_focused {
                            Color32::from_rgb(10, 132, 255)
                        } else {
                            Color32::from_rgb(60, 60, 70)
                        };
                        ui.label(
                            RichText::new("Mot de passe")
                                .font(FontId::proportional(22.0))
                                .color(Color32::from_rgb(180, 180, 180)),
                        );
                        ui.add_space(8.0);
                        egui::Frame::none()
                            .stroke(egui::Stroke::new(2.0_f32, password_border))
                            .rounding(8.0)
                            .inner_margin(8.0)
                            .show(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.password)
                                        .password(true)
                                        .hint_text("••••••••")
                                        .desired_width(width)
                                        .font(FontId::proportional(24.0)),
                                );
                            });

                        ui.add_space(32.0);

                        // Error message
                        if let Some(err) = &self.error_message {
                            ui.label(
                                RichText::new(err)
                                    .color(Color32::from_rgb(255, 80, 80))
                                    .font(FontId::proportional(20.0)),
                            );
                            ui.add_space(16.0);
                        }

                        // Submit button
                        let submit_focused = self.field_focus == FieldFocus::SubmitButton;
                        let (btn_bg, btn_border) = if submit_focused {
                            (
                                Color32::from_rgb(10, 132, 255),
                                Color32::from_rgb(80, 180, 255),
                            )
                        } else {
                            (
                                Color32::from_rgb(30, 80, 160),
                                Color32::from_rgb(60, 60, 70),
                            )
                        };
                        let btn_label = if self.loading {
                            "Connexion…"
                        } else {
                            "Regarder la TV"
                        };
                        let btn = egui::Button::new(
                            RichText::new(btn_label)
                                .font(FontId::proportional(28.0))
                                .color(Color32::WHITE),
                        )
                        .fill(btn_bg)
                        .stroke(egui::Stroke::new(
                            if submit_focused { 3.0 } else { 1.5 },
                            btn_border,
                        ))
                        .rounding(12.0)
                        .min_size(Vec2::new(280.0, 64.0));

                        ui.add_enabled(!self.loading, btn);

                        ui.add_space(32.0);
                        ui.label(
                            RichText::new("D-pad pour naviguer  •  Entrée pour valider")
                                .font(FontId::proportional(18.0))
                                .color(Color32::from_rgb(100, 100, 110)),
                        );
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
