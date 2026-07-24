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
    (0.00, (253, 164, 60)),  // #FDA43C
    (0.30, (236, 72, 153)),  // #EC4899
    (0.62, (124, 92, 255)),  // #7C5CFF
    (1.00, (34, 211, 238)),  // #22D3EE
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
            let local = if span <= f32::EPSILON { 0.0 } else { (t - t0) / span };
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
    (a as f32 + (b as f32 - a as f32) * t).round().clamp(0.0, 255.0) as u8
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

/// What the current terminal supports.
///
/// Deliberately checks stdout rather than stderr: the wordmark appears in
/// output a user reads, and `knaix ... | grep` must get clean bytes.
pub fn level() -> Level {
    // An explicit opt-out, for a terminal whose background the gradient reads
    // badly against. Honoured before anything else.
    if std::env::var_os("KNAIX_NO_GRADIENT").is_some() {
        return Level::Ansi16;
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return Level::None;
    }
    if !std::io::stdout().is_terminal() {
        return Level::None;
    }

    match std::env::var("COLORTERM").as_deref() {
        Ok("truecolor") | Ok("24bit") => return Level::TrueColor,
        _ => {}
    }

    match std::env::var("TERM") {
        Err(_) => Level::None,
        Ok(term) if term.is_empty() || term == "dumb" => Level::None,
        Ok(term) if term.contains("256color") || term.contains("direct") => Level::Ansi256,
        Ok(_) => Level::Ansi16,
    }
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
                let t = if last == 0 { 0.0 } else { i as f32 / last as f32 };
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

/// The `knaix` wordmark in the brand gradient.
pub fn wordmark() -> String {
    gradient("knaix", level())
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
            (253, 164, 60),  // k
            (239, 87, 138),  // n
            (166, 85, 217),  // a
            (93, 133, 249),  // i
            (34, 211, 238),  // x
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
