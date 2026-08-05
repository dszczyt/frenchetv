use egui::{Align, FontId, Layout, RichText, Vec2};

const LOGO_BYTES: &[u8] = include_bytes!("../../../../assets/logo.png");
use crate::theme::{color, space, text};
use crate::widgets::{accent_button, field_label, hover_card};
use egui_phosphor::regular as icon;
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
            .frame(egui::Frame::none().fill(color::BG))
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(space::XL + space::SM);

                    // Logo
                    ui.add(
                        egui::Image::from_bytes("bytes://frenchetv-logo.png", LOGO_BYTES)
                            .max_size(egui::vec2(270.0, 67.0))
                            .maintain_aspect_ratio(true),
                    );
                    ui.add_space(space::SM);
                    ui.label(
                        RichText::new("Choisissez votre opérateur")
                            .font(FontId::proportional(text::SUBTITLE))
                            .color(color::TEXT_MUTED),
                    );
                    ui.add_space(space::XL);

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
                            // `row_w` budgets exactly `CARD_GAP` between cards via
                            // the manual `add_space` below — zero out the theme's
                            // global item_spacing here, or `left_to_right` would
                            // add its own gap on top and the row would overflow
                            // its allocated (and therefore centered) width.
                            ui.spacing_mut().item_spacing.x = 0.0;
                            for kind in OperatorRegistry::all().iter() {
                                let selected = self.selected_operator.as_ref() == Some(kind);
                                let id =
                                    ui.make_persistent_id(("operator_card", kind.config_str()));

                                let resp = hover_card(
                                    ui,
                                    id,
                                    Vec2::new(CARD_W, 130.0),
                                    selected,
                                    true,
                                    |ui| {
                                        ui.centered_and_justified(|ui| {
                                            ui.label(
                                                RichText::new(kind.display_name())
                                                    .font(FontId::proportional(18.0))
                                                    .color(color::TEXT),
                                            );
                                        });
                                    },
                                );

                                if resp.clicked() {
                                    self.selected_operator = Some(kind.clone());
                                    self.error_message = None;
                                }

                                ui.add_space(CARD_GAP);
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
                                        field_label(ui, label);
                                        ui.add(
                                            egui::TextEdit::singleline(&mut self.extra)
                                                .desired_width(f32::INFINITY)
                                                .font(FontId::proportional(text::BODY)),
                                        );
                                        ui.add_space(space::SM);
                                    }
                                    field_label(ui, "Identifiant");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.username)
                                            .hint_text("email@example.com")
                                            .desired_width(f32::INFINITY)
                                            .font(FontId::proportional(text::BODY)),
                                    );
                                    ui.add_space(space::SM);
                                    field_label(ui, "Mot de passe");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.password)
                                            .password(true)
                                            .hint_text("••••••••")
                                            .desired_width(f32::INFINITY)
                                            .font(FontId::proportional(text::BODY)),
                                    );
                                },
                            );

                            ui.add_space(space::LG);

                            // Error message
                            if let Some(err) = &self.error_message {
                                ui.label(
                                    RichText::new(format!("{}  {}", icon::WARNING_CIRCLE, err))
                                        .color(color::ERROR)
                                        .font(FontId::proportional(text::LABEL)),
                                );
                                ui.add_space(space::SM);
                            }

                            // Watch TV button, with an inline spinner while loading.
                            let btn_label = if self.loading {
                                "Connexion…"
                            } else {
                                "Regarder la TV"
                            };
                            let clicked = ui
                                .horizontal(|ui| {
                                    let resp = accent_button(
                                        ui,
                                        btn_label,
                                        !self.loading,
                                        Vec2::new(200.0, 48.0),
                                    );
                                    if self.loading {
                                        ui.add_space(space::SM);
                                        ui.add(egui::Spinner::new().size(20.0).color(color::TEXT));
                                    }
                                    resp.clicked()
                                })
                                .inner;

                            let extra_ok =
                                op.extra_credential_label().is_none() || !self.extra.is_empty();
                            if clicked
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
