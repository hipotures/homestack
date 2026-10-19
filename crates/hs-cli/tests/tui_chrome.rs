//! The border grammar of the TUI (phase 13b, step 1; mockups v2): controls
//! in the borders framed `┐title┌` on top and `┘label└` at the bottom, the
//! hotkey letter in the hotkey colour, counters, the scrollbar; and which
//! panels fit the window.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use hs_cli::tui::chrome::{Control, Scrolls, Sides, frame, scrollbar};
use hs_cli::tui::layout::{Shown, arrange};
use hs_cli::tui::theme::Palette;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

fn line(buf: &Buffer, y: u16) -> String {
    (buf.area.x..buf.area.x + buf.area.width)
        .map(|x| buf[(x, y)].symbol().to_owned())
        .collect()
}

fn find(buf: &Buffer, y: u16, text: &str) -> u16 {
    let row = line(buf, y);
    let at = row.find(text).unwrap_or_else(|| panic!("{text:?} not in {row:?}"));
    u16::try_from(row[..at].chars().count()).unwrap()
}

fn palette() -> Palette {
    Palette::named("HomeStack", false).unwrap()
}

#[test]
fn titles_are_framed_in_the_top_border_and_buttons_in_the_bottom_border() {
    let mut buf = Buffer::empty(Rect::new(0, 0, 50, 5));
    let area = buf.area;
    let sides = Sides {
        top_left: vec![Control::panel(1, "workspaces"), Control::key("&filter")],
        top_right: vec![Control::key("&new")],
        bottom_left: vec![Control::key("info ↵"), Control::key("&refresh")],
        bottom_right: vec![Control::plain("1/12")],
        focused: false,
    };
    let inner = frame(&mut buf, area, &palette(), &sides);
    assert_eq!(inner, Rect::new(1, 1, 48, 3));
    assert_eq!(
        line(&buf, 0),
        format!("╭─┐¹workspaces┌┐filter┌{}┐new┌─╮", "─".repeat(50 - 23 - 7))
    );
    assert_eq!(
        line(&buf, 4),
        format!("╰─┘info ↵└┘refresh└{}┘1/12└─╯", "─".repeat(50 - 19 - 8))
    );
    assert_eq!(line(&buf, 2), format!("│{}│", " ".repeat(48)));
}

#[test]
fn the_hotkey_letter_and_the_panel_digit_have_the_hotkey_colour() {
    let palette = palette();
    let mut buf = Buffer::empty(Rect::new(0, 0, 50, 5));
    let area = buf.area;
    let sides = Sides {
        top_left: vec![Control::panel(1, "workspaces"), Control::key("&filter")],
        bottom_left: vec![Control::key("re&size")],
        ..Sides::default()
    };
    frame(&mut buf, area, &palette, &sides);
    let digit = find(&buf, 0, "¹");
    assert_eq!(buf[(digit, 0)].fg, palette.hotkey);
    assert_eq!(buf[(digit + 1, 0)].fg, palette.foreground);
    let f = find(&buf, 0, "filter");
    assert_eq!(buf[(f, 0)].fg, palette.hotkey);
    assert_eq!(buf[(f + 1, 0)].fg, palette.foreground);
    // The key may be inside the word: re*s*ize.
    let r = find(&buf, 4, "resize");
    assert_eq!(buf[(r, 4)].fg, palette.foreground);
    assert_eq!(buf[(r + 2, 4)].fg, palette.hotkey);
    // The frame characters are border-coloured.
    assert_eq!(buf[(f - 1, 0)].fg, palette.border);
    assert_eq!(buf[(0, 0)].fg, palette.border);
}

#[test]
fn enter_can_highlight_both_e_letters_and_keep_the_enter_key_for_clicks() {
    use crossterm::event::KeyCode;
    use hs_cli::tui::chrome::control_key;

    let palette = palette();
    let control = Control {
        code: Some(KeyCode::Enter),
        ..Control::key("&ent&er")
    };
    assert_eq!(control.text, "enter");
    assert_eq!(control.key_char(), Some('e'));
    assert_eq!(control_key(&control), Some(KeyCode::Enter));
    let mut buf = Buffer::empty(Rect::new(0, 0, 20, 4));
    let area = buf.area;
    frame(
        &mut buf,
        area,
        &palette,
        &Sides {
            bottom_left: vec![control],
            ..Sides::default()
        },
    );
    let x = find(&buf, 3, "enter");
    for offset in 0..5 {
        let expected = if offset == 0 || offset == 3 {
            palette.hotkey
        } else {
            palette.foreground
        };
        assert_eq!(buf[(x + offset, 3)].fg, expected);
    }
}

