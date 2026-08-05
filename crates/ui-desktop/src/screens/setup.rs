use egui::{Align, FontId, Key, Layout, RichText, Vec2};

const LOGO_BYTES: &[u8] = include_bytes!("../../../../assets/logo.png");
use crate::theme::{color, space, text};
use crate::widgets::{accent_button, field_label, focus_ring, hover_card};
use egui_phosphor::regular as icon;
use frenchetv_core::{OperatorKind, OperatorRegistry};

/// Which element arrow keys currently move focus within — the same D-pad
/// focus model `ui-android` uses, adapted so desktop keyboard users get a
/// visible focus ring too, not just mouse hover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldFocus {
    OperatorCards,
    Extra,
    Username,
    Password,
    Submit,
}

pub struct SetupScreen {
    selected_operator: Option<OperatorKind>,
    focused_op_idx: usize,
    field_focus: FieldFocus,
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
            focused_op_idx: 0,
            field_focus: FieldFocus::OperatorCards,
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
        let operators = OperatorRegistry::all();
        let op_count = operators.len();

        let (left, right, up, down, enter) = ctx.input(|i| {
            (
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::Enter),
            )
        });
        // Don't hijack arrow/enter while a real widget (a text field the
        // user clicked into) holds keyboard focus.
        let nav_active = ctx.memory(|m| m.focused().is_none());

        if nav_active {
            let has_extra = self
                .selected_operator
                .as_ref()
                .is_some_and(|op| op.extra_credential_label().is_some());

            match self.field_focus {
                FieldFocus::OperatorCards => {
                    if left && self.focused_op_idx > 0 {
                        self.focused_op_idx -= 1;
                    }
                    if right && self.focused_op_idx + 1 < op_count {
                        self.focused_op_idx += 1;
                    }
                    if enter {
                        self.selected_operator = Some(operators[self.focused_op_idx].clone());
                        self.error_message = None;
                    }
                    if down {
                        let op = &operators[self.focused_op_idx];
                        self.selected_operator = Some(op.clone());
                        self.error_message = None;
                        if op.requires_auth() {
                            self.field_focus = if op.extra_credential_label().is_some() {
                                FieldFocus::Extra
                            } else {
                                FieldFocus::Username
                            };
                        }
                    }
                }
                FieldFocus::Extra => {
                    if up {
                        self.field_focus = FieldFocus::OperatorCards;
                    }
                    if down {
                        self.field_focus = FieldFocus::Username;
                    }
                }
                FieldFocus::Username => {
                    if up {
                        self.field_focus = if has_extra {
                            FieldFocus::Extra
                        } else {
                            FieldFocus::OperatorCards
                        };
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
                        self.field_focus = FieldFocus::Submit;
                    }
                }
                FieldFocus::Submit => {
                    if up {
                        self.field_focus = FieldFocus::Password;
                    }
                    if enter && !self.loading {
                        if let Some(op) = &self.selected_operator {
                            let extra_ok =
                                op.extra_credential_label().is_none() || !self.extra.is_empty();
                            if !self.username.is_empty() && !self.password.is_empty() && extra_ok {
                                let extra = if op.extra_credential_label().is_some() {
                                    Some(self.extra.clone())
                                } else {
                                    None
                                };
                                action = SetupAction::StartAuth {
                                    operator: op.clone(),
                                    username: self.username.clone(),
                                    password: self.password.clone(),
                                    extra,
                                };
                                self.set_loading(true);
                            }
                        }
                    }
                }
            }
        }

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
                    let n = op_count as f32;
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
                            for (idx, kind) in operators.iter().enumerate() {
                                let selected = self.selected_operator.as_ref() == Some(kind);
                                let is_kbd_focused = self.field_focus == FieldFocus::OperatorCards
                                    && self.focused_op_idx == idx;
                                let id =
                                    ui.make_persistent_id(("operator_card", kind.config_str()));

                                let resp = hover_card(
                                    ui,
                                    id,
                                    Vec2::new(CARD_W, 130.0),
                                    selected || is_kbd_focused,
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
                                    self.field_focus = FieldFocus::OperatorCards;
                                    self.focused_op_idx = idx;
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
                                        let resp = focus_ring(
                                            ui,
                                            self.field_focus == FieldFocus::Extra,
                                            |ui| {
                                                ui.add(
                                                    egui::TextEdit::singleline(&mut self.extra)
                                                        .desired_width(f32::INFINITY)
                                                        .font(FontId::proportional(text::BODY)),
                                                )
                                            },
                                        );
                                        if resp.gained_focus() {
                                            self.field_focus = FieldFocus::Extra;
                                        }
                                        ui.add_space(space::SM);
                                    }
                                    field_label(ui, "Identifiant");
                                    let resp = focus_ring(
                                        ui,
                                        self.field_focus == FieldFocus::Username,
                                        |ui| {
                                            ui.add(
                                                egui::TextEdit::singleline(&mut self.username)
                                                    .hint_text("email@example.com")
                                                    .desired_width(f32::INFINITY)
                                                    .font(FontId::proportional(text::BODY)),
                                            )
                                        },
                                    );
                                    if resp.gained_focus() {
                                        self.field_focus = FieldFocus::Username;
                                    }
                                    ui.add_space(space::SM);
                                    field_label(ui, "Mot de passe");
                                    let resp = focus_ring(
                                        ui,
                                        self.field_focus == FieldFocus::Password,
                                        |ui| {
                                            ui.add(
                                                egui::TextEdit::singleline(&mut self.password)
                                                    .password(true)
                                                    .hint_text("••••••••")
                                                    .desired_width(f32::INFINITY)
                                                    .font(FontId::proportional(text::BODY)),
                                            )
                                        },
                                    );
                                    if resp.gained_focus() {
                                        self.field_focus = FieldFocus::Password;
                                    }
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
                            let (clicked, gained_focus) =
                                focus_ring(ui, self.field_focus == FieldFocus::Submit, |ui| {
                                    ui.horizontal(|ui| {
                                        let resp = accent_button(
                                            ui,
                                            btn_label,
                                            !self.loading,
                                            Vec2::new(200.0, 48.0),
                                        );
                                        if self.loading {
                                            ui.add_space(space::SM);
                                            ui.add(
                                                egui::Spinner::new().size(20.0).color(color::TEXT),
                                            );
                                        }
                                        (resp.clicked(), resp.gained_focus())
                                    })
                                    .inner
                                });
                            if gained_focus {
                                self.field_focus = FieldFocus::Submit;
                            }

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
