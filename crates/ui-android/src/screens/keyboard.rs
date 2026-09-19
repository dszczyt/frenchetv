//! D-pad-driven on-screen keyboard.
//!
//! Android text entry does not reach this app. The system IME opens correctly
//! (egui requests it, winit forwards it to `AndroidApp::show_soft_input`), but
//! the text it produces is committed through GameActivity's text-input state,
//! which `android-activity` exposes as `text_input_state()` and winit's Android
//! backend never reads. Keystrokes land in a buffer nobody is listening to, so
//! nothing reaches egui.
//!
//! Rather than bridge that gap, this screen does its own text entry: the
//! `TextEdit`s are render-only, never take egui focus, and never request the
//! IME. This widget mutates the target `String` directly from D-pad input.

use egui::{Color32, FontId, Key, RichText, Vec2};

/// One key on the grid.
#[derive(Debug, Clone, Copy, PartialEq)]
enum KeyCap {
    Char(char),
    Space,
    Backspace,
    Shift,
    Done,
}

impl KeyCap {
    /// Cell span. Row 4's action keys are wider, but navigation still treats
    /// each `KeyCap` as one stop, so movement stays predictable.
    fn weight(&self) -> f32 {
        match self {
            Self::Char(_) => 1.0,
            Self::Shift | Self::Backspace => 2.0,
            Self::Done => 2.0,
            Self::Space => 4.0,
        }
    }

    fn label(&self, shift: bool) -> String {
        match self {
            Self::Char(c) => {
                if shift {
                    c.to_uppercase().to_string()
                } else {
                    c.to_string()
                }
            }
            // Plain words, not symbols: the bundled default font has no glyph
            // for U+2325 / U+232B and renders them as tofu on the device.
            Self::Space => "Espace".to_string(),
            Self::Backspace => "Effacer".to_string(),
            Self::Shift => "Maj".to_string(),
            Self::Done => "OK".to_string(),
        }
    }
}

/// AZERTY-ordered, for a French-facing app. Row 3 carries the characters an
/// email address actually needs, so `@` and `.` are never more than a few
/// D-pad presses away.
fn layout() -> Vec<Vec<KeyCap>> {
    let row = |s: &str| -> Vec<KeyCap> { s.chars().map(KeyCap::Char).collect() };
    vec![
        row("1234567890"),
        row("azertyuiop"),
        row("qsdfghjklm"),
        row("wxcvbn.-_@"),
        vec![
            KeyCap::Shift,
            KeyCap::Space,
            KeyCap::Backspace,
            KeyCap::Done,
        ],
    ]
}

/// Map a column between rows of differing length by horizontal position rather
/// than by index. The bottom row has four wide keys against ten narrow ones
/// above it, so clamping by index would drop the cursor on "OK" from anywhere
/// right of centre — including from "b", which sits above "Espace".
fn remap_col(col: usize, from_len: usize, to_len: usize) -> usize {
    if from_len <= 1 || to_len == 0 {
        return 0;
    }
    let ratio = col as f32 / (from_len - 1) as f32;
    (ratio * (to_len - 1) as f32).round() as usize
}

/// What the caller should do after `show`.
#[derive(Debug, PartialEq)]
pub enum KeyboardAction {
    /// Still open, keep routing input here.
    None,
    /// User confirmed; close and return focus to the form.
    Close,
}

pub struct OnScreenKeyboard {
    open: bool,
    row: usize,
    col: usize,
    shift: bool,
    /// The Enter that opens the keyboard is still in this frame's input when
    /// `show` runs, so without this it would immediately be read as pressing
    /// whichever key starts focused.
    just_opened: bool,
}

