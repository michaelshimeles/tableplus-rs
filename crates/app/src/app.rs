//! Top-level application state and frame loop.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, Key, KeyboardShortcut, Layout, Modifiers, RichText, Sense, Stroke,
    StrokeKind, Ui, vec2,
};
use tableplus_core::config::{ConnectionConfig, Credentials};
use tableplus_core::driver::{self, DynConnection};
use tableplus_core::model::SchemaObject;
use tableplus_core::secrets::{SecretKind, SecretStore};
use tableplus_core::store::{ConnectionStore, History, HistoryEntry};
use uuid::Uuid;

use crate::conn_dialog::{ConnDialog, DialogOutcome};
use crate::export_dialog::{ExportDialog, ExportSource, ImportDialog};
use crate::icons::{self, Icon};
use crate::quick_open::{QuickOpen, QuickPick};
use crate::session::{Session, TabKind};
use crate::settings::Settings;
use crate::task::{Spawner, Task};
use crate::theme::{self, ThemeChoice, palette};
use crate::widgets;

/// Things UI code asks the app to do after the frame.
pub enum Action {
    Connect(Uuid),
    NewConnection,
    EditConnection(Uuid),
    DuplicateConnection(Uuid),
    DeleteConnection(Uuid),
    ShowWelcome,
    SwitchSession(usize),
    CloseSession(usize),
    OpenObject {
        object: SchemaObject,
        structure: bool,
    },
    ShowDefinition(SchemaObject),
    NewQuery(Option<String>),
    RecordHistory(HistoryEntry),
    Toast(ToastKind, String),
    RefreshObjects,
    InvalidateStructure {
        schema: String,
        table: String,
    },
    /// Reload open data tabs of a table whose rows changed outside them (e.g. an import).
    ReloadTable(SchemaObject),
    LoadStructure {
        schema: String,
        table: String,
    },
    Export(ExportSource),
    Import(SchemaObject),
    QuickOpen,
    CloseTab(usize),
    ClearHistory,
}

/// Global shortcuts, consumed before widgets see the key events.
#[derive(Default, Clone, Copy)]
pub struct Shortcuts {
    pub quick_open: bool,
    pub new_query: bool,
    pub close_tab: bool,
    pub commit: bool,
    pub run: bool,
    pub run_all: bool,
    pub explain: bool,
    pub explain_analyze: bool,
    pub refresh: bool,
    pub find: bool,
    pub new_connection: bool,
    pub next_tab: bool,
    pub prev_tab: bool,
}

impl Shortcuts {
    fn read(ctx: &egui::Context) -> Self {
        let sc = |m: Modifiers, k: Key| KeyboardShortcut::new(m, k);
        let cmd = Modifiers::COMMAND;
        let cmd_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        ctx.input_mut(|i| Shortcuts {
            // `consume_shortcut` ignores extra Shift, so each ⇧ variant is consumed first.
            run_all: i.consume_shortcut(&sc(cmd_shift, Key::Enter)),
            run: i.consume_shortcut(&sc(cmd, Key::Enter)),
            explain_analyze: i.consume_shortcut(&sc(cmd_shift, Key::E)),
            explain: i.consume_shortcut(&sc(cmd, Key::E)),
            quick_open: i.consume_shortcut(&sc(cmd, Key::K)) | i.consume_shortcut(&sc(cmd, Key::P)),
            new_query: i.consume_shortcut(&sc(cmd, Key::T)),
            close_tab: i.consume_shortcut(&sc(cmd, Key::W)),
            commit: i.consume_shortcut(&sc(cmd, Key::S)),
            refresh: i.consume_shortcut(&sc(cmd, Key::R)),
            find: i.consume_shortcut(&sc(cmd, Key::F)),
            new_connection: i.consume_shortcut(&sc(cmd, Key::N)),
            next_tab: i.consume_shortcut(&sc(cmd_shift, Key::CloseBracket))
                | i.consume_shortcut(&sc(Modifiers::CTRL, Key::Tab)),
            prev_tab: i.consume_shortcut(&sc(cmd_shift, Key::OpenBracket)),
        })
    }
}

