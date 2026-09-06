//! The Settings tab. Edits happen on a draft; Apply validates, saves, and
//! hands the new config to the app.

use crate::keychain;
use crate::tips::{self, tip_row, Tipped};
use crate::ui_util::hint;
use quicksearch_core::config::{ColumnsConfig, Config};

/// Who a row is for.
///
/// [`Level::Advanced`] means one of two things, and usually both: a person who
/// indexed their home folder and nothing else will never need to change it, or
/// they could not tell what it does without already knowing how the indexer
/// works. A byte budget over the writer's batching is both. Password
/// protection is neither, however technical it sounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    Everyday,
    Advanced,
}

/// The settings form's render context: which rows are on screen.
///
/// Every row goes through [`Form`], which is what keeps the two lists from
/// drifting — a setting cannot be added to the tab without saying who it is
/// for, and it cannot be shown without a tooltip either, because
/// [`tips::tip_row`] is the only way through.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Form {
    pub advanced: bool,
}

impl Form {
    fn shows(self, level: Level) -> bool {
        level == Level::Everyday || self.advanced
    }

    fn row(
        self,
        level: Level,
        ui: &mut egui::Ui,
        label: impl Into<egui::WidgetText>,
        tip: &'static tips::Tip,
        widget: impl FnOnce(&mut egui::Ui) -> egui::Response,
    ) {
        if self.shows(level) {
            tip_row(ui, label, tip, widget);
        }
    }

    fn drag<N: egui::emath::Numeric>(
        self,
        level: Level,
        ui: &mut egui::Ui,
        label: impl Into<egui::WidgetText>,
        tip: &'static tips::Tip,
        value: &mut N,
        range: std::ops::RangeInclusive<N>,
    ) {
        self.row(level, ui, label, tip, |ui| {
            ui.add(egui::DragValue::new(value).range(range))
        });
    }
}

/// A label ruled underneath in the palette's orange.
///
/// For the advanced toggle, which sits in the same two-column form as the
/// settings but is not one of them — it decides which of them are on screen.
/// The rule marks that difference without a second type size or a box: the
/// text keeps the ordinary label color, so it reads as part of the form.
fn accented_label(ui: &egui::Ui, text: &str) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    job.append(
        text,
        0.0,
        egui::text::TextFormat {
            font_id: egui::TextStyle::Body.resolve(ui.style()),
            color: ui.visuals().text_color(),
            underline: egui::Stroke::new(1.0, crate::color::palette(ui.visuals().dark_mode).orange),
            ..Default::default()
        },
    );
    job
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Indexing,
    Processing,
    Search,
}

/// Not draft edits: every action runs its own explicit flow in the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityAction {
    Enable,
    Disable,
    ChangePassword,
    SetKeychain(bool),
    ShowKey,
}

#[derive(Default)]
pub struct SettingsOutput {
    pub applied: Option<Config>,
    pub security: Option<SecurityAction>,
    /// Like Security, edits the live config, so it takes effect without Apply.
    pub columns: Option<ColumnsConfig>,
    /// Also live: a view preference should not need an Apply to look at, and
    /// drafting it would make merely revealing a setting read as an edit.
    pub show_advanced: Option<bool>,
}

pub struct SettingsTab {
    /// Staged the first frame the tab is shown; dropped when it is left.
    draft: Option<Config>,
    /// The `use_keychain` preference the cached probe answer was taken under.
    keychain_probed_for: Option<bool>,
    keychain_active: bool,
    /// The search-shortcut button is waiting for a key press to bind.
    capturing_hotkey: bool,
}

impl SettingsTab {
    pub fn new() -> SettingsTab {
        SettingsTab {
            draft: None,
            keychain_probed_for: None,
            keychain_active: false,
            capturing_hotkey: false,
        }
    }

    /// Whether the shortcut button is reading a key press, so the app can
    /// hold the shortcut it is about to replace.
    pub fn capturing_hotkey(&self) -> bool {
        self.capturing_hotkey
    }

