use std::collections::HashSet;

use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

use super::ChatComposer;
use super::tests::new_test_composer;
use super::workflow_keyword::workflow_keyword_highlights_for_frame;
use super::workflow_keyword::workflow_keyword_ranges;
use crate::render::renderable::Renderable;
use crate::terminal_palette::StdoutColorLevel;
use crate::tui::FrameRequester;

#[test]
fn workflow_keyword_ranges_match_complete_opt_in_words() {
    let text =
        "workflow workflows preworkflow preworkflows Workflow, ultracode ultracoder 中文workflow。";
    let matches = workflow_keyword_ranges(text)
        .into_iter()
        .map(|range| text[range].to_string())
        .collect::<Vec<_>>();

    assert_eq!(
        matches,
        vec!["workflow", "workflows", "Workflow", "ultracode", "workflow"]
    );
}

#[test]
fn workflow_keyword_shimmer_moves_over_static_rainbow() {
    let static_highlights = workflow_keyword_highlights_for_frame(
        "workflow",
        /*animations_enabled*/ false,
        /*animation_frame*/ 10,
        StdoutColorLevel::TrueColor,
    );
    let animated_highlights = workflow_keyword_highlights_for_frame(
        "workflow",
        /*animations_enabled*/ true,
        /*animation_frame*/ 10,
        StdoutColorLevel::TrueColor,
    );

    assert_eq!(
        static_highlights
            .iter()
            .map(|(range, _)| range.clone())
            .collect::<Vec<_>>(),
        (0..8).map(|index| index..index + 1).collect::<Vec<_>>()
    );
    assert_ne!(animated_highlights[0].1, static_highlights[0].1);
    assert_ne!(animated_highlights[1].1, static_highlights[1].1);
    assert_eq!(animated_highlights[2..], static_highlights[2..]);
    assert!(
        static_highlights
            .iter()
            .filter_map(|(_, style)| style.fg)
            .collect::<HashSet<_>>()
            .len()
            > 1
    );
}

#[test]
fn workflow_keyword_shimmer_sweeps_across_all_matches() {
    let text = "workflow then workflows then ultracode";
    let static_highlights = workflow_keyword_highlights_for_frame(
        text,
        /*animations_enabled*/ false,
        /*animation_frame*/ 0,
        StdoutColorLevel::TrueColor,
    );
    let first_match_sweep = workflow_keyword_highlights_for_frame(
        text,
        /*animations_enabled*/ true,
        /*animation_frame*/ 10,
        StdoutColorLevel::TrueColor,
    );
    let plural_match_sweep = workflow_keyword_highlights_for_frame(
        text,
        /*animations_enabled*/ true,
        /*animation_frame*/ 24,
        StdoutColorLevel::TrueColor,
    );
    let third_match_sweep = workflow_keyword_highlights_for_frame(
        text,
        /*animations_enabled*/ true,
        /*animation_frame*/ 39,
        StdoutColorLevel::TrueColor,
    );

    assert_ne!(first_match_sweep[0..2], static_highlights[0..2]);
    assert_eq!(first_match_sweep[2..], static_highlights[2..]);
    assert_eq!(plural_match_sweep[0..8], static_highlights[0..8]);
    assert_ne!(plural_match_sweep[8..10], static_highlights[8..10]);
    assert_eq!(plural_match_sweep[10..], static_highlights[10..]);
    assert_eq!(third_match_sweep[0..17], static_highlights[0..17]);
    assert_ne!(third_match_sweep[17..19], static_highlights[17..19]);
    assert_eq!(third_match_sweep[19..], static_highlights[19..]);
}

#[test]
fn workflow_keyword_highlight_is_feature_gated() {
    let (mut composer, _rx) = new_test_composer();
    composer.config.animations_enabled = false;
    composer.set_text_content(
        "Run workflow, not workflows.".to_string(),
        Vec::new(),
        Vec::new(),
    );

    let disabled = render_composer(&composer);
    composer.set_workflow_command_enabled(/*enabled*/ true);
    let enabled = render_composer(&composer);

    assert_eq!(
        keyword_colors(&disabled, "workflows"),
        vec![Some(Color::Reset); 9]
    );
    assert_eq!(
        keyword_colors(&disabled, "workflow"),
        vec![Some(Color::Reset); 8]
    );
    assert!(
        keyword_colors(&enabled, "workflow")
            .into_iter()
            .all(|color| color != Some(Color::Reset))
    );
    assert!(
        keyword_colors(&enabled, "workflows")
            .into_iter()
            .all(|color| color != Some(Color::Reset))
    );
}

#[test]
fn workflow_keywords_render_with_rainbow_highlight_snapshot() {
    let (mut composer, _rx) = new_test_composer();
    composer.config.animations_enabled = false;
    composer.set_workflow_command_enabled(/*enabled*/ true);
    composer.set_text_content(
        "Run workflow, then ULTRACODE; workflows also shines.".to_string(),
        Vec::new(),
        Vec::new(),
    );

    let buffer = render_composer(&composer);
    let row = composer_row(&buffer);
    let mut colored = String::new();
    for x in 0..buffer.area.width {
        colored.push(if buffer[(x, 1)].style().fg != Some(Color::Reset) {
            '^'
        } else {
            ' '
        });
    }

    insta::assert_snapshot!(
        "workflow_keywords_render_with_rainbow_highlight",
        format!(
            "text:    {}\ncolored: {}",
            row.trim_end(),
            colored.trim_end()
        )
    );
}

#[test]
fn animated_workflow_keyword_requests_another_frame() {
    let (mut composer, _rx) = new_test_composer();
    let (frame_requester, mut frame_rx) = FrameRequester::test_channel();
    composer.set_frame_requester(frame_requester);
    composer.config.animations_enabled = true;
    composer.set_workflow_command_enabled(/*enabled*/ true);
    composer.set_text_content("workflow".to_string(), Vec::new(), Vec::new());

    let _buffer = render_composer(&composer);

    assert!(frame_rx.try_recv().is_ok());
}

fn render_composer(composer: &ChatComposer) -> Buffer {
    let area = Rect::new(0, 0, 72, 6);
    let mut buffer = Buffer::empty(area);
    composer.render(area, &mut buffer);
    buffer
}

fn composer_row(buffer: &Buffer) -> String {
    (0..buffer.area.width)
        .map(|x| buffer[(x, 1)].symbol().chars().next().unwrap_or(' '))
        .collect()
}

fn keyword_colors(buffer: &Buffer, keyword: &str) -> Vec<Option<Color>> {
    let row = composer_row(buffer);
    let byte_start = row.find(keyword).expect("keyword should be visible");
    let start = row[..byte_start].chars().count() as u16;
    (start..start + keyword.len() as u16)
        .map(|x| buffer[(x, 1)].style().fg)
        .collect()
}
