//! Terminal-native Bongo Cat animation used when image protocols are unavailable.

use std::time::Duration;
use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

use crate::tui::FrameRequester;

use super::AmbientPetActivity;

pub(super) const BONGO_CAT_WIDTH: u16 = 32;
pub(super) const BONGO_CAT_HEIGHT: u16 = 10;
/// Terminal width below which the ASCII Bongo Cat is deliberately hidden.
///
/// Requiring 64 columns keeps the 32-column sprite plus its wrap gap from
/// crowding the conversation; on narrower terminals the reserved layout
/// space leaves almost no room for text, so we stop drawing (and reserving
/// space) there.
pub(super) const MIN_BONGO_TERMINAL_WIDTH: u16 = 64;
const FRAME_DURATION: Duration = Duration::from_millis(80);
const TYPING_FRAMES: [BongoFrame; 8] = [
    BongoFrame::LeftPress,
    BongoFrame::Resting,
    BongoFrame::RightPress,
    BongoFrame::Resting,
    BongoFrame::LeftPress,
    BongoFrame::Resting,
    BongoFrame::RightPress,
    BongoFrame::Resting,
];

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
        let was_typing = self.typing_idle_at(now).is_some();
        let next_activity = match activity {
            AmbientPetActivity::Typing { idle_in }
                if self.animations_enabled && !idle_in.is_zero() =>
            {
                now.checked_add(idle_in)
                    .map_or(BongoActivity::Resting, |idle_at| BongoActivity::Typing {
                        idle_at,
                    })
            }
            AmbientPetActivity::Idle | AmbientPetActivity::Typing { .. } => BongoActivity::Resting,
        };
        if !was_typing && matches!(next_activity, BongoActivity::Typing { .. }) {
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
        render_frame(self.frame_at(Instant::now()), draw_area, buf);
        true
    }

    pub(super) fn render_preview(area: Rect, buf: &mut Buffer) {
        if area.width < MIN_BONGO_TERMINAL_WIDTH || area.height < BONGO_CAT_HEIGHT {
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

    fn frame_at(&self, now: Instant) -> BongoFrame {
        if self.typing_idle_at(now).is_none() {
            return BongoFrame::Resting;
        }
        BongoFrame::at_elapsed(now.saturating_duration_since(self.animation_started_at))
    }

    fn next_frame_delay_at(&self, now: Instant) -> Option<Duration> {
        let idle_at = self.typing_idle_at(now)?;
        let elapsed = now.saturating_duration_since(self.animation_started_at);
        let frame_nanos = FRAME_DURATION.as_nanos();
        let elapsed_in_frame = elapsed.as_nanos() % frame_nanos;
        let frame_delay = Duration::from_nanos((frame_nanos - elapsed_in_frame) as u64);
        Some(frame_delay.min(idle_at.saturating_duration_since(now)))
    }

    fn typing_idle_at(&self, now: Instant) -> Option<Instant> {
        match self.activity {
            BongoActivity::Typing { idle_at } if now < idle_at => Some(idle_at),
            BongoActivity::Resting | BongoActivity::Typing { .. } => None,
        }
    }
}

fn draw_area(area: Rect, anchor_bottom_y: u16) -> Option<Rect> {
    let sprite_bottom_y = anchor_bottom_y.saturating_sub(/*rhs*/ 1);
    if area.width < MIN_BONGO_TERMINAL_WIDTH
        || sprite_bottom_y < area.y.saturating_add(BONGO_CAT_HEIGHT)
    {
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
    Resting,
    LeftPress,
    RightPress,
}

impl BongoFrame {
    fn at_elapsed(elapsed: Duration) -> Self {
        let index = (elapsed.as_nanos() / FRAME_DURATION.as_nanos()) % TYPING_FRAMES.len() as u128;
        TYPING_FRAMES[index as usize]
    }
}

fn render_frame(frame: BongoFrame, area: Rect, buf: &mut Buffer) {
    Clear.render(area, buf);
    Paragraph::new(frame_lines(frame)).render(area, buf);
}

fn frame_lines(frame: BongoFrame) -> Vec<Line<'static>> {
    let art = match frame {
        BongoFrame::Resting => RESTING_ART,
        BongoFrame::LeftPress => LEFT_PRESS_ART,
        BongoFrame::RightPress => RIGHT_PRESS_ART,
    };
    art.into_iter().map(Line::from).collect()
}

const RESTING_ART: [&str; BONGO_CAT_HEIGHT as usize] = [
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣀⠴⢄⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⣠⠤⠒⠁⠀⠈⠣⢤⣀⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⣀⣀⡀⢀⡠⠖⠉⠀⠀⠀⠀⠀⠀⠀⠀⠀⠉⠓⠒⠤⣄⠀⠀⠀⠀⢀⡀⠀",
    "⠀⠀⠀⡎⠁⢀⠙⢏⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠙⠲⠖⠋⠉⡇⠀",
    "⠀⠀⠀⡇⠁⠂⠀⠀⠀⠀⠀⠾⠆⢠⣀⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢰⠃⠀",
    "⠀⠀⠀⠷⠤⠤⣀⣀⡀⠀⠀⠀⠀⠀⠈⠙⠒⠀⠀⢠⣤⠀⡞⠉⢉⠳⡀⠀⠀⡞⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠉⠉⠓⠒⠦⠤⣄⣀⡀⠀⠀⠀⠀⠀⡇⠁⠂⠀⠀⠀⠀⠘⡄⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠉⠉⠓⠒⠒⠤⠧⣄⣀⣀⠀⠀⠀⠀⠹⡄",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠉⠉⠛⠒⠲⠤⢵",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
];

const LEFT_PRESS_ART: [&str; BONGO_CAT_HEIGHT as usize] = [
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣀⠴⢄⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⣠⠤⠒⠁⠀⠈⠣⢤⣀⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⣀⣀⡀⢀⡠⠖⠉⠀⠀⠀⠀⠀⠀⠀⠀⠀⠉⠓⠒⠤⣄⠀⠀⠀⠀⢀⡀⠀",
    "⠀⠀⠀⡎⠁⢀⠙⢏⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠙⠲⠖⠋⠉⡇⠀",
    "⠀⠀⠀⡇⠁⠂⠀⠀⠀⠀⠀⠾⠆⢠⣀⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢰⠃⠀",
    "⠀⠀⠀⠷⠤⠤⣀⣀⡀⠀⠀⠀⠀⠀⠈⠙⠒⠀⠀⢠⣤⠀⠀⠀⠀⠀⠀⠀⠀⡞⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠉⠉⠓⠒⠦⠤⣄⣀⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠘⡄⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠉⠉⠓⠒⡞⠁⠀⠀⠀⠀⠀⠀⠀⠀⠹⡄",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣀⡀⠣⠤⠤⠤⠖⠚⠉⠛⠒⠲⠤⢵",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠁⠐⠀⠀⠒⡄⠀⠀⠀⠀⠀⠀⠀⠀",
];

const RIGHT_PRESS_ART: [&str; BONGO_CAT_HEIGHT as usize] = [
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣀⠴⢄⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⣠⠤⠒⠁⠀⠈⠣⢤⣀⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⢀⡠⠖⠉⠀⠀⠀⠀⠀⠀⠀⠀⠀⠉⠓⠒⠤⣄⠀⠀⠀⠀⢀⡀⠀",
    "⠀⠀⠀⠀⠀⢀⡔⠋⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠙⠲⠖⠋⠉⡇⠀",
    "⠀⠀⠀⢀⡔⠉⠀⠀⠀⠀⠀⠾⠆⢠⣀⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢰⠃⠀",
    "⠀⠀⢠⠊⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⠙⠒⠀⠀⢠⣤⠀⡞⠉⢉⠳⡀⠀⠀⡞⠀⠀",
    "⢠⠤⠸⣄⣀⣀⣠⠤⠒⠊⠓⠒⠦⠤⣄⣀⡀⠀⠀⠀⠀⠀⡇⠁⠂⠀⠀⠀⠀⠘⡄⠀",
    "⠀⠐⠀⠀⢠⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠉⠉⠓⠒⠒⠤⠧⣄⣀⣀⠀⠀⠀⠀⠹⡄",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠉⠉⠛⠒⠲⠤⢵",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
];