    /// The fields the app pins on apply are neutralized first, so the
    /// Security block never makes the tab read as dirty.
    pub fn is_dirty(&self, current: &Config) -> bool {
        let Some(draft) = &self.draft else {
            return false;
        };
        let mut d = draft.clone();
        crate::app::pin_live_fields(&mut d, current);
        d != *current
    }

    pub fn draft_config(&self) -> Option<Config> {
        self.draft.clone()
    }

    /// Drop the draft; the next frame stages a fresh copy of the live
    /// config, which keeps a draft from going stale against other tabs.
    pub fn discard(&mut self) {
        self.draft = None;
        self.capturing_hotkey = false;
        self.keychain_probed_for = None;
    }

    fn stage(&mut self, current: &Config) {
        if self.draft.is_none() {
            self.draft = Some(current.clone());
        }
    }

    /// True when the key really is in the OS keychain: the preference is on
    /// *and* the keychain answers (a dead daemon or locked keyring reads as
    /// "no"). Probed sparingly — a keychain read is an IPC round trip.
    fn keychain_active(&mut self, current: &Config) -> bool {
        if self.keychain_probed_for != Some(current.security.use_keychain) {
            let db_path = current.resolved_database_path();
            self.keychain_active = current.security.use_keychain
                && matches!(keychain::load_key(&db_path.to_string_lossy()), Ok(Some(_)));
            self.keychain_probed_for = Some(current.security.use_keychain);
        }
        self.keychain_active
    }