#[test]
fn a_muted_control_is_muted_also_on_its_key_and_a_raised_one_has_the_focus_colour() {
    let palette = palette();
    let mut buf = Buffer::empty(Rect::new(0, 0, 50, 5));
    let area = buf.area;
    let sides = Sides {
        top_left: vec![Control::plain("install"), Control::plain("settings").raised()],
        bottom_left: vec![Control::key("&destroy").muted()],
        ..Sides::default()
    };
    frame(&mut buf, area, &palette, &sides);
    let d = find(&buf, 4, "destroy");
    assert_eq!(buf[(d, 4)].fg, palette.muted);
    assert_eq!(buf[(d + 1, 4)].fg, palette.muted);
    let s = find(&buf, 0, "settings");
    assert_eq!(buf[(s, 0)].fg, palette.focus);
}

#[test]
fn a_focused_panel_has_the_focus_colour_on_its_border() {
    let palette = palette();
    let mut buf = Buffer::empty(Rect::new(0, 0, 20, 4));
    let area = buf.area;
    let sides = Sides {
        focused: true,
        ..Sides::default()
    };
    frame(&mut buf, area, &palette, &sides);
    assert_eq!(buf[(0, 0)].fg, palette.focus);
    assert_eq!(buf[(19, 2)].fg, palette.focus);
}

#[test]
fn controls_that_do_not_fit_are_left_out_from_the_end_and_the_counter_stays() {
    let mut buf = Buffer::empty(Rect::new(0, 0, 30, 4));
    let area = buf.area;
    let sides = Sides {
        bottom_left: vec![
            Control::key("info ↵"),
            Control::key("&ssh"),
            Control::key("&refresh"),
            Control::key("&destroy"),
        ],
        bottom_right: vec![Control::plain("1/12")],
        ..Sides::default()
    };
    frame(&mut buf, area, &palette(), &sides);
    let bottom = line(&buf, 3);
    assert!(bottom.contains("╰─┘info ↵└┘ssh└"), "{bottom}");
    assert!(!bottom.contains("destroy"), "{bottom}");
    assert!(bottom.ends_with("┘1/12└─╯"), "{bottom}");
    assert_eq!(bottom.chars().count(), 30);
}

#[test]
fn the_scrollbar_replaces_the_right_border_only_when_the_list_is_longer() {
    let palette = palette();
    let mut buf = Buffer::empty(Rect::new(0, 0, 20, 12));
    let area = buf.area;
    frame(&mut buf, area, &palette, &Sides::default());
    scrollbar(&mut buf, area, &palette, Scrolls::Log, 0, 10, 10);
    assert!((1..11).all(|y| buf[(19, y)].symbol() == "│"));
    scrollbar(&mut buf, area, &palette, Scrolls::Log, 0, 10, 20);
    assert_eq!(buf[(19, 1)].symbol(), "↑");
    assert_eq!(buf[(19, 10)].symbol(), "↓");
    let thumb: Vec<u16> = (2..10).filter(|y| buf[(19, *y)].symbol() == "█").collect();
    assert_eq!(thumb.first(), Some(&2), "the thumb starts at the top: {thumb:?}");
    assert_eq!(thumb.len(), 4, "half the list is visible: {thumb:?}");
    scrollbar(&mut buf, area, &palette, Scrolls::Log, 10, 10, 20);
    let thumb: Vec<u16> = (2..10).filter(|y| buf[(19, *y)].symbol() == "█").collect();
    assert_eq!(
        thumb.last(),
        Some(&9),
        "at the end the thumb is at the bottom: {thumb:?}"
    );
}

#[test]
fn the_scrollbar_s_thumb_is_held_and_dragged_and_its_arrows_and_track_pressed() {
    use hs_cli::tui::chrome::{Hit, clear_hits, hit_at};
    let palette = palette();
    let mut buf = Buffer::empty(Rect::new(0, 0, 20, 12));
    let area = buf.area;
    clear_hits();
    scrollbar(&mut buf, area, &palette, Scrolls::Log, 0, 10, 20);
    let Some(Hit::Bar(bar)) = hit_at(19, 2) else {
        panic!("no scrollbar at its thumb");
    };
    assert_eq!(bar.scrolls, Scrolls::Log);
    // The thumb held at its second row moves nothing; dragged, it follows.
    assert_eq!(bar.press(3), (0, Some(1)));
    assert_eq!(bar.drag(9, 1), 10, "dragged to the bottom: the end");
    assert_eq!(bar.drag(0, 1), 0, "dragged above the bar: the start");
    assert_eq!(bar.drag(5, 1), 5, "halfway");
    // The arrows move a line; the track beside the thumb puts its middle there.
    assert_eq!(bar.press(1), (0, None));
    assert_eq!(bar.press(10), (1, None));
    assert_eq!(bar.press(9).0, 10);
    // A long list: the thumb stays shorter than the track, so it can move.
    clear_hits();
    scrollbar(&mut buf, area, &palette, Scrolls::Log, 0, 11, 12);
    let thumb = (2..10).filter(|y| buf[(19, *y)].symbol() == "█").count();
    assert_eq!(thumb, 7);
}

