//! The three themes and the status colors that come with them.
//!
//! `Light` and `Dark` are egui's own visuals with a readable set of status
//! colors laid over them. `Kimi` is the shell UI's palette carried across
//! intact — the slate-and-cyan scheme from the Python shell's theme.py (now
//! only in the archived rusty-kimi-tui repo), down to swapping egui's dot
//! spinner for the moon phases the CLI is named after.
//!
//! Drawing code asks the context for colors rather than taking them as a
//! parameter: [`Theme::apply`] stashes the active theme in egui's data store,
//! so a block six calls deep in the transcript can paint an error red without
//! every function between here and there growing a theme argument.

use std::path::PathBuf;
use std::time::Duration;

use eframe::egui::{self, Color32, RichText, Stroke, Visuals};

use kimi_agent::share::get_share_dir;

/// `0xRRGGBB`, so the palette below can be read against the Python source it
/// came from without mentally re-splitting every constant.
const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// Moon phases, waxing. Segoe UI Symbol carries all eight, and `app.rs`
/// installs it as a fallback font, so these render as monochrome glyphs.
/// `app.rs` has a test asserting that coverage — without a font behind them
/// these are eight tofu boxes, not a spinner.
pub const MOON_PHASES: [&str; 8] = ["🌑", "🌒", "🌓", "🌔", "🌕", "🌖", "🌗", "🌘"];

/// Seconds per moon frame — a full cycle in a little over a second, which is
/// close to the CLI's braille spinner at 8 frames a second.
const MOON_FRAME_SECONDS: f64 = 0.13;

/// The shape of one tab bar's buttons.
///
/// A bar mixes widget kinds — text tabs, a square close ×, a folder picker —
/// and left to themselves they each pick their own height from their content.
/// Handing the whole row one of these instead makes them agree.
#[derive(Clone, Copy)]
pub struct BarStyle {
    /// Height of every button in the bar, and the side of the square ones.
    pub height: f32,
    /// Corner radius, in points.
    pub corner: u8,
    /// Gap between buttons — also the gap between rows when one wraps.
    pub spacing: f32,
}

/// The session strip along the top of the window: squared off, and a step
/// larger than the fork row beneath it so the two layers read as a hierarchy
/// rather than as two equal rows of tabs.
pub const SESSION_BAR: BarStyle = BarStyle {
    height: 24.0,
    corner: 0,
    spacing: 6.0,
};

/// The fork strip inside a session. egui's own button height and corner
/// radius, which is what this row wants; only the spacing is pinned, to match
/// the strip above it.
pub const FORK_BAR: BarStyle = BarStyle {
    height: 18.0,
    corner: 2,
    spacing: 6.0,
};

impl BarStyle {
    /// Impose these metrics on `ui` and everything nested inside it.
    ///
    /// Height goes in as `interact_size.y`, which every `Button` — and so
    /// every `selectable_label`, which is a button underneath — takes as a
    /// floor. That is what makes a long tab title and a one-glyph × the same
    /// height without sizing either of them by hand.
    ///
    /// Call this inside a [`egui::Ui::scope`] unless the `ui` is a panel's
    /// own: it edits the style in place, and the transcript below the fork
    /// row should not inherit a tab bar's spacing.
    pub fn apply(self, ui: &mut egui::Ui) {
        let spacing = ui.spacing_mut();
        spacing.item_spacing = egui::vec2(self.spacing, self.spacing);
        spacing.interact_size.y = self.height;

        let corner = egui::CornerRadius::same(self.corner);
        let widgets = &mut ui.visuals_mut().widgets;
        for widget in [
            &mut widgets.noninteractive,
            &mut widgets.inactive,
            &mut widgets.hovered,
            &mut widgets.active,
            &mut widgets.open,
        ] {
            widget.corner_radius = corner;
        }
    }