impl Shortcuts {
    /// Combines shortcuts from another source (the native menu bar).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn merge(&mut self, o: Shortcuts) {
        self.quick_open |= o.quick_open;
        self.new_query |= o.new_query;
        self.close_tab |= o.close_tab;
        self.commit |= o.commit;
        self.run |= o.run;
        self.run_all |= o.run_all;
        self.explain |= o.explain;
        self.explain_analyze |= o.explain_analyze;
        self.refresh |= o.refresh;
        self.find |= o.find;
        self.new_connection |= o.new_connection;
        self.next_tab |= o.next_tab;
        self.prev_tab |= o.prev_tab;
    }
}

/// `⌘K` on macOS, `Ctrl+K` elsewhere.
pub fn shortcut_label(key: &str) -> String {
    if cfg!(target_os = "macos") { format!("⌘{key}") } else { format!("Ctrl+{key}") }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

struct Toast {
    kind: ToastKind,
    text: String,
    created: Instant,
}

struct Connecting {
    config: ConnectionConfig,
    task: Task<DynConnection>,
}

struct PasswordPrompt {
    config: ConnectionConfig,
    password: String,
    remember: bool,
}

/// Everything shared between the welcome screen and sessions.
pub struct TablePlusApp {
    spawner: Spawner,
    _runtime: tokio::runtime::Runtime,
    dir: PathBuf,
    pub store: ConnectionStore,
    pub secrets: SecretStore,
    pub history: History,
    pub settings: Settings,
    sessions: Vec<Session>,
    active: Option<usize>,
    next_session_key: u64,
    connecting: Option<Connecting>,
    connect_errors: std::collections::HashMap<Uuid, String>,
    pub welcome: crate::welcome::WelcomeState,
    conn_dialog: Option<ConnDialog>,
    password_prompt: Option<PasswordPrompt>,
    quick_open: Option<QuickOpen>,
    export_dialog: Option<ExportDialog>,
    import_dialog: Option<ImportDialog>,
    toasts: Vec<Toast>,
    applied_theme: Option<ThemeChoice>,
    title: String,
    settings_open: bool,
    #[cfg(target_os = "macos")]
    menu: Option<crate::macos_menu::AppMenu>,
}

impl TablePlusApp {
    pub fn new(cc: &eframe::CreationContext<'_>, runtime: tokio::runtime::Runtime) -> Self {
        theme::install_fonts(&cc.egui_ctx);
        theme::install_style(&cc.egui_ctx);
        let dir = tableplus_core::store::config_dir();
        let store = ConnectionStore::load(&dir).unwrap_or_else(|e| {
            tracing::error!("could not load connections: {e:#}");
            ConnectionStore::load(&std::env::temp_dir()).expect("temp store")
        });
        let spawner = Spawner::new(runtime.handle().clone(), cc.egui_ctx.clone());
        let mut app = Self {
            spawner,
            _runtime: runtime,
            secrets: SecretStore::new(&dir),
            history: History::load(&dir),
            settings: Settings::load(&dir),
            store,
            dir,
            sessions: Vec::new(),
            active: None,
            next_session_key: 1,
            connecting: None,
            connect_errors: Default::default(),
            welcome: Default::default(),
            conn_dialog: None,
            password_prompt: None,
            quick_open: None,
            export_dialog: None,
            import_dialog: None,
            toasts: Vec::new(),
            applied_theme: None,
            title: String::new(),
            settings_open: false,
            #[cfg(target_os = "macos")]
            menu: crate::macos_menu::AppMenu::install(),
        };
        // `--connect <name>` opens a saved connection on startup.
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            if arg == "--connect"
                && let Some(name) = args.next()
                && let Some(c) =
                    app.store.connections.iter().find(|c| c.display_name() == name || c.id.to_string() == name)
            {
                let id = c.id;
                app.connect(id);
            }
        }
        app
    }

    pub fn toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        self.toasts.push(Toast { kind, text: text.into(), created: Instant::now() });
    }

    fn credentials(&self, config: &ConnectionConfig) -> Credentials {
        Credentials {
            password: self.secrets.get_for(config.id, SecretKind::Password),
            ssh_password: self.secrets.get_for(config.id, SecretKind::SshPassword),
            ssh_passphrase: self.secrets.get_for(config.id, SecretKind::SshPassphrase),
        }
    }

    pub fn connect(&mut self, id: Uuid) {
        let Some(config) = self.store.get(id).cloned() else { return };
        if let Some(i) = self.sessions.iter().position(|s| s.config.id == id) {
            self.active = Some(i);
            return;
        }
        let creds = self.credentials(&config);
        let needs_password = !config.driver.is_file_based() && !config.save_password && creds.password.is_none();
        if needs_password {
            self.password_prompt = Some(PasswordPrompt { config, password: String::new(), remember: false });
            return;
        }
        self.start_connect(config, creds);
    }

    fn start_connect(&mut self, config: ConnectionConfig, creds: Credentials) {
        self.connect_errors.remove(&config.id);
        let cfg = config.clone();
        let task = self.spawner.spawn(async move { driver::connect(&cfg, &creds).await });
        self.connecting = Some(Connecting { config, task });
    }

    fn poll(&mut self) {
        if let Some(c) = &mut self.connecting
            && let Some(result) = c.task.poll()
        {
            let c = self.connecting.take().expect("connecting");
            match result {
                Ok(conn) => {
                    self.next_session_key += 1;
                    let session = Session::new(self.next_session_key, c.config, conn, &self.spawner);
                    self.sessions.push(session);
                    self.active = Some(self.sessions.len() - 1);
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    self.toast(ToastKind::Error, format!("Could not connect to {}: {msg}", c.config.display_name()));
                    self.connect_errors.insert(c.config.id, msg);
                }
            }
        }
        for s in &mut self.sessions {
            s.poll();
        }
        self.toasts
            .retain(|t| t.created.elapsed() < Duration::from_secs(if t.kind == ToastKind::Error { 8 } else { 4 }));
    }

    fn apply_theme(&mut self, ctx: &egui::Context) {
        if self.applied_theme != Some(self.settings.theme) {
            ctx.set_theme(match self.settings.theme {
                ThemeChoice::System => egui::ThemePreference::System,
                ThemeChoice::Light => egui::ThemePreference::Light,
                ThemeChoice::Dark => egui::ThemePreference::Dark,
            });
            self.applied_theme = Some(self.settings.theme);
        }
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let title = match self.active.and_then(|i| self.sessions.get(i)) {
            Some(s) => format!("{} — tableplus-rs", s.display_name()),
            None => "tableplus-rs".to_string(),
        };
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }

    fn toolbar(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        let p = palette(ui);
        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, CornerRadius::ZERO, p.toolbar_bg);
        ui.painter().line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, p.border));
        // The empty toolbar background moves the window (it doubles as the titlebar on macOS).
        let drag = ui.interact(rect, egui::Id::new("toolbar_drag"), Sense::click_and_drag());
        if drag.drag_started() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        if drag.double_clicked() {
            let maximized = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
        }
        ui.horizontal_centered(|ui| {
            // Leave room for the traffic-light buttons on macOS.
            let fullscreen = ui.ctx().input(|i| i.viewport().fullscreen.unwrap_or(false));
            ui.add_space(if cfg!(target_os = "macos") && !fullscreen { 76.0 } else { 6.0 });
            if !self.sessions.is_empty() {
                let home = widgets::icon_button(ui, Icon::Home, "Connections");
                if self.active.is_none() {
                    ui.painter().rect_stroke(
                        home.rect,
                        CornerRadius::same(5),
                        Stroke::new(1.0, p.border),
                        StrokeKind::Inside,
                    );
                }
                if home.clicked() {
                    actions.push(Action::ShowWelcome);
                }
                for (i, s) in self.sessions.iter().enumerate() {
                    let active = self.active == Some(i);
                    if session_chip(ui, s, active, i, actions) {
                        actions.push(Action::SwitchSession(i));
                    }
                }
            }

            // Connection pill: centered in the window when there is room, otherwise in the
            // space between the session chips and the toolbar buttons.
            let left_edge = ui.cursor().left() + 10.0;
            let right_edge = rect.right() - 150.0;
            let pill_w = (right_edge - left_edge).clamp(160.0, 560.0);
            let cx = rect
                .center()
                .x
                .clamp(left_edge + pill_w / 2.0, (right_edge - pill_w / 2.0).max(left_edge + pill_w / 2.0));
            let pill = egui::Rect::from_center_size(egui::pos2(cx, rect.center().y), vec2(pill_w, 26.0));
            let painter = ui.painter();
            painter.rect(
                pill,
                CornerRadius::same(7),
                p.content_bg.gamma_multiply(if ui.visuals().dark_mode { 0.9 } else { 0.85 }),
                Stroke::new(1.0, p.border),
                StrokeKind::Inside,
            );
            let font = theme::ui_font(12.5);
            let bold = theme::bold_font(12.5);
            let mut job = egui::text::LayoutJob::default();
            let fmt =
                |f: &egui::FontId, c: Color32| egui::TextFormat { font_id: f.clone(), color: c, ..Default::default() };
            let mut dot = None;
            if let Some(c) = &self.connecting {
                job.append(&format!("Connecting to {}…", c.config.display_name()), 0.0, fmt(&font, p.text_muted));
            } else if let Some(s) = self.active.and_then(|i| self.sessions.get(i)) {
                dot = widgets::tag_color(s.config.color);
                job.append(&s.display_name(), 0.0, fmt(&bold, p.text));
                job.append("  ", 0.0, fmt(&font, p.text));
                job.append(&s.server_version, 0.0, fmt(&font, p.text_muted));
                if !s.database.is_empty() {
                    job.append(" : ", 0.0, fmt(&font, p.text_faint));
                    job.append(&s.database, 0.0, fmt(&font, p.text_muted));
                }
                if let Some(o) = s.active_object() {
                    job.append(" : ", 0.0, fmt(&font, p.text_faint));
                    job.append(&o.name, 0.0, fmt(&font, p.text_muted));
                }
                if s.config.ssh.enabled {
                    job.append("   via SSH", 0.0, fmt(&font, p.text_faint));
                }
            } else {
                job.append("tableplus-rs", 0.0, fmt(&bold, p.text));
                job.append(
                    &format!("  ·  {} connections", self.store.connections.len()),
                    0.0,
                    fmt(&font, p.text_muted),
                );
            }
            let galley = ui.painter().layout_job(job);
            let mut text_pos = pill.center() - galley.size() / 2.0;
            if let Some(color) = dot {
                text_pos.x += 7.0;
                ui.painter().circle_filled(egui::pos2(text_pos.x - 11.0, pill.center().y), 4.5, color);
            }
            ui.painter().with_clip_rect(pill.shrink(6.0)).galley(text_pos, galley, p.text);
            if self.connecting.is_some() {
                let spin =
                    egui::Rect::from_center_size(egui::pos2(pill.right() - 16.0, pill.center().y), vec2(14.0, 14.0));
                ui.put(spin, egui::Spinner::new().size(12.0));
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(6.0);
                let theme_icon = if ui.visuals().dark_mode { Icon::Moon } else { Icon::Sun };
                let r = widgets::icon_button(ui, theme_icon, "Appearance");
                egui::Popup::menu(&r).show(|ui| {
                    ui.set_min_width(140.0);
                    for choice in [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark] {
                        if ui.selectable_label(self.settings.theme == choice, choice.label()).clicked() {
                            self.settings.theme = choice;
                            self.settings.save(&self.dir);
                        }
                    }
                    ui.separator();
                    if ui.button("Preferences…").clicked() {
                        self.settings_open = true;
                    }
                });
                if self.active.is_some() {
                    if widgets::icon_button(ui, Icon::Refresh, &format!("Reload ({})", shortcut_label("R"))).clicked() {
                        actions.push(Action::RefreshObjects);
                    }
                    if widgets::icon_button(ui, Icon::Sql, &format!("New SQL query ({})", shortcut_label("T")))
                        .clicked()
                    {
                        actions.push(Action::NewQuery(None));
                    }
                }
                if widgets::icon_button(ui, Icon::Search, &format!("Open anything ({})", shortcut_label("K"))).clicked()
                {
                    actions.push(Action::QuickOpen);
                }
            });
        });
    }

    fn handle_shortcuts(&mut self, sc: &Shortcuts, actions: &mut Vec<Action>) {
        if sc.quick_open {
            actions.push(Action::QuickOpen);
        }
        if sc.new_connection {
            actions.push(Action::NewConnection);
        }
        let Some(session) = self.active.and_then(|i| self.sessions.get_mut(i)) else { return };
        if sc.new_query {
            actions.push(Action::NewQuery(None));
        }
        if sc.close_tab
            && let Some(i) = session.active_tab
        {
            actions.push(Action::CloseTab(i));
        }
        if !session.tabs.is_empty() {
            let n = session.tabs.len();
            if sc.next_tab {
                session.active_tab = Some(session.active_tab.map_or(0, |a| (a + 1) % n));
            }
            if sc.prev_tab {
                session.active_tab = Some(session.active_tab.map_or(0, |a| (a + n - 1) % n));
            }
        }
        if sc.refresh && session.active_tab.is_none() {
            actions.push(Action::RefreshObjects);
        }
    }

    fn process(&mut self, ctx: &egui::Context, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Connect(id) => self.connect(id),
                Action::NewConnection => {
                    self.conn_dialog = Some(ConnDialog::new(ConnectionConfig::default(), true, &self.secrets))
                }
                Action::EditConnection(id) => {
                    if let Some(c) = self.store.get(id).cloned() {
                        self.conn_dialog = Some(ConnDialog::new(c, false, &self.secrets));
                    }
                }
                Action::DuplicateConnection(id) => match self.store.duplicate(id) {
                    Ok(Some(copy)) => {
                        for kind in [SecretKind::Password, SecretKind::SshPassword, SecretKind::SshPassphrase] {
                            if let Some(v) = self.secrets.get_for(id, kind) {
                                let _ = self.secrets.set_for(copy.id, kind, &v);
                            }
                        }
                        self.welcome.selected = Some(copy.id);
                    }
                    Ok(None) => {}
                    Err(e) => self.toast(ToastKind::Error, format!("{e:#}")),
                },
                Action::DeleteConnection(id) => {
                    if let Err(e) = self.store.remove(id) {
                        self.toast(ToastKind::Error, format!("{e:#}"));
                    }
                    let _ = self.secrets.delete_all(id);
                }
                Action::ShowWelcome => self.active = None,
                Action::SwitchSession(i) => self.active = Some(i),
                Action::CloseSession(i) => {
                    if i < self.sessions.len() {
                        self.sessions.remove(i);
                        self.active = match self.active {
                            Some(a) if a == i => (!self.sessions.is_empty()).then(|| i.min(self.sessions.len() - 1)),
                            Some(a) if a > i => Some(a - 1),
                            other => other,
                        };
                    }
                }
                Action::OpenObject { object, structure } => {
                    let page = self.settings.page_size;
                    if let Some(s) = self.active.and_then(|i| self.sessions.get_mut(i)) {
                        s.open_object(&self.spawner, &object, structure, page);
                        if object.kind.is_relation() {
                            s.ensure_structure(&self.spawner, &object.schema, &object.name);
                        }
                    }
                }
                Action::ShowDefinition(object) => {
                    if let Some(s) = self.active.and_then(|i| self.sessions.get_mut(i)) {
                        s.open_definition(&self.spawner, &object);
                    }
                }
                Action::NewQuery(sql) => {
                    if let Some(s) = self.active.and_then(|i| self.sessions.get_mut(i)) {
                        s.new_query(sql);
                    }
                }
                Action::RecordHistory(entry) => {
                    if let Err(e) = self.history.push(entry) {
                        tracing::warn!("could not save history: {e:#}");
                    }
                }
                Action::ClearHistory => {
                    let _ = self.history.clear();
                }
                Action::Toast(kind, text) => self.toast(kind, text),
                Action::RefreshObjects => {
                    if let Some(s) = self.active.and_then(|i| self.sessions.get_mut(i)) {
                        s.reload_objects(&self.spawner);
                        s.catalog.structures.clear();
                    }
                }
                Action::ReloadTable(object) => {
                    if let Some(s) = self.active.and_then(|i| self.sessions.get_mut(i)) {
                        for tab in &mut s.tabs {
                            if let TabKind::Table(t) = &mut tab.kind
                                && t.object == object
                            {
                                t.reload(&self.spawner, true);
                            }
                        }
                    }
                }
                Action::InvalidateStructure { schema, table } => {
                    if let Some(s) = self.active.and_then(|i| self.sessions.get_mut(i)) {
                        s.invalidate_structure(&schema, &table);
                        s.ensure_structure(&self.spawner, &schema, &table);
                    }
                }
                Action::LoadStructure { schema, table } => {
                    if let Some(s) = self.active.and_then(|i| self.sessions.get_mut(i)) {
                        s.ensure_structure(&self.spawner, &schema, &table);
                    }
                }
                Action::Export(source) => {
                    if let Some(s) = self.active.and_then(|i| self.sessions.get(i)) {
                        self.export_dialog = Some(ExportDialog::new(source, s.conn.clone(), s.dialect));
                    }
                }
                Action::Import(object) => {
                    if let Some(s) = self.active.and_then(|i| self.sessions.get(i)) {
                        self.import_dialog = Some(ImportDialog::new(object, s.conn.clone(), s.dialect));
                    }
                }
                Action::QuickOpen => {
                    if self.quick_open.is_none() {
                        self.quick_open = Some(QuickOpen::default());
                    } else {
                        self.quick_open = None;
                    }
                }
                Action::CloseTab(i) => {
                    if let Some(s) = self.active.and_then(|i| self.sessions.get_mut(i)) {
                        s.close_tab(i);
                    }
                }
            }
        }
        let _ = ctx;
    }

    fn dialogs(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        if let Some(dialog) = &mut self.conn_dialog {
            match dialog.show(ctx, &self.spawner) {
                DialogOutcome::Open => {}
                DialogOutcome::Cancel => self.conn_dialog = None,
                DialogOutcome::Save { connect } => {
                    let dialog = self.conn_dialog.take().expect("dialog");
                    let config = dialog.config.clone();
                    if let Err(e) = dialog.save_secrets(&self.secrets) {
                        self.toast(
                            ToastKind::Error,
                            format!("Could not store password in {}: {e:#}", self.secrets.backend_name()),
                        );
                    }
                    match self.store.upsert(config.clone()) {
                        Ok(()) => {
                            self.welcome.selected = Some(config.id);
                            if connect {
                                if dialog.config.save_password || config.driver.is_file_based() {
                                    actions.push(Action::Connect(config.id));
                                } else {
                                    self.start_connect(config, dialog.credentials());
                                }
                            }
                        }
                        Err(e) => self.toast(ToastKind::Error, format!("Could not save connection: {e:#}")),
                    }
                }
            }
        }
        if let Some(prompt) = &mut self.password_prompt {
            let mut submit = false;
            let mut cancel = false;
            crate::widgets::modal(ctx, "password_prompt").show(ctx, |ui| {
                ui.set_width(340.0);
                ui.heading(format!("Password for {}", prompt.config.display_name()));
                ui.label(widgets::muted(ui, prompt.config.summary()));
                ui.add_space(8.0);
                let r = ui
                    .add(egui::TextEdit::singleline(&mut prompt.password).password(true).desired_width(f32::INFINITY));
                r.request_focus();
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    submit = true;
                }
                ui.checkbox(&mut prompt.remember, crate::util::SAVE_PASSWORD_LABEL);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if widgets::primary_button(ui, "Connect").clicked() {
                        submit = true;
                    }
                    if widgets::button(ui, "Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        cancel = true;
                    }
                });
            });
            if submit {
                let prompt = self.password_prompt.take().expect("prompt");
                if prompt.remember {
                    let _ = self.secrets.set_for(prompt.config.id, SecretKind::Password, &prompt.password);
                }
                let mut creds = self.credentials(&prompt.config);
                creds.password = Some(prompt.password);
                self.start_connect(prompt.config, creds);
            } else if cancel {
                self.password_prompt = None;
            }
        }
        if let Some(q) = &mut self.quick_open {
            let session = self.active.and_then(|i| self.sessions.get(i));
            match q.show(ctx, session, &self.store.connections) {
                None => {}
                Some(QuickPick::Close) => self.quick_open = None,
                Some(QuickPick::Object(o)) => {
                    self.quick_open = None;
                    actions.push(Action::OpenObject { object: o, structure: false });
                }
                Some(QuickPick::Connection(id)) => {
                    self.quick_open = None;
                    actions.push(Action::Connect(id));
                }
                Some(QuickPick::Tab(i)) => {
                    self.quick_open = None;
                    if let Some(s) = self.active.and_then(|i| self.sessions.get_mut(i)) {
                        s.active_tab = Some(i);
                    }
                }
            }
        }
        if let Some(d) = &mut self.export_dialog
            && !d.show(ctx, &self.spawner, actions)
        {
            self.export_dialog = None;
        }
        if let Some(d) = &mut self.import_dialog
            && !d.show(ctx, &self.spawner, actions)
        {
            self.import_dialog = None;
        }
        if self.settings_open {
            let mut open = true;
            let before = self.settings.clone();
            egui::Window::new("Preferences")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .default_width(360.0)
                .show(ctx, |ui| {
                    egui::Grid::new("prefs").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
                        ui.label("Appearance");
                        widgets::segmented(
                            ui,
                            &mut self.settings.theme,
                            &[
                                (ThemeChoice::System, "System"),
                                (ThemeChoice::Light, "Light"),
                                (ThemeChoice::Dark, "Dark"),
                            ],
                        );
                        ui.end_row();
                        ui.label("Rows per page");
                        ui.add(egui::DragValue::new(&mut self.settings.page_size).range(50..=100_000).speed(10));
                        ui.end_row();
                        ui.label("Query row limit");
                        ui.add(
                            egui::DragValue::new(&mut self.settings.query_row_limit).range(0..=10_000_000).speed(100),
                        )
                        .on_hover_text("Maximum rows kept per statement in the SQL editor (0 = unlimited)");
                        ui.end_row();
                        ui.label("Editor font size");
                        ui.add(egui::DragValue::new(&mut self.settings.editor_font_size).range(9.0..=24.0).speed(0.2));
                        ui.end_row();
                        ui.label("Secrets");
                        ui.label(widgets::muted(ui, self.secrets.backend_name()));
                        ui.end_row();
                        ui.label("Data directory");
                        ui.label(widgets::muted(ui, self.dir.display().to_string()));
                        ui.end_row();
                    });
                });
            if self.settings != before {
                self.settings.save(&self.dir);
            }
            self.settings_open = open;
        }
    }

    fn paint_toasts(&self, ctx: &egui::Context) {
        if self.toasts.is_empty() {
            return;
        }
        let dark = ctx.global_style().visuals.dark_mode;
        let p = theme::palette_for(dark);
        let screen = ctx.content_rect();
        let mut y = screen.bottom() - 44.0;
        for (i, t) in self.toasts.iter().rev().take(4).enumerate() {
            egui::Area::new(egui::Id::new(("toast", i)))
                .order(egui::Order::Tooltip)
                .fixed_pos(egui::pos2(screen.right() - 16.0, y))
                .pivot(Align2::RIGHT_BOTTOM)
                .interactable(false)
                .show(ctx, |ui| {
                    let color = match t.kind {
                        ToastKind::Info => p.accent,
                        ToastKind::Success => p.success,
                        ToastKind::Error => p.danger,
                    };
                    egui::Frame::new()
                        .fill(p.content_bg)
                        .stroke(Stroke::new(1.0, p.border))
                        .corner_radius(CornerRadius::same(8))
                        .shadow(ui.visuals().popup_shadow)
                        .inner_margin(egui::Margin::symmetric(12, 9))
                        .show(ui, |ui| {
                            ui.set_max_width(420.0);
                            ui.horizontal(|ui| {
                                let (r, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                                ui.painter().circle_filled(r.center(), 4.0, color);
                                ui.add(egui::Label::new(RichText::new(&t.text).color(p.text)).wrap());
                            });
                        });
                });
            y -= 52.0;
        }
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

/// A session switcher chip in the toolbar. Returns true when clicked.
fn session_chip(ui: &mut Ui, s: &Session, active: bool, index: usize, actions: &mut Vec<Action>) -> bool {
    let p = palette(ui);
    let font = theme::ui_font(12.0);
    let mut name = s.display_name();
    if name.chars().count() > 22 {
        name = name.chars().take(21).collect::<String>() + "…";
    }
    let galley = ui.painter().layout_no_wrap(name, font, if active { p.text } else { p.text_muted });
    let size = vec2(galley.size().x + 44.0, 24.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if active {
        ui.painter().rect(rect, CornerRadius::same(6), p.content_bg, Stroke::new(1.0, p.border), StrokeKind::Inside);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(6), p.hover);
    }
    let dot = widgets::tag_color(s.config.color).unwrap_or(p.text_faint);
    ui.painter().circle_filled(egui::pos2(rect.left() + 11.0, rect.center().y), 4.0, dot);
    ui.painter().galley(egui::pos2(rect.left() + 20.0, rect.center().y - galley.size().y / 2.0), galley, p.text);
    let close = egui::Rect::from_center_size(egui::pos2(rect.right() - 12.0, rect.center().y), vec2(14.0, 14.0));
    let close_resp = ui.interact(close, ui.id().with(("close_session", s.key)), Sense::click());
    if resp.hovered() || close_resp.hovered() {
        if close_resp.hovered() {
            ui.painter().rect_filled(close, CornerRadius::same(4), p.border);
        }
        icons::paint(ui.painter(), close.shrink(2.0), Icon::Close, p.text_muted);
    }
    if close_resp.clicked() {
        actions.push(Action::CloseSession(index));
        return false;
    }
    resp.on_hover_text(s.config.summary()).clicked()
}

impl eframe::App for TablePlusApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.apply_theme(&ctx);
        self.poll();
        self.update_title(&ctx);
        let modal_open = self.conn_dialog.is_some()
            || self.password_prompt.is_some()
            || self.export_dialog.is_some()
            || self.import_dialog.is_some()
            || self.settings_open;
        #[allow(unused_mut)]
        let mut shortcuts = if modal_open { Shortcuts::default() } else { Shortcuts::read(&ctx) };
        let mut actions = Vec::new();
        #[cfg(target_os = "macos")]
        if let Some(menu) = &self.menu {
            let mut theme = None;
            let mut menu_shortcuts = Shortcuts::default();
            menu.poll(&mut menu_shortcuts, &mut actions, &mut theme, &mut self.settings_open);
            if !modal_open {
                shortcuts.merge(menu_shortcuts);
            }
            if let Some(t) = theme {
                self.settings.theme = t;
                self.settings.save(&self.dir);
            }
        }
        self.handle_shortcuts(&shortcuts, &mut actions);

        egui::Panel::top("toolbar").exact_size(40.0).frame(egui::Frame::NONE).show(ui, |ui| {
            self.toolbar(ui, &mut actions);
        });

        match self.active {
            Some(i) if i < self.sessions.len() => {
                let session = &mut self.sessions[i];
                crate::workspace::show(
                    ui,
                    session,
                    &self.spawner,
                    &self.settings,
                    &self.history,
                    &shortcuts,
                    &mut actions,
                );
            }
            _ => {
                let busy = self.connecting.as_ref().map(|c| c.config.id);
                let keyboard = !modal_open && self.quick_open.is_none();
                let view = crate::welcome::WelcomeView {
                    connections: &self.store.connections,
                    connecting: busy,
                    errors: &self.connect_errors,
                    secrets_backend: self.secrets.backend_name(),
                    keyboard,
                };
                crate::welcome::show(ui, &mut self.welcome, view, &mut actions);
            }
        }

        self.dialogs(&ctx, &mut actions);
        self.paint_toasts(&ctx);
        self.process(&ctx, actions);
    }

    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        self.settings.save(&self.dir);
    }
}
