//! The app's palette, applied to the one dark theme.
//!
//! dconvert is *permanently* dark. The light mode was egui's stock visuals,
//! and the status colours cannot survive on it — measured on white: teal
//! 2.3:1, orange 2.6:1, red 3.7:1, all far below the 4.5:1 a small status
//! label needs — because `status_color`'s palette is the navy-world's
//! palette. Building a second, light-tuned copy of the whole token set to
//! keep that mode alive costs more than the two dark-only bugs it would fix
//! (the "Dark mode" toggle and the light-contrast failure), so the toggle
//! went instead. The contrast floor for every colour below is now pinned by
//! the tests at the bottom of this file, which also keep the *premise*
//! guarded: if some future change claims to fix RED as text by retuning RED
//! itself, the test that fails tells you which half drifted.
//!
//! Five user-chosen colours:
//!
//! | | hex | role |
//! | --- | --- | --- |
//! | navy | `#142537` | every large surface — panels, windows, the queue |
//! | teal | `#04BFBF` | the accent: selection, links, progress, Start, running |
//! | orange | `#FE773D` | attention: warnings, the done badge, hover |
//! | red | `#F24130` | failure *base*, surfaces and derivation only: 4.1:1 as text on the navy, so it never reads as text |
//! | crimson | `#A63F48` | the danger surface: the Cancel-all button and the overwrite confirmation (also ~2.5:1 on the navy — off text by design) |
//!
//! The deliberately added text tone is [`RED_TEXT`] (`#F57164`, RED blended
//! a quarter toward white): the failed badge and every error line need
//! 4.5:1, and RED itself stops at 4.1:1. Everything else is blended, not
//! invented: depth steps are navy mixed toward black or white, body text is
//! near-white, and weak text is body text blended toward the navy — pinned
//! explicitly via `Visuals::weak_text_color` rather than left to egui's
//! 0.6 gamma fallback, so the numbers are exact and portable.
//!
//! Measured contrast of text on the surface it sits on (WCAG 2.1, held by
//! the tests):
//!
//! | colour | on navy | reads as |
//! | --- | --- | --- |
//! | body text `#F2F5F7` | 14.2:1 | primary text |
//! | weak text `#8E97A1` | 5.3:1 | secondary text; the queued / cancelled badges |
//! | teal / orange | 6.8:1 / 5.9:1 | running / done badges |
//! | `RED_TEXT` `#F57164` | 5.5:1 | failed badge, error lines |
//! | navy on teal / body text on crimson | 6.8:1 / 5.6:1 | button faces |

use eframe::egui::{Color32, Context, Stroke, Theme, Visuals};

pub const NAVY: Color32 = Color32::from_rgb(0x14, 0x25, 0x37); // #142537
pub const CRIMSON: Color32 = Color32::from_rgb(0xA6, 0x3F, 0x48); // #A63F48
pub const TEAL: Color32 = Color32::from_rgb(0x04, 0xBF, 0xBF); // #04BFBF
pub const ORANGE: Color32 = Color32::from_rgb(0xFE, 0x77, 0x3D); // #FE773D
/// RED as the failure family's base. It is deliberately never applied: it
/// measures 4.1:1 as text on the navy (the tests below hold both halves of
/// that premise), and every fill this app paints is either the navy or the
/// crimson danger surface — nothing needed a red fill. It stays a named
/// constant because the palette documents five colours and RED_TEXT is
/// *derived* from this one: collapsing it would make the brightened red
/// look like an invented tone rather than a measured stop.
#[cfg_attr(not(test), allow(dead_code))]
pub const RED: Color32 = Color32::from_rgb(0xF2, 0x41, 0x30); // #F24130

/// RED as *text*: the palette's brighter red stop (`blend(RED, WHITE, 0.25)`
/// = `#F57164`, 5.5:1 on the navy), used wherever failure is read as a word —
/// the failed badge, error log lines, "engine unavailable". RED itself
/// remains the base for surfaces and derivation; its 4.1:1 on the navy is
/// the whole reason this variant exists.
pub const RED_TEXT: Color32 = Color32::from_rgb(0xF5, 0x71, 0x64);

/// Body text (`blend` input for [`TEXT_WEAK`] below).
pub const TEXT: Color32 = Color32::from_rgb(0xF2, 0xF5, 0xF7);

/// Weak text: `blend(TEXT, NAVY, 0.45)`. The previous step — 0.55 — measured
/// 4.0:1 on the navy, just under the 4.5:1 secondary text needs; one blend
/// step buys 5.3:1 without changing how "quiet" the text reads.
pub const TEXT_WEAK: Color32 = Color32::from_rgb(0x8E, 0x97, 0xA1);

/// Linear blend of two colours: t = 0 gives `a`, t = 1 gives `b`.
pub fn blend(a: Color32, b: Color32, t: f32) -> Color32 {
    Color32::from_rgb(
        (a.r() as f32 * (1.0 - t) + b.r() as f32 * t).round() as u8,
        (a.g() as f32 * (1.0 - t) + b.g() as f32 * t).round() as u8,
        (a.b() as f32 * (1.0 - t) + b.b() as f32 * t).round() as u8,
    )
}