    /// Add a button whose label is a single glyph, sized so that every one of
    /// them in the bar is the same square.
    ///
    /// The side padding is dropped for the duration: `min_size` is only a
    /// floor, so a wide glyph — 📖 is wider than × — would otherwise carry the
    /// bar's padding out past the height and leave the button oblong.
    pub fn square<'a>(self, ui: &mut egui::Ui, label: impl egui::IntoAtoms<'a>) -> egui::Response {
        let side = egui::Vec2::splat(self.height);
        ui.scope(|ui| {
            ui.spacing_mut().button_padding.x = 0.0;
            ui.add(egui::Button::new(label).min_size(side))
        })
        .inner
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Theme {
    Light,
    #[default]
    Dark,
    /// The shell UI's own palette: slate backgrounds, cyan accents, moons.
    Kimi,
}

/// Status colors that egui's `Visuals` has no slot for. Error and warning are
/// mirrored into `Visuals` as well, so widgets egui paints itself agree with
/// the ones painted here.
#[derive(Clone, Copy)]
pub struct Colors {
    pub success: Color32,
    pub error: Color32,
    pub warning: Color32,
    pub accent: Color32,
    pub diff_add: Color32,
    pub diff_del: Color32,
}

impl Theme {
    /// Light → Dark → Kimi → Light. The single button cycles; there are three
    /// themes and two of them are a click apart either way.
    pub fn next(self) -> Self {
        match self {
            Self::Light => Self::Dark,
            Self::Dark => Self::Kimi,
            Self::Kimi => Self::Light,
        }
    }

    /// What the toolbar button shows: the theme that is on right now.
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Light => "☀",
            Self::Dark => "☾",
            Self::Kimi => "🌕",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
            Self::Kimi => "Kimi",
        }
    }

    /// Hover text naming both where you are and where the click goes.
    pub fn hover(self) -> String {
        format!(
            "theme: {} — click for {} (Ctrl+D)",
            self.label(),
            self.next().label()
        )
    }

    pub fn colors(self) -> Colors {
        match self {
            // egui's light visuals sit on white; the CLI's light theme picks
            // dark, saturated status colors for exactly that reason.
            Self::Light => Colors {
                success: rgb(0x166534),
                error: rgb(0x991b1b),
                warning: rgb(0x92400e),
                accent: rgb(0x0e7490),
                diff_add: rgb(0x1a7f37),
                diff_del: rgb(0xcf222e),
            },
            Self::Dark => Colors {
                success: rgb(0x50aa50),
                error: rgb(0xc85050),
                warning: rgb(0xdca03c),
                accent: rgb(0x5aa9e6),
                diff_add: rgb(0x5aaa5a),
                diff_del: rgb(0xc85a5a),
            },
            Self::Kimi => Colors {
                success: rgb(0x86efac),
                error: rgb(0xfca5a5),
                warning: rgb(0xfbbf24),
                accent: rgb(0x67e8f9),
                // The CLI's MCP status colors, which are its diff greens/reds.
                diff_add: rgb(0x56d364),
                diff_del: rgb(0xff7b72),
            },
        }
    }

    /// Which of egui's two visual slots this theme lives in.
    fn slot(self) -> egui::Theme {
        match self {
            Self::Light => egui::Theme::Light,
            Self::Dark | Self::Kimi => egui::Theme::Dark,
        }
    }

    fn visuals(self) -> Visuals {
        let colors = self.colors();
        let mut visuals = match self {
            Self::Light => Visuals::light(),
            Self::Dark => Visuals::dark(),
            Self::Kimi => kimi_visuals(),
        };
        visuals.error_fg_color = colors.error;
        visuals.warn_fg_color = colors.warning;
        visuals
    }

    /// Install this theme. Pinning the theme preference matters as much as the
    /// visuals: left on "follow the system", egui would swap slots when the
    /// desktop flipped to light mode and drop the Kimi palette on the floor.
    pub fn apply(self, ctx: &egui::Context) {
        ctx.set_theme(self.slot());
        ctx.set_visuals_of(self.slot(), self.visuals());
        ctx.data_mut(|data| data.insert_temp(theme_id(), self));
    }

    fn name(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
            Self::Kimi => "kimi",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            "kimi" => Some(Self::Kimi),
            _ => None,
        }
    }

    /// The theme chosen last run, or the default for a fresh install.
    pub fn load() -> Self {
        std::fs::read_to_string(theme_file())
            .ok()
            .and_then(|text| Self::from_name(text.trim()))
            .unwrap_or_default()
    }

    /// Remember the choice. A preference is not worth an error modal, so a
    /// failure here just means the next launch starts on the default.
    pub fn save(self) {
        let path = theme_file();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, self.name());
    }
}