impl OnScreenKeyboard {
    pub fn new() -> Self {
        Self {
            open: false,
            row: 1,
            col: 0,
            shift: false,
            just_opened: false,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self) {
        self.open = true;
        self.just_opened = true;
        self.row = 1;
        self.col = 0;
        self.shift = false;
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    /// Draw the keyboard and apply D-pad input to `target`.
    ///
    /// Only called while open, and while open the caller must not process its
    /// own navigation — every key press belongs to the keyboard.
    pub fn show(&mut self, ctx: &egui::Context, target: &mut String) -> KeyboardAction {
        let keys = layout();
        let mut action = KeyboardAction::None;

        if self.just_opened {
            self.just_opened = false;
            self.paint(ctx, &keys);
            return action;
        }

        let (left, right, up, down, enter, escape) = ctx.input(|i| {
            (
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::Enter),
                i.key_pressed(Key::Escape),
            )
        });

        if up && self.row > 0 {
            self.col = remap_col(self.col, keys[self.row].len(), keys[self.row - 1].len());
            self.row -= 1;
        }
        if down && self.row + 1 < keys.len() {
            self.col = remap_col(self.col, keys[self.row].len(), keys[self.row + 1].len());
            self.row += 1;
        }
        if left && self.col > 0 {
            self.col -= 1;
        }
        if right && self.col + 1 < keys[self.row].len() {
            self.col += 1;
        }

        if escape {
            self.close();
            return KeyboardAction::Close;
        }

        if enter {
            match keys[self.row][self.col] {
                KeyCap::Char(c) => {
                    if self.shift {
                        target.extend(c.to_uppercase());
                    } else {
                        target.push(c);
                    }
                }
                KeyCap::Space => target.push(' '),
                KeyCap::Backspace => {
                    target.pop();
                }
                KeyCap::Shift => self.shift = !self.shift,
                KeyCap::Done => {
                    self.close();
                    action = KeyboardAction::Close;
                }
            }
        }

        self.paint(ctx, &keys);
        action
    }

    fn paint(&self, ctx: &egui::Context, keys: &[Vec<KeyCap>]) {
        let screen = ctx.screen_rect();
        // Sits in the right half while the form occupies the left, so the whole
        // form and the whole keyboard are legible at once — on a TV there is no
        // pointer to scroll with, and a keyboard covering the fields hides the
        // very text being typed.
        let grid_w = (screen.width() * 0.46).clamp(280.0, 560.0);
        let key_h = ((screen.height() * 0.085).clamp(26.0, 42.0)).floor();
        let unit_w = grid_w / 10.0;

        egui::Area::new(egui::Id::new("osk"))
            .anchor(egui::Align2::RIGHT_CENTER, egui::vec2(-16.0, 0.0))
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(Color32::from_rgb(18, 20, 26))
                    .stroke(egui::Stroke::new(1.5_f32, Color32::from_rgb(60, 60, 70)))
                    .rounding(10.0)
                    .inner_margin(10.0)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing = Vec2::new(4.0, 4.0);
                        for (r, row) in keys.iter().enumerate() {
                            ui.horizontal(|ui| {
                                for (c, key) in row.iter().enumerate() {
                                    let focused = r == self.row && c == self.col;
                                    let active = matches!(key, KeyCap::Shift) && self.shift;

                                    let (bg, border) = if focused {
                                        (
                                            Color32::from_rgb(10, 132, 255),
                                            Color32::from_rgb(120, 200, 255),
                                        )
                                    } else if active {
                                        (
                                            Color32::from_rgb(30, 80, 160),
                                            Color32::from_rgb(80, 140, 200),
                                        )
                                    } else {
                                        (
                                            Color32::from_rgb(32, 35, 43),
                                            Color32::from_rgb(60, 60, 70),
                                        )
                                    };

                                    let w = unit_w * key.weight() - 4.0;
                                    let (rect, _) = ui.allocate_exact_size(
                                        Vec2::new(w, key_h),
                                        egui::Sense::hover(),
                                    );
                                    ui.painter().rect(
                                        rect,
                                        6.0,
                                        bg,
                                        egui::Stroke::new(
                                            if focused { 2.0_f32 } else { 1.0_f32 },
                                            border,
                                        ),
                                    );
                                    ui.painter().text(
                                        rect.center(),
                                        egui::Align2::CENTER_CENTER,
                                        key.label(self.shift),
                                        FontId::proportional((key_h * 0.45).clamp(12.0, 20.0)),
                                        Color32::WHITE,
                                    );
                                }
                            });
                        }

                        ui.add_space(2.0);
                        ui.vertical_centered(|ui| {
                            ui.label(
                                RichText::new("D-pad déplace  •  Entrée saisit  •  OK termine")
                                    .font(FontId::proportional(12.0))
                                    .color(Color32::from_rgb(110, 110, 120)),
                            );
                        });
                    });
            });
    }
}

impl Default for OnScreenKeyboard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remap_keeps_the_edges_on_the_edges() {
        // 10-wide row down to the 4-wide action row.
        assert_eq!(remap_col(0, 10, 4), 0); // leftmost -> Maj
        assert_eq!(remap_col(9, 10, 4), 3); // rightmost -> OK
    }

    #[test]
    fn remap_lands_mid_row_on_space_not_ok() {
        // "b" sits above Espace; index clamping used to drop it on OK.
        assert_eq!(remap_col(4, 10, 4), 1); // Espace
        assert_eq!(remap_col(5, 10, 4), 2); // Effacer
    }

    #[test]
    fn remap_handles_degenerate_rows() {
        assert_eq!(remap_col(3, 1, 4), 0);
        assert_eq!(remap_col(3, 10, 0), 0);
    }
}
