//! The app's palette, applied to the dark theme.
//!
//! Five user-chosen colours:
//!
//! | | hex | role |
//! | --- | --- | --- |
//! | navy | `#142537` | every large surface — panels, windows, the queue |
//! | teal | `#04BFBF` | the accent: selection, links, progress, Start, running |
//! | orange | `#FE773D` | attention: warnings, the done badge, hover |
//! | red | `#F24130` | failure: error text and the failed badge |
//! | crimson | `#A63F48` | the danger surface: the Cancel-all button |
//!
//! Everything else the interface needs is *derived*, not invented: the depth
//! steps are navy blended toward black or white and the body text is
//! near-white, so nothing new competes with the five colours. Crimson is the
//! one colour kept off text — it fails contrast on the navy at ~2.3:1 — so
//! it paints surfaces instead, which is what a danger surface is for.
//! Light mode keeps egui's stock visuals, so the "Dark mode" toggle still
//! means something: dark = this palette, light = stock.

use eframe::egui::{Color32, Context, Stroke, Theme, Visuals};

pub const NAVY: Color32 = Color32::from_rgb(0x14, 0x25, 0x37); // #142537
pub const CRIMSON: Color32 = Color32::from_rgb(0xA6, 0x3F, 0x48); // #A63F48
pub const TEAL: Color32 = Color32::from_rgb(0x04, 0xBF, 0xBF); // #04BFBF
pub const ORANGE: Color32 = Color32::from_rgb(0xFE, 0x77, 0x3D); // #FE773D
pub const RED: Color32 = Color32::from_rgb(0xF2, 0x41, 0x30); // #F24130

/// Linear blend of two colours: t = 0 gives `a`, t = 1 gives `b`.
pub fn blend(a: Color32, b: Color32, t: f32) -> Color32 {
    Color32::from_rgb(
        (a.r() as f32 + (b.r() as f32 - a.r() as f32) * t).round() as u8,
        (a.g() as f32 + (b.g() as f32 - a.g() as f32) * t).round() as u8,
        (a.b() as f32 + (b.b() as f32 - a.b() as f32) * t).round() as u8,
    )
}

/// Customise the dark theme once, at startup; `apply_theme` then only has to
/// pick between "this palette" (dark) and stock light. `Context::set_theme`
/// only records a preference and never resets a style, so this survives
/// every toggle.
pub fn install(ctx: &Context) {
    ctx.style_mut_of(Theme::Dark, |style| style.visuals = dark());
}

fn dark() -> Visuals {
    let background = NAVY;
    let raised = blend(NAVY, Color32::WHITE, 0.08); // buttons, secondary surfaces
    let dim = blend(NAVY, Color32::BLACK, 0.35); // row striping, separators
    let deeper = blend(NAVY, Color32::BLACK, 0.5); // text edits, code, insets
    let text = Color32::from_rgb(0xF2, 0xF5, 0xF7); // near-white body text
    let weak = blend(text, NAVY, 0.55); // secondary text, disabled labels

    let mut v = Visuals::dark();
    v.dark_mode = true;
    v.override_text_color = Some(text);

    // Surfaces: the navy everywhere, depth by mixing toward black.
    v.window_fill = background;
    v.panel_fill = background;
    v.faint_bg_color = dim;
    v.extreme_bg_color = deeper;
    v.code_bg_color = deeper;

    // Statuses.
    v.warn_fg_color = ORANGE;
    v.error_fg_color = RED;
    v.hyperlink_color = TEAL;

    // Selection: teal, tinted back toward the navy a little so a selected
    // row is a highlight, not a wall of accent.
    v.selection.bg_fill = blend(TEAL, background, 0.25);
    v.selection.stroke = Stroke::new(1.0, blend(TEAL, Color32::WHITE, 0.25));

    // Text-entry widgets need the most surface separation.
    v.widgets.noninteractive.bg_fill = background;
    v.widgets.noninteractive.weak_bg_fill = dim;
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, weak);

    // Idle interactive widgets: navy raised a step, light stroke borders
    // kept subtle so the accent pops only where it is meant to.
    v.widgets.inactive.bg_fill = raised;
    v.widgets.inactive.weak_bg_fill = raised;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, text);
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, blend(NAVY, Color32::WHITE, 0.14));

    // Hover: teal leaning into view.
    v.widgets.hovered.bg_fill = blend(TEAL, background, 0.55);
    v.widgets.hovered.weak_bg_fill = blend(TEAL, background, 0.55);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, text);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, blend(TEAL, background, 0.3));

    // Pressed: the accent itself, with navy text on it for contrast — teal
    // against near-white reads at under 2:1.
    v.widgets.active.bg_fill = TEAL;
    v.widgets.active.weak_bg_fill = TEAL;
    v.widgets.active.fg_stroke = Stroke::new(1.0, background);
    v.widgets.active.bg_stroke = Stroke::new(1.0, blend(TEAL, Color32::BLACK, 0.25));

    // Menus, combobox popups and open state inherit the same steps.
    v.widgets.open.bg_fill = raised;
    v.widgets.open.weak_bg_fill = raised;
    v.widgets.open.fg_stroke = Stroke::new(1.0, text);

    v
}
