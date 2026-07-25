//! The `knaix` wordmark, rendered in the brand gradient.
//!
//! Both websites already treat the wordmark as the only thing that gets any
//! colour, on the reasoning that a page highlighting everything highlights
//! nothing. This module brings the CLI in line with them.
//!
//! Colours are sampled from the brand stop list rather than hardcoded per
//! letter, so retuning the gradient retunes the wordmark and the two cannot
//! drift apart.

use std::io::IsTerminal;

/// The brand gradient, as (position, RGB) stops. Same four stops the sites use.
const STOPS: &[(f32, (u8, u8, u8))] = &[
    (0.00, (253, 164, 60)), // #FDA43C
    (0.30, (236, 72, 153)), // #EC4899
    (0.62, (124, 92, 255)), // #7C5CFF
    (1.00, (34, 211, 238)), // #22D3EE
];

/// How much colour the attached terminal can actually render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// No escape sequences at all: piped, NO_COLOR, or a dumb terminal.
    None,
    /// The basic ANSI palette. The wordmark falls back to a single cyan here,
    /// which is what it looked like before this module existed.
    Ansi16,
    /// The 256-colour cube. The gradient bands slightly and still reads.
    Ansi256,
    /// 24-bit colour. The gradient as designed.
    TrueColor,
}

/// Sample the gradient at `t` in 0..=1.
fn sample(t: f32) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);

    // Find the segment `t` falls in and interpolate across it. The stop list is
    // short enough that a scan beats anything cleverer.
    for pair in STOPS.windows(2) {
        let (t0, c0) = pair[0];
        let (t1, c1) = pair[1];
        if t <= t1 {
            let span = t1 - t0;
            // Coincident stops would divide by zero; treat them as a hard edge.
            let local = if span <= f32::EPSILON {
                0.0
            } else {
                (t - t0) / span
            };
            return (
                lerp(c0.0, c1.0, local),
                lerp(c0.1, c1.1, local),
                lerp(c0.2, c1.2, local),
            );
        }
    }

    STOPS[STOPS.len() - 1].1
}

fn lerp(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t)
        .round()
        .clamp(0.0, 255.0) as u8
}

/// The six levels each channel of the xterm-256 colour cube can take.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// Nearest index in the 6x6x6 cube for an RGB triple.
fn to_ansi256(r: u8, g: u8, b: u8) -> u8 {
    let idx = |v: u8| -> u8 {
        let mut best = 0usize;
        let mut best_delta = u16::MAX;
        for (i, level) in CUBE_LEVELS.iter().enumerate() {
            let delta = (*level as i16 - v as i16).unsigned_abs();
            if delta < best_delta {
                best_delta = delta;
                best = i;
            }
        }
        best as u8
    };
    16 + 36 * idx(r) + 6 * idx(g) + idx(b)
}

/// Terminals that render true 24-bit colour, identified by `TERM_PROGRAM`.
///
/// An allowlist rather than a guess: a terminal that renders 24-bit colour and
/// forgets to advertise it is common, and the failure is silent. `COLORTERM` is
/// the only signal that is supposed to carry this, and plenty of capable
/// terminals never set it.
const TRUECOLOR_TERM_PROGRAMS: &[&str] = &[
    "iTerm.app",
    "WezTerm",
    "vscode",
    "Hyper",
    "ghostty",
    "Tabby",
    "rio",
    "WarpTerminal",
];

/// Terminals known to stop at 256 colours, identified by `TERM_PROGRAM`.
///
/// macOS Terminal.app is the one that matters here, and it is a genuine trap:
/// it *parses* a 24-bit SGR sequence and approximates it to its own palette
/// rather than ignoring it. So printing a truecolor gradient to it appears to
/// work, which means the obvious "do I see distinct colours?" test cannot tell
/// the two apart. Naming it explicitly means we emit what it actually renders
/// rather than relying on it to quietly clean up after us.
const ANSI256_TERM_PROGRAMS: &[&str] = &["Apple_Terminal"];

