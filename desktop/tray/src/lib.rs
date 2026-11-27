//! The tray app's logic. `model` is pure (Portico's JSON documents in, menu
//! rows and argument lists out) and `layout` turns them into the menu as plain
//! data; both are tested on every platform.

pub mod instance;
pub mod layout;
pub mod model;
pub mod portico;
