use egui::{Color32, FontId, Key, RichText, Vec2};

use crate::screens::keyboard::{KeyboardAction, OnScreenKeyboard};

const LOGO_BYTES: &[u8] = include_bytes!("../../../../assets/logo.png");
use frenchetv_core::{OperatorKind, OperatorRegistry};

#[derive(Debug, Clone, PartialEq)]
enum FieldFocus {
    OperatorCards,
    Method,
    Username,
    Password,
    Remember,
    SubmitButton,
}

/// How to authenticate. Orange routes app-capable accounts to "Orange et Moi"
/// itself — `/api/access` takes no parameter to request it — so this does not
/// force a method. It decides whether the screen demands a password up front:
/// in app mode there is nothing to type, and requiring one made that flow
/// impossible to even submit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    Password,
    App,
}

#[derive(Clone, Copy)]
enum FieldKind {
    Username,
    Password,
}

/// Sizes for one screen-height budget.
///
/// A Fire TV Stick reports 1920x1080 at density 2.0, which leaves egui roughly
/// 540 logical points of height — the comfortable sizes do not fit and the
/// submit button lands off-screen. Rather than make a login form scroll, use
/// smaller metrics when the viewport is short. Chosen from the actual
/// available height, so this adapts to whatever it is given instead of testing
/// for a specific device.
struct Metrics {
    top: f32,
    logo_h: f32,
    subtitle: f32,
    gap_lg: f32,
    gap_md: f32,
    card_w: f32,
    card_h: f32,
    card_font: f32,
    label: f32,
    gap_sm: f32,
    field_font: f32,
    btn_w: f32,
    btn_h: f32,
    btn_font: f32,
    hint: f32,
}

impl Metrics {
    fn for_height(h: f32) -> Self {
        if h < 620.0 {
            // Compact: ~330pt of content inside a ~540pt budget, leaving room
            // to sit beside the keyboard rather than under it.
            Self {
                top: 8.0,
                logo_h: 34.0,
                subtitle: 15.0,
                gap_lg: 10.0,
                gap_md: 7.0,
                card_w: 132.0,
                card_h: 42.0,
                card_font: 17.0,
                label: 14.0,
                gap_sm: 3.0,
                field_font: 17.0,
                btn_w: 210.0,
                btn_h: 36.0,
                btn_font: 18.0,
                hint: 12.0,
            }
        } else {
            Self {
                top: 60.0,
                logo_h: 90.0,
                subtitle: 24.0,
                gap_lg: 48.0,
                gap_md: 16.0,
                card_w: 200.0,
                card_h: 120.0,
                card_font: 28.0,
                label: 22.0,
                gap_sm: 8.0,
                field_font: 24.0,
                btn_w: 280.0,
                btn_h: 64.0,
                btn_font: 28.0,
                hint: 18.0,
            }
        }
    }
}

pub struct SetupScreen {
    selected_op_idx: usize,
    field_focus: FieldFocus,
    username: String,
    password: String,
    error_message: Option<String>,
    loading: bool,
    keyboard: OnScreenKeyboard,
    /// Hidden entirely when the device cannot store credentials (API < 23).
    remember_available: bool,
    remember: bool,
    method: AuthMethod,
}