    /// `indexed_files` is the coordinator's count, used only to show what the
    /// automatic search cache works out to for *this* index; `None` while it
    /// is not yet known.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        current: &Config,
        indexed_files: Option<i64>,
    ) -> SettingsOutput {
        self.stage(current);
        let mut out = SettingsOutput::default();
        let keychain_active = self.keychain_active(current);
        let dirty = self.is_dirty(current);
        let capturing = &mut self.capturing_hotkey;
        let draft = self.draft.as_mut().unwrap();

        // Live, like the columns below: read from the saved config, not the
        // draft, so ticking it reveals the rows at once instead of after Apply.
        let form = Form {
            advanced: current.ui.show_advanced_settings,
        };

        let scroll = egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                // A maximized window would stretch every hint into one line.
                ui.set_max_width(620.0);

                // The same two-column shape as every settings row — label
                // left, control right — so it reads as part of the form; the
                // orange rule is what says it governs the form rather than
                // belonging to it.
                let label = accented_label(ui, "Show advanced settings");
                egui::Grid::new("opt-advanced")
                    .num_columns(2)
                    .show(ui, |ui| {
                        tip_row(ui, label, &tips::SHOW_ADVANCED, |ui| {
                            let mut advanced = form.advanced;
                            let response = ui.checkbox(&mut advanced, "");
                            if response.changed() {
                                out.show_advanced = Some(advanced);
                            }
                            response
                        });
                    });
                ui.label(hint(
                    "Advanced settings control how the index is built, stored \
                     and searched. The defaults suit almost every installation.",
                ));
                ui.separator();

                // The whole section, heading and all: its only row is the
                // database path.
                if form.advanced {
                    ui.heading(egui::RichText::new("Paths").strong());
                    egui::Grid::new("opt-paths").num_columns(2).show(ui, |ui| {
                        form.row(
                            Level::Advanced,
                            ui,
                            "Database file",
                            &tips::DATABASE_PATH,
                            |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut draft.paths.database_path)
                                        .desired_width(260.0),
                                )
                            },
                        );
                    });
                    ui.label(hint("Indexed folders are managed on the Manage Index tab."));
                    ui.separator();
                }

                ui.heading(egui::RichText::new("Indexing").strong());
                config_editor_ui(ui, draft, Section::Indexing, indexed_files, form);
                ui.label(hint(
                    "Automatic and manual indexing are switched on the \
                     Manage Index tab.",
                ));
                ui.separator();

                ui.heading(egui::RichText::new("Search").strong());
                config_editor_ui(ui, draft, Section::Search, indexed_files, form);
                ui.add_space(6.0);
                // Live, not drafted — see `columns_ui`.
                out.columns = columns_ui(ui, &current.search.columns);
                ui.separator();

                ui.heading(egui::RichText::new("Interface").strong());
                egui::Grid::new("opt-ui").num_columns(2).show(ui, |ui| {
                    form.row(Level::Everyday, ui, "UI scale", &tips::UI_SCALE, |ui| {
                        ui.add(
                            egui::Slider::new(&mut draft.ui.scale, crate::app::SCALE_RANGE)
                                .step_by(0.05)
                                .fixed_decimals(2),
                        )
                    });
                    form.row(
                        Level::Everyday,
                        ui,
                        "Search shortcut",
                        &tips::SEARCH_HOTKEY,
                        |ui| hotkey_edit(ui, &mut draft.ui.search_hotkey, capturing),
                    );
                    form.row(
                        Level::Everyday,
                        ui,
                        "Color scheme",
                        &tips::COLOR_SCHEME,
                        |ui| color_scheme_edit(ui, &mut draft.ui.color_scheme),
                    );
                });
                hotkey_note(ui, &draft.ui.search_hotkey, &current.ui.search_hotkey);
                shortcut_note(ui, &current.ui.search_hotkey);
                ui.separator();

                // Security acts on the live config, not the draft; the KDF
                // salt is never shown anywhere in the GUI.
                ui.heading(egui::RichText::new("Security").strong());
                out.security = security_ui(ui, current, keychain_active, form);
                ui.separator();

                ui.heading(egui::RichText::new("Processing").strong());
                config_editor_ui(ui, draft, Section::Processing, indexed_files, form);
                ui.separator();

                let p = crate::color::palette(ui.visuals().dark_mode);
                ui.horizontal(|ui| {
                    let apply = ui
                        .add(crate::ui_util::bordered_button(
                            "Apply & Save",
                            if dirty { p.orange } else { p.blue },
                        ))
                        .tip(&tips::APPLY_SAVE);
                    if apply.clicked() {
                        out.applied = Some(draft.clone());
                    }
                    // Comes and goes with the dirty state.
                    crate::ui_util::stable_section(ui, |ui| {
                        if dirty {
                            ui.label(
                                egui::RichText::new("Unsaved changes")
                                    .small()
                                    .color(p.orange),
                            );
                        }
                    });
                });
                // The second sentence names two rows that are only on screen
                // with advanced settings shown.
                ui.label(hint(if form.advanced {
                    "Narrowing a filter removes the entries it excludes; widening \
                     one reindexes to find what it now allows. Only the tokenizer \
                     and hash length require a full rebuild."
                } else {
                    "Narrowing a filter removes the entries it excludes; widening \
                     one reindexes to find what it now allows."
                }));
            });
        crate::ui_util::more_below_hint(ui, &scroll);

        out
    }
}

const COLOR_SCHEMES: [(&str, &str); 2] = [("dark", "Dark"), ("light", "Light")];

/// Via [`crate::app::theme_for`], so the box says what the app will actually do.
fn scheme_label(value: &str) -> &'static str {
    match crate::app::theme_for(value) {
        egui::Theme::Dark => "Dark",
        egui::Theme::Light => "Light",
    }
}

fn color_scheme_edit(ui: &mut egui::Ui, setting: &mut String) -> egui::Response {
    egui::ComboBox::from_id_salt("cfg-color-scheme")
        .selected_text(scheme_label(setting))
        .show_ui(ui, |ui| {
            for (stored, label) in COLOR_SCHEMES {
                ui.selectable_value(setting, stored.to_string(), label);
            }
        })
        .response
}