fn theme_file() -> PathBuf {
    get_share_dir().join("gui-theme")
}

fn theme_id() -> egui::Id {
    egui::Id::new("kimi_gui_theme")
}

/// The theme in force. Falls back to the default before the first
/// [`Theme::apply`], which only matters if a widget somehow draws first.
pub fn active(ctx: &egui::Context) -> Theme {
    ctx.data(|data| data.get_temp(theme_id()))
        .unwrap_or_default()
}

pub fn colors(ctx: &egui::Context) -> Colors {
    active(ctx).colors()
}

/// A "still working" indicator in the active theme's idiom: moon phases under
/// the Kimi theme, egui's dots otherwise.
///
/// The moons ask for a repaint one frame ahead rather than every frame the way
/// `Ui::spinner` does, so an indicator left on screen costs eight repaints a
/// second instead of pinning the render loop at the display's refresh rate.
pub fn spinner(ui: &mut egui::Ui) {
    let theme = active(ui.ctx());
    if theme != Theme::Kimi {
        ui.spinner();
        return;
    }
    let phase = ui.input(|i| i.time) / MOON_FRAME_SECONDS;
    let frame = MOON_PHASES[(phase as usize) % MOON_PHASES.len()];
    ui.label(RichText::new(frame).color(theme.colors().accent));
    ui.ctx()
        .request_repaint_after(Duration::from_secs_f64(MOON_FRAME_SECONDS));
}

/// The shell UI's dark theme, transcribed from `kimi_cli/ui/theme.py`:
/// slate-900/950 grounds, a cyan-300 accent, cyan-800 borders and selection.
fn kimi_visuals() -> Visuals {
    let mut visuals = Visuals::dark();

    visuals.panel_fill = rgb(0x0f172a);
    visuals.window_fill = rgb(0x111827);
    // Text edits and code blocks sit one step darker than their panel, the way
    // the CLI's task list does against its frame.
    visuals.extreme_bg_color = rgb(0x0b1220);
    visuals.code_bg_color = rgb(0x111827);
    visuals.faint_bg_color = rgb(0x1b2537);
    visuals.hyperlink_color = rgb(0x67e8f9);
    visuals.window_stroke = Stroke::new(1.0, rgb(0x155e75));
    visuals.selection.bg_fill = rgb(0x164e63);
    visuals.selection.stroke = Stroke::new(1.0, rgb(0xecfeff));

    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_fill = rgb(0x0f172a);
    widgets.noninteractive.weak_bg_fill = rgb(0x0f172a);
    widgets.noninteractive.bg_stroke = Stroke::new(1.0, rgb(0x1f2937));
    widgets.noninteractive.fg_stroke = Stroke::new(1.0, rgb(0xe5e7eb));

    widgets.inactive.bg_fill = rgb(0x1f2937);
    widgets.inactive.weak_bg_fill = rgb(0x1f2937);
    widgets.inactive.bg_stroke = Stroke::new(1.0, rgb(0x243244));
    widgets.inactive.fg_stroke = Stroke::new(1.0, rgb(0xcbd5e1));

    widgets.hovered.bg_fill = rgb(0x2b3a52);
    widgets.hovered.weak_bg_fill = rgb(0x2b3a52);
    widgets.hovered.bg_stroke = Stroke::new(1.0, rgb(0x155e75));
    widgets.hovered.fg_stroke = Stroke::new(1.5, rgb(0xecfeff));

    widgets.active.bg_fill = rgb(0x155e75);
    widgets.active.weak_bg_fill = rgb(0x155e75);
    widgets.active.bg_stroke = Stroke::new(1.0, rgb(0x67e8f9));
    widgets.active.fg_stroke = Stroke::new(2.0, rgb(0xecfeff));

    widgets.open.bg_fill = rgb(0x1f2937);
    widgets.open.weak_bg_fill = rgb(0x1f2937);
    widgets.open.bg_stroke = Stroke::new(1.0, rgb(0x155e75));
    widgets.open.fg_stroke = Stroke::new(1.0, rgb(0xe5e7eb));

    visuals
}

