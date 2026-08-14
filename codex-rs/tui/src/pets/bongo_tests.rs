use pretty_assertions::assert_eq;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

use super::*;

const TYPING_WINDOW: Duration = Duration::from_secs(1);

#[test]
fn bongo_rests_until_typing_starts_and_stops_at_the_idle_deadline() {
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
            cat.frame_at(now + TYPING_WINDOW),
        ],
        [BongoFrame::Left, BongoFrame::Right, BongoFrame::Resting]
    );
    assert_eq!(
        [
            cat.next_frame_delay_at(now),
            cat.next_frame_delay_at(now + TYPING_WINDOW),
        ],
        [Some(FRAME_DURATION), None]
    );
}

#[test]
fn continued_typing_extends_idle_without_restarting_the_beat() {
    let now = Instant::now();
    let mut cat = typing_cat(now, /*animations_enabled*/ true);
    let continued_at = now + FRAME_DURATION;

    cat.set_activity_at(typing_activity(), continued_at);

    assert_eq!(
        [
            cat.frame_at(continued_at),
            cat.frame_at(now + TYPING_WINDOW),
            cat.frame_at(continued_at + TYPING_WINDOW),
        ],
        [BongoFrame::Right, BongoFrame::Left, BongoFrame::Resting]
    );
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
fn bongo_frame_snapshots() {
    for (name, frame) in [
        ("bongo_left_frame", BongoFrame::Left),
        ("bongo_right_frame", BongoFrame::Right),
        ("bongo_resting_frame", BongoFrame::Resting),
    ] {
        assert_frame_snapshot(name, frame);
    }
}

#[test]
fn bongo_accents_use_terminal_colors() {
    let area = Rect::new(0, 0, BONGO_CAT_WIDTH, BONGO_CAT_HEIGHT);
    let mut buffer = Buffer::empty(area);
    render_frame(BongoFrame::Left, area, &mut buffer);

    assert_eq!(
        [buffer[(0, 0)].fg, buffer[(0, 4)].fg, buffer[(1, 4)].fg,],
        [Color::Magenta, Color::Cyan, Color::Magenta]
    );
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

fn assert_frame_snapshot(name: &str, frame: BongoFrame) {
    let mut terminal =
        Terminal::new(TestBackend::new(BONGO_CAT_WIDTH, BONGO_CAT_HEIGHT)).expect("terminal");
    terminal
        .draw(|terminal_frame| {
            render_frame(frame, terminal_frame.area(), terminal_frame.buffer_mut());
        })
        .expect("draw bongo frame");
    insta::assert_snapshot!(name, terminal.backend());
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
