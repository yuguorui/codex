use pretty_assertions::assert_eq;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

use super::*;

const TYPING_WINDOW: Duration = Duration::from_millis(300);

#[test]
fn bongo_types_with_both_paws_and_stops_at_the_idle_deadline() {
    let now = Instant::now();
    let mut cat = BongoCat::new(
        FrameRequester::test_dummy(),
        /*animations_enabled*/ true,
    );

    assert_eq!(cat.next_frame_delay_at(now), None);
    cat.set_activity_at(typing_activity(), now);

    assert_eq!(
        [
            cat.frame_at(now),
            cat.frame_at(now + FRAME_DURATION),
            cat.frame_at(now + FRAME_DURATION * 2),
            cat.frame_at(now + FRAME_DURATION * 3),
            cat.frame_at(now + FRAME_DURATION * 7),
            cat.frame_at(now + TYPING_WINDOW),
        ],
        [
            BongoFrame::LeftPress,
            BongoFrame::LeftLow,
            BongoFrame::LeftMid,
            BongoFrame::LeftHigh,
            BongoFrame::Resting,
            BongoFrame::Resting,
        ]
    );
    assert_eq!(
        [
            cat.next_frame_delay_at(now),
            cat.next_frame_delay_at(now + Duration::from_millis(/*millis*/ 275)),
            cat.next_frame_delay_at(now + TYPING_WINDOW),
        ],
        [
            Some(FRAME_DURATION),
            Some(Duration::from_millis(/*millis*/ 5)),
            None,
        ]
    );
}

#[test]
fn continued_typing_extends_idle_without_restarting_the_sequence() {
    let now = Instant::now();
    let mut cat = typing_cat(now, /*animations_enabled*/ true);
    let continued_at = now + Duration::from_millis(/*millis*/ 250);

    cat.set_activity_at(typing_activity(), continued_at);

    assert_eq!(
        [
            cat.frame_at(now + Duration::from_millis(/*millis*/ 320)),
            cat.frame_at(now + Duration::from_millis(/*millis*/ 360)),
            cat.frame_at(continued_at + TYPING_WINDOW),
        ],
        [
            BongoFrame::RightPress,
            BongoFrame::RightLow,
            BongoFrame::Resting,
        ]
    );
}

#[test]
fn a_new_typing_burst_restarts_with_the_left_paw() {
    let now = Instant::now();
    let mut cat = typing_cat(now, /*animations_enabled*/ true);
    let restarted_at = now + TYPING_WINDOW + Duration::from_millis(/*millis*/ 1);

    cat.set_activity_at(typing_activity(), restarted_at);

    assert_eq!(cat.frame_at(restarted_at), BongoFrame::LeftPress);
}

#[test]
fn disabled_animations_keep_bongo_resting_while_typing() {
    let now = Instant::now();
    let cat = typing_cat(now, /*animations_enabled*/ false);

    assert_eq!(
        (cat.frame_at(now), cat.next_frame_delay_at(now)),
        (BongoFrame::Resting, None)
    );
}

#[test]
fn bongo_ascii_frame_snapshots() {
    let frames = [
        BongoFrame::Resting,
        BongoFrame::LeftPress,
        BongoFrame::LeftLow,
        BongoFrame::LeftMid,
        BongoFrame::LeftHigh,
        BongoFrame::RightPress,
        BongoFrame::RightLow,
        BongoFrame::RightMid,
        BongoFrame::RightHigh,
    ];
    let height = BONGO_CAT_HEIGHT * frames.len() as u16 + frames.len() as u16 - 1;
    let mut terminal = Terminal::new(TestBackend::new(BONGO_CAT_WIDTH, height)).expect("terminal");
    terminal
        .draw(|terminal_frame| {
            for (index, frame) in frames.into_iter().enumerate() {
                render_frame(
                    frame,
                    Rect::new(
                        /*x*/ 0,
                        index as u16 * (BONGO_CAT_HEIGHT + 1),
                        BONGO_CAT_WIDTH,
                        BONGO_CAT_HEIGHT,
                    ),
                    terminal_frame.buffer_mut(),
                );
            }
        })
        .expect("draw bongo frames");
    insta::assert_snapshot!("bongo_ascii_frames", terminal.backend());
}

#[test]
fn bongo_accents_use_terminal_colors() {
    let area = Rect::new(0, 0, BONGO_CAT_WIDTH, BONGO_CAT_HEIGHT);
    let mut buffer = Buffer::empty(area);
    render_frame(BongoFrame::LeftPress, area, &mut buffer);

    assert_eq!(
        [buffer[(4, 5)].fg, buffer[(0, 6)].fg],
        [Color::Magenta, Color::Cyan]
    );
}

#[test]
fn bongo_frames_keep_stable_dimensions() {
    for frame in TYPING_FRAMES {
        let lines = frame_lines(frame);
        assert_eq!(lines.len(), usize::from(BONGO_CAT_HEIGHT));
        assert!(
            lines
                .iter()
                .all(|line| line.width() == usize::from(BONGO_CAT_WIDTH))
        );
    }
}

#[test]
fn bongo_is_hidden_when_the_terminal_is_too_narrow() {
    let area = Rect::new(
        0,
        0,
        BONGO_CAT_WIDTH.saturating_sub(/*rhs*/ 1),
        BONGO_CAT_HEIGHT + 1,
    );

    assert_eq!(draw_area(area, area.bottom()), None);
}

fn typing_cat(now: Instant, animations_enabled: bool) -> BongoCat {
    let mut cat = BongoCat::new(FrameRequester::test_dummy(), animations_enabled);
    cat.set_activity_at(typing_activity(), now);
    cat
}

fn typing_activity() -> AmbientPetActivity {
    AmbientPetActivity::Typing {
        idle_in: TYPING_WINDOW,
    }
}