/// `TERM` values that imply 24-bit colour on their own.
const TRUECOLOR_TERMS: &[&str] = &[
    "kitty",
    "alacritty",
    "wezterm",
    "contour",
    "foot",
    "ghostty",
];

/// The environment the decision is made from, captured so the decision itself
/// is a pure function.
///
/// Reading the process environment inside the tests would make them race: env
/// vars are process-global and `cargo test` runs threads in parallel, so one
/// test's `set_var` lands in the middle of another's read. Passing the
/// environment in means every branch below is testable and none of it is racy.
pub struct TermEnv<'a> {
    pub knaix_color: Option<&'a str>,
    pub knaix_no_gradient: bool,
    pub no_color: Option<&'a str>,
    pub clicolor_force: Option<&'a str>,
    pub colorterm: Option<&'a str>,
    pub term: Option<&'a str>,
    pub term_program: Option<&'a str>,
    pub is_tty: bool,
}

/// Decide the colour level from a captured environment.
pub fn level_for(env: &TermEnv) -> Level {
    // An explicit answer wins over every heuristic below. Terminal detection is
    // guesswork at the edges, so anyone in a setup we guess wrong about needs a
    // way to say so that does not involve waiting for a release.
    if let Some(forced) = env.knaix_color {
        match forced.trim().to_ascii_lowercase().as_str() {
            "none" | "off" | "0" => return Level::None,
            "16" | "ansi" | "basic" => return Level::Ansi16,
            "256" | "ansi256" => return Level::Ansi256,
            "truecolor" | "24bit" | "rgb" => return Level::TrueColor,
            // An unrecognised value falls through rather than failing: a typo in
            // an env var should not cost someone their output.
            _ => {}
        }
    }

    // Opt out of the gradient specifically, for a terminal background it reads
    // badly against, without giving up colour altogether.
    if env.knaix_no_gradient {
        return Level::Ansi16;
    }

    // The NO_COLOR spec is "present and not an empty string". An empty value is
    // not opting out, and treating it as though it were is a common bug.
    if env.no_color.is_some_and(|v| !v.is_empty()) {
        return Level::None;
    }

    // CLICOLOR_FORCE keeps colour on when output is redirected, which is what
    // recording a demo through a pipe needs. Zero means "no force", not "off".
    let forced_on = env
        .clicolor_force
        .is_some_and(|v| !v.is_empty() && v != "0");

    if !env.is_tty && !forced_on {
        return Level::None;
    }

    let term = env.term.unwrap_or("");
    if term == "dumb" || (term.is_empty() && env.term_program.is_none()) {
        return Level::None;
    }

    // The one signal that is actually meant to carry this.
    if matches!(env.colorterm, Some("truecolor") | Some("24bit")) {
        return Level::TrueColor;
    }

    // terminfo's convention for a direct-colour entry (xterm-direct, and so on).
    // This previously mapped to 256, which was backwards: `-direct` is precisely
    // the name for 24-bit.
    if term.contains("direct") {
        return Level::TrueColor;
    }

    if let Some(program) = env.term_program {
        if TRUECOLOR_TERM_PROGRAMS.iter().any(|p| p == &program) {
            return Level::TrueColor;
        }
        if ANSI256_TERM_PROGRAMS.iter().any(|p| p == &program) {
            return Level::Ansi256;
        }
    }

    if TRUECOLOR_TERMS.iter().any(|t| term.contains(t)) {
        return Level::TrueColor;
    }

    // Inside tmux or screen, 24-bit only reaches the outer terminal if the
    // multiplexer was configured to pass it through, and nothing in the
    // environment says whether it was. Stay at 256: the gradient bands a little
    // and still reads, where guessing wrong produces garbage.
    if term.contains("256color") || term.starts_with("screen") || term.starts_with("tmux") {
        return Level::Ansi256;
    }

    Level::Ansi16
}

