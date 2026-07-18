//! Restrained brand styling for CLI output (Manager CLI, minimal — brand.md §4/§6.3/§10).
//!
//! One pigment: **cinnabar** is the single accent (headings, URLs), per the fourteen laws
//! (§10.1 — cinnabar never lands on rules or decoration). The status colours — mint = success,
//! amber = advisory, coral = error — are *semantic only*, deliberately distinct from the accent
//! (§3.4), and are not a "spectrum" (§4.1). Colour is opt-in per the terminal: the semantic
//! helpers go through `if_supports_color`, which consults the `supports-color` crate — it honours
//! `NO_COLOR`, `CLICOLOR(_FORCE)`, `TERM=dumb`, and TTY detection. So piped/redirected output and
//! `NO_COLOR=1` stay plain ASCII, no escape codes.

use owo_colors::{OwoColorize, Stream, Style};

// The one accent — cinnabar `#B94430` (brand.md §2.2, the light-mode hex of the console's
// `oklch(.545 .155 32)`). Violet is retired (§2.2); it is no longer an accent on any surface.
const CINNABAR: (u8, u8, u8) = (0xB9, 0x44, 0x30);

// Semantic status colours (brand.md §3.4/§4.1) — meaning, not decoration.
const MINT: (u8, u8, u8) = (0x01, 0xCB, 0x8B); // success / positive
const AMBER: (u8, u8, u8) = (0xFB, 0xA1, 0x00); // warning / advisory
const CORAL: (u8, u8, u8) = (0xF3, 0x50, 0x45); // danger / error

fn paint(text: &str, stream: Stream, style: Style) -> String {
    text.if_supports_color(stream, move |t| t.style(style))
        .to_string()
}

/// Cinnabar, bold — section headings and the wordmark.
pub fn heading(text: &str) -> String {
    let s = Style::new().truecolor(CINNABAR.0, CINNABAR.1, CINNABAR.2).bold();
    paint(text, Stream::Stdout, s)
}

/// Cinnabar — URLs / links.
pub fn url(text: &str) -> String {
    let s = Style::new().truecolor(CINNABAR.0, CINNABAR.1, CINNABAR.2);
    paint(text, Stream::Stdout, s)
}

/// Mint — success confirmations.
pub fn success(text: &str) -> String {
    let s = Style::new().truecolor(MINT.0, MINT.1, MINT.2);
    paint(text, Stream::Stdout, s)
}

/// Amber — advisories and aborted actions.
pub fn warn(text: &str) -> String {
    let s = Style::new().truecolor(AMBER.0, AMBER.1, AMBER.2);
    paint(text, Stream::Stdout, s)
}

/// Coral, bold — error prefix (written to stderr, so it gates on the stderr stream).
pub fn error(text: &str) -> String {
    let s = Style::new().truecolor(CORAL.0, CORAL.1, CORAL.2).bold();
    paint(text, Stream::Stderr, s)
}

/// A checklist mark: mint `[x]` when ok, plain `[ ]` when not.
pub fn mark(ok: bool) -> String {
    if ok {
        let s = Style::new().truecolor(MINT.0, MINT.1, MINT.2).bold();
        format!("[{}]", paint("x", Stream::Stdout, s))
    } else {
        "[ ]".to_string()
    }
}

/// A restrained hairline rule for banner separators — a single dimmed line, no pigment
/// (§10.1: the accent never lands on rules or decoration; the retired four-band spectrum is
/// gone, §4.1). Returns `None` when colour is off (non-TTY / `NO_COLOR`) so scripts never see
/// stray box-drawing characters.
pub fn rule() -> Option<String> {
    if !color_enabled() {
        return None;
    }
    let s = Style::new().dimmed();
    Some(paint(&"─".repeat(32), Stream::Stdout, s))
}

/// Whether to emit the decorative (non-semantic) rule. Conservative: a real TTY and `NO_COLOR`
/// unset. Semantic colour uses owo-colors' richer detection instead.
fn color_enabled() -> bool {
    use std::io::IsTerminal;
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}