/// A button showing the current binding that turns into a key-press reader
/// when clicked, and a Clear beside it.
/// The shortcut button: click it, press a combination, or Clear. Shared with
/// the tour's shortcut page, which offers the same setting.
pub(crate) fn hotkey_edit(
    ui: &mut egui::Ui,
    setting: &mut String,
    capturing: &mut bool,
) -> egui::Response {
    let p = crate::color::palette(ui.visuals().dark_mode);
    ui.horizontal(|ui| {
        let label = if *capturing {
            "Press a key combination…".to_string()
        } else if setting.trim().is_empty() {
            "None".to_string()
        } else {
            setting.clone()
        };
        let button = ui.add(crate::ui_util::bordered_button(
            label,
            if *capturing { p.orange } else { p.blue },
        ));
        // A second click backs out.
        if button.clicked() {
            *capturing = !*capturing;
        } else if *capturing {
            match read_capture(ui) {
                Some(Some(binding)) => {
                    *setting = binding.to_string();
                    *capturing = false;
                }
                Some(None) => *capturing = false,
                None => {}
            }
        }
        if ui
            .add_enabled(
                !setting.trim().is_empty(),
                egui::Button::new("Clear").small(),
            )
            .clicked()
        {
            setting.clear();
            *capturing = false;
        }
        button
    })
    .inner
}

/// One frame of capture: `Some(Some(binding))` for a press worth binding,
/// `Some(None)` for a cancel, `None` while nothing usable arrived. Raw
/// events, because egui's shortcut matching cannot report an arbitrary
/// combination; invalid presses are ignored, not treated as a cancel.
fn read_capture(ui: &egui::Ui) -> Option<Option<crate::hotkey::Binding>> {
    ui.input(|i| {
        for event in &i.events {
            let egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } = event
            else {
                continue;
            };
            if *key == egui::Key::Escape {
                return Some(None);
            }
            if let Some(binding) = crate::hotkey::Binding::from_egui(*key, modifiers) {
                return Some(Some(binding));
            }
        }
        None
    })
}

/// Silent while registered and working; a line appears only when what is on
/// the button is not what is in force.
fn hotkey_note(ui: &mut egui::Ui, draft: &str, live: &str) {
    let (text, is_error) = if draft.trim() != live.trim() {
        ("Not registered until Apply and Save.".to_string(), false)
    } else {
        hotkey_status_line(crate::hotkey::status())
    };
    let color = is_error.then(|| crate::color::palette(ui.visuals().dark_mode).orange);
    crate::ui_util::stable_section(ui, |ui| {
        if text.is_empty() {
            return;
        }
        let rich = egui::RichText::new(text).small();
        ui.label(match color {
            Some(color) => rich.color(color),
            None => rich.weak(),
        });
    });
}

/// The sentence for a registration state, and whether it is an error worth
/// the warning colour. Split from the rendering so the states are testable.
fn hotkey_status_line(status: crate::hotkey::Status) -> (String, bool) {
    use crate::hotkey::Status;
    match status {
        Status::Disabled | Status::Active => (String::new(), false),
        Status::SystemOwned => (
            "Your desktop delivers this key: it starts QuickSearch when \
             closed and brings it forward when running."
                .to_string(),
            false,
        ),
        Status::DesktopOnly => (
            "On Wayland only your desktop can hold a global key. Use \
             \"Set up system shortcut\" below, or bind the command shown \
             there by hand."
                .to_string(),
            false,
        ),
        // On Windows a failed registration is the *expected* state whenever
        // the Start-menu shortcut owns the same key — Explorer registers it
        // at logon, wins, and every press then reaches us through the
        // `--toggle` relay anyway. An error color would cry wolf on every
        // installed copy.
        Status::Error(why) if cfg!(windows) => (
            format!(
                "Another program holds this key ({}) — usually the Start \
                 menu shortcut that starts QuickSearch, which also brings \
                 it forward while it is running. If the key does nothing, \
                 pick a different combination.",
                why
            ),
            false,
        ),
        Status::Error(why) => (format!("The shortcut is not active: {}.", why), true),
    }
}

