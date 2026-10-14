//! The choice lists of `homestack install` (ratatui): the keys, the
//! rendering into a test buffer, and the numbered fallback used where there
//! is no terminal to draw on.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::Recorded;
use hs_cli::picker::{Item, Key, Outcome, Picker};
use hs_cli::system::Console;

fn items(count: usize) -> Vec<Item> {
    (0..count)
        .map(|i| Item {
            value: format!("storage-{i}"),
            detail: format!("lvmthin  pve{i}"),
        })
        .collect()
}

fn keys(picker: &mut Picker, keys: &[Key]) {
    for key in keys {
        picker.handle(*key);
    }
}

#[test]
fn a_single_choice_is_the_item_under_the_cursor_when_enter_is_pressed() {
    let mut picker = Picker::new("Snippets", false, items(3));
    assert_eq!(picker.outcome(), None);
    keys(&mut picker, &[Key::Down, Key::Enter]);
    assert_eq!(picker.outcome(), Some(Outcome::Chosen(vec![1])));

    let mut picker = Picker::new("Snippets", false, items(3));
    keys(&mut picker, &[Key::Up, Key::Enter]);
    assert_eq!(
        picker.outcome(),
        Some(Outcome::Chosen(vec![0])),
        "the cursor stops at the top"
    );

    let mut picker = Picker::new("Snippets", false, items(3));
    keys(&mut picker, &[Key::End, Key::Down, Key::Enter]);
    assert_eq!(picker.outcome(), Some(Outcome::Chosen(vec![2])), "and at the bottom");

    let mut picker = Picker::new("Snippets", false, items(3));
    keys(&mut picker, &[Key::End, Key::Home, Key::Space, Key::Enter]);
    assert_eq!(
        picker.outcome(),
        Some(Outcome::Chosen(vec![0])),
        "space does nothing in a single choice"
    );
}

#[test]
fn a_multiple_choice_is_ticked_with_space_and_confirmed_with_enter() {
    let mut picker = Picker::new("Storages", true, items(4));
    keys(&mut picker, &[Key::Space, Key::Down, Key::Down, Key::Space, Key::Enter]);
    assert_eq!(picker.outcome(), Some(Outcome::Chosen(vec![0, 2])));

    // Ticked twice is not ticked.
    let mut picker = Picker::new("Storages", true, items(4));
    keys(
        &mut picker,
        &[Key::Space, Key::Space, Key::Down, Key::Space, Key::Enter],
    );
    assert_eq!(picker.outcome(), Some(Outcome::Chosen(vec![1])));

    // Nothing ticked: Enter takes the item under the cursor.
    let mut picker = Picker::new("Storages", true, items(4));
    keys(&mut picker, &[Key::Down, Key::Enter]);
    assert_eq!(picker.outcome(), Some(Outcome::Chosen(vec![1])));
}

#[test]
fn escape_cancels() {
    let mut picker = Picker::new("Storages", true, items(4));
    keys(&mut picker, &[Key::Space, Key::Cancel]);
    assert_eq!(picker.outcome(), Some(Outcome::Cancelled));
}

#[test]
fn each_choice_is_drawn_on_its_own_line_with_the_cursor_and_ticks_marked() {
    let mut picker = Picker::new("--storages: where the workspace disks go", true, items(3));
    keys(&mut picker, &[Key::Space, Key::Down]);
    let screen = picker.render_to_string(70, 14);
    let lines: Vec<&str> = screen.lines().collect();
    assert!(screen.contains("--storages: where the workspace disks go"), "{screen}");
    let row = |needle: &str| lines.iter().find(|l| l.contains(needle)).copied().unwrap_or_default();
    assert!(
        row("storage-0").contains("[x]") && row("storage-0").contains("lvmthin"),
        "{screen}"
    );
    assert!(
        row("storage-1").contains("[ ]") && row("storage-1").contains('>'),
        "the cursor is on 1:\n{screen}"
    );
    assert!(!row("storage-2").contains('>'), "{screen}");
    assert!(
        screen.contains("space") && screen.contains("enter"),
        "the keys are named:\n{screen}"
    );
    // A single choice has round marks instead of boxes.
    let single = Picker::new("Snippets", false, items(2)).render_to_string(70, 10);
    assert!(
        single.contains("( )") || single.contains("(•)") || single.contains("(*)"),
        "{single}"
    );
    assert!(!single.contains("[x]"), "{single}");
}

#[test]
fn a_long_list_scrolls_to_keep_the_cursor_in_view() {
    let mut picker = Picker::new("Many", false, items(40));
    for _ in 0..30 {
        picker.handle(Key::Down);
    }
    let screen = picker.render_to_string(60, 12);
    assert!(
        screen.contains("storage-30"),
        "the cursor row is not visible:\n{screen}"
    );
    assert!(!screen.contains("storage-0 "), "{screen}");
}

#[test]
fn the_numbered_fallback_asks_until_the_answer_is_valid() {
    let choices = [
        ("local-lvm", "lvmthin  pve2"),
        ("local-zfs", "zfspool  pve1"),
        ("vm-ssd-a", "lvmthin  pve1"),
    ]
    .map(|(v, d)| (v.to_owned(), d.to_owned()));
    let mut console = Recorded::answering(&["x", "9", "2"]);
    assert_eq!(console.pick("Snippets", false, &choices), Some(vec![1]));
    let shown = console.out();
    assert!(
        shown.contains("1) local-lvm") && shown.contains("3) vm-ssd-a"),
        "{shown}"
    );
    assert!(shown.contains("lvmthin  pve2"), "{shown}");

    let mut console = Recorded::answering(&["1, 3"]);
    assert_eq!(console.pick("Storages", true, &choices), Some(vec![0, 2]));
    // A single choice takes one number only.
    let mut console = Recorded::answering(&["1,2", "3"]);
    assert_eq!(console.pick("Snippets", false, &choices), Some(vec![2]));
    // End of input is a cancel.
    let mut console = Recorded::answering(&[]);
    assert_eq!(console.pick("Snippets", false, &choices), None);
}

/// The title of the frame starts with the two hyphens of an option name; it
/// must not run into the corner and look like part of the border.
#[test]
fn the_title_is_set_off_from_the_frame_by_spaces() {
    let screen = Picker::new("--storages: where the disks go", true, items(2)).render_to_string(60, 8);
    let top = screen.lines().next().unwrap_or_default().to_owned();
    assert!(top.starts_with("\u{250c} --storages"), "{top}");
    let title_end = top.find("go").map(|at| at + 2).unwrap_or(0);
    assert!(
        top[title_end..].starts_with(' '),
        "the title runs into the border: {top}"
    );
}
