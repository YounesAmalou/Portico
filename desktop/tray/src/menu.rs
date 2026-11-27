//! The native menu (`muda`, through `tray-icon`) for the app's state: the
//! layout is `portico_tray::layout`'s, this only builds it.

use portico_tray::layout::{self, Entry};
pub use portico_tray::layout::{Click, State};
use std::collections::HashMap;
use tray_icon::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};

/// `&` marks a mnemonic in a muda title; `&&` is a literal one.
fn escaped(text: &str) -> String {
    text.replace('&', "&&")
}

/// The same click has the same ID in every menu built, so a click in a menu
/// that was replaced while it was open still does what it showed.
fn id_of(click: &Click) -> MenuId {
    MenuId::new(format!("{click:?}"))
}

fn native(entry: &Entry, clicks: &mut HashMap<MenuId, Click>) -> Box<dyn IsMenuItem> {
    match entry {
        Entry::Line(text) => Box::new(MenuItem::new(escaped(text), false, None)),
        Entry::Separator => Box::new(PredefinedMenuItem::separator()),
        Entry::Item { text, click, check } => {
            let id = id_of(click);
            clicks.insert(id.clone(), click.clone());
            match check {
                Some(checked) => Box::new(CheckMenuItem::with_id(
                    id,
                    escaped(text),
                    true,
                    *checked,
                    None,
                )),
                None => Box::new(MenuItem::with_id(id, escaped(text), true, None)),
            }
        }
        Entry::Submenu(text, entries) => {
            let submenu = Submenu::new(escaped(text), true);
            for entry in entries {
                // Appending fails only for an item already in this submenu: none is.
                let _ = submenu.append(native(entry, clicks).as_ref());
            }
            Box::new(submenu)
        }
    }
}

/// The menu for `state`, and what each of its items does.
pub fn build(state: &State) -> (Menu, HashMap<MenuId, Click>) {
    let menu = Menu::new();
    let mut clicks = HashMap::new();
    for entry in layout::describe(state) {
        let _ = menu.append(native(&entry, &mut clicks).as_ref());
    }
    (menu, clicks)
}