/// How to get a shortcut that also *starts* QuickSearch.
///
/// The shortcut above is ours and needs no setup, but it cannot fire while
/// QuickSearch is not running — see `crate::activate`. Only the desktop can
/// bind a key that launches something. Where the desktop's own configuration
/// has a known home for that binding (`crate::shortcut_setup`), one button
/// writes it — into the place the desktop's settings UI lists and edits, so
/// it stays the user's to see and change. Everywhere else this says what to
/// bind and opens the place to bind it.
///
/// `hotkey_setting` is the shortcut in force, which the one-click binding
/// mirrors system-wide.
///
/// Shared with the tour's shortcut page, which puts it under the same
/// shortcut button this sentence says is "above".
pub(crate) fn shortcut_note(ui: &mut egui::Ui, hotkey_setting: &str) {
    // Tests reach this through the tour's shortcut page; probing the real
    // desktop there would make them depend on the machine they run on and
    // spawn `gsettings`. Tests that want a desktop pin one through
    // `shortcut_note_for`.
    let desktop = if cfg!(test) {
        crate::shortcut_setup::Desktop::Unsupported
    } else {
        crate::shortcut_setup::detect()
    };
    shortcut_note_for(ui, hotkey_setting, desktop);
}

/// One frame's answer from [`crate::shortcut_setup`], cached: `installed`
/// probes the desktop with a subprocess, which must not run per frame.
#[derive(Clone)]
struct SystemShortcutState {
    installed: bool,
    /// The last install/remove outcome, `(succeeded, what to say)`.
    feedback: Option<(bool, String)>,
}

/// The desktop split out of the environment so tests can pick one.
fn shortcut_note_for(ui: &mut egui::Ui, hotkey_setting: &str, desktop: crate::shortcut_setup::Desktop) {
    let command = format!("{} --toggle", crate::activate::command_name());
    crate::ui_util::stable_section(ui, |ui| {
        // One-click only with a key to write: an unset or unparseable
        // shortcut leaves nothing to bind system-wide.
        let binding = crate::hotkey::parse_setting(hotkey_setting).ok().flatten();
        let one_click = binding.filter(|_| desktop != crate::shortcut_setup::Desktop::Unsupported);

        if let Some(binding) = one_click {
            // One id for the settings tab and the tour: they show one fact.
            let id = egui::Id::new("system-shortcut-state");
            let mut state = ui
                .data_mut(|d| d.get_temp::<SystemShortcutState>(id))
                .unwrap_or_else(|| SystemShortcutState {
                    installed: crate::shortcut_setup::installed(),
                    feedback: None,
                });
            ui.label(
                egui::RichText::new(
                    "The shortcut above works while QuickSearch is open. Your \
                     desktop can also bind it to start QuickSearch when it is \
                     not:",
                )
                .small()
                .weak(),
            );
            ui.horizontal_wrapped(|ui| {
                if state.installed {
                    ui.label(egui::RichText::new("The system shortcut is set up.").small());
                    if ui.add(egui::Button::new("Remove").small()).clicked() {
                        match crate::shortcut_setup::remove() {
                            Ok(()) => {
                                state.installed = false;
                                state.feedback =
                                    Some((true, "System shortcut removed.".to_string()));
                                // Take the key back in-app now, not at the
                                // next save.
                                crate::hotkey::apply(hotkey_setting);
                            }
                            Err(e) => state.feedback = Some((false, e)),
                        }
                    }
                } else if ui
                    .add(egui::Button::new(format!("Set up {} system-wide", binding)).small())
                    .clicked()
                {
                    match crate::shortcut_setup::install(&binding) {
                        Ok(when) => {
                            state.installed = true;
                            state.feedback = Some((
                                true,
                                match when {
                                    crate::shortcut_setup::Installed::Immediately => {
                                        "Added to your desktop's keyboard shortcuts."
                                            .to_string()
                                    }
                                    crate::shortcut_setup::Installed::AfterRelogin => {
                                        "Added to your desktop's keyboard shortcuts; \
                                         the key starts answering after you next log in."
                                            .to_string()
                                    }
                                },
                            ));
                            // Stand the in-app registration down at once so
                            // the desktop's binding is not fought for the key
                            // (see `hotkey::Status::SystemOwned`).
                            crate::hotkey::apply(hotkey_setting);
                        }
                        Err(e) => state.feedback = Some((false, e)),
                    }
                }
            });
            if let Some((ok, text)) = &state.feedback {
                let rich = egui::RichText::new(text).small();
                ui.label(if *ok {
                    rich.weak()
                } else {
                    rich.color(crate::color::palette(ui.visuals().dark_mode).orange)
                });
            }
            ui.data_mut(|d| d.insert_temp(id, state));
            ui.label(
                egui::RichText::new("Or bind this command there yourself:")
                    .small()
                    .weak(),
            );
        } else {
            ui.label(
                egui::RichText::new(
                    "The shortcut above works while QuickSearch is open. To have a key \
                     start it as well, bind this command in your desktop's keyboard \
                     settings:",
                )
                .small()
                .weak(),
            );
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(&command).small().monospace());
            if ui.add(egui::Button::new("Copy").small()).clicked() {
                ui.ctx().copy_text(command.clone());
            }
            if let Some(label) = crate::platform::keyboard_settings_label() {
                if ui.add(egui::Button::new(label).small()).clicked() {
                    crate::platform::open_keyboard_settings();
                }
            }
        });
        if crate::activate::raise::is_wayland() {
            ui.label(
                egui::RichText::new(
                    "On Wayland, focus follows the shortcut's activation token: \
                     the binding set up above passes one along, so QuickSearch \
                     comes to the front. A binding made by hand whose launcher \
                     provides no token can only highlight QuickSearch in the \
                     task bar.",
                )
                .small()
                .weak(),
            );
        }
    });
}