#[test]
fn below_80_by_24_nothing_but_the_size_notice_fits() {
    for (width, height) in [(79, 24), (80, 23), (40, 10)] {
        assert!(arrange(Rect::new(0, 0, width, height), Shown::all()).too_small);
    }
    assert!(!arrange(Rect::new(0, 0, 80, 24), Shown::all()).too_small);
}

#[test]
fn at_80_by_24_cluster_and_workspaces_fit_and_storage_and_log_do_not() {
    let screen = arrange(Rect::new(0, 0, 80, 24), Shown::all());
    let cluster = screen.cluster.unwrap();
    let workspaces = screen.workspaces.unwrap();
    assert_eq!(cluster, Rect::new(0, 0, 80, 3));
    assert_eq!(workspaces, Rect::new(0, 3, 80, 21));
    assert!(screen.storage.is_none() && screen.log.is_none());
}

#[test]
fn at_120_by_36_storage_and_log_lie_side_by_side_under_the_whole_width_list() {
    let screen = arrange(Rect::new(0, 0, 120, 36), Shown::all());
    let cluster = screen.cluster.unwrap();
    let workspaces = screen.workspaces.unwrap();
    let storage = screen.storage.unwrap();
    let log = screen.log.unwrap();
    assert_eq!(cluster, Rect::new(0, 0, 120, 6));
    assert_eq!(workspaces, Rect::new(0, 6, 120, 22));
    assert_eq!(log, Rect::new(0, 28, 84, 8));
    assert_eq!(storage, Rect::new(84, 28, 36, 8));
}

#[test]
fn at_160_by_45_the_storage_column_is_wider_and_the_bottom_taller() {
    let screen = arrange(Rect::new(0, 0, 160, 45), Shown::all());
    assert_eq!(screen.cluster.unwrap().height, 8);
    assert_eq!(screen.workspaces.unwrap().width, 160);
    assert_eq!(screen.storage.unwrap(), Rect::new(112, 35, 48, 10));
    assert_eq!(screen.log.unwrap(), Rect::new(0, 35, 112, 10));
}

#[test]
fn a_hidden_panel_gives_its_room_to_the_others() {
    let mut shown = Shown::all();
    shown.toggle(2);
    let screen = arrange(Rect::new(0, 0, 120, 36), shown);
    assert!(screen.cluster.is_none());
    assert_eq!(screen.workspaces.unwrap(), Rect::new(0, 0, 120, 28));
    // The log alone takes the whole width under the list.
    shown.toggle(3);
    let screen = arrange(Rect::new(0, 0, 120, 36), shown);
    assert_eq!(screen.log.unwrap(), Rect::new(0, 28, 120, 8));
    shown.toggle(4);
    let screen = arrange(Rect::new(0, 0, 120, 36), shown);
    assert_eq!(screen.workspaces.unwrap(), Rect::new(0, 0, 120, 36));
}

#[test]
fn the_last_shown_panel_cannot_be_hidden() {
    let mut shown = Shown::all();
    for digit in [2, 3, 4, 1] {
        shown.toggle(digit);
    }
    assert!(shown.workspaces);
    assert_eq!(
        arrange(Rect::new(0, 0, 80, 24), shown).workspaces.unwrap(),
        Rect::new(0, 0, 80, 24)
    );
}

#[test]
fn a_tall_narrow_window_puts_storage_and_log_side_by_side_under_the_list() {
    let screen = arrange(Rect::new(0, 0, 80, 48), Shown::all());
    let workspaces = screen.workspaces.unwrap();
    let storage = screen.storage.unwrap();
    let log = screen.log.unwrap();
    assert_eq!(workspaces.width, 80);
    assert_eq!(log, Rect::new(0, 38, 44, 10));
    assert_eq!(storage, Rect::new(44, 38, 36, 10));
    assert_eq!(workspaces.y + workspaces.height, 38);
}