/// Record the dark preference and paint the palette, once. `Context::set_theme`
/// only records a preference and never resets a style underneath it, so this
/// survives every frame; there is deliberately no Light branch left (module
/// comment).
pub fn install(ctx: &Context) {
    ctx.set_theme(Theme::Dark);
    ctx.style_mut_of(Theme::Dark, |style| style.visuals = dark());
}

fn dark() -> Visuals {
    let background = NAVY;
    let raised = blend(NAVY, Color32::WHITE, 0.08); // buttons, secondary surfaces
    let dim = blend(NAVY, Color32::BLACK, 0.35); // row striping, separators
    let deeper = blend(NAVY, Color32::BLACK, 0.5); // text edits, code, insets

    let mut v = Visuals::dark();
    v.dark_mode = true;
    v.override_text_color = Some(TEXT);
    // Pinned rather than egui's graphical-fade default (`gamma_multiply`),
    // because the contrast table at the top of this file is asserted on the
    // exact value this constant holds.
    v.weak_text_color = Some(TEXT_WEAK);

    // Surfaces: the navy everywhere, depth by mixing toward black.
    v.window_fill = background;
    v.panel_fill = background;
    v.faint_bg_color = dim;
    v.extreme_bg_color = deeper;
    v.code_bg_color = deeper;

    // Statuses. Errors read as text (`RED_TEXT`); RED stays for surfaces.
    v.warn_fg_color = ORANGE;
    v.error_fg_color = RED_TEXT;
    v.hyperlink_color = TEAL;

    // Selection: teal, tinted back toward the navy a little so a selected
    // row is a highlight, not a wall of accent.
    v.selection.bg_fill = blend(TEAL, background, 0.25);
    v.selection.stroke = Stroke::new(1.0, blend(TEAL, Color32::WHITE, 0.25));

    // Text-entry widgets need the most surface separation.
    v.widgets.noninteractive.bg_fill = background;
    v.widgets.noninteractive.weak_bg_fill = dim;
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_WEAK);

    // Idle interactive widgets: navy raised a step, light stroke borders
    // kept subtle so the accent pops only where it is meant to.
    v.widgets.inactive.bg_fill = raised;
    v.widgets.inactive.weak_bg_fill = raised;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, blend(NAVY, Color32::WHITE, 0.14));

    // Hover: teal leaning into view.
    v.widgets.hovered.bg_fill = blend(TEAL, background, 0.55);
    v.widgets.hovered.weak_bg_fill = blend(TEAL, background, 0.55);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT);
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
    v.widgets.open.fg_stroke = Stroke::new(1.0, TEXT);

    v
}

// ---------------------------------------------------------------------------
// Tests: the contrast floor, held where the palette lives
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG 2.1 relative luminance of one sRGB channel, verbatim from the
    /// spec so this test can be checked against it.
    fn channel(c: u8) -> f64 {
        let v = c as f64 / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    }

    fn luminance(c: Color32) -> f64 {
        0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
    }

    fn contrast(a: Color32, b: Color32) -> f64 {
        let (hi, lo) = if luminance(a) >= luminance(b) {
            (a, b)
        } else {
            (b, a)
        };
        (luminance(hi) + 0.05) / (luminance(lo) + 0.05)
    }

    fn assert_aa(what: &str, fg: Color32, bg: Color32) {
        let ratio = contrast(fg, bg);
        assert!(
            ratio >= 4.5,
            "{what} is at {ratio:.2}:1 against its surface, below the 4.5:1 \
             small text needs"
        );
    }

    #[test]
    fn every_text_colour_clears_45_to_1_on_its_surface() {
        assert_aa("body text", TEXT, NAVY);
        // Hints, the status bar, the queued and cancelled badges — the quiet
        // half of the interface is still text someone has to read.
        assert_aa("weak text", TEXT_WEAK, NAVY);
        assert_aa("running badge", TEAL, NAVY);
        assert_aa("done badge", ORANGE, NAVY);
        assert_aa("failed badge and error lines", RED_TEXT, NAVY);
        // Faces: the Start button and the pressed-widget state put navy text
        // on teal; Cancel all and the overwrite confirmation put body text
        // on crimson.
        assert_aa("Start / pressed-widget face", NAVY, TEAL);
        assert_aa("danger-surface label", TEXT, CRIMSON);
    }

    #[test]
    fn red_as_text_is_its_own_brighter_stop() {
        // The premise (file comment): plain RED is below AA as text on the
        // navy, and RED_TEXT is the palette family's pass. Both halves are
        // asserted so a change to either the base or the text tone that
        // breaks one fails here rather than by accident elsewhere.
        assert!(
            contrast(RED, NAVY) < 4.5,
            "RED as text measured {:+.2}:1 — the premise changed",
            contrast(RED, NAVY)
        );
        assert_ne!(RED_TEXT, RED);
        assert!(contrast(RED_TEXT, NAVY) > contrast(RED, NAVY));
    }
}
