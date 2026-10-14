//! The installer's wizard (ratatui): the steps, Esc going back, the summary
//! with Install and Back, the drawing on a buffer, and the numbered way used
//! where there is no terminal to draw on.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::Recorded;
use hs_cli::wizard::{Answer, Item, Key, Outcome, Step, Summary, Wizard, run_numbered};

fn storages() -> Vec<Item> {
    vec![
        Item::new("local-lvm", "lvmthin  pve2,pve3"),
        Item::new("local-zfs", "zfspool  pve1"),
        Item::new("worker-roots", "lvmthin  pve1"),
    ]
}

fn wizard() -> Wizard {
    Wizard::new(
        vec![
            Step::pick(
                "Storages for the workspace disks",
                "Where the disks go.",
                true,
                storages(),
                &[0, 2],
            ),
            Step::pick(
                "Storage for the Cloud-Init snippets",
                "Where the snippets go.",
                false,
                vec![
                    Item::new("shared-nfs", "nfs  all nodes (shared)"),
                    Item::new("local", "dir  all nodes"),
                ],
                &[1],
            ),
        ],
        true,
    )
}

fn summary() -> Summary {
    Summary {
        sections: vec![(
            "Changes in Proxmox (2)".to_owned(),
            vec!["create user".to_owned(), "create pool".to_owned()],
        )],
        problems: Vec::new(),
    }
}

fn press(wizard: &mut Wizard, keys: &[Key]) {
    for key in keys {
        if wizard.needs_summary() {
            wizard.set_summary(summary());
        }
        wizard.handle(*key);
    }
    if wizard.needs_summary() {
        wizard.set_summary(summary());
    }
}

#[test]
fn the_defaults_are_ticked_and_enter_goes_through_the_steps_to_the_summary() {
    let mut w = wizard();
    assert_eq!(w.answers(), [Answer::Picked(vec![0, 2]), Answer::Picked(vec![1])]);
    press(&mut w, &[Key::Enter, Key::Enter]);
    assert_eq!(w.outcome(), None, "the summary has to be confirmed");
    press(&mut w, &[Key::Enter]);
    assert_eq!(
        w.outcome(),
        Some(Outcome::Done(vec![Answer::Picked(vec![0, 2]), Answer::Picked(vec![1])]))
    );
}

#[test]
fn nothing_is_done_before_install_and_back_or_esc_return_to_the_choices() {
    let mut w = wizard();
    press(&mut w, &[Key::Enter, Key::Enter]);
    // Back is the other button.
    press(&mut w, &[Key::Right, Key::Enter]);
    assert_eq!(w.outcome(), None);
    let screen = w.render_to_string(70, 16);
    assert!(
        screen.contains("Storage for the Cloud-Init snippets"),
        "Back returns to the last step:\n{screen}"
    );
    // Esc on the summary does the same; Esc on a step goes back a step.
    press(&mut w, &[Key::Enter, Key::Back, Key::Back]);
    let screen = w.render_to_string(70, 16);
    assert!(screen.contains("Storages for the workspace disks"), "{screen}");
    assert_eq!(w.outcome(), None);
    // Esc on the first step leaves.
    press(&mut w, &[Key::Back]);
    assert_eq!(w.outcome(), Some(Outcome::Cancelled));
}

#[test]
fn space_ticks_and_enter_with_nothing_ticked_takes_the_cursor() {
    let mut w = wizard();
    press(&mut w, &[Key::Space, Key::Down, Key::Down, Key::Space]);
    assert_eq!(w.answers()[0], Answer::Picked(vec![]));
    press(&mut w, &[Key::Down, Key::Enter]);
    assert_eq!(w.answers()[0], Answer::Picked(vec![2]));
    // A single choice is where the cursor is.
    press(&mut w, &[Key::Up, Key::Enter]);
    assert_eq!(w.answers()[1], Answer::Picked(vec![0]));
}

#[test]
fn install_cannot_be_pressed_while_there_is_a_problem() {
    let mut w = wizard();
    press(&mut w, &[Key::Enter, Key::Enter]);
    w.set_summary(Summary {
        sections: Vec::new(),
        problems: vec!["storage local does not allow content type snippets".to_owned()],
    });
    press(&mut w, &[Key::Right, Key::Left, Key::Enter]);
    assert_eq!(w.outcome(), None, "Enter pressed Back, not Install");
    let screen = w.render_to_string(70, 16);
    assert!(screen.contains("Storage for the Cloud-Init snippets"), "{screen}");
}

#[test]
fn a_step_shows_its_heading_step_number_note_and_only_its_choices() {
    let mut w = wizard();
    let screen = w.render_to_string(70, 14);
    let top = screen.lines().next().unwrap_or_default();
    assert!(
        top.starts_with("\u{250c} Storages for the workspace disks (1 of 3) "),
        "{top}"
    );
    assert!(screen.contains("Where the disks go."), "{screen}");
    let row = |name: &str| screen.lines().find(|l| l.contains(name)).unwrap_or_default().to_owned();
    assert!(
        row("local-lvm").contains("[x]") && row("local-lvm").contains("lvmthin"),
        "{screen}"
    );
    assert!(row("local-zfs").contains("[ ]"), "{screen}");
    assert!(row("worker-roots").contains("[x]"), "{screen}");
    assert!(!screen.contains("--"), "no command-line option is shown:\n{screen}");
    assert!(screen.contains("space tick") && screen.contains("esc back"), "{screen}");
}

#[test]
fn the_summary_lists_the_changes_and_the_two_buttons() {
    let mut w = wizard();
    press(&mut w, &[Key::Enter, Key::Enter]);
    let screen = w.render_to_string(70, 16);
    assert!(screen.contains("Summary (3 of 3)"), "{screen}");
    assert!(
        screen.contains("Changes in Proxmox (2)") && screen.contains("create user"),
        "{screen}"
    );
    assert!(
        screen.contains("[ Install ]") && screen.contains("[ Back ]"),
        "{screen}"
    );
}

#[test]
fn a_text_step_takes_what_is_typed() {
    let mut w = Wizard::new(vec![Step::text("Node to log in to", "USER@HOST", "root@")], false);
    for c in "pve9".chars() {
        w.handle(Key::Char(c));
    }
    w.handle(Key::Backspace);
    w.handle(Key::Char('1'));
    w.handle(Key::Enter);
    assert_eq!(
        w.outcome(),
        Some(Outcome::Done(vec![Answer::Text("root@pve1".to_owned())]))
    );
}

#[test]
fn the_numbered_way_keeps_the_defaults_on_enter_and_asks_install() {
    let mut console = Recorded::answering(&["", "1", "y"]);
    let mut w = wizard();
    let outcome = run_numbered(&mut console, &mut w, &mut |_| summary());
    assert_eq!(
        outcome,
        Outcome::Done(vec![Answer::Picked(vec![0, 2]), Answer::Picked(vec![0])])
    );
    let shown = console.out();
    assert!(
        shown.contains("* 1) local-lvm") && shown.contains("  2) local-zfs"),
        "{shown}"
    );
    assert!(shown.contains("create user"), "{shown}");

    let mut console = Recorded::answering(&["", "", "n"]);
    assert_eq!(
        run_numbered(&mut console, &mut wizard(), &mut |_| summary()),
        Outcome::Cancelled
    );
}