/// The Search-tab column picker, mirroring the header right-click menu.
/// Acts on the **live** config: the header menu writes columns the instant
/// they change, and a draft-backed copy here would silently revert that on
/// the next Apply.
fn columns_ui(ui: &mut egui::Ui, current: &ColumnsConfig) -> Option<ColumnsConfig> {
    let mut next = current.clone();
    ui.label("Search columns").on_hover_text(tips::COLUMNS.body);
    ui.horizontal_wrapped(|ui| {
        ui.checkbox(&mut next.name, "Name").tip(&tips::COLUMNS);
        ui.add_enabled(false, egui::Checkbox::new(&mut true, "Path"))
            .on_disabled_hover_text(
                "The path is always shown — it is the only column that \
                 identifies a result on its own.",
            );
        ui.checkbox(&mut next.content_match, "Content Match")
            .tip(&tips::COLUMNS);
        ui.checkbox(&mut next.size, "Size").tip(&tips::COLUMNS);
        ui.checkbox(&mut next.modified, "Modified")
            .tip(&tips::COLUMNS);
        ui.checkbox(&mut next.rank, "Rank").tip(&tips::COLUMNS);
    });
    ui.label(hint(
        "Also on the Search tab: right-click any column header. Applied and \
         saved immediately.",
    ));
    (next != *current).then_some(next)
}

/// Never renders the salt.
fn security_ui(
    ui: &mut egui::Ui,
    current: &Config,
    keychain_active: bool,
    form: Form,
) -> Option<SecurityAction> {
    let mut action = None;
    if current.security.password_protected {
        if keychain_active {
            ui.label(
                "The index is encrypted; its password is securely stored by \
                 your Operating System.",
            );
        } else {
            ui.label("The index is encrypted; a password is required at startup.");
        }
        ui.horizontal(|ui| {
            if ui
                .button("Change password")
                .tip(&tips::CHANGE_PASSWORD)
                .clicked()
            {
                action = Some(SecurityAction::ChangePassword);
            }
            if ui
                .button("Disable protection")
                .tip(&tips::DISABLE_PASSWORD)
                .clicked()
            {
                action = Some(SecurityAction::Disable);
            }
            // The raw key is for someone recovering the file by hand; the
            // password controls beside it are for everyone.
            if form.advanced
                && ui
                    .button("Show database key")
                    .tip(&tips::SHOW_KEY)
                    .clicked()
            {
                action = Some(SecurityAction::ShowKey);
            }
        });
        let mut remember = current.security.use_keychain;
        if ui
            .checkbox(&mut remember, "Remember on this device")
            .tip(&tips::REMEMBER_KEYCHAIN)
            .changed()
        {
            action = Some(SecurityAction::SetKeychain(remember));
        }
    } else {
        ui.label("The index is not encrypted.");
        if ui
            .button("Enable password protection")
            .tip(&tips::ENABLE_PASSWORD)
            .clicked()
        {
            action = Some(SecurityAction::Enable);
        }
        ui.label(hint(
            "The index stores the names and text of your files. A password \
                 encrypts it on disk; enabling one rebuilds the index.",
        ));
    }
    action
}