/// What the current terminal supports.
///
/// Deliberately checks stdout rather than stderr: the wordmark appears in
/// output a user reads, and `knaix ... | grep` must get clean bytes.
pub fn level() -> Level {
    let knaix_color = std::env::var("KNAIX_COLOR").ok();
    let no_color = std::env::var("NO_COLOR").ok();
    let clicolor_force = std::env::var("CLICOLOR_FORCE").ok();
    let colorterm = std::env::var("COLORTERM").ok();
    let term = std::env::var("TERM").ok();
    let term_program = std::env::var("TERM_PROGRAM").ok();

    level_for(&TermEnv {
        knaix_color: knaix_color.as_deref(),
        knaix_no_gradient: std::env::var_os("KNAIX_NO_GRADIENT").is_some(),
        no_color: no_color.as_deref(),
        clicolor_force: clicolor_force.as_deref(),
        colorterm: colorterm.as_deref(),
        term: term.as_deref(),
        term_program: term_program.as_deref(),
        is_tty: std::io::stdout().is_terminal(),
    })
}

const RESET: &str = "\x1b[0m";
const CYAN: &str = "\x1b[36m";

/// Paint `text` across the gradient, one colour per character.
fn gradient(text: &str, lvl: Level) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return String::new();
    }

    match lvl {
        Level::None => text.to_string(),
        Level::Ansi16 => format!("{CYAN}{text}{RESET}"),
        Level::Ansi256 | Level::TrueColor => {
            // A single character sits at the start of the ramp rather than
            // dividing by zero.
            let last = chars.len().saturating_sub(1);
            let mut out = String::with_capacity(text.len() + chars.len() * 20);
            for (i, ch) in chars.iter().enumerate() {
                let t = if last == 0 {
                    0.0
                } else {
                    i as f32 / last as f32
                };
                let (r, g, b) = sample(t);
                if lvl == Level::TrueColor {
                    out.push_str(&format!("\x1b[38;2;{r};{g};{b}m"));
                } else {
                    out.push_str(&format!("\x1b[38;5;{}m", to_ansi256(r, g, b)));
                }
                out.push(*ch);
            }
            out.push_str(RESET);
            out
        }
    }
}

/// The word the gradient is for.
pub const WORDMARK: &str = "knaix";

/// The `knaix` wordmark in the brand gradient.
pub fn wordmark() -> String {
    gradient(WORDMARK, level())
}

/// Sample the ramp once per letter of the wordmark.
fn ramp() -> Vec<(u8, u8, u8)> {
    let n = WORDMARK.chars().count();
    let last = n.saturating_sub(1);
    (0..n)
        .map(|i| {
            sample(if last == 0 {
                0.0
            } else {
                i as f32 / last as f32
            })
        })
        .collect()
}

/// The wordmark colours as `#RRGGBB`, for a shell integration to embed.
///
/// Exposed so the shell snippet is generated from this stop list rather than
/// carrying its own copy. A copy in a dotfile is a copy that goes stale the
/// first time the brand gradient is retuned, and nobody ever goes back for it.
pub fn ramp_hex() -> Vec<String> {
    ramp()
        .into_iter()
        .map(|(r, g, b)| format!("#{r:02X}{g:02X}{b:02X}"))
        .collect()
}

/// The wordmark colours as xterm-256 indices, for terminals without 24-bit.
pub fn ramp_256() -> Vec<u8> {
    ramp()
        .into_iter()
        .map(|(r, g, b)| to_ansi256(r, g, b))
        .collect()
}