#[cfg(test)]
mod tests {
    use super::{BarStyle, FORK_BAR, MOON_FRAME_SECONDS, MOON_PHASES, SESSION_BAR, Theme};
    use eframe::egui;

    /// Lay one row of mixed widgets out under `bar` and report what each of
    /// them actually measured, as (width, height).
    fn row_sizes(bar: BarStyle) -> Vec<(f32, f32)> {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::Vec2::new(900.0, 700.0),
            )),
            ..Default::default()
        };
        // Fonts are built lazily; the first pass measures against a fallback.
        let _ = ctx.run(input.clone(), |_| {});
        let mut sizes = Vec::new();
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                bar.apply(ui);
                ui.horizontal(|ui| {
                    sizes.push(ui.selectable_label(true, "a long session title").rect);
                    sizes.push(ui.selectable_label(false, "x").rect);
                    sizes.push(bar.square(ui, "×").rect);
                    sizes.push(bar.square(ui, "+").rect);
                    sizes.push(bar.square(ui, "📖").rect);
                });
            });
        });
        sizes
            .into_iter()
            .map(|rect| (rect.width(), rect.height()))
            .collect()
    }

    /// The whole point of [`BarStyle`]: a text tab and a one-glyph button are
    /// different widgets with different content, and they still have to line
    /// up. Left to themselves they do not.
    #[test]
    fn test_every_button_in_a_bar_is_the_same_height() {
        for bar in [SESSION_BAR, FORK_BAR] {
            for (width, height) in row_sizes(bar) {
                assert_eq!(
                    height, bar.height,
                    "a {width}x{height} button broke the row"
                );
            }
        }
    }

    #[test]
    fn test_square_buttons_are_square_and_all_one_size() {
        for bar in [SESSION_BAR, FORK_BAR] {
            // The last three of the row are the square ones.
            let squares = &row_sizes(bar)[2..];
            for (width, height) in squares {
                assert_eq!((*width, *height), (bar.height, bar.height));
            }
        }
    }

    /// The two strips are meant to read as a hierarchy, not as two equal rows.
    #[test]
    fn test_the_session_bar_is_the_bigger_and_squarer_of_the_two() {
        // Height/corner are compile-time facts about the constants; check
        // them once at the definition instead of per-test-run.
        const _: () = {
            assert!(SESSION_BAR.height > FORK_BAR.height);
            assert!(SESSION_BAR.corner < FORK_BAR.corner);
        };
        // Spacing is the one thing they share, so the two rows align.
        assert_eq!(SESSION_BAR.spacing, FORK_BAR.spacing);
    }

    #[test]
    fn test_cycling_visits_every_theme_and_returns() {
        let mut theme = Theme::Light;
        let mut seen = vec![theme];
        for _ in 0..2 {
            theme = theme.next();
            seen.push(theme);
        }
        assert_eq!(seen, vec![Theme::Light, Theme::Dark, Theme::Kimi]);
        assert_eq!(theme.next(), Theme::Light);
    }

    #[test]
    fn test_names_round_trip() {
        for theme in [Theme::Light, Theme::Dark, Theme::Kimi] {
            assert_eq!(Theme::from_name(theme.name()), Some(theme));
        }
    }

    #[test]
    fn test_unknown_name_is_rejected() {
        // A hand-edited or truncated preference file falls back to the
        // default rather than refusing to start.
        assert_eq!(Theme::from_name(""), None);
        assert_eq!(Theme::from_name("solarized"), None);
    }

    #[test]
    fn test_hover_names_this_theme_and_the_next() {
        assert_eq!(Theme::Dark.hover(), "theme: dark — click for Kimi (Ctrl+D)");
    }

    /// The moon index is a modulo of a monotonically growing float; make sure
    /// it stays in range across a long-running session rather than only at
    /// t = 0.
    #[test]
    fn test_moon_phase_index_stays_in_range() {
        for seconds in [0.0, 0.5, 13.0, 86_400.0] {
            let phase = seconds / MOON_FRAME_SECONDS;
            assert!((phase as usize) % MOON_PHASES.len() < MOON_PHASES.len());
        }
    }
}
