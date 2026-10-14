//! The installer's form (ratatui): every setting on one screen, a list per
//! setting, the confirmation with Install and Back.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use hs_cli::form::{Field, Form, Outcome};
use hs_cli::wizard::{Answer, Item, Key, Summary};

fn form() -> Form {
    Form::new(
        "HomeStack install",
        vec![
            Field::fixed("Root access", "ssh root@node1"),
            Field::choose(
                "Workspace disks",
                "Where the disks go.",
                true,
                vec![Item::new("disk-a", "lvmthin  node1"), Item::new("disk-b", "dir  node1")],
                &[0],
            ),
            Field::choose(
                "Gold VM",
                "The base VM.",
                false,
                vec![Item::new("100", "desktop  node1"), Item::new("101", "gold  node1")],
                &[1],
            ),
        ],
    )
}

fn changes() -> Summary {
    Summary {
        sections: vec![("Changes in Proxmox (1)".to_owned(), vec!["create user".to_owned()])],
        problems: Vec::new(),
    }
}

fn press(form: &mut Form, keys: &[Key]) {
    for key in keys {
        form.handle(*key);
        if form.needs_summary() {
            form.set_summary(changes());
        }
    }
}

#[test]
fn every_setting_gold_and_the_buttons_are_on_one_screen() {
    let screen = form().render_to_string(80, 16);
    for text in [
        "HomeStack install",
        "Root access",
        "ssh root@node1",
        "Workspace disks",
        "disk-a",
        "Gold VM",
        "101 gold",
        "[ Install ]",
        "[ Cancel ]",
    ] {
        assert!(screen.contains(text), "{text:?} is not shown:\n{screen}");
    }
    assert!(!screen.contains("--"), "{screen}");
}

#[test]
fn a_list_changes_a_setting_and_esc_undoes_it() {
    let mut f = form();
    // Workspace disks: tick the second.
    press(&mut f, &[Key::Enter, Key::Down, Key::Space, Key::Enter]);
    assert_eq!(f.answers()[0], Answer::Picked(vec![0, 1]));
    // Gold: move to the first, then Esc: unchanged.
    press(&mut f, &[Key::Down, Key::Enter, Key::Up, Key::Back]);
    assert_eq!(f.answers()[1], Answer::Picked(vec![1]));
    // Gold: the first, with Enter.
    press(&mut f, &[Key::Enter, Key::Up, Key::Enter]);
    assert_eq!(f.answers()[1], Answer::Picked(vec![0]));
    let screen = f.render_to_string(80, 16);
    assert!(screen.contains("100 desktop"), "{screen}");
}

#[test]
fn install_opens_the_confirmation_and_only_its_install_installs() {
    let mut f = form();
    press(&mut f, &[Key::Down, Key::Down, Key::Enter]);
    assert_eq!(f.outcome(), None, "the first Install only opens the confirmation");
    let screen = f.render_to_string(80, 16);
    assert!(
        screen.contains("Confirm the installation") && screen.contains("create user"),
        "{screen}"
    );
    assert!(
        screen.contains("[ Install ]") && screen.contains("[ Back ]"),
        "{screen}"
    );
    // Esc returns to the settings.
    press(&mut f, &[Key::Back]);
    assert!(!f.render_to_string(80, 16).contains("Confirm the installation"));
    // Back too.
    press(&mut f, &[Key::Enter, Key::Right, Key::Enter]);
    assert!(!f.render_to_string(80, 16).contains("Confirm the installation"));
    assert_eq!(f.outcome(), None);
    press(&mut f, &[Key::Enter, Key::Enter]);
    assert_eq!(
        f.outcome(),
        Some(Outcome::Install(vec![Answer::Picked(vec![0]), Answer::Picked(vec![1])]))
    );
}

#[test]
fn a_problem_keeps_install_from_being_pressed() {
    let mut f = form();
    press(&mut f, &[Key::Down, Key::Down]);
    f.handle(Key::Enter);
    f.set_summary(Summary {
        sections: Vec::new(),
        problems: vec!["storage disk-b does not allow content type images".to_owned()],
    });
    press(&mut f, &[Key::Left, Key::Enter]);
    assert_eq!(f.outcome(), None);
    assert!(
        !f.render_to_string(80, 16).contains("Confirm the installation"),
        "Enter pressed Back"
    );
}

#[test]
fn cancel_and_esc_leave_without_installing() {
    let mut f = form();
    press(&mut f, &[Key::Back]);
    assert_eq!(f.outcome(), Some(Outcome::Cancelled));
    let mut f = form();
    press(&mut f, &[Key::Down, Key::Down, Key::Right, Key::Enter]);
    assert_eq!(f.outcome(), Some(Outcome::Cancelled));
}

#[test]
fn a_long_confirmation_has_a_scrollbar_and_stops_at_its_end() {
    let mut f = form();
    press(&mut f, &[Key::Down, Key::Down]);
    f.handle(Key::Enter);
    let many: Vec<String> = (0..40).map(|i| format!("change {i}")).collect();
    f.set_summary(Summary {
        sections: vec![("Changes".to_owned(), many)],
        problems: Vec::new(),
    });
    let first = f.render_to_string(70, 20);
    assert!(
        first.contains('\u{2193}') || first.contains('\u{2588}'),
        "no scrollbar:\n{first}"
    );
    assert!(first.contains("change 0"), "{first}");
    for _ in 0..200 {
        f.handle(Key::Down);
    }
    let end = f.render_to_string(70, 20);
    assert!(end.contains("change 39"), "the end is not shown:\n{end}");
    // Scrolled to the end it stays there: one Up shows the line above.
    f.handle(Key::Up);
    let up = f.render_to_string(70, 20);
    assert!(
        !up.contains("change 39") || up.contains("change 30"),
        "scrolled past the end:\n{up}"
    );
    let mut short = form();
    press(&mut short, &[Key::Down, Key::Down, Key::Enter]);
    let screen = short.render_to_string(70, 20);
    assert!(!screen.contains('\u{2588}'), "a scrollbar on a short list:\n{screen}");
}