#[derive(Debug)]
pub enum SetupAction {
    None,
    StartAuth {
        operator: OperatorKind,
        username: String,
        password: String,
        method: AuthMethod,
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
            keyboard: OnScreenKeyboard::new(),
            remember_available: false,
            remember: false,
            method: AuthMethod::Password,
        }
    }

    /// Start with credentials restored from the keystore. `remember` is on
    /// precisely when something was stored, so the toggle reflects reality.
    pub fn with_saved(remember_available: bool, saved: Option<(String, String)>) -> Self {
        let mut s = Self::new();
        s.remember_available = remember_available;
        if let Some((u, p)) = saved {
            s.username = u;
            s.password = p;
            s.remember = true;
        }
        s
    }

    /// Whether the selected operator offers app-based auth at all.
    fn method_available(&self) -> bool {
        OperatorRegistry::all()[self.selected_op_idx].supports_app_auth()
    }

    /// App auth identifies the line itself, so there is nothing to type and
    /// nothing to validate.
    fn needs_password(&self) -> bool {
        !(self.method_available() && self.method == AuthMethod::App)
    }

    /// Whether the user asked for these credentials to be kept.
    pub fn remember_requested(&self) -> bool {
        self.remember_available && self.remember
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

        // While the keyboard is open it owns every key press. The form must not
        // also act on them, or one D-pad press would move both the key cursor
        // and the field focus.
        if !self.keyboard.is_open() {
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
                        self.field_focus = if self.method_available() {
                            FieldFocus::Method
                        } else {
                            FieldFocus::Username
                        };
                    }
                    if enter {
                        self.error_message = None;
                    }
                }
                FieldFocus::Method => {
                    if up {
                        self.field_focus = FieldFocus::OperatorCards;
                    }
                    if down {
                        self.field_focus = FieldFocus::Username;
                    }
                    if left || right || enter {
                        self.method = match self.method {
                            AuthMethod::Password => AuthMethod::App,
                            AuthMethod::App => AuthMethod::Password,
                        };
                    }
                }
                FieldFocus::Username => {
                    if up {
                        self.field_focus = if self.method_available() {
                            FieldFocus::Method
                        } else {
                            FieldFocus::OperatorCards
                        };
                    }
                    if down {
                        self.field_focus = if self.needs_password() {
                            FieldFocus::Password
                        } else if self.remember_available {
                            FieldFocus::Remember
                        } else {
                            FieldFocus::SubmitButton
                        };
                    }
                    if enter {
                        self.keyboard.open();
                    }
                }
                FieldFocus::Password => {
                    if up {
                        self.field_focus = FieldFocus::Username;
                    }
                    if down {
                        self.field_focus = if self.remember_available {
                            FieldFocus::Remember
                        } else {
                            FieldFocus::SubmitButton
                        };
                    }
                    if enter {
                        self.keyboard.open();
                    }
                }
                FieldFocus::Remember => {
                    if up {
                        self.field_focus = if self.needs_password() {
                            FieldFocus::Password
                        } else {
                            FieldFocus::Username
                        };
                    }
                    if down {
                        self.field_focus = FieldFocus::SubmitButton;
                    }
                    if enter {
                        self.remember = !self.remember;
                    }
                }
                FieldFocus::SubmitButton => {
                    if up {
                        self.field_focus = if self.remember_available {
                            FieldFocus::Remember
                        } else if self.needs_password() {
                            FieldFocus::Password
                        } else {
                            FieldFocus::Username
                        };
                    }
                    if enter && !self.loading {
                        let op = &operators[self.selected_op_idx];
                        // App auth needs no credentials at all: Orange
                        // identifies the line and returns the account after
                        // approval. Demanding a password here made that flow
                        // impossible to submit.
                        let ready = if self.needs_password() {
                            !self.username.is_empty() && !self.password.is_empty()
                        } else {
                            true
                        };
                        if ready {
                            action = SetupAction::StartAuth {
                                operator: op.clone(),
                                username: self.username.clone(),
                                password: self.password.clone(),
                                method: self.method,
                            };
                            self.set_loading(true);
                        }
                    }
                }
            }
        }

        let m = Metrics::for_height(ctx.screen_rect().height());

        // With the keyboard open the form is confined to the left column and
        // the keyboard occupies the right, so both are fully visible at once.
        // Closed, the form uses the whole panel.
        let screen = ctx.screen_rect();
        let form_rect = if self.keyboard.is_open() {
            egui::Rect::from_min_size(
                screen.min,
                egui::vec2(screen.width() * 0.50, screen.height()),
            )
        } else {
            screen
        };

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::from_rgb(13, 15, 20)))
            .show(ctx, |ui| {
                ui.allocate_new_ui(egui::UiBuilder::new().max_rect(form_rect), |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(m.top);

                        ui.add(
                            egui::Image::from_bytes("bytes://frenchetv-logo.png", LOGO_BYTES)
                                .max_size(egui::vec2(m.logo_h * 4.0, m.logo_h))
                                .maintain_aspect_ratio(true),
                        );
                        ui.add_space(m.gap_sm);
                        ui.label(
                            RichText::new("Choisissez votre opérateur")
                                .font(FontId::proportional(m.subtitle))
                                .color(Color32::from_rgb(180, 180, 180)),
                        );
                        ui.add_space(m.gap_lg);

                        ui.horizontal(|ui| {
                            let total = op_count as f32 * (m.card_w + 16.0);
                            let offset = (ui.available_width() - total).max(0.0) / 2.0;
                            ui.add_space(offset);

                            for (idx, kind) in operators.iter().enumerate() {
                                // Selection and focus are separate states. Keying
                                // the highlight off focus alone meant that as soon
                                // as the cursor moved to the fields, no card looked
                                // chosen and the operator in use was invisible.
                                let is_selected = self.selected_op_idx == idx;
                                let has_focus = self.field_focus == FieldFocus::OperatorCards;
                                let (border_color, bg_color, stroke_width) =
                                    match (is_selected, has_focus) {
                                        (true, true) => (
                                            Color32::from_rgb(10, 132, 255),
                                            Color32::from_rgb(20, 40, 70),
                                            3.0_f32,
                                        ),
                                        // Chosen, but the cursor is elsewhere: keep
                                        // the fill, dim the edge.
                                        (true, false) => (
                                            Color32::from_rgb(50, 100, 160),
                                            Color32::from_rgb(20, 40, 70),
                                            2.0_f32,
                                        ),
                                        _ => (
                                            Color32::from_rgb(60, 60, 70),
                                            Color32::from_rgb(25, 27, 34),
                                            1.5_f32,
                                        ),
                                    };

                                let (rect, _) = ui.allocate_exact_size(
                                    Vec2::new(m.card_w, m.card_h),
                                    egui::Sense::hover(),
                                );
                                ui.painter().rect(
                                    rect,
                                    12.0,
                                    bg_color,
                                    egui::Stroke::new(stroke_width, border_color),
                                );
                                ui.painter().text(
                                    rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    kind.display_name(),
                                    FontId::proportional(m.card_font),
                                    if is_selected {
                                        Color32::WHITE
                                    } else {
                                        Color32::from_rgb(150, 150, 160)
                                    },
                                );
                                ui.add_space(16.0);
                            }
                        });

                        ui.add_space(m.gap_lg);

                        let current_op = &operators[self.selected_op_idx];
                        if current_op.requires_auth() {
                            let width = 400.0_f32.min(ui.available_width() - 80.0);

                            if self.method_available() {
                                let focused = self.field_focus == FieldFocus::Method;
                                let label = match self.method {
                                    AuthMethod::Password => "Mot de passe",
                                    AuthMethod::App => "App Orange et Moi",
                                };
                                ui.label(
                                    RichText::new("Méthode de connexion")
                                        .font(FontId::proportional(m.label))
                                        .color(Color32::from_rgb(180, 180, 180)),
                                );
                                ui.add_space(m.gap_sm);
                                let h = m.field_font + 16.0;
                                let (rect, _) =
                                    ui.allocate_exact_size(Vec2::new(width, h), egui::Sense::hover());
                                ui.painter().rect(
                                    rect,
                                    8.0,
                                    Color32::from_rgb(8, 9, 12),
                                    egui::Stroke::new(
                                        2.0_f32,
                                        if focused {
                                            Color32::from_rgb(10, 132, 255)
                                        } else {
                                            Color32::from_rgb(60, 60, 70)
                                        },
                                    ),
                                );
                                // Arrows both sides: a two-value cycle, and a TV user needs to
                                // see that it is changeable at all.
                                for (align, glyph) in [
                                    (egui::Align2::LEFT_CENTER, "<"),
                                    (egui::Align2::RIGHT_CENTER, ">"),
                                ] {
                                    let at = if glyph == "<" {
                                        rect.left_center() + egui::vec2(12.0, 0.0)
                                    } else {
                                        rect.right_center() - egui::vec2(12.0, 0.0)
                                    };
                                    ui.painter().text(
                                        at,
                                        align,
                                        glyph,
                                        FontId::proportional(m.field_font),
                                        Color32::from_rgb(120, 120, 130),
                                    );
                                }
                                ui.painter().text(
                                    rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    label,
                                    FontId::proportional(m.field_font),
                                    Color32::WHITE,
                                );
                                ui.add_space(m.gap_md);
                            }

                            let user_focused = self.field_focus == FieldFocus::Username;
                            let pass_focused = self.field_focus == FieldFocus::Password;
                            Self::field(
                                ui,
                                &m,
                                "Identifiant",
                                user_focused,
                                width,
                                FieldKind::Username,
                                &self.username,
                            );
                            if self.needs_password() {
                                ui.add_space(m.gap_md);
                                Self::field(
                                    ui,
                                    &m,
                                    "Mot de passe",
                                    pass_focused,
                                    width,
                                    FieldKind::Password,
                                    &self.password,
                                );
                            } else {
                                ui.add_space(m.gap_sm);
                                ui.label(
                                    RichText::new(
                                        "Validez la connexion dans l'app Orange et Moi sur votre téléphone.",
                                    )
                                    .font(FontId::proportional(m.hint))
                                    .color(Color32::from_rgb(140, 140, 150)),
                                );
                            }

                            ui.add_space(m.gap_md);

                            if self.remember_available {
                                let focused = self.field_focus == FieldFocus::Remember;
                                let accent = Color32::from_rgb(10, 132, 255);
                                let box_side = m.label + 2.0;
                                let text = "Se souvenir de moi";
                                let font = FontId::proportional(m.label);
                                let text_w = ui
                                    .fonts(|f| {
                                        f.layout_no_wrap(
                                            text.to_string(),
                                            font.clone(),
                                            Color32::WHITE,
                                        )
                                    })
                                    .size()
                                    .x;

                                let (rect, _) = ui.allocate_exact_size(
                                    Vec2::new(box_side + 10.0 + text_w, box_side + 8.0),
                                    egui::Sense::hover(),
                                );
                                let box_rect = egui::Rect::from_min_size(
                                    egui::pos2(rect.left(), rect.center().y - box_side / 2.0),
                                    Vec2::splat(box_side),
                                );
                                ui.painter().rect(
                                    box_rect,
                                    4.0,
                                    if self.remember {
                                        accent
                                    } else {
                                        Color32::from_rgb(8, 9, 12)
                                    },
                                    egui::Stroke::new(
                                        if focused { 2.5_f32 } else { 1.5_f32 },
                                        if focused {
                                            accent
                                        } else {
                                            Color32::from_rgb(60, 60, 70)
                                        },
                                    ),
                                );
                                if self.remember {
                                    // Drawn rather than a glyph: the bundled font has
                                    // no check mark and would render tofu.
                                    let c = box_rect.center();
                                    let s2 = box_side * 0.28;
                                    ui.painter().line_segment(
                                        [
                                            egui::pos2(c.x - s2, c.y),
                                            egui::pos2(c.x - s2 * 0.2, c.y + s2 * 0.8),
                                        ],
                                        egui::Stroke::new(2.0_f32, Color32::WHITE),
                                    );
                                    ui.painter().line_segment(
                                        [
                                            egui::pos2(c.x - s2 * 0.2, c.y + s2 * 0.8),
                                            egui::pos2(c.x + s2, c.y - s2 * 0.7),
                                        ],
                                        egui::Stroke::new(2.0_f32, Color32::WHITE),
                                    );
                                }
                                ui.painter().text(
                                    egui::pos2(box_rect.right() + 10.0, rect.center().y),
                                    egui::Align2::LEFT_CENTER,
                                    text,
                                    font,
                                    if focused {
                                        Color32::WHITE
                                    } else {
                                        Color32::from_rgb(180, 180, 180)
                                    },
                                );
                                ui.add_space(m.gap_sm);
                            }

                            if let Some(err) = &self.error_message {
                                ui.label(
                                    RichText::new(err)
                                        .color(Color32::from_rgb(255, 80, 80))
                                        .font(FontId::proportional(m.label)),
                                );
                                ui.add_space(m.gap_sm);
                            }

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
                                    .font(FontId::proportional(m.btn_font))
                                    .color(Color32::WHITE),
                            )
                            .fill(btn_bg)
                            .stroke(egui::Stroke::new(
                                if submit_focused { 3.0_f32 } else { 1.5_f32 },
                                btn_border,
                            ))
                            .rounding(10.0)
                            .min_size(Vec2::new(m.btn_w, m.btn_h));
                            ui.add_enabled(!self.loading, btn);

                            ui.add_space(m.gap_sm);
                            ui.label(
                                RichText::new(
                                    "D-pad pour naviguer  •  Entrée pour saisir ou valider",
                                )
                                .font(FontId::proportional(m.hint))
                                .color(Color32::from_rgb(100, 100, 110)),
                            );
                        }
                    });
                });
            });

        if self.keyboard.is_open() {
            // Borrow only the field being edited, so the keyboard never needs to
            // know which one it is.
            let target = match self.field_focus {
                FieldFocus::Password => &mut self.password,
                _ => &mut self.username,
            };
            if self.keyboard.show(ctx, target) == KeyboardAction::Close {
                self.keyboard.close();
            }
        }

        action
    }

    /// Render-only field. Deliberately not an `egui::TextEdit`: a focused
    /// TextEdit asks egui for keyboard input, which on Android opens the system
    /// IME whose text never reaches this app (see `keyboard.rs`). Drawing the
    /// value ourselves keeps the IME out of the picture entirely.
    #[allow(clippy::too_many_arguments)]
    fn field(
        ui: &mut egui::Ui,
        m: &Metrics,
        label: &str,
        focused: bool,
        width: f32,
        kind: FieldKind,
        value: &str,
    ) {
        let border = if focused {
            Color32::from_rgb(10, 132, 255)
        } else {
            Color32::from_rgb(60, 60, 70)
        };

        ui.label(
            RichText::new(label)
                .font(FontId::proportional(m.label))
                .color(Color32::from_rgb(180, 180, 180)),
        );
        ui.add_space(m.gap_sm);

        let empty = value.is_empty();
        let shown = match kind {
            FieldKind::Username => {
                if empty {
                    "email@example.com".to_string()
                } else {
                    value.to_string()
                }
            }
            FieldKind::Password => {
                if empty {
                    "••••••••".to_string()
                } else {
                    "•".repeat(value.chars().count())
                }
            }
        };

        // Painted directly rather than wrapped in an `egui::Frame`: inside a
        // `vertical_centered` layout the frame claims the panel's full width,
        // so its border stretched edge to edge instead of hugging the field.
        let h = m.field_font + 16.0;
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, h), egui::Sense::hover());
        ui.painter().rect(
            rect,
            8.0,
            Color32::from_rgb(8, 9, 12),
            egui::Stroke::new(2.0_f32, border),
        );
        ui.painter().text(
            rect.left_center() + egui::vec2(12.0, 0.0),
            egui::Align2::LEFT_CENTER,
            shown,
            FontId::proportional(m.field_font),
            if empty {
                Color32::from_rgb(90, 90, 100)
            } else {
                Color32::WHITE
            },
        );
    }
}