/// A command hint: the wordmark in the gradient, the rest in cyan.
///
/// This is the shape both sites render, and the reason the arguments stay cyan
/// rather than joining the gradient: the gradient marks the product, not the
/// command.
pub fn cmd(rest: &str) -> String {
    let lvl = level();
    let mark = gradient("knaix", lvl);
    if rest.is_empty() {
        return mark;
    }
    match lvl {
        Level::None => format!("knaix {rest}"),
        _ => format!("{mark} {CYAN}{rest}{RESET}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The five letters, pinned. If the brand stops move these move with them,
    /// and this test is the record of what they were.
    #[test]
    fn samples_the_five_letters() {
        let expected = [
            (253, 164, 60), // k
            (239, 87, 138), // n
            (166, 85, 217), // a
            (93, 133, 249), // i
            (34, 211, 238), // x
        ];
        for (i, want) in expected.iter().enumerate() {
            let t = i as f32 / 4.0;
            assert_eq!(sample(t), *want, "letter {i} at t={t}");
        }
    }

    #[test]
    fn endpoints_are_the_outer_stops() {
        assert_eq!(sample(0.0), (253, 164, 60));
        assert_eq!(sample(1.0), (34, 211, 238));
        // Out of range clamps rather than panicking.
        assert_eq!(sample(-1.0), (253, 164, 60));
        assert_eq!(sample(2.0), (34, 211, 238));
    }

    #[test]
    fn none_level_emits_no_escapes() {
        let out = gradient("knaix", Level::None);
        assert_eq!(out, "knaix");
        assert!(!out.contains('\x1b'));
    }

    #[test]
    fn ansi16_falls_back_to_one_colour() {
        let out = gradient("knaix", Level::Ansi16);
        // One colour open, one reset, and no per-letter sequences.
        assert_eq!(out.matches('\x1b').count(), 2);
    }

    #[test]
    fn truecolor_paints_every_letter() {
        let out = gradient("knaix", Level::TrueColor);
        assert_eq!(out.matches("\x1b[38;2;").count(), 5);
        assert!(out.ends_with(RESET));
        assert!(out.contains("\x1b[38;2;253;164;60mk"));
        assert!(out.contains("\x1b[38;2;34;211;238mx"));
    }

    #[test]
    fn ansi256_paints_every_letter_from_the_cube() {
        let out = gradient("knaix", Level::Ansi256);
        assert_eq!(out.matches("\x1b[38;5;").count(), 5);
        assert!(out.ends_with(RESET));
    }

    #[test]
    fn cube_maps_to_the_nearest_level() {
        // Pure white and pure black are the cube's corners.
        assert_eq!(to_ansi256(255, 255, 255), 16 + 36 * 5 + 6 * 5 + 5);
        assert_eq!(to_ansi256(0, 0, 0), 16);
    }

    #[test]
    fn empty_and_single_character_do_not_panic() {
        assert_eq!(gradient("", Level::TrueColor), "");
        let one = gradient("k", Level::TrueColor);
        assert!(one.contains("\x1b[38;2;253;164;60mk"));
    }

    /// A terminal that is a TTY and says nothing else about itself.
    fn env() -> TermEnv<'static> {
        TermEnv {
            knaix_color: None,
            knaix_no_gradient: false,
            no_color: None,
            clicolor_force: None,
            colorterm: None,
            term: Some("xterm"),
            term_program: None,
            is_tty: true,
        }
    }

    #[test]
    fn not_a_tty_emits_nothing() {
        let e = TermEnv {
            is_tty: false,
            ..env()
        };
        assert_eq!(level_for(&e), Level::None);
    }

    #[test]
    fn clicolor_force_keeps_colour_through_a_pipe() {
        let e = TermEnv {
            is_tty: false,
            clicolor_force: Some("1"),
            colorterm: Some("truecolor"),
            ..env()
        };
        assert_eq!(level_for(&e), Level::TrueColor);
        // Zero is "no force", not "force off".
        let e = TermEnv {
            is_tty: false,
            clicolor_force: Some("0"),
            ..env()
        };
        assert_eq!(level_for(&e), Level::None);
    }

    #[test]
    fn empty_no_color_does_not_opt_out() {
        // The spec is "present and not an empty string".
        let e = TermEnv {
            no_color: Some(""),
            colorterm: Some("truecolor"),
            ..env()
        };
        assert_eq!(level_for(&e), Level::TrueColor);
        let e = TermEnv {
            no_color: Some("1"),
            colorterm: Some("truecolor"),
            ..env()
        };
        assert_eq!(level_for(&e), Level::None);
    }

    #[test]
    fn colorterm_is_honoured() {
        for v in ["truecolor", "24bit"] {
            let e = TermEnv {
                colorterm: Some(v),
                ..env()
            };
            assert_eq!(level_for(&e), Level::TrueColor, "COLORTERM={v}");
        }
        // A terminal name in COLORTERM does not mean 24-bit.
        let e = TermEnv {
            colorterm: Some("gnome-terminal"),
            ..env()
        };
        assert_eq!(level_for(&e), Level::Ansi16);
    }

    #[test]
    fn direct_terminfo_entries_are_truecolor() {
        // This is the branch that was backwards: `-direct` means 24-bit.
        let e = TermEnv {
            term: Some("xterm-direct"),
            ..env()
        };
        assert_eq!(level_for(&e), Level::TrueColor);
    }

    #[test]
    fn known_terminals_are_recognised_without_colorterm() {
        for p in TRUECOLOR_TERM_PROGRAMS {
            let e = TermEnv {
                term_program: Some(p),
                term: Some("xterm-256color"),
                ..env()
            };
            assert_eq!(level_for(&e), Level::TrueColor, "TERM_PROGRAM={p}");
        }
        for t in ["xterm-kitty", "alacritty", "xterm-ghostty", "foot"] {
            let e = TermEnv {
                term: Some(t),
                ..env()
            };
            assert_eq!(level_for(&e), Level::TrueColor, "TERM={t}");
        }
    }

    #[test]
    fn apple_terminal_stops_at_256() {
        // It approximates a 24-bit sequence to its own palette rather than
        // ignoring it, so "I can see distinct colours" does not prove 24-bit.
        // Emit what it actually renders.
        let e = TermEnv {
            term_program: Some("Apple_Terminal"),
            term: Some("xterm-256color"),
            ..env()
        };
        assert_eq!(level_for(&e), Level::Ansi256);
    }

    #[test]
    fn multiplexers_stay_conservative() {
        for t in ["screen-256color", "tmux-256color", "screen"] {
            let e = TermEnv {
                term: Some(t),
                ..env()
            };
            assert_eq!(level_for(&e), Level::Ansi256, "TERM={t}");
        }
        // Unless the multiplexer was configured to pass 24-bit through and says so.
        let e = TermEnv {
            term: Some("tmux-256color"),
            colorterm: Some("truecolor"),
            ..env()
        };
        assert_eq!(level_for(&e), Level::TrueColor);
    }

    #[test]
    fn dumb_and_unset_terminals_emit_nothing() {
        assert_eq!(
            level_for(&TermEnv {
                term: Some("dumb"),
                ..env()
            }),
            Level::None
        );
        assert_eq!(
            level_for(&TermEnv {
                term: None,
                ..env()
            }),
            Level::None
        );
    }

    #[test]
    fn explicit_override_beats_every_heuristic() {
        for (v, want) in [
            ("none", Level::None),
            ("16", Level::Ansi16),
            ("256", Level::Ansi256),
            ("truecolor", Level::TrueColor),
            ("TrueColor", Level::TrueColor),
        ] {
            let e = TermEnv {
                knaix_color: Some(v),
                term: Some("dumb"),
                ..env()
            };
            assert_eq!(level_for(&e), want, "KNAIX_COLOR={v}");
        }
        // A typo falls through to detection rather than costing someone output.
        let e = TermEnv {
            knaix_color: Some("nonsense"),
            colorterm: Some("truecolor"),
            ..env()
        };
        assert_eq!(level_for(&e), Level::TrueColor);
    }

    #[test]
    fn no_gradient_keeps_colour_but_drops_the_ramp() {
        let e = TermEnv {
            knaix_no_gradient: true,
            colorterm: Some("truecolor"),
            ..env()
        };
        assert_eq!(level_for(&e), Level::Ansi16);
    }

    #[test]
    fn plain_command_hint_is_plain() {
        // The whole hint, wordmark included, must survive with no escapes.
        assert_eq!(
            {
                let lvl = Level::None;
                let mark = gradient("knaix", lvl);
                format!("{mark} local up")
            },
            "knaix local up"
        );
    }
}
