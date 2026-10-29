//! Native macOS menu bar.
//!
//! Items with key equivalents are dispatched by AppKit before the key reaches egui, so each
//! menu command is turned back into the same [`Shortcuts`] flag or [`Action`] the keyboard
//! path produces. There is deliberately no Edit menu: AppKit's predefined copy/paste items
//! would send `copy:`/`paste:` to the window and swallow ⌘C/⌘V before egui sees them.

use muda::accelerator::Accelerator;
use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};

use crate::app::{Action, Shortcuts};
use crate::theme::ThemeChoice;

pub struct AppMenu {
    _menu: Menu,
}

fn item(id: &str, label: &str, accel: Option<&str>) -> MenuItem {
    MenuItem::with_id(id, label, true, accel.and_then(|a| a.parse::<Accelerator>().ok()))
}

impl AppMenu {
    /// Builds the menu bar and installs it as the application menu.
    pub fn install() -> Option<Self> {
        let menu = Menu::new();
        let app = Submenu::with_items(
            "tableplus-rs",
            true,
            &[
                &PredefinedMenuItem::about(Some("About tableplus-rs"), None),
                &PredefinedMenuItem::separator(),
                &item("preferences", "Preferences…", Some("CmdOrCtrl+Comma")),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::services(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::hide(None),
                &PredefinedMenuItem::hide_others(None),
                &PredefinedMenuItem::show_all(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::quit(None),
            ],
        )
        .ok()?;
        let file = Submenu::with_items(
            "File",
            true,
            &[
                &item("new_connection", "New Connection…", Some("CmdOrCtrl+N")),
                &item("connections", "Connections", None),
                &PredefinedMenuItem::separator(),
                &item("new_query", "New SQL Query", Some("CmdOrCtrl+T")),
                &item("quick_open", "Open Anything…", Some("CmdOrCtrl+K")),
                &PredefinedMenuItem::separator(),
                &item("close_tab", "Close Tab", Some("CmdOrCtrl+W")),
            ],
        )
        .ok()?;
        let data = Submenu::with_items(
            "Data",
            true,
            &[
                &item("commit", "Commit Changes", Some("CmdOrCtrl+S")),
                &item("reload", "Reload", Some("CmdOrCtrl+R")),
                &item("filter", "Filter Rows", Some("CmdOrCtrl+F")),
                &PredefinedMenuItem::separator(),
                &item("run", "Run Current Statement", Some("CmdOrCtrl+Enter")),
                &item("run_all", "Run All", Some("CmdOrCtrl+Shift+Enter")),
                &PredefinedMenuItem::separator(),
                &item("explain", "Explain", Some("CmdOrCtrl+E")),
                &item("explain_analyze", "Explain Analyze", Some("CmdOrCtrl+Shift+E")),
            ],
        )
        .ok()?;
        let view = Submenu::with_items(
            "View",
            true,
            &[
                &item("theme_system", "Use System Appearance", None),
                &item("theme_light", "Light Appearance", None),
                &item("theme_dark", "Dark Appearance", None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::fullscreen(None),
            ],
        )
        .ok()?;
        let window = Submenu::with_items(
            "Window",
            true,
            &[
                &PredefinedMenuItem::minimize(None),
                &item("next_tab", "Show Next Tab", Some("CmdOrCtrl+Shift+BracketRight")),
                &item("prev_tab", "Show Previous Tab", Some("CmdOrCtrl+Shift+BracketLeft")),
            ],
        )
        .ok()?;
        menu.append_items(&[&app, &file, &data, &view, &window]).ok()?;
        menu.init_for_nsapp();
        window.set_as_windows_menu_for_nsapp();
        Some(Self { _menu: menu })
    }

    /// Drains menu clicks into this frame's shortcuts/actions.
    pub fn poll(
        &self,
        sc: &mut Shortcuts,
        actions: &mut Vec<Action>,
        theme: &mut Option<ThemeChoice>,
        preferences: &mut bool,
    ) {
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            match event.id.0.as_str() {
                "preferences" => *preferences = true,
                "new_connection" => sc.new_connection = true,
                "connections" => actions.push(Action::ShowWelcome),
                "new_query" => sc.new_query = true,
                "quick_open" => sc.quick_open = true,
                "close_tab" => sc.close_tab = true,
                "commit" => sc.commit = true,
                "reload" => sc.refresh = true,
                "filter" => sc.find = true,
                "run" => sc.run = true,
                "run_all" => sc.run_all = true,
                "explain" => sc.explain = true,
                "explain_analyze" => sc.explain_analyze = true,
                "next_tab" => sc.next_tab = true,
                "prev_tab" => sc.prev_tab = true,
                "theme_system" => *theme = Some(ThemeChoice::System),
                "theme_light" => *theme = Some(ThemeChoice::Light),
                "theme_dark" => *theme = Some(ThemeChoice::Dark),
                _ => {}
            }
        }
    }
}