impl Default for SetupScreen {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Index of Orange in the registry — the only operator offering app auth.
    fn orange_idx() -> usize {
        OperatorRegistry::all()
            .iter()
            .position(|k| k.supports_app_auth())
            .expect("an operator with app auth")
    }

    fn other_idx() -> usize {
        OperatorRegistry::all()
            .iter()
            .position(|k| !k.supports_app_auth())
            .expect("an operator without app auth")
    }

    #[test]
    fn app_auth_needs_no_password_but_password_mode_does() {
        let mut s = SetupScreen::new();
        s.selected_op_idx = orange_idx();

        assert!(s.method_available());
        assert!(s.needs_password(), "password mode still demands one");

        s.method = AuthMethod::App;
        assert!(
            !s.needs_password(),
            "app auth identifies the line; demanding a password made this \
             flow impossible to submit"
        );
    }

    #[test]
    fn an_operator_without_app_auth_always_needs_a_password() {
        let mut s = SetupScreen::new();
        s.selected_op_idx = other_idx();
        assert!(!s.method_available());

        // Even if the mode somehow got set, an operator with no app flow must
        // never skip the password.
        s.method = AuthMethod::App;
        assert!(s.needs_password());
    }

    #[test]
    fn the_method_row_is_skipped_when_the_operator_has_no_app_auth() {
        let mut s = SetupScreen::new();
        s.selected_op_idx = other_idx();
        s.field_focus = FieldFocus::OperatorCards;

        // Down from the cards lands on Username, not on an invisible row.
        let available = s.method_available();
        s.field_focus = if available {
            FieldFocus::Method
        } else {
            FieldFocus::Username
        };
        assert_eq!(s.field_focus, FieldFocus::Username);
    }
}
