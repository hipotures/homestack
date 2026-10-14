//! Themes of the TUI (phase 13b, step 1): the list of MyPowers, the
//! HomeStack default, `[colors]` overrides and `tui.toml`.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::os::unix::fs::PermissionsExt;

use hs_cli::tui::theme::{Palette, Preferences, THEMES};
use ratatui::style::Color;

#[test]
fn the_themes_are_those_of_mypowers_with_homestack_first() {
    assert_eq!(
        THEMES,
        [
            "HomeStack",
            "Catppuccin",
            "Nord",
            "Gruvbox",
            "Tokyo Night",
            "Solarized",
            "Terminal"
        ]
    );
    for name in THEMES {
        let palette = Palette::named(name, false).unwrap();
        assert_eq!(palette.name, name);
    }
}

#[test]
fn an_unknown_theme_is_refused_with_the_list() {
    let error = Palette::named("Neon", false).unwrap_err();
    assert!(error.0.contains("Neon"), "{error}");
    assert!(error.0.contains("Tokyo Night"), "{error}");
}

#[test]
fn homestack_is_the_owners_picture() {
    let palette = Palette::named("HomeStack", false).unwrap();
    assert_eq!(palette.background, Color::Rgb(13, 17, 23));
    assert_eq!(palette.foreground, Color::Rgb(200, 208, 224));
    assert_eq!(palette.focus, Color::Rgb(122, 162, 247));
    assert_eq!(palette.value, Color::Rgb(94, 212, 180));
    assert_eq!(palette.hotkey, Color::Rgb(240, 98, 112));
    assert_eq!(palette.selection, Color::Rgb(30, 38, 56));
}

#[test]
fn the_named_themes_come_from_the_ratcn_presets_of_mypowers() {
    let nord = Palette::named("Nord", false).unwrap();
    let preset = ratcn::Theme::nord();
    assert_eq!(nord.background, preset.background);
    assert_eq!(nord.foreground, preset.foreground);
    assert_eq!(nord.border, preset.border);
    assert_eq!(nord.focus, preset.primary);
    // A hotkey is red as in btop: the theme's own red.
    assert_eq!(nord.hotkey, preset.destructive);
}

#[test]
fn no_color_draws_with_the_terminals_own_colours() {
    let palette = Palette::named("HomeStack", true).unwrap();
    assert!(palette.no_color);
    for color in [
        palette.background,
        palette.foreground,
        palette.muted,
        palette.border,
        palette.focus,
        palette.value,
        palette.hotkey,
    ] {
        assert_eq!(color, Color::Reset);
    }
}

#[test]
fn a_missing_tui_toml_gives_the_homestack_theme() {
    let dir = tempfile::tempdir().unwrap();
    let preferences = Preferences::load(&dir.path().join("tui.toml")).unwrap();
    assert_eq!(preferences.theme, "HomeStack");
    assert_eq!(preferences.palette(false).unwrap().name, "HomeStack");
}

#[test]
fn colors_override_single_roles() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tui.toml");
    std::fs::write(
        &path,
        "theme = \"Nord\"\n[colors]\nhotkey = \"#ff0000\"\nvalue = \"#00ff00\"\n",
    )
    .unwrap();
    let palette = Preferences::load(&path).unwrap().palette(false).unwrap();
    assert_eq!(palette.name, "Nord");
    assert_eq!(palette.hotkey, Color::Rgb(255, 0, 0));
    assert_eq!(palette.value, Color::Rgb(0, 255, 0));
    assert_eq!(palette.background, ratcn::Theme::nord().background);
}

#[test]
fn a_bad_role_colour_or_theme_in_tui_toml_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tui.toml");
    for (text, word) in [
        ("[colors]\nsparkle = \"#ff0000\"\n", "sparkle"),
        ("[colors]\nhotkey = \"red\"\n", "#RRGGBB"),
        ("theme = \"Neon\"\n", "Neon"),
        ("theme = [\n", "tui.toml"),
    ] {
        std::fs::write(&path, text).unwrap();
        let error = Preferences::load(&path).unwrap_err();
        assert!(error.0.contains(word), "{text:?}: {error}");
    }
}

#[test]
fn saving_the_theme_keeps_the_other_settings_and_is_private() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("homestack").join("tui.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "preset = 1\n[colors]\nhotkey = \"#ff0000\"\n").unwrap();
    let mut preferences = Preferences::load(&path).unwrap();
    preferences.save_theme("Gruvbox").unwrap();
    assert_eq!(preferences.theme, "Gruvbox");
    let text = std::fs::read_to_string(&path).unwrap();
    let table: toml::Table = text.parse().unwrap();
    assert_eq!(table["theme"].as_str(), Some("Gruvbox"));
    assert_eq!(table["preset"].as_integer(), Some(1));
    assert_eq!(table["colors"]["hotkey"].as_str(), Some("#ff0000"));
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    // No temporary file is left next to it.
    let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 1, "{names:?}");
}

#[test]
fn saving_an_unknown_theme_is_refused_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tui.toml");
    let mut preferences = Preferences::load(&path).unwrap();
    assert!(preferences.save_theme("Neon").is_err());
    assert!(!path.exists());
    assert_eq!(preferences.theme, "HomeStack");
}
