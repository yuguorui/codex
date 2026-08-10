//! Rainbow highlights for explicit dynamic-workflow trigger words in the composer.

use std::ops::Range;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use ratatui::style::Color;
use ratatui::style::Style;

use crate::terminal_palette::StdoutColorLevel;
use crate::terminal_palette::best_color_for_level;
use crate::terminal_palette::effective_stdout_color_level;

pub(super) const WORKFLOW_KEYWORD_FRAME_TICK: Duration = Duration::from_millis(50);

const KEYWORDS: [&str; 3] = ["workflow", "workflows", "ultracode"];
const SWEEP_PADDING: usize = 10;
const RAINBOW_COLORS: [(u8, u8, u8); 7] = [
    (235, 95, 87),
    (245, 139, 87),
    (250, 195, 95),
    (145, 200, 130),
    (130, 170, 220),
    (155, 130, 200),
    (200, 130, 180),
];
const RAINBOW_SHIMMER_COLORS: [(u8, u8, u8); 7] = [
    (250, 155, 147),
    (255, 185, 137),
    (255, 225, 155),
    (185, 230, 180),
    (180, 205, 240),
    (195, 180, 230),
    (230, 180, 210),
];
const ANSI_RAINBOW_COLORS: [Color; 7] = [
    Color::Red,
    Color::LightRed,
    Color::Yellow,
    Color::Green,
    Color::Cyan,
    Color::Blue,
    Color::Magenta,
];
const ANSI_RAINBOW_SHIMMER_COLORS: [Color; 7] = [
    Color::LightRed,
    Color::Yellow,
    Color::LightYellow,
    Color::LightGreen,
    Color::LightCyan,
    Color::LightBlue,
    Color::LightMagenta,
];

static ANIMATION_START: OnceLock<Instant> = OnceLock::new();

pub(super) fn workflow_keyword_highlights(
    text: &str,
    animations_enabled: bool,
) -> Vec<(Range<usize>, Style)> {
    let animation_frame = ANIMATION_START
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis()
        / WORKFLOW_KEYWORD_FRAME_TICK.as_millis();
    workflow_keyword_highlights_for_frame(
        text,
        animations_enabled,
        animation_frame,
        effective_stdout_color_level(),
    )
}

pub(super) fn workflow_keyword_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    for (start, _) in text.char_indices() {
        for keyword in KEYWORDS {
            let end = start.saturating_add(keyword.len());
            let Some(candidate) = text.get(start..end) else {
                continue;
            };
            if candidate.eq_ignore_ascii_case(keyword)
                && has_word_boundary_before(text, start)
                && has_word_boundary_after(text, end)
            {
                ranges.push(start..end);
            }
        }
    }
    ranges.sort_unstable_by_key(|range| range.start);
    ranges
}

pub(super) fn workflow_keyword_highlights_for_frame(
    text: &str,
    animations_enabled: bool,
    animation_frame: u128,
    color_level: StdoutColorLevel,
) -> Vec<(Range<usize>, Style)> {
    let ranges = workflow_keyword_ranges(text);
    let sweep_bounds = ranges
        .first()
        .zip(ranges.last())
        .map(|(first, last)| first.start..last.end);
    let glimmer_index = if animations_enabled {
        sweep_bounds.map_or(isize::MIN, |bounds| {
            let cycle_length = bounds.len() + SWEEP_PADDING * 2;
            bounds.start as isize - SWEEP_PADDING as isize
                + (animation_frame % cycle_length as u128) as isize
        })
    } else {
        isize::MIN
    };
    let mut highlights = Vec::new();
    for range in ranges {
        for (char_index, offset) in (range.start..range.end).enumerate() {
            let use_shimmer =
                animations_enabled && (offset as isize - glimmer_index).unsigned_abs() <= 1;
            let color = rainbow_color(char_index, use_shimmer, color_level);
            highlights.push((offset..offset + 1, Style::default().fg(color)));
        }
    }
    highlights
}

fn has_word_boundary_before(text: &str, start: usize) -> bool {
    text[..start]
        .chars()
        .next_back()
        .is_none_or(|ch| !is_ascii_word_character(ch))
}

fn has_word_boundary_after(text: &str, end: usize) -> bool {
    text[end..]
        .chars()
        .next()
        .is_none_or(|ch| !is_ascii_word_character(ch))
}

fn is_ascii_word_character(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn rainbow_color(index: usize, shimmer: bool, color_level: StdoutColorLevel) -> Color {
    let palette_index = index % RAINBOW_COLORS.len();
    match color_level {
        StdoutColorLevel::TrueColor | StdoutColorLevel::Ansi256 => {
            let palette = if shimmer {
                RAINBOW_SHIMMER_COLORS
            } else {
                RAINBOW_COLORS
            };
            best_color_for_level(palette[palette_index], color_level)
        }
        StdoutColorLevel::Ansi16 | StdoutColorLevel::Unknown => {
            let palette = if shimmer {
                ANSI_RAINBOW_SHIMMER_COLORS
            } else {
                ANSI_RAINBOW_COLORS
            };
            palette[palette_index]
        }
    }
}
