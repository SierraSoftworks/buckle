//! Dark by default, light on request (design §12): both themes are the same
//! CSS custom properties with different values. With nothing chosen the page
//! follows `prefers-color-scheme`; a choice made with the toggle is kept in
//! `localStorage` and applied by `index.html` before the bundle loads, so the
//! page never flashes the other theme.

use crate::util::{store, stored, window};

pub const KEY: &str = "buckle.theme";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Theme {
    Dark,
    Light,
}

impl Theme {
    pub fn name(self) -> &'static str {
        match self {
            Theme::Dark => "dark",
            Theme::Light => "light",
        }
    }

    pub fn other(self) -> Theme {
        match self {
            Theme::Dark => Theme::Light,
            Theme::Light => Theme::Dark,
        }
    }
}

fn system() -> Theme {
    match window().match_media("(prefers-color-scheme: light)") {
        Ok(Some(m)) if m.matches() => Theme::Light,
        _ => Theme::Dark,
    }
}

/// The theme in effect: the stored choice, or the system's.
pub fn current() -> Theme {
    match stored(KEY).as_deref() {
        Some("light") => Theme::Light,
        Some("dark") => Theme::Dark,
        _ => system(),
    }
}

/// Applies and remembers a theme.
pub fn set(theme: Theme) {
    store(KEY, Some(theme.name()));
    apply(theme);
}

pub fn apply(theme: Theme) {
    if let Some(root) = window().document().and_then(|d| d.document_element()) {
        let _ = root.set_attribute("data-theme", theme.name());
    }
}
