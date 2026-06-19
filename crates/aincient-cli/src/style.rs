//! Restrained brand styling for CLI output (marketing tier, minimal — brand.md §4/§6.3).
//!
//! Violet is the signature accent (headings, URLs); mint = success, amber = advisory,
//! coral = error. Colour is opt-in per the terminal: the semantic helpers go through
//! `if_supports_color`, which consults the `supports-color` crate — it honours
//! `NO_COLOR`, `CLICOLOR(_FORCE)`, `TERM=dumb`, and TTY detection. So piped/redirected
//! output and `NO_COLOR=1` stay plain ASCII, no escape codes.

use owo_colors::{OwoColorize, Stream, Style};

// Marketing-tier spectrum (brand.md §4.1): coral · amber · mint · violet.
const CORAL: (u8, u8, u8) = (0xF3, 0x50, 0x45);
const AMBER: (u8, u8, u8) = (0xFB, 0xA1, 0x00);
const MINT: (u8, u8, u8) = (0x01, 0xCB, 0x8B);
const VIOLET: (u8, u8, u8) = (0x82, 0x57, 0xE9);

fn paint(text: &str, stream: Stream, style: Style) -> String {
    text.if_supports_color(stream, move |t| t.style(style))
        .to_string()
}

/// Violet, bold — section headings and the wordmark.
pub fn heading(text: &str) -> String {
    let s = Style::new().truecolor(VIOLET.0, VIOLET.1, VIOLET.2).bold();
    paint(text, Stream::Stdout, s)
}

/// Violet — URLs / links.
pub fn url(text: &str) -> String {
    let s = Style::new().truecolor(VIOLET.0, VIOLET.1, VIOLET.2);
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

/// A restrained four-band spectrum rule (coral · amber · mint · violet), hard stops
/// only — the brand's graphic device, not ANSI noise. Returns `None` when colour is
/// off (non-TTY / `NO_COLOR`) so scripts never see stray box-drawing characters.
pub fn spectrum_rule() -> Option<String> {
    if !color_enabled() {
        return None;
    }
    let rule = [CORAL, AMBER, MINT, VIOLET]
        .iter()
        .map(|(r, g, b)| {
            "━━━"
                .if_supports_color(Stream::Stdout, move |t| t.truecolor(*r, *g, *b))
                .to_string()
        })
        .collect::<String>();
    Some(rule)
}

/// Whether to emit the decorative (non-semantic) spectrum rule. Conservative: a real
/// TTY and `NO_COLOR` unset. Semantic colour uses owo-colors' richer detection instead.
fn color_enabled() -> bool {
    use std::io::IsTerminal;
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}