/// What the automatic search cache resolves to, as a sentence. `None` when the
/// file count is not known yet, in which case the row shows nothing rather
/// than a number that would be wrong.
fn search_cache_hint(config: &Config, indexed_files: Option<i64>) -> Option<String> {
    use quicksearch_core::db::schema::{
        recommended_search_cache_mib, SEARCH_CACHE_BYTES_PER_FILE, SEARCH_CACHE_MAX_MIB,
    };
    if config.search.cache_size_mib != 0 {
        return None;
    }
    let files = indexed_files?;
    let keyed = config.security.password_protected;
    let mib = recommended_search_cache_mib(files, keyed);
    if !keyed {
        return Some(format!(
            "Automatic: {} MiB. An unencrypted index reads a cache miss \
             straight from the operating system, so a larger cache measures no \
             faster.",
            mib
        ));
    }
    let counted = crate::format::group_thousands(files.max(0) as u64);
    // Past ~800k files the automatic value is capped below what the index
    // wants. Saying so is the only way the override is discoverable in the one
    // case that needs it.
    let wanted = files.max(0).saturating_mul(SEARCH_CACHE_BYTES_PER_FILE) / (1024 * 1024);
    if wanted > SEARCH_CACHE_MAX_MIB {
        return Some(format!(
            "Automatic: {} MiB, the most it will choose on its own. This \
             index's {} files want about {} MiB to search at full speed — set \
             that here if you would rather spend the memory than the time.",
            mib, counted, wanted
        ));
    }
    Some(format!(
        "Automatic: {} MiB, sized to hold this index's {} file records — an \
         encrypted index re-decrypts them on every keystroke when they do not \
         fit.",
        mib, counted
    ))
}

