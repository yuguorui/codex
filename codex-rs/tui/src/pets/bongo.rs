//! Terminal-native Bongo Cat animation.

use std::time::Duration;
use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

use crate::tui::FrameRequester;

use super::AmbientPetActivity;

pub(super) const BONGO_CAT_WIDTH: u16 = 9;
pub(super) const BONGO_CAT_HEIGHT: u16 = 6;
const FRAME_DURATION: Duration = Duration::from_millis(240);

#[derive(Debug)]
pub(super) struct BongoCat {
    frame_requester: FrameRequester,
    animation_started_at: Instant,
    activity: BongoActivity,
    animations_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BongoActivity {
    Resting,
    Typing { idle_at: Instant },
}

impl BongoCat {
    pub(super) fn new(frame_requester: FrameRequester, animations_enabled: bool) -> Self {
        Self {
            frame_requester,
            animation_started_at: Instant::now(),
            activity: BongoActivity::Resting,
            animations_enabled,
        }
    }

    pub(super) fn set_activity_at(&mut self, activity: AmbientPetActivity, now: Instant) {
        let next_activity = match activity {
            AmbientPetActivity::Typing { idle_in }
                if self.animations_enabled && !idle_in.is_zero() =>
            {
                BongoActivity::Typing {
                    idle_at: now + idle_in,
                }
            }
            AmbientPetActivity::Idle | AmbientPetActivity::Typing { .. } => BongoActivity::Resting,
        };
        if matches!(self.activity, BongoActivity::Resting)
            && matches!(next_activity, BongoActivity::Typing { .. })
        {
            self.animation_started_at = now;
        }
        self.activity = next_activity;
    }

    pub(super) fn schedule_next_frame_at(&self, now: Instant) {
        if let Some(delay) = self.next_frame_delay_at(now) {
            self.frame_requester.schedule_frame_in(delay);
        }
    }

    pub(super) fn render(&self, area: Rect, anchor_bottom_y: u16, buf: &mut Buffer) -> bool {
        let Some(draw_area) = draw_area(area, anchor_bottom_y) else {
            return false;
        };
        self.render_in_area(draw_area, buf);
        true
    }

    pub(super) fn render_preview(area: Rect, buf: &mut Buffer) {
        if area.width < BONGO_CAT_WIDTH || area.height < BONGO_CAT_HEIGHT {
            return;
        }
        let draw_area = Rect::new(
            area.x + area.width.saturating_sub(BONGO_CAT_WIDTH) / 2,
            area.y + area.height.saturating_sub(BONGO_CAT_HEIGHT) / 2,
            BONGO_CAT_WIDTH,
            BONGO_CAT_HEIGHT,
        );
        render_frame(BongoFrame::Resting, draw_area, buf);
    }

    fn render_in_area(&self, area: Rect, buf: &mut Buffer) {
        let frame = self.frame_at(Instant::now());
        render_frame(frame, area, buf);
    }

    fn frame_at(&self, now: Instant) -> BongoFrame {
        match self.activity {
            BongoActivity::Typing { idle_at } if now < idle_at => {
                BongoFrame::at_elapsed(now.saturating_duration_since(self.animation_started_at))
            }
            BongoActivity::Resting | BongoActivity::Typing { .. } => BongoFrame::Resting,
        }
    }

    fn next_frame_delay_at(&self, now: Instant) -> Option<Duration> {
        let BongoActivity::Typing { idle_at } = self.activity else {
            return None;
        };
        idle_at
            .checked_duration_since(now)
            .filter(|remaining| !remaining.is_zero())
            .map(|remaining| remaining.min(FRAME_DURATION))
    }
}

fn draw_area(area: Rect, anchor_bottom_y: u16) -> Option<Rect> {
    let sprite_bottom_y = anchor_bottom_y.saturating_sub(/*rhs*/ 1);
    if area.width < BONGO_CAT_WIDTH || sprite_bottom_y < area.y.saturating_add(BONGO_CAT_HEIGHT) {
        return None;
    }

    Some(Rect::new(
        area.right().saturating_sub(BONGO_CAT_WIDTH),
        sprite_bottom_y.saturating_sub(BONGO_CAT_HEIGHT),
        BONGO_CAT_WIDTH,
        BONGO_CAT_HEIGHT,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BongoFrame {
    Left,
    Right,
    Resting,
}

impl BongoFrame {
    fn at_elapsed(elapsed: Duration) -> Self {
        if (elapsed.as_millis() / FRAME_DURATION.as_millis()).is_multiple_of(2) {
            Self::Left
        } else {
            Self::Right
        }
    }
}

fn render_frame(frame: BongoFrame, area: Rect, buf: &mut Buffer) {
    Clear.render(area, buf);
    Paragraph::new(frame_lines(frame)).render(area, buf);
}

fn frame_lines(frame: BongoFrame) -> Vec<Line<'static>> {
    let (header, drums) = match frame {
        BongoFrame::Left => (
            vec!["\u{266a}".magenta(), " /\\_/\\".into()],
            vec![
                "\u{256d}".cyan(),
                "\u{25cf}".magenta(),
                "\u{256e}   \u{256d}\u{2500}\u{256e}".cyan(),
            ],
        ),
        BongoFrame::Right => (
            vec!["  /\\_/\\ ".into(), "\u{266b}".magenta()],
            vec![
                "\u{256d}\u{2500}\u{256e}   \u{256d}".cyan(),
                "\u{25cf}".magenta(),
                "\u{256e}".cyan(),
            ],
        ),
        BongoFrame::Resting => (
            vec!["  /\\_/\\".into()],
            vec!["\u{256d}\u{2500}\u{256e}   \u{256d}\u{2500}\u{256e}".cyan()],
        ),
    };

    vec![
        Line::from(header),
        " ( o.o )".into(),
        " /|   |\\".into(),
        "\u{256d}\u{256f}|___|\u{2570}\u{256e}".into(),
        Line::from(drums),
        Line::from("\u{2570}\u{2500}\u{256f}   \u{2570}\u{2500}\u{256f}".cyan()),
    ]
}

#[cfg(test)]
#[path = "bongo_tests.rs"]
mod tests;
