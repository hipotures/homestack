//! The choice lists of `homestack install` (ratatui): the keys, the
//! rendering into a test buffer, and the numbered fallback used where there
//! is no terminal to draw on.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::Recorded;
use hs_cli::picker::{Item, Key, Outcome, Picker};
use hs_cli::system::{Console, Entry};

fn items(count: usize) -> Vec<Item> {
    (0..count)
        .map(|i| Item {
            value: format!("storage-{i}"),
            detail: format!("lvmthin  pve{i}"),
            blocked: false,
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
    .map(|(v, d)| Entry::new(v, d));
    let mut console = Recorded::answering(&["x", "9", "2"]);
    assert_eq!(
        console.pick("--snippet-storage", "where the snippets go", false, &choices),
        Some(vec![1])
    );
    let shown = console.out();
    assert!(
        shown.contains("1) local-lvm") && shown.contains("3) vm-ssd-a"),
        "{shown}"
    );
    assert!(shown.contains("lvmthin  pve2"), "{shown}");

    let mut console = Recorded::answering(&["1, 3"]);
    assert_eq!(
        console.pick("--storages", "where the disks go", true, &choices),
        Some(vec![0, 2])
    );
    // A single choice takes one number only.
    let mut console = Recorded::answering(&["1,2", "3"]);
    assert_eq!(
        console.pick("--snippet-storage", "where the snippets go", false, &choices),
        Some(vec![2])
    );
    // End of input is a cancel.
    let mut console = Recorded::answering(&[]);
    assert_eq!(
        console.pick("--snippet-storage", "where the snippets go", false, &choices),
        None
    );
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

/// The description is not in the frame's title, where a narrow window cuts
/// it off: it is drawn, wrapped, inside the window.
#[test]
fn the_description_is_wrapped_inside_the_window_not_cut_off_in_the_title() {
    let description = "where the workspace disks go and a few more words to make it long";
    let mut picker = Picker::new("--storages", true, items(2)).with_description(description);
    let screen = picker.render_to_string(34, 14);
    let top = screen.lines().next().unwrap_or_default();
    assert!(top.starts_with("\u{250c} --storages "), "{top}");
    let flat = screen.replace(['\u{2502}', '\n'], " ");
    for word in description.split_whitespace() {
        assert!(flat.contains(word), "{word:?} is cut off:\n{screen}");
    }
    assert!(screen.contains("storage-0") && screen.contains("storage-1"), "{screen}");
}

fn mixed() -> Vec<Item> {
    [("a", false), ("b", true), ("c", false), ("d", true)]
        .iter()
        .map(|(v, blocked)| Item {
            value: (*v).to_owned(),
            detail: if *blocked {
                "dir  no disk images".to_owned()
            } else {
                "lvmthin".to_owned()
            },
            blocked: *blocked,
        })
        .collect()
}

#[test]
fn a_blocked_item_is_shown_but_the_cursor_skips_it_and_it_cannot_be_chosen() {
    let mut picker = Picker::new("Storages", true, mixed());
    keys(
        &mut picker,
        &[Key::Down, Key::Space, Key::Down, Key::Down, Key::Space, Key::Enter],
    );
    // a -> c (b skipped) -> stays on c (d is blocked): ticks c twice = none, Enter takes c.
    assert_eq!(picker.outcome(), Some(Outcome::Chosen(vec![2])));
    let mut picker = Picker::new("Storages", true, mixed());
    keys(&mut picker, &[Key::End, Key::Space, Key::Enter]);
    assert_eq!(
        picker.outcome(),
        Some(Outcome::Chosen(vec![2])),
        "End lands on the last item that can be chosen"
    );
    let screen = Picker::new("Storages", true, mixed()).render_to_string(60, 12);
    assert!(
        screen
            .lines()
            .any(|l| l.contains('b') && l.contains("no disk images") && l.contains("[-]")),
        "{screen}"
    );
}

#[test]
fn the_numbered_fallback_numbers_only_what_can_be_chosen() {
    let choices = vec![
        Entry::new("local-lvm", "lvmthin"),
        Entry::blocked("local", "dir  no disk images"),
        Entry::new("vm-ssd-a", "lvmthin"),
    ];
    let mut console = Recorded::answering(&["3", "2"]);
    // "3" is out of range (two can be chosen); "2" is the third entry.
    assert_eq!(console.pick("Storages", "where", false, &choices), Some(vec![2]));
    let shown = console.out();
    assert!(
        shown.contains("1) local-lvm") && shown.contains("2) vm-ssd-a"),
        "{shown}"
    );
    assert!(
        shown
            .lines()
            .any(|l| l.contains("local ") && l.contains("no disk images") && !l.contains(") local ")),
        "{shown}"
    );
}