/// Every row goes through [`Form`], so a setting cannot arrive here without a
/// tooltip or without saying who it is for.
fn config_editor_ui(
    ui: &mut egui::Ui,
    config: &mut Config,
    section: Section,
    indexed_files: Option<i64>,
    form: Form,
) {
    match section {
        Section::Indexing => {
            egui::Grid::new("cfg-indexing")
                .num_columns(2)
                .show(ui, |ui| {
                    // Automatic vs manual is absent: it is live state, and a
                    // staged copy would fight the Manage Index buttons.
                    form.row(
                        Level::Advanced,
                        ui,
                        "Full reindex every",
                        &tips::REINDEX_INTERVAL,
                        |ui| {
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::DragValue::new(
                                        &mut config.indexing.reindex_interval_minutes,
                                    )
                                    .range(5..=60 * 24 * 30),
                                );
                                ui.label("minutes");
                            })
                            .response
                        },
                    );

                    form.row(
                        Level::Advanced,
                        ui,
                        "Follow symlinks",
                        &tips::FOLLOW_SYMLINKS,
                        |ui| ui.checkbox(&mut config.indexing.follow_symlinks, ""),
                    );

                    form.row(
                        Level::Everyday,
                        ui,
                        "Include hidden files",
                        &tips::INCLUDE_HIDDEN,
                        |ui| ui.checkbox(&mut config.indexing.include_hidden, ""),
                    );
                });
        }
        Section::Processing => {
            egui::Grid::new("cfg-processing")
                .num_columns(2)
                .show(ui, |ui| {
                    form.row(Level::Advanced, ui, "Tokenizer", &tips::TOKENIZER, |ui| {
                        egui::ComboBox::from_id_salt("cfg-tokenize")
                            .selected_text(&config.processing.tokenize)
                            .show_ui(ui, |ui| {
                                for opt in ["trigram", "unicode61", "porter"] {
                                    ui.selectable_value(
                                        &mut config.processing.tokenize,
                                        opt.to_string(),
                                        opt,
                                    );
                                }
                            })
                            .response
                    });

                    // Not a row, so it needs its own guard: it documents the
                    // tokenizer above and makes no sense without it.
                    if form.advanced {
                        ui.label("");
                        ui.hyperlink_to(
                            "Tokenizer documentation",
                            "https://www.sqlite.org/fts5.html#tokenizers",
                        );
                        ui.end_row();
                    }

                    form.drag(
                        Level::Advanced,
                        ui,
                        "Hash sample size (bytes)",
                        &tips::HASH_LENGTH,
                        &mut config.processing.hash_length,
                        512..=1_048_576,
                    );

                    form.drag(
                        Level::Advanced,
                        ui,
                        "Max stored text (bytes)",
                        &tips::MAX_STORED_TEXT,
                        &mut config.processing.maximum_text_size,
                        1024..=16_777_216,
                    );

                    form.drag(
                        Level::Advanced,
                        ui,
                        "Max text file size (bytes)",
                        &tips::MAX_TEXT_FILE_SIZE,
                        &mut config.processing.maximum_text_file_size,
                        1024..=1_073_741_824,
                    );

                    form.drag(
                        Level::Advanced,
                        ui,
                        "Batch size",
                        &tips::BATCH_SIZE,
                        &mut config.processing.batch_size,
                        10..=100_000,
                    );

                    form.drag(
                        Level::Advanced,
                        ui,
                        "Max WAL size (bytes)",
                        &tips::MAX_WAL_SIZE,
                        &mut config.processing.maximum_wal_size,
                        0u64..=8_589_934_592u64,
                    );

                    form.row(
                        Level::Everyday,
                        ui,
                        "Store text for snippets",
                        &tips::STORE_TEXT,
                        |ui| ui.checkbox(&mut config.processing.store_text_for_snippets, ""),
                    );
                });
        }
        Section::Search => {
            egui::Grid::new("cfg-search").num_columns(2).show(ui, |ui| {
                form.row(
                    Level::Everyday,
                    ui,
                    "Fuzzy search ON by default",
                    &tips::FUZZY_DEFAULT,
                    |ui| ui.checkbox(&mut config.search.fuzzy_default, ""),
                );

                form.drag(
                    Level::Advanced,
                    ui,
                    "Fuzzy edit distance",
                    &tips::FUZZY_EDITS,
                    &mut config.search.fuzzy_max_edits,
                    0..=8,
                );

                form.drag(
                    Level::Advanced,
                    ui,
                    "Display limit",
                    &tips::DISPLAY_LIMIT,
                    &mut config.search.display_limit,
                    50..=100_000,
                );

                form.drag(
                    Level::Advanced,
                    ui,
                    "Stream batch size",
                    &tips::RESULTS_PER_PAGE,
                    &mut config.search.results_per_page,
                    10..=10_000,
                );

                form.drag(
                    Level::Advanced,
                    ui,
                    "Debounce (ms)",
                    &tips::DEBOUNCE,
                    &mut config.search.debounce_ms,
                    0..=2000,
                );

                form.row(
                    Level::Everyday,
                    ui,
                    "Live results",
                    &tips::LIVE_RESULTS,
                    |ui| ui.checkbox(&mut config.search.live_results, ""),
                );

                // 0 is "derive it from the index", which is why this is a
                // plain box rather than a range starting at the floor.
                form.drag(
                    Level::Advanced,
                    ui,
                    "Search cache MiB (0 = auto)",
                    &tips::SEARCH_CACHE,
                    &mut config.search.cache_size_mib,
                    0..=quicksearch_core::db::schema::SEARCH_CACHE_OVERRIDE_MAX_MIB as usize,
                );
            });
            // Both come and go as their values are edited, and both explain
            // advanced rows — there is nothing to say when those are hidden.
            if form.advanced {
                crate::ui_util::stable_section(ui, |ui| {
                    if let Some(warning) = config.search.fuzzy_edits_warning() {
                        ui.colored_label(ui.visuals().warn_fg_color, warning);
                    }
                    if let Some(recommended) = search_cache_hint(config, indexed_files) {
                        ui.label(hint(&recommended));
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests;
