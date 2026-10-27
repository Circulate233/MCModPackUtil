use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, Sense, Stroke, TextEdit};

use crate::config::{CleanroomRelauncher, Config, DownloadSource, LoaderKind, TestRunConfig};
use crate::curseforge::client::CurseForgeClient;
use crate::curseforge::resolver::ApiKeyResolver;
use crate::gui::command::{
    CommandForm, GuiCommand, GuiCommandError, GuiCommandSpec, GuiCommandTask,
    normalize_build_options_for_project, normalize_build_server_platform,
};
use crate::gui::loader_versions::{
    GuiNetworkEngineCache, LoaderVersionCandidateState, LoaderVersionCandidates, LoaderVersionQuery,
};
use crate::gui::mod_list::{
    AddProjectDraft, FileCandidate, GuiDependencyList, GuiDependencyType, ManifestList,
    ModListProjectSettings, ProjectCandidate, ProjectLookup, ProjectSearchInput,
    QueryFileCandidateCache, QueryFileCandidates, load_project_config_for_mod_list,
    load_project_settings_for_mod_list, parse_project_search_input, query_file_candidates_cached,
    search_project_candidates,
};
use crate::gui::overrides::{
    OverrideCatalog, OverrideLayer, OverrideScope, apply_scope, delete_entry,
};
use crate::gui::settings::SettingsForm;
use crate::gui::sync_tree::{
    SyncTree, SyncTreeAction, SyncTreeNode, apply_tree_actions, folder_non_deleted_selection,
};
use crate::i18n::Language;
use crate::project::build::{
    BuildProgress, BuildTarget, ServerPlatform, ServerPlatformChoice,
    build_project_with_api_key_and_options_and_progress,
};
use crate::project::check::check_project_with_api_key_and_progress;
use crate::project::init::{
    InitProgress, initialize_from_zip_with_api_and_progress, initialize_from_zip_with_progress,
};
use crate::project::run::{
    ConsoleInput, NeverConsole, NeverStop, PendingSyncChange, PendingSyncManifest,
    ProcessOutputStream, RunFileReadySource, RunProgress, RunSide, RunSourceKind,
    SharedConsoleInput, SharedStopSignal, StopSignal, SyncTarget, apply_pending_sync,
    run_project_with_api_key_and_progress_and_stop_signal, sync_config_for_project_for_gui,
};

const ACCENT: Color32 = Color32::from_rgb(28, 172, 134);
/// Destructive actions read in this color so a delete is never mistaken for a toggle.
const DANGER: Color32 = Color32::from_rgb(214, 92, 92);
const ACCENT_HOVER: Color32 = Color32::from_rgb(48, 195, 155);
const ACCENT_ACTIVE: Color32 = Color32::from_rgb(18, 148, 115);
const MAX_GUI_EVENT_QUEUE: usize = 2_048;
const MAX_GUI_EVENTS_PER_FRAME: usize = 256;
const MAX_GUI_LOG_BYTES: usize = 512 * 1024;
const GUI_LOG_TRUNCATION_NOTICE: &str = "[log truncated: retaining most recent output]\n";

thread_local! {
    static DROPPED_GUI_LOG_EVENTS: Cell<usize> = const { Cell::new(0) };
}

#[derive(Debug)]
pub struct McmpuGuiApp {
    form: CommandForm,
    log: String,
    status: String,
    running: bool,
    receiver: Option<Receiver<GuiEvent>>,
    mod_list: ModListPageState,
    overrides: OverridesPageState,
    settings: SettingsPageState,
    sync: SyncPageState,
    network_engines: GuiNetworkEngineCache,
    /// Whether the run console fills the page instead of collapsing to a bottom strip.
    console_expanded: bool,
    /// The console scroll area's id, kept so the wheel can scroll it during a text selection.
    console_scroll_id: Option<egui::Id>,
    /// Draft line shown in the run console input row.
    console_line: String,
    /// Client run log, kept apart so each side owns its own terminal.
    client_log: String,
    /// Server run log.
    server_log: String,
    /// Side of the run in progress; its terminal receives every line until the run ends.
    client_run: Option<SideRun>,
    server_run: Option<SideRun>,
}

/// One in-progress client or server run and the handles that control it.
#[derive(Debug)]
struct SideRun {
    /// Event channel of this run's worker thread.
    receiver: Receiver<GuiEvent>,
    /// Requests shutdown of this run.
    stop_signal: SharedStopSignal,
    /// Console input for this run's process.
    console_input: SharedConsoleInput,
}

pub(crate) fn install_accent_styles(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        let v = &mut style.visuals;
        v.selection.bg_fill = ACCENT.gamma_multiply(0.55);
        v.selection.stroke = Stroke::new(1.0, ACCENT_HOVER);
        v.hyperlink_color = ACCENT;
        if v.dark_mode {
            v.text_edit_bg_color = Some(Color32::from_gray(26));
            v.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_gray(66));
            v.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_gray(92));
        } else {
            v.text_edit_bg_color = Some(Color32::from_gray(243));
            v.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_gray(199));
            v.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_gray(168));
        }
    });
}

#[derive(Debug)]
enum GuiEvent {
    Log(String),
    Finished(i32),
    Failed(String),
    ModListLoaded(Result<ModListLoadResult, String>),
    ModListSaved {
        revision: u64,
        result: Result<(), String>,
    },
    ModListProjectsSearched {
        generation: u64,
        input: ProjectSearchInput,
        result: Result<Vec<ProjectCandidate>, String>,
    },
    ModListFileCandidatesQueried(ModListFileCandidateResult),
    ModListAddFileCandidatesQueried(AddProjectFileCandidateResult),
    SettingsLoaded(Box<Result<SettingsForm, String>>),
    SettingsSaved {
        revision: u64,
        result: Result<(), String>,
    },
    SettingsLoaderVersionsLoaded {
        generation: u64,
        query: LoaderVersionQuery,
        result: Result<LoaderVersionCandidates, String>,
    },
    SyncLoaded(Result<PendingSyncManifest, String>),
    SyncWindowLoaded(PendingSyncManifest),
    SyncWindowCheckFailed(String),
    SyncApplied(Result<SyncApplyOutcome, String>),
    OverridesLoaded(Result<OverrideCatalog, String>),
}

#[derive(Debug)]
struct SyncApplyOutcome {
    applied: Vec<String>,
    remaining: PendingSyncManifest,
}

#[derive(Debug)]
struct ModListPageState {
    list: Option<GuiDependencyList>,
    dirty: bool,
    load_attempted: bool,
    loading: bool,
    saving: bool,
    project_search_input: String,
    project_search_results: Vec<ProjectCandidate>,
    project_search_error: Option<String>,
    project_search_loading: bool,
    project_search_deadline: Option<Instant>,
    project_search_generation: u64,
    project_search_cache: HashMap<ProjectSearchInput, Vec<ProjectCandidate>>,
    querying_files: bool,
    file_candidate_generation: u64,
    querying_add_files: bool,
    add_file_candidate_generation: u64,
    row_ids: Vec<u64>,
    next_row_id: u64,
    revision: u64,
    file_candidate_row_id: Option<u64>,
    file_candidates: Vec<FileCandidate>,
    add_file_candidates: Option<Vec<FileCandidate>>,
    search: String,
    message: String,
    minecraft_version: Option<String>,
    config_loader: Option<String>,
    file_candidate_cache: std::sync::Arc<QueryFileCandidateCache>,
    add_draft: Option<AddProjectDraft>,
}

#[derive(Debug)]
struct SyncPageState {
    manifest: Option<PendingSyncManifest>,
    side: RunSide,
    loading: bool,
    applying: bool,
    window_open: bool,
    /// Free-text filter applied to the pending paths shown in the window.
    filter: String,
    /// Entry awaiting delete confirmation, keyed by pending path.
    pending_delete: Option<String>,
    message: String,
}

#[derive(Debug, Default)]
struct SettingsPageState {
    form: Option<SettingsForm>,
    dirty: bool,
    load_attempted: bool,
    loading: bool,
    saving: bool,
    revision: u64,
    message: String,
    loader_query_generation: u64,
    loader_query: Option<LoaderVersionQuery>,
    loader_versions: Vec<String>,
    forge_compat_versions: Vec<String>,
    loader_versions_state: LoaderVersionCandidateState,
    loader_versions_cache: HashMap<LoaderVersionQuery, LoaderVersionCandidates>,
}

impl Default for SyncPageState {
    fn default() -> Self {
        Self {
            manifest: None,
            side: RunSide::Client,
            loading: false,
            applying: false,
            window_open: false,
            filter: String::new(),
            pending_delete: None,
            message: String::new(),
        }
    }
}

/// GUI state for the Overrides page: the scanned catalog plus a free-text filter.
#[derive(Debug, Default)]
struct OverridesPageState {
    catalog: Option<OverrideCatalog>,
    loading: bool,
    load_attempted: bool,
    filter: String,
    scope_filter: OverrideScopeFilter,
    /// Entry awaiting a delete confirmation; the second click performs the deletion.
    pending_delete: Option<String>,
    message: String,
    /// Folder tree of the current filter, rebuilt only when its inputs change.
    tree: Option<Vec<OverrideTreeNode>>,
    /// Inputs that produced `tree`; a mismatch rebuilds it.
    tree_key: Option<OverrideTreeKey>,
    /// Bumped whenever the catalog changes in place, so the cached tree is rebuilt.
    revision: u64,
}

/// Inputs that decide which folders and files the Overrides tree shows.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OverrideTreeKey {
    filter: String,
    scope: OverrideScopeFilter,
    revision: u64,
}

/// Which scope the Overrides page currently lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum OverrideScopeFilter {
    #[default]
    All,
    Common,
    ClientAndServer,
    Client,
    Server,
    Conflicting,
}

impl OverrideScopeFilter {
    fn matches(self, scope: OverrideScope) -> bool {
        match self {
            Self::All => true,
            Self::Common => matches!(
                scope,
                OverrideScope::Common | OverrideScope::CommonWithSide(_)
            ),
            Self::ClientAndServer => scope == OverrideScope::ClientAndServer,
            Self::Client => matches!(
                scope,
                OverrideScope::Client | OverrideScope::CommonWithSide(OverrideLayer::Client)
            ),
            Self::Server => matches!(
                scope,
                OverrideScope::Server | OverrideScope::CommonWithSide(OverrideLayer::Server)
            ),
            Self::Conflicting => scope == OverrideScope::Conflicting,
        }
    }
}

impl Default for ModListPageState {
    fn default() -> Self {
        Self {
            list: None,
            dirty: false,
            load_attempted: false,
            loading: false,
            saving: false,
            project_search_input: String::new(),
            project_search_results: Vec::new(),
            project_search_error: None,
            project_search_loading: false,
            project_search_deadline: None,
            project_search_generation: 0,
            project_search_cache: HashMap::new(),
            querying_files: false,
            file_candidate_generation: 0,
            querying_add_files: false,
            add_file_candidate_generation: 0,
            row_ids: Vec::new(),
            next_row_id: 1,
            revision: 0,
            file_candidate_row_id: None,
            file_candidates: Vec::new(),
            add_file_candidates: None,
            search: String::new(),
            message: String::new(),
            minecraft_version: None,
            config_loader: None,
            file_candidate_cache: std::sync::Arc::new(QueryFileCandidateCache::default()),
            add_draft: None,
        }
    }
}

#[derive(Debug)]
struct ModListLoadResult {
    list: GuiDependencyList,
    settings: ModListProjectSettings,
}

#[derive(Debug)]
struct ModListFileCandidateResult {
    generation: u64,
    revision: u64,
    row_id: u64,
    project: ProjectLookup,
    loader: Option<String>,
    result: Result<Vec<FileCandidate>, String>,
}

#[derive(Debug)]
struct AddProjectFileCandidateResult {
    generation: u64,
    project_id: u64,
    loader: Option<String>,
    result: Result<Vec<FileCandidate>, String>,
}

impl Default for McmpuGuiApp {
    fn default() -> Self {
        let lang = gui_language("");
        Self {
            form: CommandForm::default(),
            log: "[init] waiting\n".to_string(),
            status: lang.status_ready().to_string(),
            running: false,
            receiver: None,
            mod_list: ModListPageState::default(),
            overrides: OverridesPageState::default(),
            settings: SettingsPageState::default(),
            sync: SyncPageState::default(),
            network_engines: GuiNetworkEngineCache::default(),
            console_expanded: false,
            console_scroll_id: None,
            console_line: String::new(),
            client_log: String::new(),
            server_log: String::new(),
            client_run: None,
            server_run: None,
        }
    }
}

impl eframe::App for McmpuGuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events();
        let dark = ui.visuals().dark_mode;
        egui::Frame::NONE.fill(page_bg(dark)).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            egui::Panel::left("mcmpu_sidebar")
                .resizable(false)
                .exact_size(238.0)
                .show_separator_line(false)
                .frame(
                    egui::Frame::NONE
                        .fill(sidebar_bg(dark))
                        .inner_margin(egui::Margin::same(14)),
                )
                .show(ui, |ui| self.sidebar(ui, dark));
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| self.content(ui));
        });
        self.sync_window(ui.ctx());
        if self.any_operation_running() || self.settings.loader_versions_state.is_loading() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(80));
        }
    }
}

impl McmpuGuiApp {
    fn current_lang(&self) -> Language {
        gui_language(&self.form.lang)
    }

    fn sidebar(&mut self, ui: &mut egui::Ui, dark: bool) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("mcmpu").size(21.0).strong().color(if dark {
                Color32::WHITE
            } else {
                Color32::from_gray(30)
            }));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.theme_toggle_button(ui, dark);
                ui.label(
                    RichText::new(crate::BUILD_VERSION)
                        .size(11.0)
                        .color(text_muted(dark)),
                );
            });
        });
        ui.add_space(16.0);
        for command in GuiCommand::all() {
            let selected = self.form.command == command;
            let title = command_title(self.current_lang(), command);
            self.sidebar_button(ui, command, title, selected, dark);
        }
    }

    fn sidebar_button(
        &mut self,
        ui: &mut egui::Ui,
        command: GuiCommand,
        title: &str,
        selected: bool,
        dark: bool,
    ) {
        let (bg_normal, bg_hover, bg_active) = if selected {
            (
                ACCENT.gamma_multiply(if dark { 0.22 } else { 0.16 }),
                ACCENT.gamma_multiply(if dark { 0.30 } else { 0.22 }),
                ACCENT.gamma_multiply(if dark { 0.38 } else { 0.28 }),
            )
        } else if dark {
            (
                Color32::TRANSPARENT,
                Color32::from_gray(44),
                Color32::from_gray(54),
            )
        } else {
            (
                Color32::TRANSPARENT,
                Color32::from_gray(216),
                Color32::from_gray(206),
            )
        };
        let text_color = if selected {
            if dark { ACCENT_HOVER } else { ACCENT_ACTIVE }
        } else if dark {
            Color32::from_gray(210)
        } else {
            Color32::from_gray(60)
        };
        let response = ui
            .scope(|ui| {
                let v = ui.visuals_mut();
                v.widgets.inactive.weak_bg_fill = bg_normal;
                v.widgets.inactive.bg_fill = bg_normal;
                v.widgets.inactive.fg_stroke = Stroke::new(0.0, text_color);
                v.widgets.inactive.bg_stroke = Stroke::NONE;
                v.widgets.hovered.weak_bg_fill = bg_hover;
                v.widgets.hovered.bg_fill = bg_hover;
                v.widgets.hovered.fg_stroke = Stroke::new(0.0, text_color);
                v.widgets.hovered.bg_stroke = Stroke::NONE;
                v.widgets.active.weak_bg_fill = bg_active;
                v.widgets.active.bg_fill = bg_active;
                v.widgets.active.fg_stroke = Stroke::new(0.0, text_color);
                v.widgets.active.bg_stroke = Stroke::NONE;
                let weight = if selected {
                    RichText::new(title).size(13.5).color(text_color).strong()
                } else {
                    RichText::new(title).size(13.5).color(text_color)
                };
                ui.add_enabled(
                    self.navigation_enabled(),
                    egui::Button::new(weight)
                        .min_size(egui::vec2(182.0, 36.0))
                        .corner_radius(7),
                )
            })
            .inner;
        if selected {
            let rect = response.rect;
            let x = rect.left() + 1.5;
            let painter = ui.painter();
            painter.line_segment(
                [
                    egui::pos2(x, rect.top() + 7.0),
                    egui::pos2(x, rect.bottom() - 7.0),
                ],
                Stroke::new(3.0, ACCENT),
            );
        }
        if response.clicked() {
            if self.form.command == command {
                return;
            }
            if self.form.command == GuiCommand::ModList
                && command != GuiCommand::ModList
                && self.mod_list.dirty
            {
                self.mod_list.message = self.current_lang().mod_list_unsaved_block().to_string();
                self.status = self.mod_list.message.clone();
                return;
            }
            if self.form.command == GuiCommand::Settings
                && command != GuiCommand::Settings
                && self.settings.dirty
            {
                self.settings.message = self.current_lang().settings_unsaved_block().to_string();
                self.status = self.settings.message.clone();
                return;
            }
            self.form.command = command;
            // Switching pages must not wipe a terminal that already has output.
            let waiting = format!("[{}] waiting\n", command.cli_name());
            let log = match command {
                GuiCommand::RunClient => &mut self.client_log,
                GuiCommand::RunServer => &mut self.server_log,
                _ => &mut self.log,
            };
            if log.is_empty() {
                *log = waiting;
            }
            self.status = self.current_lang().status_ready().to_string();
            if command == GuiCommand::ModList {
                self.open_mod_list();
            } else if command == GuiCommand::Settings {
                self.open_settings();
            } else if matches!(command, GuiCommand::RunClient | GuiCommand::RunServer) {
                self.load_run_configuration_if_needed();
            }
        }
        ui.add_space(3.0);
    }

    fn theme_toggle_button(&self, ui: &mut egui::Ui, dark: bool) {
        let (icon, tooltip, target) = if dark {
            ("☀", "切换到亮色模式", egui::Theme::Light)
        } else {
            ("🌙", "切换到暗色模式", egui::Theme::Dark)
        };
        if ui
            .add(
                egui::Button::new(RichText::new(icon).size(15.0))
                    .frame(false)
                    .min_size(egui::vec2(26.0, 26.0)),
            )
            .on_hover_text(tooltip)
            .clicked()
        {
            ui.ctx().set_theme(target);
        }
    }

    fn content(&mut self, ui: &mut egui::Ui) {
        if !self.run_console_mode() {
            egui::Panel::bottom("mcmpu_footer")
                .resizable(false)
                .show_separator_line(false)
                .frame(egui::Frame::NONE.inner_margin(egui::Margin {
                    left: 12,
                    right: 12,
                    top: 10,
                    bottom: 12,
                }))
                .show(ui, |ui| self.footer(ui));
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.inner_margin(egui::Margin {
                left: 12,
                right: 12,
                top: 12,
                bottom: 0,
            }))
            .show(ui, |ui| match self.form.command {
                GuiCommand::ModList => self.mod_list_panel(ui),
                GuiCommand::Overrides => self.overrides_panel(ui),
                GuiCommand::Settings => self.settings_panel(ui),
                _ if self.run_console_mode() => {
                    self.run_console_panel(ui);
                }
                _ => self.command_form_and_log(ui),
            });
    }

    fn command_form_and_log(&mut self, ui: &mut egui::Ui) {
        // Collapsed keeps the form in view above a short console; expanded hands the whole page
        // to the console. Without this the toggle did nothing at all until a run had started,
        // because only the running page reacted to it.
        let layout = if self.console_expanded {
            CommandPageLayout {
                form_max_height: 0.0,
                gap: 0.0,
            }
        } else {
            CommandPageLayout::new(ui.available_height())
        };
        if layout.form_max_height > 0.0 {
            vertical_scroll_area()
                .id_salt("mcmpu_command_form")
                .auto_shrink([false, true])
                .max_height(layout.form_max_height)
                .show(ui, |ui| self.form_panel(ui));
        }
        if layout.gap > 0.0 {
            ui.add_space(layout.gap);
        }
        self.log_panel(ui, available_height(ui.available_height()));
    }

    fn sync_window(&mut self, ctx: &egui::Context) {
        if !self.sync.window_open {
            return;
        }
        let mut open = self.sync.window_open;
        egui::Window::new(self.current_lang().cmd_title_sync())
            .open(&mut open)
            .default_size(egui::vec2(820.0, 520.0))
            .resizable(true)
            .show(ctx, |ui| self.sync_panel(ui));
        self.sync.window_open = open;
    }

    fn form_panel(&mut self, ui: &mut egui::Ui) {
        let dark = ui.visuals().dark_mode;
        let lang = self.current_lang();
        egui::Frame::NONE
            .fill(panel_bg(dark))
            .stroke(Stroke::new(1.0, border_color(dark)))
            .corner_radius(10)
            .inner_margin(egui::Margin::same(18))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(command_title(lang, self.form.command));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(command_description(lang, self.form.command))
                                .size(12.5)
                                .color(text_muted(dark)),
                        );
                    });
                });
                ui.add_space(14.0);
                toggle_row(
                    ui,
                    lang.field_project_dir(),
                    &mut self.form.use_custom_project_dir,
                    label_color(dark),
                );
                if self.form.use_custom_project_dir {
                    self.path_row(ui, "", PathField::ProjectDir, true, label_color(dark));
                }
                if self.form.command == GuiCommand::Init {
                    self.path_row(
                        ui,
                        lang.field_pack_zip(),
                        PathField::PackZip,
                        false,
                        label_color(dark),
                    );
                }
                if matches!(self.form.command, GuiCommand::Init | GuiCommand::Build) {
                    if self.form.command == GuiCommand::Build {
                        self.build_options_rows(ui, lang, label_color(dark));
                    }
                    toggle_row(
                        ui,
                        lang.field_output_dir(),
                        &mut self.form.use_custom_output_dir,
                        label_color(dark),
                    );
                    if self.form.use_custom_output_dir {
                        self.path_row(ui, "", PathField::OutputDir, true, label_color(dark));
                    }
                }
                self.language_row(ui, lang, label_color(dark));
                self.proxy_row(ui, lang, label_color(dark));
                if matches!(
                    self.form.command,
                    GuiCommand::RunClient | GuiCommand::RunServer
                ) {
                    ui.add_space(6.0);
                    self.run_test_config_rows(ui, lang, dark);
                    self.run_sync_config_row(ui, lang, dark);
                }
            });
    }

    fn build_options_rows(&mut self, ui: &mut egui::Ui, lang: Language, label_color: Color32) {
        let previous_target = self.form.build_target;
        ui.horizontal(|ui| {
            ui.set_height(40.0);
            field_label(ui, lang.field_build_target(), label_color);
            egui::ComboBox::from_id_salt("build_target")
                .selected_text(build_target_label(lang, self.form.build_target))
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.form.build_target,
                        BuildTarget::Both,
                        lang.build_target_both(),
                    );
                    ui.selectable_value(
                        &mut self.form.build_target,
                        BuildTarget::Client,
                        lang.build_target_client(),
                    );
                    ui.selectable_value(
                        &mut self.form.build_target,
                        BuildTarget::Server,
                        lang.build_target_server(),
                    );
                });
        });
        if self.form.build_target != previous_target {
            self.form.build_server_platform = normalize_build_server_platform(
                self.form.build_target,
                self.form.build_server_platform,
            );
        }
        if self.form.build_target != BuildTarget::Client {
            ui.horizontal(|ui| {
                ui.set_height(40.0);
                field_label(ui, lang.field_server_platform(), label_color);
                egui::ComboBox::from_id_salt("build_server_platform")
                    .selected_text(server_platform_label(lang, self.form.build_server_platform))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.form.build_server_platform,
                            ServerPlatformChoice::Current,
                            lang.server_platform_current(),
                        );
                        ui.selectable_value(
                            &mut self.form.build_server_platform,
                            ServerPlatformChoice::Explicit(ServerPlatform::WindowsX86_64),
                            lang.server_platform_windows_x86_64(),
                        );
                        ui.selectable_value(
                            &mut self.form.build_server_platform,
                            ServerPlatformChoice::Explicit(ServerPlatform::LinuxX86_64),
                            lang.server_platform_linux_x86_64(),
                        );
                        ui.selectable_value(
                            &mut self.form.build_server_platform,
                            ServerPlatformChoice::Explicit(ServerPlatform::MacosAarch64),
                            lang.server_platform_macos_aarch64(),
                        );
                    });
            });
        }
    }

    fn run_test_config_rows(&mut self, ui: &mut egui::Ui, lang: Language, dark: bool) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(lang.run_test_config_section())
                    .strong()
                    .color(label_color(dark)),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self
                    .accent_button(ui, lang.run_test_config_reload(), [104.0, 28.0])
                    .clicked()
                {
                    self.reload_run_configuration();
                }
            });
        });
        if self.form.command == GuiCommand::RunClient {
            run_text_row(
                ui,
                lang.run_player_name(),
                &mut self.form.test_run.player_name,
                dark,
            );
        }
        run_download_source_combo(ui, lang, &mut self.form.test_run.download_source, dark);
        run_u64_row(
            ui,
            lang.run_sync_interval(),
            &mut self.form.test_run.sync_interval_seconds,
            1..=u64::MAX,
            dark,
        );
        run_jvm_args_row(
            ui,
            lang.run_debug_jvm_args(),
            &mut self.form.test_run.debug_extra_jvm_args,
            dark,
        );
        run_text_row(
            ui,
            lang.run_java_path(),
            &mut self.form.test_run.java_path,
            dark,
        );
    }

    fn run_sync_config_row(&mut self, ui: &mut egui::Ui, lang: Language, dark: bool) {
        ui.horizontal(|ui| {
            field_label(ui, lang.run_sync_include_paths(), label_color(dark));
            if self
                .accent_button(ui, lang.run_sync_reload(), [104.0, 28.0])
                .clicked()
            {
                self.load_run_sync_config();
            }
        });
        let value = match self.form.command {
            GuiCommand::RunClient => &mut self.form.client_sync_include_paths,
            GuiCommand::RunServer => &mut self.form.server_sync_include_paths,
            _ => return,
        };
        ui.horizontal(|ui| {
            ui.add_space(LABEL_COL_WIDTH + 10.0);
            ui.add_sized(
                [non_negative_width(ui.available_width()), 76.0],
                TextEdit::multiline(value).hint_text(lang.run_sync_include_hint()),
            );
        });
    }

    fn load_run_sync_config(&mut self) {
        let result = self.try_load_run_sync_config();
        self.finish_run_configuration_load(result);
    }

    fn try_load_run_sync_config(&mut self) -> Result<(), String> {
        let side = match self.form.command {
            GuiCommand::RunClient => RunSide::Client,
            GuiCommand::RunServer => RunSide::Server,
            _ => return Ok(()),
        };
        let config = match sync_config_for_project_for_gui(&self.current_project_dir(), side) {
            Ok(config) => config,
            Err(error) => {
                match side {
                    RunSide::Client => self.form.client_sync_include_paths.clear(),
                    RunSide::Server => self.form.server_sync_include_paths.clear(),
                }
                return Err(error.to_string());
            }
        };
        let text = config.include_paths.join("\n");
        match side {
            RunSide::Client => self.form.client_sync_include_paths = text,
            RunSide::Server => self.form.server_sync_include_paths = text,
        }
        Ok(())
    }

    fn load_run_configuration_if_needed(&mut self) {
        let project_dir = self.current_project_dir();
        let test_run_result = if self.form.test_run_project_dir.as_ref() != Some(&project_dir) {
            self.try_load_run_test_config()
        } else {
            Ok(())
        };
        let sync_result = self.try_load_run_sync_config();
        self.finish_run_configuration_load(test_run_result.and(sync_result));
    }

    fn reload_run_configuration(&mut self) {
        let test_run_result = self.try_load_run_test_config();
        let sync_result = self.try_load_run_sync_config();
        self.finish_run_configuration_load(test_run_result.and(sync_result));
    }

    fn try_load_run_test_config(&mut self) -> Result<(), String> {
        let project_dir = self.current_project_dir();
        self.form.test_run_project_dir = None;
        self.form.test_run = TestRunConfig::default();
        self.form.test_run =
            TestRunConfig::from_project_for_gui(&project_dir).map_err(|error| error.to_string())?;
        self.form.test_run_project_dir = Some(project_dir);
        Ok(())
    }

    fn finish_run_configuration_load(&mut self, result: Result<(), String>) {
        self.status = match result {
            Ok(()) => self.current_lang().run_test_config_loaded().to_string(),
            Err(error) => error,
        };
    }

    fn path_row(
        &mut self,
        ui: &mut egui::Ui,
        label: &str,
        field: PathField,
        folder: bool,
        label_color: Color32,
    ) {
        let lang = self.current_lang();
        ui.horizontal(|ui| {
            ui.set_height(40.0);
            field_label(ui, label, label_color);
            let mut editable = path_mut(&mut self.form, field)
                .to_string_lossy()
                .to_string();
            let available = path_text_width(ui.available_width());
            ui.add_sized([available, 30.0], TextEdit::singleline(&mut editable));
            *path_mut(&mut self.form, field) = PathBuf::from(editable.trim());
            if self
                .accent_button(ui, lang.btn_browse(), [68.0, 30.0])
                .clicked()
            {
                let picked = if folder {
                    rfd::FileDialog::new().pick_folder()
                } else {
                    rfd::FileDialog::new()
                        .add_filter("Zip", &["zip"])
                        .pick_file()
                };
                if let Some(path) = picked {
                    *path_mut(&mut self.form, field) = path;
                }
            }
        });
    }

    fn language_row(&mut self, ui: &mut egui::Ui, lang: Language, label_color: Color32) {
        ui.horizontal(|ui| {
            ui.set_height(40.0);
            field_label(ui, lang.field_language(), label_color);
            egui::ComboBox::from_id_salt("language")
                .selected_text(if self.form.lang.trim().is_empty() {
                    lang.lang_option_system()
                } else {
                    self.form.lang.as_str()
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.form.lang,
                        String::new(),
                        lang.lang_option_system(),
                    );
                    ui.selectable_value(&mut self.form.lang, "zh-CN".to_string(), "zh-CN");
                    ui.selectable_value(&mut self.form.lang, "en-US".to_string(), "en-US");
                });
        });
    }

    fn proxy_row(&mut self, ui: &mut egui::Ui, lang: Language, label_color: Color32) {
        ui.horizontal(|ui| {
            ui.set_height(34.0);
            field_label(ui, lang.field_proxy(), label_color);
            ui.checkbox(&mut self.form.proxy.enabled, "");
        });
        if self.form.proxy.enabled {
            self.text_row(
                ui,
                lang.field_proxy_ip(),
                TextField::ProxyIp,
                false,
                label_color,
            );
            self.text_row(
                ui,
                lang.field_proxy_port(),
                TextField::ProxyPort,
                false,
                label_color,
            );
        }
    }

    fn text_row(
        &mut self,
        ui: &mut egui::Ui,
        label: &str,
        field: TextField,
        password: bool,
        label_color: Color32,
    ) {
        ui.horizontal(|ui| {
            ui.set_height(40.0);
            field_label(ui, label, label_color);
            let edit = TextEdit::singleline(text_mut(&mut self.form, field))
                .password(password)
                .desired_width(ui.available_width());
            ui.add_sized([non_negative_width(ui.available_width()), 30.0], edit);
        });
    }

    fn mod_list_panel(&mut self, ui: &mut egui::Ui) {
        let dark = ui.visuals().dark_mode;
        let lang = self.current_lang();
        self.ensure_mod_list_loaded();
        egui::Frame::NONE
            .fill(panel_bg(dark))
            .stroke(Stroke::new(1.0, border_color(dark)))
            .corner_radius(10)
            .inner_margin(egui::Margin::same(14))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(lang.cmd_title_mod_list());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_sized(
                            [180.0, 30.0],
                            TextEdit::singleline(&mut self.mod_list.search)
                                .hint_text(lang.mod_list_search()),
                        );
                        let search_enabled =
                            !self.mod_list.loading && self.mod_list.add_draft.is_none();
                        let search_changed = ui
                            .add_enabled_ui(search_enabled, |ui| {
                                ui.add_sized(
                                    [310.0, 30.0],
                                    TextEdit::singleline(&mut self.mod_list.project_search_input)
                                        .hint_text(lang.mod_list_project_search()),
                                )
                            })
                            .inner
                            .changed();
                        if search_changed {
                            self.schedule_project_search();
                        }
                    });
                });
                self.render_project_search_results(ui, dark, lang);
                if !self.mod_list.message.trim().is_empty() {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(&self.mod_list.message)
                            .size(12.0)
                            .color(text_muted(dark)),
                    );
                }
            });

        ui.add_space(10.0);
        egui::Frame::NONE
            .fill(panel_bg(dark))
            .stroke(Stroke::new(1.0, border_color(dark)))
            .corner_radius(10)
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                let height = (ui.available_height() - 2.0).max(260.0);
                vertical_scroll_area()
                    .auto_shrink([false, false])
                    .max_height(height)
                    .show(ui, |ui| {
                        self.mod_list_group(ui, lang.mod_list_group_mods(), GuiDependencyType::Mod);
                        ui.add_space(12.0);
                        self.mod_list_group(
                            ui,
                            lang.mod_list_group_resource_packs(),
                            GuiDependencyType::ResourcePack,
                        );
                        ui.add_space(12.0);
                        self.mod_list_group(
                            ui,
                            lang.mod_list_group_shader_packs(),
                            GuiDependencyType::ShaderPack,
                        );
                    });
            });
        self.dispatch_pending_project_search(ui.ctx());
        self.add_project_dialog(ui);
    }

    fn sync_panel(&mut self, ui: &mut egui::Ui) {
        let dark = ui.visuals().dark_mode;
        let lang = self.current_lang();
        let mut select_non_deleted = false;
        let mut actions = Vec::new();
        egui::Frame::NONE
            .fill(panel_bg(dark))
            .stroke(Stroke::new(1.0, border_color(dark)))
            .corner_radius(10)
            .inner_margin(egui::Margin::same(14))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(lang.cmd_title_sync());
                    egui::ComboBox::from_id_salt("sync_side")
                        .selected_text(match self.sync.side {
                            RunSide::Client => lang.sync_side_client(),
                            RunSide::Server => lang.sync_side_server(),
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.sync.side,
                                RunSide::Client,
                                lang.sync_side_client(),
                            );
                            ui.selectable_value(
                                &mut self.sync.side,
                                RunSide::Server,
                                lang.sync_side_server(),
                            );
                        });
                    if self
                        .accent_button(ui, lang.sync_load(), [120.0, 30.0])
                        .clicked()
                    {
                        self.load_sync_manifest();
                    }
                    if self
                        .accent_button(ui, lang.sync_select_all_modified(), [150.0, 30.0])
                        .clicked()
                    {
                        select_non_deleted = true;
                    }
                    if self
                        .accent_button(ui, lang.sync_apply(), [120.0, 30.0])
                        .clicked()
                    {
                        self.apply_sync_manifest();
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(lang.sync_search())
                            .size(12.5)
                            .color(text_muted(dark)),
                    );
                    ui.add_sized(
                        [260.0, 26.0],
                        TextEdit::singleline(&mut self.sync.filter)
                            .hint_text(lang.sync_search_hint()),
                    );
                    if !self.sync.filter.trim().is_empty()
                        && ui.button(lang.sync_search_clear()).clicked()
                    {
                        self.sync.filter.clear();
                    }
                });
                if !self.sync.message.trim().is_empty() {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(&self.sync.message)
                            .size(12.0)
                            .color(text_muted(dark)),
                    );
                }
            });

        ui.add_space(10.0);
        egui::Frame::NONE
            .fill(panel_bg(dark))
            .stroke(Stroke::new(1.0, border_color(dark)))
            .corner_radius(10)
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                let height = (ui.available_height() - 2.0).max(260.0);
                vertical_scroll_area()
                    .auto_shrink([false, false])
                    .max_height(height)
                    .show(ui, |ui| {
                        let Some(manifest) = self.sync.manifest.as_ref() else {
                            ui.label(lang.sync_empty());
                            return;
                        };
                        if manifest.entries.is_empty() {
                            ui.label(lang.sync_empty());
                            return;
                        }
                        let needle = self.sync.filter.trim().to_ascii_lowercase();
                        match SyncTree::from_manifest_filtered(manifest, |entry| {
                            needle.is_empty() || entry.path.to_ascii_lowercase().contains(&needle)
                        }) {
                            Ok(tree) => {
                                if tree.nodes.is_empty() {
                                    ui.label(lang.sync_no_matches(&self.sync.filter));
                                    return;
                                }
                                render_sync_tree(
                                    ui,
                                    &tree,
                                    &mut SyncTreeRender {
                                        manifest,
                                        lang,
                                        dark,
                                        expand_all: !needle.is_empty(),
                                        actions: &mut actions,
                                        pending_delete: &mut self.sync.pending_delete,
                                    },
                                )
                            }
                            Err(error) => {
                                ui.label(lang.sync_tree_error(&error.to_string()));
                            }
                        }
                    });
            });
        if select_non_deleted && let Some(manifest) = self.sync.manifest.as_ref() {
            actions.push(SyncTreeAction::SetFolderNonDeletedApply {
                entry_indices: (0..manifest.entries.len()).collect(),
                apply: true,
            });
        }
        if !actions.is_empty()
            && let Some(manifest) = self.sync.manifest.as_mut()
            && let Err(error) = apply_tree_actions(manifest, &actions)
        {
            self.sync.message = lang.sync_tree_error(&error.to_string());
        }
    }

    fn settings_panel(&mut self, ui: &mut egui::Ui) {
        let dark = ui.visuals().dark_mode;
        let lang = self.current_lang();
        self.ensure_settings_loaded();
        let loader_versions = self.settings.loader_versions.clone();
        let forge_compat_versions = self.settings.forge_compat_versions.clone();
        let versions_state = self.settings.loader_versions_state.clone();
        let mut loader_query_changed = false;
        let mut request_loader_versions = false;
        egui::Frame::NONE
            .fill(panel_bg(dark))
            .stroke(Stroke::new(1.0, border_color(dark)))
            .corner_radius(10)
            .inner_margin(egui::Margin::same(14))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(lang.cmd_title_settings());
                    if self
                        .accent_button(ui, lang.settings_load(), [120.0, 30.0])
                        .clicked()
                    {
                        self.reload_settings();
                    }
                });
                if !self.settings.message.trim().is_empty() {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(&self.settings.message)
                            .size(12.0)
                            .color(text_muted(dark)),
                    );
                }
            });

        ui.add_space(10.0);
        egui::Frame::NONE
            .fill(panel_bg(dark))
            .stroke(Stroke::new(1.0, border_color(dark)))
            .corner_radius(10)
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                let height = (ui.available_height() - 2.0).max(260.0);
                vertical_scroll_area()
                    .auto_shrink([false, false])
                    .max_height(height)
                    .show(ui, |ui| {
                        let Some(form) = self.settings.form.as_mut() else {
                            ui.label(lang.settings_not_loaded());
                            return;
                        };
                        let before = LoaderVersionQuery {
                            minecraft_version: form.minecraft_version.trim().to_string(),
                            loader: form.loader.clone(),
                        };
                        let dirty = settings_form_ui(
                            ui,
                            form,
                            lang,
                            dark,
                            SettingsVersionView {
                                loader_versions: &loader_versions,
                                forge_compat_versions: &forge_compat_versions,
                                state: &versions_state,
                            },
                        );
                        let after = LoaderVersionQuery {
                            minecraft_version: form.minecraft_version.trim().to_string(),
                            loader: form.loader.clone(),
                        };
                        loader_query_changed = before != after;
                        if loader_query_changed {
                            form.loader_version.clear();
                            form.forge_compat_version.clear();
                        }
                        request_loader_versions = dirty.request_loader_versions;
                        if dirty.form_changed {
                            self.mark_settings_dirty();
                        }
                    });
            });
        if loader_query_changed {
            self.invalidate_settings_loader_versions();
        }
        if request_loader_versions && !loader_query_changed {
            self.query_settings_loader_versions();
        }
    }

    fn mod_list_group(
        &mut self,
        ui: &mut egui::Ui,
        title: &str,
        dependency_type: GuiDependencyType,
    ) {
        ui.label(RichText::new(title).strong().size(15.0));
        ui.add_space(4.0);
        let Some(list) = self.mod_list.list.as_ref() else {
            ui.label(self.current_lang().mod_list_not_loaded());
            return;
        };
        let rows = list.group_indices(dependency_type, &self.mod_list.search);
        if rows.is_empty() {
            ui.label(
                RichText::new(self.current_lang().mod_list_empty())
                    .color(text_muted(ui.visuals().dark_mode)),
            );
            return;
        }
        let mut delete = None;
        for index in rows {
            let Some(row_id) = self.mod_list.row_ids.get(index).copied() else {
                self.mod_list.message = self.current_lang().mod_list_row_state_error().to_string();
                self.status = self.mod_list.message.clone();
                break;
            };
            if self.mod_list_row(ui, index, row_id) {
                delete = Some((index, row_id));
                break;
            }
        }
        if let Some((index, row_id)) = delete {
            self.delete_mod_list_row(index, row_id);
        }
    }

    fn mod_list_row(&mut self, ui: &mut egui::Ui, index: usize, row_id: u64) -> bool {
        let dark = ui.visuals().dark_mode;
        let lang = self.current_lang();
        let mut delete = false;
        let mut query_files = false;
        let mut dirty = false;
        let mut clear_file_picker = false;
        let mut picker_changed = false;
        egui::Frame::NONE
            .fill(if dark {
                Color32::from_gray(38)
            } else {
                Color32::from_gray(248)
            })
            .stroke(Stroke::new(1.0, border_color(dark)))
            .corner_radius(7)
            .inner_margin(egui::Margin::same(8))
            .show(ui, |ui| {
                if let Some(list) = &mut self.mod_list.list {
                    let entry = &mut list.entries[index];
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("#").color(text_muted(dark)));
                        ui.add_sized(
                            [150.0, 26.0],
                            egui::Label::new(RichText::new(&entry.name).color(label_color(dark)))
                                .truncate(),
                        );
                        if ui
                            .add_sized(
                                [130.0, 26.0],
                                TextEdit::singleline(option_string_mut(&mut entry.slug))
                                    .hint_text(lang.mod_list_slug_hint()),
                            )
                            .changed()
                        {
                            dirty = true;
                        }
                        if combo_manifest(ui, row_id, &mut entry.manifest) {
                            dirty = true;
                        }
                        if combo_dependency_type(ui, row_id, &mut entry.dependency_type) {
                            dirty = true;
                        }
                        if combo_loader(ui, row_id, &mut entry.loader, lang) {
                            dirty = true;
                        }
                        if optional_u64_edit(
                            ui,
                            &mut entry.project_id,
                            lang.mod_list_project_id_hint(),
                            82.0,
                        ) {
                            dirty = true;
                        }
                        let before_auto = entry.auto_update;
                        if ui
                            .checkbox(&mut entry.auto_update, lang.mod_list_auto())
                            .changed()
                        {
                            entry.set_auto_update(entry.auto_update);
                            clear_file_picker = entry.auto_update;
                            dirty = true;
                        }
                        if !entry.auto_update {
                            if optional_u64_edit(
                                ui,
                                &mut entry.file_id,
                                lang.mod_list_file_id_hint(),
                                82.0,
                            ) {
                                dirty = true;
                            }
                        } else if before_auto != entry.auto_update {
                            dirty = true;
                        }
                        if ui
                            .checkbox(&mut entry.required, lang.mod_list_required())
                            .changed()
                        {
                            dirty = true;
                        }
                        if entry.allows_file_version_selection()
                            && ui.button(lang.mod_list_files()).clicked()
                        {
                            query_files = true;
                        }
                        if ui.button(lang.mod_list_delete()).clicked() {
                            delete = true;
                        }
                    });
                    if self.mod_list.file_candidate_row_id == Some(row_id)
                        && list.entries[index].allows_file_version_selection()
                    {
                        picker_changed = self.file_candidate_picker(ui, index, row_id);
                    }
                }
            });
        ui.add_space(6.0);
        if dirty || picker_changed {
            self.mark_mod_list_dirty();
        }
        if clear_file_picker && self.mod_list.file_candidate_row_id == Some(row_id) {
            self.mod_list.file_candidate_row_id = None;
            self.mod_list.file_candidates.clear();
        }
        if query_files {
            self.query_file_candidates_for_row(index, row_id);
        }
        delete
    }

    fn file_candidate_picker(&mut self, ui: &mut egui::Ui, index: usize, row_id: u64) -> bool {
        let lang = self.current_lang();
        if self.mod_list.file_candidates.is_empty() {
            ui.label(
                RichText::new(lang.mod_list_no_candidates())
                    .color(text_muted(ui.visuals().dark_mode)),
            );
            return false;
        }
        let mut changed = false;
        if let Some(list) = &mut self.mod_list.list {
            let entry = &mut list.entries[index];
            egui::ComboBox::from_id_salt(("file_candidate", row_id))
                .selected_text(
                    entry
                        .file_id
                        .map(|file_id| file_id.to_string())
                        .unwrap_or_else(|| lang.mod_list_select_file().to_string()),
                )
                .show_ui(ui, |ui| {
                    for candidate in &self.mod_list.file_candidates {
                        let label = format!(
                            "{} - {}",
                            candidate.file_id,
                            candidate
                                .display_name
                                .as_deref()
                                .unwrap_or(candidate.file_name.as_str())
                        );
                        if ui
                            .selectable_value(&mut entry.file_id, Some(candidate.file_id), label)
                            .clicked()
                        {
                            entry.auto_update = false;
                            changed = true;
                        }
                    }
                });
        }
        changed
    }

    fn add_project_dialog(&mut self, ui: &mut egui::Ui) {
        if self.mod_list.add_draft.is_none() {
            return;
        }
        let lang = self.current_lang();
        let dark = ui.visuals().dark_mode;
        let mut confirm = false;
        let mut cancel = false;
        let mut query_file_candidates = false;
        egui::Window::new(lang.mod_list_add_dialog_title())
            .collapsible(false)
            .resizable(false)
            .default_width(520.0)
            .show(ui.ctx(), |ui| {
                let can_confirm = {
                    let Some(draft) = &mut self.mod_list.add_draft else {
                        return;
                    };
                    ui.label(RichText::new(&draft.project.name).strong().size(16.0));
                    ui.label(
                        RichText::new(format!(
                            "{} / {}",
                            draft.project.project_id, draft.project.slug
                        ))
                        .size(12.0)
                        .color(text_muted(dark)),
                    );
                    ui.add_space(10.0);
                    labeled_add_manifest(
                        ui,
                        lang.mod_list_manifest(),
                        "add_manifest",
                        &mut draft.manifest,
                        draft.project.dependency_type,
                        dark,
                    );
                    labeled_readonly_value(
                        ui,
                        lang.mod_list_type(),
                        draft.project.dependency_type.label(),
                        dark,
                    );
                    let loader_changed = labeled_loader_combo(
                        ui,
                        lang.mod_list_loader(),
                        "add_loader",
                        &mut draft.loader,
                        lang,
                        dark,
                    );
                    ui.horizontal(|ui| {
                        field_label(ui, lang.mod_list_auto(), label_color(dark));
                        let auto_update_changed = ui.checkbox(&mut draft.auto_update, "").changed();
                        if draft.auto_update {
                            draft.file_id = None;
                        }
                        if !draft.auto_update && (auto_update_changed || loader_changed) {
                            query_file_candidates = true;
                        }
                    });
                    if draft.allows_file_version_selection() {
                        ui.horizontal(|ui| {
                            field_label(ui, lang.mod_list_file_id_hint(), label_color(dark));
                            if self.mod_list.querying_add_files {
                                ui.spinner();
                                ui.label(lang.mod_list_querying_files());
                            } else if let Some(candidates) = &self.mod_list.add_file_candidates {
                                if candidates.is_empty() {
                                    ui.label(
                                        RichText::new(lang.mod_list_no_candidates())
                                            .color(text_muted(dark)),
                                    );
                                } else {
                                    egui::ComboBox::from_id_salt("add_file_candidate")
                                        .width(300.0)
                                        .selected_text(
                                            draft
                                                .file_id
                                                .map(|file_id| file_id.to_string())
                                                .unwrap_or_else(|| {
                                                    lang.mod_list_select_file().to_string()
                                                }),
                                        )
                                        .show_ui(ui, |ui| {
                                            for candidate in candidates {
                                                let label = format!(
                                                    "{} - {}",
                                                    candidate.file_id,
                                                    candidate
                                                        .display_name
                                                        .as_deref()
                                                        .unwrap_or(candidate.file_name.as_str())
                                                );
                                                if ui
                                                    .selectable_label(
                                                        draft.file_id == Some(candidate.file_id),
                                                        label,
                                                    )
                                                    .clicked()
                                                {
                                                    draft.select_file_version(candidate.file_id);
                                                }
                                            }
                                        });
                                }
                            } else {
                                ui.label(
                                    RichText::new(lang.mod_list_querying_files())
                                        .color(text_muted(dark)),
                                );
                            }
                        });
                    }
                    ui.horizontal(|ui| {
                        field_label(ui, lang.mod_list_required(), label_color(dark));
                        ui.checkbox(&mut draft.required, "");
                    });
                    draft.auto_update || draft.file_id.is_some()
                };
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled_ui(can_confirm, |ui| {
                            self.accent_button(
                                ui,
                                lang.mod_list_add_dialog_confirm(),
                                [110.0, 32.0],
                            )
                        })
                        .inner
                        .clicked()
                    {
                        confirm = true;
                    }
                    if ui.button(lang.mod_list_add_dialog_cancel()).clicked() {
                        cancel = true;
                    }
                });
            });
        if query_file_candidates {
            self.query_file_candidates_for_add_draft();
        }
        if confirm {
            self.add_project_draft_to_list();
        }
        if cancel {
            self.mod_list.add_draft = None;
            self.mod_list.add_file_candidates = None;
            self.mod_list.add_file_candidate_generation += 1;
            self.mod_list.querying_add_files = false;
        }
    }

    fn ensure_mod_list_loaded(&mut self) {
        if self.mod_list.load_attempted {
            return;
        }
        self.open_mod_list();
    }

    fn ensure_settings_loaded(&mut self) {
        if self.settings.load_attempted {
            return;
        }
        self.open_settings();
    }

    fn open_settings(&mut self) {
        if self.settings.loading {
            return;
        }
        self.settings.load_attempted = true;
        let project_dir = self.current_project_dir();
        self.settings.loading = true;
        self.settings.message = self.current_lang().settings_loading().to_string();
        self.status = self.settings.message.clone();
        let sender = self.ensure_event_sender();
        thread::spawn(move || {
            let result = SettingsForm::load(project_dir).map_err(|err| err.to_string());
            let _ = sender.send(GuiEvent::SettingsLoaded(Box::new(result)));
        });
    }

    fn query_settings_loader_versions(&mut self) {
        let Some(form) = self.settings.form.as_ref() else {
            return;
        };
        let query = LoaderVersionQuery {
            minecraft_version: form.minecraft_version.trim().to_string(),
            loader: form.loader.clone(),
        };
        if !self.settings.loader_versions_state.begin_request() {
            return;
        }
        self.settings.loader_query = Some(query.clone());
        self.settings.loader_query_generation += 1;
        if let Some(cached) = self.settings.loader_versions_cache.get(&query) {
            self.settings.loader_versions = cached.loader_versions.clone();
            self.settings.forge_compat_versions = cached.forge_compat_versions.clone();
            self.settings.loader_versions_state.finish_request(Ok(()));
            return;
        }
        self.settings.loader_versions.clear();
        self.settings.forge_compat_versions.clear();
        let generation = self.settings.loader_query_generation;
        let proxy = settings_proxy(form);
        let networks = self.network_engines.clone();
        let sender = self.ensure_event_sender();
        thread::spawn(move || {
            let result = networks
                .loader_version_client(proxy.as_deref())
                .and_then(|client| client.query(query.clone()))
                .map_err(|error| error.to_string());
            let _ = sender.send(GuiEvent::SettingsLoaderVersionsLoaded {
                generation,
                query,
                result,
            });
        });
    }

    fn invalidate_settings_loader_versions(&mut self) {
        // Invalidate any in-flight response before clearing the displayed
        // candidates. The next request is always initiated by a user click.
        self.settings.loader_query_generation += 1;
        self.settings.loader_query = None;
        self.settings.loader_versions.clear();
        self.settings.forge_compat_versions.clear();
        self.settings.loader_versions_state.invalidate();
    }

    fn reload_settings(&mut self) {
        if self.settings.dirty {
            self.settings.message = self.current_lang().settings_unsaved_block().to_string();
            self.status = self.settings.message.clone();
            return;
        }
        self.open_settings();
    }

    fn save_settings(&mut self) {
        if self.settings.saving {
            return;
        }
        let Some(form) = &self.settings.form else {
            self.settings.message = self.current_lang().settings_not_loaded().to_string();
            self.status = self.settings.message.clone();
            return;
        };
        let project_dir = self.current_project_dir();
        let form = form.clone();
        let revision = self.settings.revision;
        self.settings.saving = true;
        self.settings.message = self.current_lang().settings_saving().to_string();
        self.status = self.settings.message.clone();
        let sender = self.ensure_event_sender();
        thread::spawn(move || {
            let result = form.save(project_dir).map_err(|err| err.to_string());
            let _ = sender.send(GuiEvent::SettingsSaved { revision, result });
        });
    }

    fn open_mod_list(&mut self) {
        if self.mod_list.loading {
            return;
        }
        if self.mod_list.dirty {
            self.mod_list.message = self.current_lang().mod_list_unsaved_block().to_string();
            self.status = self.mod_list.message.clone();
            return;
        }
        self.mod_list.load_attempted = true;
        let project_dir = self.current_project_dir();
        self.mod_list.loading = true;
        self.mod_list.message = self.current_lang().mod_list_loading().to_string();
        self.status = self.mod_list.message.clone();
        let sender = self.ensure_event_sender();
        thread::spawn(move || {
            let result = load_mod_list_thread(project_dir).map_err(|err| err.to_string());
            let _ = sender.send(GuiEvent::ModListLoaded(result));
        });
    }

    fn save_mod_list(&mut self) {
        if self.mod_list.saving {
            return;
        }
        let Some(list) = &self.mod_list.list else {
            self.mod_list.message = self.current_lang().mod_list_not_loaded().to_string();
            self.status = self.mod_list.message.clone();
            return;
        };
        let project_dir = self.current_project_dir();
        let list = list.clone();
        let revision = self.mod_list.revision;
        self.mod_list.saving = true;
        self.mod_list.message = self.current_lang().mod_list_saving().to_string();
        self.status = self.mod_list.message.clone();
        let sender = self.ensure_event_sender();
        thread::spawn(move || {
            let result = list.save(project_dir).map_err(|err| err.to_string());
            let _ = sender.send(GuiEvent::ModListSaved { revision, result });
        });
    }

    fn schedule_project_search(&mut self) {
        self.mod_list.project_search_generation += 1;
        self.mod_list.project_search_results.clear();
        self.mod_list.project_search_error = None;
        self.mod_list.project_search_loading = false;
        match parse_project_search_input(&self.mod_list.project_search_input) {
            Ok(Some(_)) => {
                self.mod_list.project_search_deadline =
                    Some(Instant::now() + Duration::from_millis(350));
            }
            Ok(None) => {
                self.mod_list.project_search_deadline = None;
            }
            Err(err) => {
                self.mod_list.project_search_deadline = None;
                self.mod_list.project_search_error = Some(err.to_string());
            }
        }
    }

    fn dispatch_pending_project_search(&mut self, ctx: &egui::Context) {
        let Some(deadline) = self.mod_list.project_search_deadline else {
            return;
        };
        let now = Instant::now();
        if now < deadline {
            ctx.request_repaint_after(deadline.duration_since(now));
            return;
        }
        self.mod_list.project_search_deadline = None;
        let input = match parse_project_search_input(&self.mod_list.project_search_input) {
            Ok(Some(input)) => input,
            Ok(None) => return,
            Err(err) => {
                self.mod_list.project_search_error = Some(err.to_string());
                return;
            }
        };
        if let Some(cached) = self.mod_list.project_search_cache.get(&input) {
            self.mod_list.project_search_results = cached.clone();
            return;
        }
        let project_dir = self.current_project_dir();
        let generation = self.mod_list.project_search_generation;
        let networks = self.network_engines.clone();
        self.mod_list.project_search_loading = true;
        let sender = self.ensure_event_sender();
        thread::spawn(move || {
            let result = search_projects_thread(project_dir, input.clone(), networks);
            let _ = sender.send(GuiEvent::ModListProjectsSearched {
                generation,
                input,
                result,
            });
        });
    }

    fn render_project_search_results(&mut self, ui: &mut egui::Ui, dark: bool, lang: Language) {
        if self.mod_list.add_draft.is_some()
            || (self.mod_list.project_search_results.is_empty()
                && !self.mod_list.project_search_loading
                && self.mod_list.project_search_error.is_none())
        {
            return;
        }
        ui.add_space(8.0);
        egui::Frame::NONE
            .fill(if dark {
                Color32::from_gray(31)
            } else {
                Color32::from_gray(248)
            })
            .stroke(Stroke::new(1.0, border_color(dark)))
            .corner_radius(7)
            .inner_margin(egui::Margin::same(8))
            .show(ui, |ui| {
                if self.mod_list.project_search_loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(lang.mod_list_searching_projects());
                    });
                }
                if let Some(error) = &self.mod_list.project_search_error {
                    ui.label(RichText::new(error).color(Color32::from_rgb(205, 85, 85)));
                } else if !self.mod_list.project_search_loading
                    && self.mod_list.project_search_results.is_empty()
                {
                    ui.label(RichText::new(lang.mod_list_no_projects()).color(text_muted(dark)));
                }

                let mut selected = None;
                vertical_scroll_area().max_height(260.0).show(ui, |ui| {
                    for project in &self.mod_list.project_search_results {
                        let response = egui::Frame::NONE
                            .fill(if dark {
                                Color32::from_gray(38)
                            } else {
                                Color32::WHITE
                            })
                            .stroke(Stroke::new(1.0, border_color(dark)))
                            .corner_radius(5)
                            .inner_margin(egui::Margin::same(8))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(&project.name)
                                            .strong()
                                            .color(label_color(dark)),
                                    );
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.label(
                                                RichText::new(localized_dependency_type(
                                                    lang,
                                                    project.dependency_type,
                                                ))
                                                .size(12.0)
                                                .color(text_muted(dark)),
                                            );
                                        },
                                    );
                                });
                                ui.label(
                                    RichText::new(format!(
                                        "PID: {}  |  slug: {}",
                                        project.project_id, project.slug
                                    ))
                                    .size(12.0)
                                    .color(text_muted(dark)),
                                );
                                if !project.authors.is_empty() {
                                    ui.label(
                                        RichText::new(project.authors.join(", "))
                                            .size(12.0)
                                            .color(text_muted(dark)),
                                    );
                                }
                            })
                            .response
                            .interact(Sense::click());
                        if response.clicked() {
                            selected = Some(project.clone());
                        }
                        ui.add_space(4.0);
                    }
                });
                if let Some(project) = selected {
                    self.select_project_search_result(project);
                }
            });
    }

    fn select_project_search_result(&mut self, project: ProjectCandidate) {
        self.mod_list.add_draft = Some(AddProjectDraft::from_candidate(
            project,
            self.mod_list.config_loader.as_deref(),
        ));
        self.mod_list.add_file_candidates = None;
        self.mod_list.querying_add_files = false;
        self.mod_list.project_search_input.clear();
        self.mod_list.project_search_results.clear();
        self.mod_list.project_search_error = None;
        self.mod_list.project_search_loading = false;
        self.mod_list.project_search_deadline = None;
    }

    fn add_project_draft_to_list(&mut self) {
        let Some(draft) = self.mod_list.add_draft.take() else {
            return;
        };
        let name = draft.project.name.clone();
        let entry = draft.into_entry();
        let list = self
            .mod_list
            .list
            .get_or_insert_with(|| GuiDependencyList::from_entries(Vec::new()));
        list.entries.push(entry);
        let row_id = self.allocate_mod_list_row_id();
        self.mod_list.row_ids.push(row_id);
        self.mark_mod_list_dirty();
        self.mod_list.message = self.current_lang().mod_list_added(&name);
        self.status = self.mod_list.message.clone();
    }

    fn query_file_candidates_for_row(&mut self, index: usize, row_id: u64) {
        if self.mod_list.querying_files {
            return;
        }
        let Some(list) = &self.mod_list.list else {
            return;
        };
        let Some(entry) = list.entries.get(index) else {
            return;
        };
        if !entry.allows_file_version_selection() {
            return;
        }
        let project = if let Some(project_id) = entry.project_id {
            crate::gui::mod_list::ProjectLookup::ProjectId(project_id)
        } else if let Some(slug) = entry.slug.clone() {
            crate::gui::mod_list::ProjectLookup::Slug(slug)
        } else {
            self.mod_list.message = self
                .current_lang()
                .mod_list_missing_project_locator()
                .to_string();
            self.status = self.mod_list.message.clone();
            return;
        };
        let project_dir = self.current_project_dir();
        let loader = entry.loader.clone();
        let cache = std::sync::Arc::clone(&self.mod_list.file_candidate_cache);
        self.mod_list.file_candidate_generation += 1;
        let generation = self.mod_list.file_candidate_generation;
        let revision = self.mod_list.revision;
        let networks = self.network_engines.clone();
        self.mod_list.querying_files = true;
        self.mod_list.message = self.current_lang().mod_list_querying_files().to_string();
        self.status = self.mod_list.message.clone();
        let sender = self.ensure_event_sender();
        thread::spawn(move || {
            let result = query_file_candidates_thread(
                project_dir,
                cache,
                generation,
                revision,
                row_id,
                project,
                loader,
                networks,
            );
            let _ = sender.send(GuiEvent::ModListFileCandidatesQueried(result));
        });
    }

    fn query_file_candidates_for_add_draft(&mut self) {
        let Some(draft) = self.mod_list.add_draft.as_ref() else {
            return;
        };
        if !draft.allows_file_version_selection() {
            return;
        }
        let project_dir = self.current_project_dir();
        let cache = std::sync::Arc::clone(&self.mod_list.file_candidate_cache);
        let project_id = draft.project.project_id;
        let loader = draft.loader.clone();
        self.mod_list.add_file_candidate_generation += 1;
        let generation = self.mod_list.add_file_candidate_generation;
        let networks = self.network_engines.clone();
        self.mod_list.querying_add_files = true;
        self.mod_list.add_file_candidates = None;
        self.mod_list.message = self.current_lang().mod_list_querying_files().to_string();
        self.status = self.mod_list.message.clone();
        let sender = self.ensure_event_sender();
        thread::spawn(move || {
            let result = query_add_project_file_candidates_thread(
                project_dir,
                cache,
                generation,
                project_id,
                loader,
                networks,
            );
            let _ = sender.send(GuiEvent::ModListAddFileCandidatesQueried(result));
        });
    }

    fn load_overrides_catalog(&mut self) {
        if self.overrides.loading {
            return;
        }
        let project_dir = self.current_project_dir();
        if !project_dir.join("overrides").is_dir() {
            self.overrides.catalog = Some(OverrideCatalog::default());
            self.overrides.load_attempted = true;
            self.overrides.tree = None;
            self.overrides.tree_key = None;
            self.overrides.message = self.current_lang().overrides_empty().to_string();
            self.status = self.overrides.message.clone();
            return;
        }
        self.overrides.loading = true;
        self.overrides.message = self.current_lang().overrides_loading().to_string();
        self.status = self.overrides.message.clone();
        let sender = self.ensure_event_sender();
        thread::spawn(move || {
            let result = OverrideCatalog::scan(&project_dir).map_err(|err| err.to_string());
            let _ = sender.send(GuiEvent::OverridesLoaded(result));
        });
    }

    /// Opens one overrides layer file, or reveals it when the entry cannot be edited in place.
    fn open_overrides_path(&mut self, request: OverrideOpenRequest) {
        let result = match request.mode {
            OverrideOpenMode::Edit => open_override_file(&request.path),
            OverrideOpenMode::Reveal => reveal_override_file(&request.path),
        };
        if let Err(error) = result {
            self.overrides.message = self.current_lang().overrides_open_failed(&error);
            self.status = self.overrides.message.clone();
        }
    }

    fn overrides_panel(&mut self, ui: &mut egui::Ui) {
        let dark = ui.visuals().dark_mode;
        let lang = self.current_lang();
        if !self.overrides.load_attempted && !self.overrides.loading {
            self.load_overrides_catalog();
        }
        let project_root = self.current_project_dir();
        let mut reload = false;
        let mut retarget: Option<(usize, OverrideScope)> = None;
        let mut open_request: Option<OverrideOpenRequest> = None;
        let mut delete_request: Option<usize> = None;

        egui::Frame::NONE
            .fill(panel_bg(dark))
            .stroke(Stroke::new(1.0, border_color(dark)))
            .corner_radius(10)
            .inner_margin(egui::Margin::same(14))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(lang.cmd_title_overrides());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self
                            .accent_button(ui, lang.overrides_reload(), [110.0, 30.0])
                            .clicked()
                        {
                            reload = true;
                        }
                    });
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(lang.overrides_scope())
                            .size(12.5)
                            .color(text_muted(dark)),
                    );
                    for filter in OVERRIDE_SCOPE_FILTERS {
                        ui.selectable_value(
                            &mut self.overrides.scope_filter,
                            *filter,
                            override_scope_filter_label(lang, *filter),
                        );
                    }
                    ui.add_sized(
                        [220.0, 26.0],
                        TextEdit::singleline(&mut self.overrides.filter)
                            .hint_text(lang.overrides_file_name()),
                    );
                });
                if !self.overrides.message.trim().is_empty() {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(&self.overrides.message)
                            .size(12.0)
                            .color(text_muted(dark)),
                    );
                }
            });

        ui.add_space(10.0);
        egui::Frame::NONE
            .fill(panel_bg(dark))
            .stroke(Stroke::new(1.0, border_color(dark)))
            .corner_radius(10)
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                let height = (ui.available_height() - 2.0).max(260.0);
                // The tree is cached between frames: rebuilding it for every frame is what made
                // the page freeze on a pack whose folders hold thousands of files.
                refresh_override_tree(&mut self.overrides);
                let Some(catalog) = self.overrides.catalog.as_ref() else {
                    ui.label(lang.overrides_loading());
                    return;
                };
                let Some(tree) = self.overrides.tree.as_ref() else {
                    ui.label(lang.overrides_empty());
                    return;
                };
                vertical_scroll_area()
                    .auto_shrink([false, false])
                    .max_height(height)
                    .show(ui, |ui| {
                        render_override_tree(
                            ui,
                            tree,
                            0,
                            &mut OverrideTreeRender {
                                catalog,
                                project_dir: &project_root,
                                lang,
                                dark,
                                retarget: &mut retarget,
                                open_request: &mut open_request,
                                pending_delete: &mut self.overrides.pending_delete,
                                delete_request: &mut delete_request,
                            },
                        );
                    });
            });

        if let Some(request) = open_request {
            self.open_overrides_path(request);
        }
        if let Some(entry_index) = delete_request {
            let project_dir = self.current_project_dir();
            self.overrides.message = match self.delete_override_entry(&project_dir, entry_index) {
                Ok(path) => format!("{}: {path}", lang.overrides_deleted()),
                Err(error) => error,
            };
            self.status = self.overrides.message.clone();
        }
        if let Some((entry_index, scope)) = retarget {
            let project_dir = self.current_project_dir();
            self.overrides.message =
                match self.retarget_override_entry(&project_dir, entry_index, scope) {
                    Ok(path) => format!("{}: {path}", lang.overrides_loaded()),
                    Err(error) => error,
                };
            self.status = self.overrides.message.clone();
        }
        if reload {
            self.overrides.load_attempted = false;
            self.overrides.catalog = None;
            self.load_overrides_catalog();
        }
    }

    /// Writes one entry into the chosen scope and updates only that row.
    ///
    /// Retargeting changes nothing but the layers of one path, so the catalog is updated in
    /// place instead of rescanning the whole `overrides` tree.
    fn retarget_override_entry(
        &mut self,
        project_dir: &Path,
        entry_index: usize,
        scope: OverrideScope,
    ) -> Result<String, String> {
        let lang = self.current_lang();
        let entry = {
            let Some(catalog) = self.overrides.catalog.as_ref() else {
                return Err(lang.overrides_empty().to_string());
            };
            let Some(entry) = catalog.entries.get(entry_index) else {
                return Err(lang.overrides_scope_missing_entry().to_string());
            };
            entry.clone()
        };
        let updated = entry.retargeted(scope).map_err(|error| error.to_string())?;
        if let Err(error) = apply_scope(project_dir, &entry, scope) {
            // A retarget can stop halfway through the layers, leaving the catalog describing a
            // layout the disk no longer has. Rescanning is the only honest row to show.
            self.invalidate_overrides_catalog();
            return Err(error.to_string());
        }
        let path = updated.path.clone();
        if let Some(catalog) = self.overrides.catalog.as_mut()
            && let Some(slot) = catalog.entries.get_mut(entry_index)
        {
            *slot = updated;
        }
        // A retarget changes which scope filter lists the row, so the cached tree is rebuilt.
        self.overrides.revision = self.overrides.revision.wrapping_add(1);
        Ok(path)
    }

    /// Deletes one entry from every layer that holds it and drops the row in place.
    fn delete_override_entry(
        &mut self,
        project_dir: &Path,
        entry_index: usize,
    ) -> Result<String, String> {
        let lang = self.current_lang();
        let entry = {
            let Some(catalog) = self.overrides.catalog.as_ref() else {
                return Err(lang.overrides_empty().to_string());
            };
            let Some(entry) = catalog.entries.get(entry_index) else {
                return Err(lang.overrides_delete_missing().to_string());
            };
            entry.clone()
        };
        self.overrides.pending_delete = None;
        if let Err(error) = delete_entry(project_dir, &entry) {
            // Some layers may already be gone, so the scanned catalog is not what the disk
            // holds any more; rescan instead of leaving a row that lies about it.
            self.invalidate_overrides_catalog();
            return Err(lang.overrides_delete_failed(&error.to_string()));
        }
        let path = entry.path.clone();
        if let Some(catalog) = self.overrides.catalog.as_mut()
            && entry_index < catalog.entries.len()
        {
            catalog.entries.remove(entry_index);
        }
        // Dropping an entry shifts every later index, so the cached rows are rebuilt.
        self.overrides.revision = self.overrides.revision.wrapping_add(1);
        Ok(path)
    }

    /// Drops the scanned catalog and the tree built from it, and rescans on the next frame.
    fn invalidate_overrides_catalog(&mut self) {
        self.overrides.load_attempted = false;
        self.overrides.catalog = None;
        self.overrides.loading = false;
        self.overrides.tree = None;
        self.overrides.tree_key = None;
    }

    fn load_sync_manifest(&mut self) {
        if self.sync.loading {
            return;
        }
        let project_dir = self.current_project_dir();
        let side = self.sync.side;
        self.sync.loading = true;
        self.sync.message = self.current_lang().sync_load().to_string();
        self.status = self.sync.message.clone();
        let sender = self.ensure_event_sender();
        thread::spawn(move || {
            let result = load_sync_manifest_thread(project_dir, side);
            let _ = sender.send(GuiEvent::SyncLoaded(result));
        });
    }

    fn apply_sync_manifest(&mut self) {
        if self.sync.applying {
            return;
        }
        let Some(manifest) = self.sync.manifest.clone() else {
            self.sync.message = self.current_lang().sync_empty().to_string();
            self.status = self.sync.message.clone();
            return;
        };
        let project_dir = self.current_project_dir();
        self.sync.applying = true;
        self.sync.message = self.current_lang().sync_apply().to_string();
        self.status = self.sync.message.clone();
        let sender = self.ensure_event_sender();
        thread::spawn(move || {
            let mut manifest = manifest;
            let result = apply_pending_sync(&project_dir, &mut manifest)
                .map(|applied| SyncApplyOutcome {
                    applied,
                    remaining: manifest,
                })
                .map_err(|err| err.to_string());
            let _ = sender.send(GuiEvent::SyncApplied(result));
        });
    }

    fn ensure_event_sender(&mut self) -> mpsc::SyncSender<GuiEvent> {
        let (sender, receiver) = mpsc::sync_channel(MAX_GUI_EVENT_QUEUE);
        if self.receiver.is_some() {
            self.receiver = Some(merge_receivers(
                self.receiver.take().expect("receiver checked"),
                receiver,
            ));
        } else {
            self.receiver = Some(receiver);
        }
        sender
    }

    fn allocate_mod_list_row_id(&mut self) -> u64 {
        let row_id = self.mod_list.next_row_id;
        self.mod_list.next_row_id += 1;
        row_id
    }

    fn reset_mod_list_row_ids(&mut self) {
        let row_count = self
            .mod_list
            .list
            .as_ref()
            .map_or(0, |list| list.entries.len());
        self.mod_list.row_ids.clear();
        for _ in 0..row_count {
            let row_id = self.allocate_mod_list_row_id();
            self.mod_list.row_ids.push(row_id);
        }
    }

    fn mark_mod_list_dirty(&mut self) {
        self.mod_list.dirty = true;
        self.mod_list.revision += 1;
    }

    fn mark_settings_dirty(&mut self) {
        self.settings.dirty = true;
        self.settings.revision += 1;
    }

    fn clear_mod_list_project_state(&mut self) {
        self.mod_list.list = None;
        self.mod_list.dirty = false;
        self.mod_list.saving = false;
        self.mod_list.project_search_input.clear();
        self.mod_list.project_search_results.clear();
        self.mod_list.project_search_error = None;
        self.mod_list.project_search_loading = false;
        self.mod_list.project_search_deadline = None;
        self.mod_list.project_search_generation =
            self.mod_list.project_search_generation.wrapping_add(1);
        self.mod_list.project_search_cache.clear();
        self.mod_list.querying_files = false;
        self.mod_list.file_candidate_generation =
            self.mod_list.file_candidate_generation.wrapping_add(1);
        self.mod_list.querying_add_files = false;
        self.mod_list.add_file_candidate_generation =
            self.mod_list.add_file_candidate_generation.wrapping_add(1);
        self.mod_list.row_ids.clear();
        self.mod_list.next_row_id = 1;
        self.mod_list.revision = self.mod_list.revision.wrapping_add(1);
        self.mod_list.file_candidate_row_id = None;
        self.mod_list.file_candidates.clear();
        self.mod_list.add_file_candidates = None;
        self.mod_list.search.clear();
        self.mod_list.minecraft_version = None;
        self.mod_list.config_loader = None;
        self.mod_list.file_candidate_cache.clear();
        self.mod_list.add_draft = None;
    }

    fn delete_mod_list_row(&mut self, index: usize, row_id: u64) {
        let Some(list) = &mut self.mod_list.list else {
            return;
        };
        if list.entries.get(index).is_none()
            || self.mod_list.row_ids.get(index).copied() != Some(row_id)
        {
            self.mod_list.message = self.current_lang().mod_list_row_state_error().to_string();
            self.status = self.mod_list.message.clone();
            return;
        }
        list.entries.remove(index);
        self.mod_list.row_ids.remove(index);
        self.mod_list.file_candidate_generation += 1;
        self.mod_list.querying_files = false;
        self.mod_list.file_candidate_row_id = None;
        self.mod_list.file_candidates.clear();
        self.mark_mod_list_dirty();
    }

    fn current_project_dir(&self) -> PathBuf {
        if self.form.use_custom_project_dir {
            self.form.project_dir.clone()
        } else {
            PathBuf::from(".")
        }
    }

    fn apply_mod_list_loaded(&mut self, result: Result<ModListLoadResult, String>) {
        self.mod_list.loading = false;
        match result {
            Ok(result) => {
                self.clear_mod_list_project_state();
                self.mod_list.list = Some(result.list);
                self.reset_mod_list_row_ids();
                self.mod_list.minecraft_version = Some(result.settings.minecraft_version);
                self.mod_list.config_loader = Some(result.settings.loader);
                self.mod_list.message = self.current_lang().mod_list_loaded().to_string();
            }
            Err(err) => {
                self.clear_mod_list_project_state();
                self.mod_list.message = err;
            }
        }
        self.status = self.mod_list.message.clone();
    }

    fn apply_mod_list_saved(&mut self, revision: u64, result: Result<(), String>) {
        self.mod_list.saving = false;
        match result {
            Ok(()) => {
                if self.mod_list.revision == revision {
                    self.mod_list.dirty = false;
                    self.mod_list.message = self.current_lang().mod_list_saved().to_string();
                } else {
                    self.mod_list.message = self
                        .current_lang()
                        .mod_list_saved_with_changes()
                        .to_string();
                }
            }
            Err(err) => {
                self.mod_list.message = err;
            }
        }
        self.status = self.mod_list.message.clone();
    }

    fn apply_projects_searched(
        &mut self,
        generation: u64,
        input: ProjectSearchInput,
        result: Result<Vec<ProjectCandidate>, String>,
    ) {
        if generation != self.mod_list.project_search_generation {
            return;
        }
        let Ok(Some(current_input)) =
            parse_project_search_input(&self.mod_list.project_search_input)
        else {
            return;
        };
        if current_input != input {
            return;
        }
        self.mod_list.project_search_loading = false;
        match result {
            Ok(projects) => {
                self.mod_list
                    .project_search_cache
                    .insert(input, projects.clone());
                self.mod_list.project_search_results = projects;
                self.mod_list.project_search_error = None;
            }
            Err(err) => {
                self.mod_list.project_search_results.clear();
                self.mod_list.project_search_error = Some(err);
            }
        }
    }

    fn apply_file_candidates_queried(&mut self, result: ModListFileCandidateResult) {
        if result.generation != self.mod_list.file_candidate_generation
            || result.revision != self.mod_list.revision
        {
            return;
        }
        self.mod_list.querying_files = false;
        let current_index = self
            .mod_list
            .row_ids
            .iter()
            .position(|row_id| *row_id == result.row_id);
        let can_select_file_version = current_index
            .and_then(|index| {
                self.mod_list
                    .list
                    .as_ref()
                    .and_then(|list| list.entries.get(index))
            })
            .is_some_and(|entry| {
                entry.allows_file_version_selection()
                    && entry.loader == result.loader
                    && project_lookup_for_entry(entry) == Some(result.project.clone())
            });
        if !can_select_file_version {
            self.mod_list.file_candidate_row_id = None;
            self.mod_list.file_candidates.clear();
            self.mod_list.message.clear();
            self.status = self.current_lang().status_ready().to_string();
            return;
        }
        match result.result {
            Ok(candidates) => {
                self.mod_list.file_candidate_row_id = Some(result.row_id);
                self.mod_list.file_candidates = candidates;
                self.mod_list.message = format!(
                    "{}: {}",
                    self.current_lang().mod_list_files(),
                    self.mod_list.file_candidates.len()
                );
            }
            Err(err) => {
                self.mod_list.message = err;
            }
        }
        self.status = self.mod_list.message.clone();
    }

    fn apply_add_file_candidates_queried(&mut self, result: AddProjectFileCandidateResult) {
        if result.generation != self.mod_list.add_file_candidate_generation {
            return;
        }
        self.mod_list.querying_add_files = false;
        let is_current_draft = self.mod_list.add_draft.as_ref().is_some_and(|draft| {
            draft.allows_file_version_selection()
                && draft.project.project_id == result.project_id
                && draft.loader == result.loader
        });
        if !is_current_draft {
            return;
        }
        match result.result {
            Ok(candidates) => {
                self.mod_list.add_file_candidates = Some(candidates);
                self.mod_list.message.clear();
                self.status = self.current_lang().status_ready().to_string();
            }
            Err(err) => {
                self.mod_list.add_file_candidates = Some(Vec::new());
                self.mod_list.message = err;
                self.status = self.mod_list.message.clone();
            }
        }
    }

    fn apply_sync_loaded(&mut self, result: Result<PendingSyncManifest, String>) {
        self.sync.loading = false;
        match result {
            Ok(manifest) => {
                self.sync.side = manifest.side;
                let count = manifest.entries.len();
                self.sync.manifest = Some(manifest);
                self.sync.message = format!("{}: {count}", self.current_lang().sync_loaded());
            }
            Err(err) => {
                self.sync.manifest = None;
                self.sync.message = err;
            }
        }
        self.status = self.sync.message.clone();
    }

    fn apply_overrides_loaded(&mut self, result: Result<OverrideCatalog, String>) {
        self.overrides.loading = false;
        self.overrides.load_attempted = true;
        // A fresh scan describes a new tree, so the cached one no longer applies.
        self.overrides.tree = None;
        self.overrides.tree_key = None;
        match result {
            Ok(catalog) => {
                let count = catalog.entries.len();
                self.overrides.catalog = Some(catalog);
                self.overrides.message =
                    format!("{}: {count}", self.current_lang().overrides_loaded());
            }
            Err(err) => {
                self.overrides.catalog = Some(OverrideCatalog::default());
                self.overrides.message = err;
            }
        }
        self.status = self.overrides.message.clone();
    }

    fn apply_settings_loaded(&mut self, result: Result<SettingsForm, String>) {
        self.settings.loading = false;
        match result {
            Ok(form) => {
                self.settings.form = Some(form);
                self.settings.dirty = false;
                self.settings.revision += 1;
                self.settings.message = self.current_lang().settings_loaded().to_string();
                self.invalidate_settings_loader_versions();
            }
            Err(err) => {
                self.settings.form = None;
                self.settings.message = err;
            }
        }
        self.status = self.settings.message.clone();
    }

    fn apply_settings_loader_versions(
        &mut self,
        generation: u64,
        query: LoaderVersionQuery,
        result: Result<LoaderVersionCandidates, String>,
    ) {
        if generation != self.settings.loader_query_generation
            || self.settings.loader_query.as_ref() != Some(&query)
        {
            return;
        }
        match result {
            Ok(candidates) => {
                self.settings.loader_versions = candidates.loader_versions.clone();
                self.settings.forge_compat_versions = candidates.forge_compat_versions.clone();
                self.settings
                    .loader_versions_cache
                    .insert(query, candidates);
                self.settings.loader_versions_state.finish_request(Ok(()));
            }
            Err(error) => {
                self.settings.loader_versions.clear();
                self.settings.forge_compat_versions.clear();
                self.settings
                    .loader_versions_state
                    .finish_request(Err(error));
            }
        }
    }

    fn apply_settings_saved(&mut self, revision: u64, result: Result<(), String>) {
        self.settings.saving = false;
        match result {
            Ok(()) => {
                if self.settings.revision == revision {
                    self.settings.dirty = false;
                    self.settings.message = self.current_lang().settings_saved().to_string();
                } else {
                    self.settings.message = self
                        .current_lang()
                        .settings_saved_with_changes()
                        .to_string();
                }
            }
            Err(err) => {
                self.settings.message = err;
            }
        }
        self.status = self.settings.message.clone();
    }

    fn apply_sync_applied(&mut self, result: Result<SyncApplyOutcome, String>) {
        self.sync.applying = false;
        match result {
            Ok(outcome) => {
                let count = outcome.applied.len();
                self.sync.manifest = Some(outcome.remaining);
                self.sync.loading = false;
                self.sync.message = format!("{}: {count}", self.current_lang().sync_apply());
                if self
                    .sync
                    .manifest
                    .as_ref()
                    .is_some_and(|manifest| manifest.entries.is_empty())
                {
                    self.sync.window_open = false;
                }
            }
            Err(err) => {
                self.sync.message = err;
            }
        }
        self.status = self.sync.message.clone();
    }

    /// Scrolls the console with the wheel while a drag is in progress.    ///
    /// egui hands a scroll area the wheel only while nothing is being dragged, and dragging
    /// inside the log is exactly how text gets selected. Releasing the drag would throw that
    /// selection away, so the wheel is added to the console's own scroll offset here; egui
    /// re-clamps the offset and unpins it from the bottom before drawing.
    fn scroll_console_while_dragging(&mut self, ui: &egui::Ui) {
        if ui.ctx().dragged_id().is_none() {
            // Nothing is being dragged, so the scroll area handles the wheel itself.
            return;
        }
        let Some(id) = self.console_scroll_id else {
            return;
        };
        let wheel = ui.input(|input| input.smooth_scroll_delta.y);
        if wheel == 0.0 {
            return;
        }
        let Some(mut state) = egui::scroll_area::State::load(ui.ctx(), id) else {
            return;
        };
        state.offset.y = console_offset_after_wheel(state.offset.y, wheel);
        state.store(ui.ctx(), id);
    }

    fn log_panel(&mut self, ui: &mut egui::Ui, height: f32) {
        let dark = ui.visuals().dark_mode;
        let lang = self.current_lang();
        // The console belongs to one side: the stop button and the input row only apply when this
        // page's own run is the one producing output.
        let page_side = self.current_page_side();
        let side_running = page_side.is_some_and(|side| self.run_active(side));
        // The indicator follows whichever terminal is actually writing: the run pages have one
        // each, and the remaining pages share a single console.
        let console_active = side_running || (self.running && page_side.is_none());
        egui::Frame::NONE
            .fill(LOG_BG)
            .stroke(Stroke::new(1.0, border_color(dark)))
            .corner_radius(10)
            .show(ui, |ui| {
                ui.set_height(height);
                ui.set_width(ui.available_width());
                egui::Frame::NONE
                    .fill(LOG_HEADER_BG)
                    .inner_margin(egui::Margin::symmetric(14, 7))
                    .corner_radius(egui::CornerRadius {
                        nw: 10,
                        ne: 10,
                        sw: 0,
                        se: 0,
                    })
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.set_width(ui.available_width());
                            let dot_color = if console_active {
                                ACCENT_HOVER
                            } else {
                                Color32::from_gray(90)
                            };
                            ui.label(RichText::new("●").size(10.0).color(dot_color));
                            ui.label(
                                RichText::new(lang.console_output())
                                    .size(11.5)
                                    .color(LOG_HEADER_TEXT),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let label = if self.console_expanded {
                                        lang.console_collapse()
                                    } else {
                                        lang.console_expand()
                                    };
                                    if ui
                                        .button(
                                            RichText::new(label).size(11.5).color(LOG_HEADER_TEXT),
                                        )
                                        .clicked()
                                    {
                                        self.console_expanded = !self.console_expanded;
                                    }
                                    if side_running
                                        && ui
                                            .button(
                                                RichText::new(lang.console_stop())
                                                    .size(11.5)
                                                    .color(DANGER),
                                            )
                                            .clicked()
                                        && let Some(run) =
                                            page_side.and_then(|side| self.side_run(side))
                                    {
                                        run.stop_signal.request_stop();
                                    }
                                },
                            );
                        });
                    });
                // Reserve the body frame's own margins and the input row. Under-reserving made the
                // panel taller than the space it was given, clipping the input row at the window
                // edge, and a forced minimum kept that overflow even in a short window.
                let reserved = LOG_BODY_VERTICAL_MARGIN
                    + if side_running {
                        CONSOLE_INPUT_ROW_HEIGHT
                    } else {
                        0.0
                    };
                let body_height = (ui.available_height() - reserved).max(0.0);
                egui::Frame::NONE
                    .inner_margin(egui::Margin::symmetric(14, 10))
                    .show(ui, |ui| {
                        let v = ui.visuals_mut();
                        v.extreme_bg_color = LOG_BG;
                        v.text_edit_bg_color = Some(LOG_BG);
                        v.widgets.inactive.bg_stroke = Stroke::NONE;
                        v.widgets.hovered.bg_stroke = Stroke::NONE;
                        v.widgets.active.bg_stroke = Stroke::NONE;
                        // egui gives a scroll area the wheel only while nothing is being dragged,
                        // so selecting text in the log would stop it from scrolling. The wheel is
                        // applied to the console's own offset instead of cancelling the drag.
                        self.scroll_console_while_dragging(ui);
                        let output = vertical_scroll_area()
                            .stick_to_bottom(true)
                            .auto_shrink([false, false])
                            .min_scrolled_height(body_height)
                            .max_height(body_height)
                            .show(ui, |ui| {
                                let mut log = self.visible_log();
                                ui.add(
                                    TextEdit::multiline(&mut log)
                                        .font(egui::TextStyle::Monospace)
                                        .text_color(LOG_TEXT)
                                        .desired_width(f32::INFINITY),
                                );
                            });
                        self.console_scroll_id = Some(output.id);
                    });
                if side_running {
                    ui.add_space(4.0);
                    egui::Frame::NONE
                        .inner_margin(egui::Margin::symmetric(14, 4))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let send_width = 72.0;
                                let field_width =
                                    (ui.available_width() - send_width - 8.0).max(120.0);
                                let field = ui.add_sized(
                                    [field_width, 24.0],
                                    TextEdit::singleline(&mut self.console_line)
                                        .font(egui::TextStyle::Monospace)
                                        .text_color(LOG_TEXT)
                                        .hint_text(lang.console_input_hint()),
                                );
                                // Enter submits without leaving the field, so the next command can
                                // be typed straight away.
                                let submitted_by_enter = field.lost_focus()
                                    && ui.input(|input| input.key_pressed(egui::Key::Enter));
                                let submitted_by_button = ui
                                    .add_sized(
                                        [send_width, 24.0],
                                        egui::Button::new(lang.console_send()),
                                    )
                                    .clicked();
                                if submitted_by_enter || submitted_by_button {
                                    if let Some(run) =
                                        page_side.and_then(|side| self.side_run(side))
                                    {
                                        run.console_input.submit(&self.console_line);
                                    }
                                    self.console_line.clear();
                                    field.request_focus();
                                }
                            });
                        });
                }
            });
    }

    fn run_console_panel(&mut self, ui: &mut egui::Ui) {
        let available = available_height(ui.available_height());
        // A running page has nothing but the console, so expanding it fills the page and
        // collapsing it leaves the console at its smallest useful height.
        let height = if self.console_expanded {
            available
        } else {
            available.min(COMMAND_LOG_MIN_HEIGHT)
        };
        self.log_panel(ui, height);
    }

    fn footer(&mut self, ui: &mut egui::Ui) {
        let lang = self.current_lang();
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(&self.status)
                    .size(13.0)
                    .color(text_muted(ui.visuals().dark_mode)),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.form.command == GuiCommand::ModList {
                    if self
                        .accent_button(ui, lang.btn_save(), [140.0, 38.0])
                        .clicked()
                        && !self.running
                    {
                        self.save_mod_list();
                    }
                } else if self.form.command == GuiCommand::Settings {
                    if self
                        .accent_button(ui, lang.btn_save(), [140.0, 38.0])
                        .clicked()
                        && !self.running
                    {
                        self.save_settings();
                    }
                } else if self.form.command == GuiCommand::Overrides {
                    // The Overrides page is browse-and-edit only; its actions live in the page
                    // header, so the footer must not offer a run button for it.
                } else {
                    let label = lang.btn_run(self.form.command.cli_name());
                    // Run pages may start while the opposite side runs, so both can run at once;
                    // everything else keeps waiting for the running command to finish.
                    let page_side = self.current_page_side();
                    let busy = self.any_global_operation_running()
                        || page_side.is_some_and(|side| self.run_active(side))
                        || (page_side.is_none() && self.any_operation_running());
                    if self
                        .accent_button_enabled(ui, &label, [140.0, 38.0], !busy)
                        .clicked()
                    {
                        self.start_command();
                    }
                }
            });
        });
    }

    fn accent_button(&self, ui: &mut egui::Ui, label: &str, min_size: [f32; 2]) -> egui::Response {
        self.accent_button_enabled(ui, label, min_size, !self.any_operation_running())
    }

    /// Accent button whose enabled state the caller controls, so a run page can start its side
    /// while the opposite side is already running.
    fn accent_button_enabled(
        &self,
        ui: &mut egui::Ui,
        label: &str,
        min_size: [f32; 2],
        enabled: bool,
    ) -> egui::Response {
        ui.scope(|ui| {
            let v = ui.visuals_mut();
            v.widgets.inactive.weak_bg_fill = ACCENT;
            v.widgets.inactive.bg_fill = ACCENT;
            v.widgets.inactive.bg_stroke = Stroke::NONE;
            v.widgets.inactive.fg_stroke = Stroke::new(0.0, Color32::WHITE);
            v.widgets.hovered.weak_bg_fill = ACCENT_HOVER;
            v.widgets.hovered.bg_fill = ACCENT_HOVER;
            v.widgets.hovered.bg_stroke = Stroke::NONE;
            v.widgets.hovered.fg_stroke = Stroke::new(0.0, Color32::WHITE);
            v.widgets.active.weak_bg_fill = ACCENT_ACTIVE;
            v.widgets.active.bg_fill = ACCENT_ACTIVE;
            v.widgets.active.bg_stroke = Stroke::NONE;
            v.widgets.active.fg_stroke = Stroke::new(0.0, Color32::WHITE);
            ui.add_enabled(
                enabled,
                egui::Button::new(RichText::new(label).color(Color32::WHITE).strong())
                    .min_size(egui::vec2(min_size[0], min_size[1]))
                    .corner_radius(7),
            )
        })
        .inner
    }

    /// Operations that block every page: the non-run commands and the project-wide editors.
    fn any_global_operation_running(&self) -> bool {
        self.running
            || self.overrides.loading
            || self.mod_list.loading
            || self.mod_list.saving
            || self.mod_list.querying_files
            || self.settings.loading
            || self.settings.saving
            || self.sync.loading
            || self.sync.applying
    }

    fn any_operation_running(&self) -> bool {
        self.any_global_operation_running()
            || self.client_run.is_some()
            || self.server_run.is_some()
    }

    fn navigation_enabled(&self) -> bool {
        // A running client or server must not trap the user on one page: the console keeps
        // receiving output while they inspect or prepare the other side.
        !(self.overrides.loading
            || self.mod_list.loading
            || self.mod_list.saving
            || self.mod_list.querying_files
            || self.settings.loading
            || self.settings.saving
            || self.sync.loading
            || self.sync.applying)
    }

    fn run_console_mode(&self) -> bool {
        match self.current_page_side() {
            Some(side) => self.run_active(side),
            None => false,
        }
    }

    fn start_command(&mut self) {
        match self.form.to_command_spec() {
            Ok(spec) => {
                self.status = self.current_lang().status_running().to_string();
                self.start_command_thread(spec);
            }
            Err(err) => {
                let lang = self.current_lang();
                self.status = lang.status_input_error().to_string();
                self.append_log_line(&localize_error(lang, &err));
            }
        }
    }

    fn start_command_thread(&mut self, spec: GuiCommandSpec) {
        self.console_line.clear();
        // A run is what the console is for, so starting one expands it; it collapses again once
        // nothing is running. The toggle keeps working by hand in between.
        self.console_expanded = true;
        let side = match spec.task {
            GuiCommandTask::RunClient => Some(RunSide::Client),
            GuiCommandTask::RunServer => Some(RunSide::Server),
            _ => None,
        };
        // Each run starts from a clean terminal of its own side.
        let header = format!("{}\n", spec.display_command());
        let (sender, receiver) = mpsc::sync_channel(MAX_GUI_EVENT_QUEUE);
        match side {
            Some(side) => {
                // Runs are tracked per side, so the client and the server can run at once and
                // each keeps its own console, input row, and stop button.
                let log = match side {
                    RunSide::Client => &mut self.client_log,
                    RunSide::Server => &mut self.server_log,
                };
                *log = header;
                let run = SideRun {
                    receiver,
                    stop_signal: SharedStopSignal::default(),
                    console_input: SharedConsoleInput::default(),
                };
                let stop_signal = run.stop_signal.clone();
                let console_input = run.console_input.clone();
                self.set_side_run(side, Some(run));
                thread::spawn(move || {
                    run_command_thread(spec, sender, Some(console_input), Some(stop_signal))
                });
            }
            None => {
                self.log = header;
                self.running = true;
                self.receiver = Some(receiver);
                thread::spawn(move || run_command_thread(spec, sender, None, None));
            }
        }
    }

    fn side_run(&self, side: RunSide) -> Option<&SideRun> {
        match side {
            RunSide::Client => self.client_run.as_ref(),
            RunSide::Server => self.server_run.as_ref(),
        }
    }

    fn set_side_run(&mut self, side: RunSide, run: Option<SideRun>) {
        match side {
            RunSide::Client => self.client_run = run,
            RunSide::Server => self.server_run = run,
        }
    }

    fn run_active(&self, side: RunSide) -> bool {
        self.side_run(side).is_some()
    }

    fn clear_side_run(&mut self, side: RunSide) {
        self.set_side_run(side, None);
    }

    /// Collapses the console once no run is left to watch, which is the state the page opens in.
    fn collapse_console_when_idle(&mut self) {
        if !self.run_active(RunSide::Client) && !self.run_active(RunSide::Server) {
            self.console_expanded = false;
        }
    }

    fn drain_events(&mut self) {
        self.drain_global_events();
        self.drain_side_events(RunSide::Client);
        self.drain_side_events(RunSide::Server);
    }

    /// Drains the shared channel used by the non-run commands (init / build / check).
    fn drain_global_events(&mut self) {
        // Drain first, then dispatch: a handler may install a fresh event channel while it runs,
        // and holding the receiver by reference here avoids overwriting that new channel with the
        // one this frame started from.
        let mut events = Vec::new();
        {
            let Some(receiver) = self.receiver.as_ref() else {
                return;
            };
            for _ in 0..MAX_GUI_EVENTS_PER_FRAME {
                let Ok(event) = receiver.try_recv() else {
                    break;
                };
                events.push(event);
            }
        }
        if events.is_empty() {
            return;
        }
        let lang = self.current_lang();
        let mut keep_receiver = true;
        for event in events {
            match event {
                GuiEvent::Log(line) => {
                    append_to_log(&mut self.log, &line);
                }
                GuiEvent::Finished(code) => {
                    self.running = false;
                    self.status = lang.status_finished(code);
                    append_to_log(&mut self.log, &format!("[process] exit code {code}"));
                    keep_receiver = false;
                    self.collapse_console_when_idle();
                }
                GuiEvent::Failed(message) => {
                    self.running = false;
                    self.status = lang.status_failed().to_string();
                    append_to_log(&mut self.log, &format!("[process] {message}"));
                    keep_receiver = false;
                    self.collapse_console_when_idle();
                }
                other => self.apply_gui_event(other, &lang),
            }
        }
        if !keep_receiver {
            self.receiver = None;
        }
    }

    /// Drains one side's run channel. Every run owns its terminal, so log lines and the final
    /// status land in that side's console no matter which page is displayed.
    fn drain_side_events(&mut self, side: RunSide) {
        let mut events = Vec::new();
        {
            let Some(run) = self.side_run(side) else {
                return;
            };
            for _ in 0..MAX_GUI_EVENTS_PER_FRAME {
                let Ok(event) = run.receiver.try_recv() else {
                    break;
                };
                events.push(event);
            }
        }
        if events.is_empty() {
            return;
        }
        let lang = self.current_lang();
        for event in events {
            match event {
                GuiEvent::Log(line) => {
                    let log = self.side_log_mut(side);
                    append_to_log(log, &line);
                }
                GuiEvent::Finished(code) => {
                    self.status = lang.status_finished(code);
                    let log = self.side_log_mut(side);
                    append_to_log(log, &format!("[process] exit code {code}"));
                    self.clear_side_run(side);
                    self.collapse_console_when_idle();
                }
                GuiEvent::Failed(message) => {
                    self.status = lang.status_failed().to_string();
                    let log = self.side_log_mut(side);
                    append_to_log(log, &format!("[process] {message}"));
                    self.clear_side_run(side);
                    self.collapse_console_when_idle();
                }
                other => self.apply_gui_event(other, &lang),
            }
        }
    }

    fn side_log_mut(&mut self, side: RunSide) -> &mut String {
        match side {
            RunSide::Client => &mut self.client_log,
            RunSide::Server => &mut self.server_log,
        }
    }

    fn apply_gui_event(&mut self, event: GuiEvent, lang: &Language) {
        match event {
            GuiEvent::Log(_) | GuiEvent::Finished(_) | GuiEvent::Failed(_) => {
                // These are handled by their own drain paths, which know which terminal or run
                // they belong to.
                unreachable!("event handled by its own drain path")
            }
            GuiEvent::ModListLoaded(result) => {
                self.apply_mod_list_loaded(result);
            }
            GuiEvent::ModListSaved { revision, result } => {
                self.apply_mod_list_saved(revision, result);
            }
            GuiEvent::ModListProjectsSearched {
                generation,
                input,
                result,
            } => {
                self.apply_projects_searched(generation, input, result);
            }
            GuiEvent::ModListFileCandidatesQueried(result) => {
                self.apply_file_candidates_queried(result);
            }
            GuiEvent::ModListAddFileCandidatesQueried(result) => {
                self.apply_add_file_candidates_queried(result);
            }
            GuiEvent::SettingsLoaded(result) => {
                self.apply_settings_loaded(*result);
            }
            GuiEvent::SettingsSaved { revision, result } => {
                self.apply_settings_saved(revision, result);
            }
            GuiEvent::SettingsLoaderVersionsLoaded {
                generation,
                query,
                result,
            } => self.apply_settings_loader_versions(generation, query, result),
            GuiEvent::SyncLoaded(result) => {
                self.apply_sync_loaded(result);
            }
            GuiEvent::SyncWindowLoaded(manifest) => {
                if !manifest.entries.is_empty() {
                    self.sync.window_open = true;
                    self.apply_sync_loaded(Ok(manifest));
                }
            }
            GuiEvent::SyncWindowCheckFailed(error) => {
                let message = lang.sync_auto_load_failed(&error);
                append_to_log(&mut self.log, &format!("[sync] {message}"));
                if self.sync.window_open {
                    self.sync.message = message;
                }
            }
            GuiEvent::SyncApplied(result) => {
                self.apply_sync_applied(result);
            }
            GuiEvent::OverridesLoaded(result) => {
                self.apply_overrides_loaded(result);
            }
        }
    }

    /// Side of the page currently displayed, when that page starts a run.
    fn current_page_side(&self) -> Option<RunSide> {
        match self.form.command {
            GuiCommand::RunClient => Some(RunSide::Client),
            GuiCommand::RunServer => Some(RunSide::Server),
            _ => None,
        }
    }

    /// Log shown on the page currently displayed.
    fn visible_log(&self) -> &str {
        match self.current_page_side() {
            Some(RunSide::Client) => &self.client_log,
            Some(RunSide::Server) => &self.server_log,
            None => &self.log,
        }
    }

    /// Log that receives new lines: the running side, otherwise the displayed page.
    fn append_log_line(&mut self, line: &str) {
        let log = match self.current_page_side() {
            Some(side) => self.side_log_mut(side),
            None => &mut self.log,
        };
        append_to_log(log, line);
    }
}

#[derive(Debug, Clone, Copy)]
enum PathField {
    ProjectDir,
    PackZip,
    OutputDir,
}

#[derive(Debug, Clone, Copy)]
enum TextField {
    ProxyIp,
    ProxyPort,
}

fn path_mut(form: &mut CommandForm, field: PathField) -> &mut PathBuf {
    match field {
        PathField::ProjectDir => &mut form.project_dir,
        PathField::PackZip => &mut form.pack_zip,
        PathField::OutputDir => &mut form.output_dir,
    }
}

fn text_mut(form: &mut CommandForm, field: TextField) -> &mut String {
    match field {
        TextField::ProxyIp => &mut form.proxy.ip,
        TextField::ProxyPort => &mut form.proxy.port,
    }
}

fn option_string_mut(value: &mut Option<String>) -> &mut String {
    value.get_or_insert_with(String::new)
}

fn optional_u64_edit(ui: &mut egui::Ui, value: &mut Option<u64>, hint: &str, width: f32) -> bool {
    let mut text = value.map(|number| number.to_string()).unwrap_or_default();
    if ui
        .add_sized(
            [width, 26.0],
            TextEdit::singleline(&mut text).hint_text(hint),
        )
        .changed()
    {
        let trimmed = text.trim();
        *value = if trimmed.is_empty() {
            None
        } else {
            trimmed.parse::<u64>().ok()
        };
        true
    } else {
        false
    }
}

fn combo_manifest(ui: &mut egui::Ui, row_id: u64, value: &mut ManifestList) -> bool {
    let before = *value;
    egui::ComboBox::from_id_salt(("manifest", row_id))
        .selected_text(value.label())
        .show_ui(ui, |ui| {
            for manifest in ManifestList::ALL {
                ui.selectable_value(value, manifest, manifest.label());
            }
        });
    before != *value
}

fn combo_dependency_type(ui: &mut egui::Ui, row_id: u64, value: &mut GuiDependencyType) -> bool {
    let before = *value;
    egui::ComboBox::from_id_salt(("dependency_type", row_id))
        .selected_text(value.label())
        .show_ui(ui, |ui| {
            for dependency_type in GuiDependencyType::ALL {
                ui.selectable_value(value, dependency_type, dependency_type.label());
            }
        });
    before != *value
}

fn labeled_add_manifest(
    ui: &mut egui::Ui,
    label: &str,
    id: &str,
    value: &mut ManifestList,
    dependency_type: GuiDependencyType,
    dark: bool,
) -> bool {
    let before = *value;
    let allowed = match dependency_type {
        GuiDependencyType::Mod => &[
            ManifestList::Common,
            ManifestList::Client,
            ManifestList::Server,
        ][..],
        GuiDependencyType::ResourcePack | GuiDependencyType::ShaderPack => {
            &[ManifestList::Resourcepacks][..]
        }
    };
    if !allowed.contains(value) {
        *value = allowed[0];
    }
    ui.horizontal(|ui| {
        field_label(ui, label, label_color(dark));
        egui::ComboBox::from_id_salt(id)
            .selected_text(value.label())
            .show_ui(ui, |ui| {
                for manifest in allowed {
                    ui.selectable_value(value, *manifest, manifest.label());
                }
            });
    });
    before != *value
}

fn labeled_readonly_value(ui: &mut egui::Ui, label: &str, value: &str, dark: bool) {
    ui.horizontal(|ui| {
        field_label(ui, label, label_color(dark));
        ui.label(RichText::new(value).color(label_color(dark)));
    });
}

fn labeled_loader_combo(
    ui: &mut egui::Ui,
    label: &str,
    id: &str,
    value: &mut Option<String>,
    lang: Language,
    dark: bool,
) -> bool {
    let before = value.clone();
    ui.horizontal(|ui| {
        field_label(ui, label, label_color(dark));
        loader_combo_body(ui, id, value, lang);
    });
    before != *value
}

fn combo_loader(
    ui: &mut egui::Ui,
    row_id: u64,
    value: &mut Option<String>,
    lang: Language,
) -> bool {
    let before = value.clone();
    loader_combo_body(ui, ("loader", row_id), value, lang);
    before != *value
}

fn loader_combo_body(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    value: &mut Option<String>,
    lang: Language,
) {
    const LOADERS: &[Option<&str>] = &[
        None,
        Some("forge"),
        Some("fabric"),
        Some("neoforge"),
        Some("cleanroom"),
        Some("quilt"),
    ];
    egui::ComboBox::from_id_salt(id)
        .selected_text(value.as_deref().unwrap_or(lang.mod_list_loader_any()))
        .show_ui(ui, |ui| {
            ui.selectable_value(value, None, lang.mod_list_loader_any());
            for loader in LOADERS.iter().flatten() {
                ui.selectable_value(value, Some((*loader).to_string()), *loader);
            }
        });
}

/// Shared inputs for one pending-sync tree render pass.
struct SyncTreeRender<'a> {
    manifest: &'a PendingSyncManifest,
    lang: Language,
    dark: bool,
    /// Keeps every folder open so filtered matches stay visible without extra clicks.
    expand_all: bool,
    /// Tree edits collected while rendering, applied by the caller after the pass.
    actions: &'a mut Vec<SyncTreeAction>,
    /// Entry whose deletion is awaiting confirmation.
    pending_delete: &'a mut Option<String>,
}

fn render_sync_tree(ui: &mut egui::Ui, tree: &SyncTree, view: &mut SyncTreeRender<'_>) {
    for node in &tree.nodes {
        render_sync_tree_node(ui, node, view);
    }
}

fn render_sync_tree_node(ui: &mut egui::Ui, node: &SyncTreeNode, view: &mut SyncTreeRender<'_>) {
    let manifest = view.manifest;
    let lang = view.lang;
    let dark = view.dark;
    let expand_all = view.expand_all;
    match node {
        SyncTreeNode::Folder {
            name,
            path,
            children,
        } => {
            let entry_indices = node.descendant_entry_indices();
            let selection = folder_non_deleted_selection(manifest, &entry_indices);
            egui::CollapsingHeader::new(RichText::new(name).strong())
                .id_salt(("sync_folder", path))
                .default_open(false)
                .open(expand_all.then_some(true))
                .show(ui, |ui| {
                    if let Ok((selected, total)) = selection {
                        ui.horizontal_wrapped(|ui| {
                            if total > 0 {
                                let all_selected = selected == total;
                                if ui
                                    .selectable_label(
                                        all_selected,
                                        lang.sync_folder_select(selected, total),
                                    )
                                    .clicked()
                                {
                                    view.actions.push(SyncTreeAction::SetFolderNonDeletedApply {
                                        entry_indices: entry_indices.clone(),
                                        apply: !all_selected,
                                    });
                                }
                            }
                            if ui.button(lang.sync_folder_ignore()).clicked() {
                                view.actions.push(SyncTreeAction::IgnoreFolder {
                                    entry_indices: entry_indices.clone(),
                                });
                            }
                        });
                    }
                    for child in children {
                        render_sync_tree_node(ui, child, view);
                    }
                });
        }
        SyncTreeNode::File {
            name,
            path,
            entry_index,
        } => {
            let Some(entry) = manifest.entries.get(*entry_index) else {
                ui.label(lang.sync_tree_error(&format!("missing entry index {entry_index}")));
                return;
            };
            ui.push_id(("sync_file", path), |ui| {
                egui::Frame::NONE
                    .fill(if dark {
                        Color32::from_gray(34)
                    } else {
                        Color32::from_gray(248)
                    })
                    .inner_margin(egui::Margin::symmetric(6, 1))
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            ui.add_sized(
                                [220.0, 20.0],
                                egui::Label::new(
                                    RichText::new(name).color(label_color(dark)).strong(),
                                )
                                .truncate(),
                            )
                            .on_hover_text(path);
                            ui.add_sized(
                                [76.0, 20.0],
                                egui::Label::new(
                                    RichText::new(sync_change_label(lang, entry.change))
                                        .size(12.5)
                                        .strong(),
                                ),
                            );
                            ui.add_sized(
                                [145.0, 20.0],
                                egui::Label::new(
                                    RichText::new(sync_source_label(lang, entry.source))
                                        .size(12.0)
                                        .color(text_muted(dark)),
                                )
                                .truncate(),
                            );

                            if entry.change == PendingSyncChange::Deleted {
                                // Deleting writes through to the project layer, so the deletion
                                // only becomes pending after the user confirms it here.
                                if view.pending_delete.as_deref() == Some(entry.path.as_str()) {
                                    if ui
                                        .button(
                                            RichText::new(lang.sync_action_delete_confirm())
                                                .color(DANGER),
                                        )
                                        .clicked()
                                    {
                                        view.actions.push(SyncTreeAction::SetEntryApply {
                                            entry_index: *entry_index,
                                            apply: true,
                                        });
                                        *view.pending_delete = None;
                                    }
                                    if ui.button(lang.sync_action_delete_cancel()).clicked() {
                                        view.actions.push(SyncTreeAction::SetEntryApply {
                                            entry_index: *entry_index,
                                            apply: false,
                                        });
                                        *view.pending_delete = None;
                                    }
                                } else if entry.apply {
                                    // Already confirmed: the row shows the queued deletion and
                                    // offers a way back out of it.
                                    if ui
                                        .button(
                                            RichText::new(lang.sync_action_delete_confirmed())
                                                .color(DANGER),
                                        )
                                        .on_hover_text(lang.sync_action_delete_confirmed_hint())
                                        .clicked()
                                    {
                                        view.actions.push(SyncTreeAction::SetEntryApply {
                                            entry_index: *entry_index,
                                            apply: false,
                                        });
                                    }
                                } else {
                                    if ui.button(lang.sync_action_delete()).clicked() {
                                        *view.pending_delete = Some(entry.path.clone());
                                    }
                                    if ui.button(lang.sync_action_ignore()).clicked() {
                                        view.actions.push(SyncTreeAction::SetEntryApply {
                                            entry_index: *entry_index,
                                            apply: false,
                                        });
                                    }
                                }
                            } else {
                                let apply_label = lang.sync_action_apply();
                                if ui.selectable_label(entry.apply, apply_label).clicked() {
                                    view.actions.push(SyncTreeAction::SetEntryApply {
                                        entry_index: *entry_index,
                                        apply: true,
                                    });
                                }
                                if ui
                                    .selectable_label(!entry.apply, lang.sync_action_ignore())
                                    .clicked()
                                {
                                    view.actions.push(SyncTreeAction::SetEntryApply {
                                        entry_index: *entry_index,
                                        apply: false,
                                    });
                                }
                            }
                            if let Some(target) =
                                sync_target_combo(ui, *entry_index, entry.target, lang)
                            {
                                view.actions.push(SyncTreeAction::SetEntryTarget {
                                    entry_index: *entry_index,
                                    target,
                                });
                            }
                        });
                    });
                ui.add_space(1.0);
            });
        }
    }
}

/// What a per-layer button should do with one overrides file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverrideOpenMode {
    /// Hand the file to the platform handler for its type.
    Edit,
    /// Reveal the file in the platform file manager.
    Reveal,
}

/// One pending open request produced while rendering an overrides row.
#[derive(Debug, Clone)]
struct OverrideOpenRequest {
    path: PathBuf,
    mode: OverrideOpenMode,
}

/// Rebuilds the cached folder tree when the filter inputs or the catalog changed.
///
/// The page draws its tree on every frame, so it is built once per change instead of once per
/// frame; walking thousands of entries per frame is what made the page freeze.
fn refresh_override_tree(state: &mut OverridesPageState) {
    let key = OverrideTreeKey {
        filter: state.filter.clone(),
        scope: state.scope_filter,
        revision: state.revision,
    };
    if state.tree_key.as_ref() == Some(&key) {
        return;
    }
    let tree = match state.catalog.as_ref() {
        Some(catalog) => override_tree(catalog, &state.filter, state.scope_filter),
        None => None,
    };
    state.tree = tree;
    state.tree_key = Some(key);
}

/// One folder or file row of the Overrides tree.
#[derive(Debug)]
enum OverrideTreeNode {
    Folder {
        name: String,
        path: String,
        children: Vec<OverrideTreeNode>,
    },
    File {
        name: String,
        entry_index: usize,
    },
}

/// Builds the filtered, sorted folder tree for the Overrides page.
///
/// Entries are grouped through a directory index instead of being searched for in the node list:
/// a single folder holding thousands of files turns that search into a quadratic scan, which is
/// what made the page unusable on a large pack.
fn override_tree(
    catalog: &OverrideCatalog,
    filter: &str,
    scope_filter: OverrideScopeFilter,
) -> Option<Vec<OverrideTreeNode>> {
    let needle = filter.trim().to_ascii_lowercase();
    // Directory path -> its child directory names, and -> the files directly inside it.
    let mut folders: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut files: BTreeMap<String, Vec<(String, usize)>> = BTreeMap::new();
    let mut any = false;
    for (entry_index, entry) in catalog.entries.iter().enumerate() {
        if !scope_filter.matches(entry.scope) {
            continue;
        }
        if !needle.is_empty() && !entry.path.to_ascii_lowercase().contains(&needle) {
            continue;
        }
        any = true;
        let (parent, name) = match entry.path.rsplit_once('/') {
            Some((parent, name)) => (parent, name),
            None => ("", entry.path.as_str()),
        };
        let mut directory = String::new();
        for segment in parent.split('/').filter(|segment| !segment.is_empty()) {
            folders
                .entry(directory.clone())
                .or_default()
                .insert(segment.to_string());
            if !directory.is_empty() {
                directory.push('/');
            }
            directory.push_str(segment);
        }
        files
            .entry(directory)
            .or_default()
            .push((name.to_string(), entry_index));
    }
    if !any {
        return None;
    }
    let mut roots = Vec::new();
    build_override_nodes("", &folders, &files, &mut roots);
    sort_override_nodes(&mut roots);
    Some(roots)
}

/// Turns one directory of the index into nodes, recursing into its child folders.
fn build_override_nodes(
    directory: &str,
    folders: &BTreeMap<String, BTreeSet<String>>,
    files: &BTreeMap<String, Vec<(String, usize)>>,
    nodes: &mut Vec<OverrideTreeNode>,
) {
    if let Some(children) = folders.get(directory) {
        for name in children {
            let path = join_override_path(directory, name);
            let mut nested = Vec::new();
            build_override_nodes(&path, folders, files, &mut nested);
            nodes.push(OverrideTreeNode::Folder {
                name: name.clone(),
                path,
                children: nested,
            });
        }
    }
    if let Some(entries) = files.get(directory) {
        for (name, entry_index) in entries {
            nodes.push(OverrideTreeNode::File {
                name: name.clone(),
                entry_index: *entry_index,
            });
        }
    }
}

/// Sorts folders before files, each by name, keeping the entry order for equal names.
fn sort_override_nodes(nodes: &mut [OverrideTreeNode]) {
    nodes.sort_by(|left, right| match (left, right) {
        (OverrideTreeNode::Folder { name: left, .. }, OverrideTreeNode::Folder { name: right, .. })
        | (OverrideTreeNode::File { name: left, .. }, OverrideTreeNode::File { name: right, .. }) => {
            left.to_ascii_lowercase().cmp(&right.to_ascii_lowercase())
        }
        (OverrideTreeNode::Folder { .. }, OverrideTreeNode::File { .. }) => {
            std::cmp::Ordering::Less
        }
        (OverrideTreeNode::File { .. }, OverrideTreeNode::Folder { .. }) => {
            std::cmp::Ordering::Greater
        }
    });
    for node in nodes.iter_mut() {
        if let OverrideTreeNode::Folder { children, .. } = node {
            sort_override_nodes(children);
        }
    }
}

fn join_override_path(directory: &str, name: &str) -> String {
    if directory.is_empty() {
        name.to_string()
    } else {
        format!("{directory}/{name}")
    }
}

/// Shared inputs for one Overrides tree render pass.
struct OverrideTreeRender<'a> {
    catalog: &'a OverrideCatalog,
    project_dir: &'a Path,
    lang: Language,
    dark: bool,
    /// Retarget request produced by clicking a scope choice.
    retarget: &'a mut Option<(usize, OverrideScope)>,
    /// Open request produced by the per-layer buttons.
    open_request: &'a mut Option<OverrideOpenRequest>,
    /// Entry whose delete button was clicked once and still needs confirmation.
    pending_delete: &'a mut Option<String>,
    /// Delete request produced by confirming that entry.
    delete_request: &'a mut Option<usize>,
}

/// Renders the Overrides tree, collecting retarget and open requests for the caller to apply.
fn render_override_tree(
    ui: &mut egui::Ui,
    nodes: &[OverrideTreeNode],
    depth: usize,
    view: &mut OverrideTreeRender<'_>,
) {
    let catalog = view.catalog;
    let project_dir = view.project_dir;
    let lang = view.lang;
    let dark = view.dark;
    for node in nodes {
        match node {
            OverrideTreeNode::Folder {
                name,
                path,
                children,
            } => {
                egui::CollapsingHeader::new(RichText::new(format!("{name}/")).size(12.5).strong())
                    .id_salt(("override_folder", path))
                    .default_open(depth == 0)
                    .show(ui, |ui| {
                        render_override_tree(ui, children, depth + 1, view);
                    });
            }
            OverrideTreeNode::File { name, entry_index } => {
                let Some(entry) = catalog.entries.get(*entry_index) else {
                    continue;
                };
                ui.push_id(("override_file", &entry.path), |ui| {
                    egui::Frame::NONE
                        .fill(if dark {
                            Color32::from_gray(34)
                        } else {
                            Color32::from_gray(248)
                        })
                        .inner_margin(egui::Margin::symmetric(6, 1))
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                ui.add_sized(
                                    [240.0, 20.0],
                                    egui::Label::new(
                                        RichText::new(name).color(label_color(dark)).strong(),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(&entry.path);
                                ui.add_sized(
                                    [130.0, 20.0],
                                    egui::Label::new(
                                        RichText::new(override_scope_label(lang, entry.scope))
                                            .size(12.0)
                                            .color(text_muted(dark)),
                                    ),
                                );
                                if let Some(current) = entry.scope.choice() {
                                    for scope in OVERRIDE_SCOPE_CHOICES {
                                        if ui
                                            .selectable_label(
                                                current == *scope,
                                                override_scope_label(lang, *scope),
                                            )
                                            .clicked()
                                        {
                                            *view.retarget = Some((*entry_index, *scope));
                                        }
                                    }
                                }
                                for layer in entry.present_layers() {
                                    let file = override_layer_file(project_dir, layer, &entry.path);
                                    if ui
                                        .button(format!(
                                            "{} {}",
                                            lang.overrides_open(),
                                            layer.dir_name()
                                        ))
                                        .clicked()
                                    {
                                        *view.open_request = Some(OverrideOpenRequest {
                                            path: file.clone(),
                                            mode: OverrideOpenMode::Edit,
                                        });
                                    }
                                    if ui
                                        .button(RichText::new("...").size(11.0))
                                        .on_hover_text(lang.overrides_reveal())
                                        .clicked()
                                    {
                                        *view.open_request = Some(OverrideOpenRequest {
                                            path: file,
                                            mode: OverrideOpenMode::Reveal,
                                        });
                                    }
                                }
                                if view.pending_delete.as_deref() == Some(entry.path.as_str()) {
                                    if ui
                                        .button(
                                            RichText::new(lang.overrides_delete_confirm())
                                                .size(11.5)
                                                .color(DANGER),
                                        )
                                        .clicked()
                                    {
                                        *view.delete_request = Some(*entry_index);
                                    }
                                    if ui
                                        .button(
                                            RichText::new(lang.overrides_delete_cancel())
                                                .size(11.5),
                                        )
                                        .clicked()
                                    {
                                        *view.pending_delete = None;
                                    }
                                } else if ui
                                    .button(RichText::new(lang.overrides_delete()).size(11.5))
                                    .clicked()
                                {
                                    *view.pending_delete = Some(entry.path.clone());
                                }
                            });
                        });
                    ui.add_space(1.0);
                });
            }
        }
    }
}

/// Scope filters offered above the Overrides tree, in display order.
const OVERRIDE_SCOPE_FILTERS: &[OverrideScopeFilter] = &[
    OverrideScopeFilter::All,
    OverrideScopeFilter::Common,
    OverrideScopeFilter::ClientAndServer,
    OverrideScopeFilter::Client,
    OverrideScopeFilter::Server,
    OverrideScopeFilter::Conflicting,
];

fn override_scope_filter_label(lang: Language, filter: OverrideScopeFilter) -> &'static str {
    match filter {
        OverrideScopeFilter::All => lang.overrides_filter_all(),
        OverrideScopeFilter::Common => lang.overrides_scope_common(),
        OverrideScopeFilter::ClientAndServer => lang.overrides_scope_both_split(),
        OverrideScopeFilter::Client => lang.overrides_scope_client(),
        OverrideScopeFilter::Server => lang.overrides_scope_server(),
        OverrideScopeFilter::Conflicting => lang.overrides_filter_conflicting(),
    }
}

fn override_scope_label(lang: Language, scope: OverrideScope) -> &'static str {
    match scope {
        OverrideScope::Common | OverrideScope::CommonWithSide(_) => lang.overrides_scope_common(),
        OverrideScope::ClientAndServer => lang.overrides_scope_both_split(),
        OverrideScope::Client => lang.overrides_scope_client(),
        OverrideScope::Server => lang.overrides_scope_server(),
        OverrideScope::Conflicting => lang.overrides_filter_conflicting(),
    }
}

/// Project-relative path of one layer file, used by the per-layer open buttons.
fn override_layer_file(project_dir: &Path, layer: OverrideLayer, path: &str) -> PathBuf {
    project_dir
        .join("overrides")
        .join(layer.dir_name())
        .join(path.split('/').collect::<PathBuf>())
}

/// Retargeting choices offered per entry: common, both sides, client only, server only.
const OVERRIDE_SCOPE_CHOICES: &[OverrideScope] = &[
    OverrideScope::Common,
    OverrideScope::ClientAndServer,
    OverrideScope::Client,
    OverrideScope::Server,
];

fn sync_change_label(lang: Language, change: PendingSyncChange) -> &'static str {
    match change {
        PendingSyncChange::Modified => lang.sync_change_modified(),
        PendingSyncChange::Added => lang.sync_change_added(),
        PendingSyncChange::Deleted => lang.sync_change_deleted(),
    }
}

fn sync_source_label(lang: Language, source: Option<RunSourceKind>) -> &'static str {
    match source {
        None => lang.sync_source_new(),
        Some(RunSourceKind::OverridesCommon) => lang.sync_source_common(),
        Some(RunSourceKind::OverridesClient) => lang.sync_source_client(),
        Some(RunSourceKind::OverridesServer) => lang.sync_source_server(),
        Some(
            RunSourceKind::DebugCommon | RunSourceKind::DebugClient | RunSourceKind::DebugServer,
        ) => lang.sync_source_debug(),
    }
}

struct SettingsVersionView<'a> {
    loader_versions: &'a [String],
    forge_compat_versions: &'a [String],
    state: &'a LoaderVersionCandidateState,
}

#[derive(Debug, Default)]
struct SettingsFormUiResult {
    form_changed: bool,
    request_loader_versions: bool,
}

#[derive(Debug, Default)]
struct SettingsVersionControlResult {
    value_changed: bool,
    request_candidates: bool,
}

fn settings_form_ui(
    ui: &mut egui::Ui,
    form: &mut SettingsForm,
    lang: Language,
    dark: bool,
    versions: SettingsVersionView<'_>,
) -> SettingsFormUiResult {
    let mut result = SettingsFormUiResult::default();
    settings_section(ui, lang.settings_section_pack(), dark);
    result.form_changed |= settings_language_combo(ui, lang, &mut form.language, dark);
    result.form_changed |= settings_text_row(
        ui,
        lang.settings_label_pack_name(),
        &mut form.pack_name,
        dark,
    );
    result.form_changed |= settings_text_row(
        ui,
        lang.settings_label_pack_version(),
        &mut form.pack_version,
        dark,
    );
    result.form_changed |=
        settings_text_row(ui, lang.settings_label_authors(), &mut form.authors, dark);
    result.form_changed |= settings_text_row(
        ui,
        lang.settings_label_minecraft_version(),
        &mut form.minecraft_version,
        dark,
    );
    result.form_changed |= settings_loader_combo(ui, lang, &mut form.loader);
    let loader_version = settings_version_combo(
        ui,
        lang.settings_label_loader_version(),
        &mut form.loader_version,
        versions.loader_versions,
        versions.state,
        lang,
    );
    result.form_changed |= loader_version.value_changed;
    result.request_loader_versions |= loader_version.request_candidates;
    if form.loader == LoaderKind::Cleanroom {
        let forge_compat_version = settings_version_combo(
            ui,
            lang.settings_label_forge_compat_version(),
            &mut form.forge_compat_version,
            versions.forge_compat_versions,
            versions.state,
            lang,
        );
        result.form_changed |= forge_compat_version.value_changed;
        result.request_loader_versions |= forge_compat_version.request_candidates;
    } else if !form.forge_compat_version.is_empty() {
        form.forge_compat_version.clear();
        result.form_changed = true;
    }
    if let Some(error) = versions.state.error() {
        ui.horizontal(|ui| {
            ui.add_space(LABEL_COL_WIDTH + 10.0);
            ui.label(
                RichText::new(format!("{error} {}", lang.settings_loader_versions_retry()))
                    .color(Color32::from_rgb(205, 85, 85)),
            );
        });
    }

    ui.add_space(12.0);
    settings_section(ui, lang.settings_section_network(), dark);
    result.form_changed |=
        settings_text_row(ui, lang.settings_label_proxy_ip(), &mut form.proxy_ip, dark);
    result.form_changed |= settings_text_row(
        ui,
        lang.settings_label_proxy_port(),
        &mut form.proxy_port,
        dark,
    );

    ui.add_space(12.0);
    settings_section(ui, lang.settings_section_build(), dark);
    result.form_changed |= settings_text_row(
        ui,
        lang.settings_label_output_dir(),
        &mut form.output_dir,
        dark,
    );

    ui.add_space(12.0);
    settings_section(ui, lang.settings_section_java_client(), dark);
    result.form_changed |= settings_u16_row(
        ui,
        lang.settings_label_java_version_major(),
        &mut form.java_version_major,
        1..=255,
        dark,
    );
    result.form_changed |= settings_text_row(
        ui,
        lang.settings_label_min_memory(),
        &mut form.client_min_memory,
        dark,
    );
    result.form_changed |= settings_text_row(
        ui,
        lang.settings_label_max_memory(),
        &mut form.client_max_memory,
        dark,
    );
    result.form_changed |= settings_multiline_row(
        ui,
        lang.settings_label_extra_jvm_args(),
        &mut form.client_extra_args,
        dark,
    );

    ui.add_space(12.0);
    settings_section(ui, lang.settings_section_java_server(), dark);
    result.form_changed |= settings_text_row(
        ui,
        lang.settings_label_min_memory(),
        &mut form.server_min_memory,
        dark,
    );
    result.form_changed |= settings_text_row(
        ui,
        lang.settings_label_max_memory(),
        &mut form.server_max_memory,
        dark,
    );
    result.form_changed |= settings_multiline_row(
        ui,
        lang.settings_label_extra_jvm_args(),
        &mut form.server_extra_args,
        dark,
    );

    ui.add_space(12.0);
    if form.loader == LoaderKind::Cleanroom {
        form.cleanroom_enabled = true;
        settings_section(ui, lang.settings_section_cleanroom(), dark);
        result.form_changed |= settings_checkbox_row(
            ui,
            lang.settings_label_use_scalar(),
            &mut form.cleanroom_use_scalar,
            dark,
        );
        result.form_changed |= settings_text_row(
            ui,
            lang.settings_label_scalar_version(),
            &mut form.cleanroom_scalar_version,
            dark,
        );
        result.form_changed |= settings_relauncher_combo(ui, lang, &mut form.cleanroom_relauncher);
    } else {
        form.cleanroom_enabled = false;
    }
    result
}

fn settings_version_combo(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut String,
    candidates: &[String],
    state: &LoaderVersionCandidateState,
    lang: Language,
) -> SettingsVersionControlResult {
    let before = value.clone();
    let mut request_candidates = false;
    ui.horizontal(|ui| {
        ui.set_height(36.0);
        field_label(ui, label, label_color(ui.visuals().dark_mode));
        if state.is_loading() {
            ui.spinner();
            ui.label(lang.settings_loader_versions_loading());
            return;
        }
        let response = egui::ComboBox::from_id_salt(("settings_version", label))
            .selected_text(if value.is_empty() {
                lang.settings_loader_version_select()
            } else {
                value.as_str()
            })
            .width(non_negative_width(ui.available_width()))
            .show_ui(ui, |ui| {
                for candidate in candidates {
                    ui.selectable_value(value, candidate.clone(), candidate);
                }
            });
        if should_request_loader_versions(state, response.response.clicked()) {
            request_candidates = true;
        }
    });
    SettingsVersionControlResult {
        value_changed: before != *value,
        request_candidates,
    }
}

fn should_request_loader_versions(
    state: &LoaderVersionCandidateState,
    selector_clicked: bool,
) -> bool {
    selector_clicked && state.can_request()
}

fn settings_section(ui: &mut egui::Ui, title: &str, dark: bool) {
    ui.label(
        RichText::new(title)
            .strong()
            .size(15.0)
            .color(label_color(dark)),
    );
    ui.add_space(4.0);
}

fn settings_text_row(ui: &mut egui::Ui, label: &str, value: &mut String, dark: bool) -> bool {
    ui.horizontal(|ui| {
        ui.set_height(38.0);
        field_label(ui, label, label_color(dark));
        ui.add_sized(
            [non_negative_width(ui.available_width()), 28.0],
            TextEdit::singleline(value),
        )
        .changed()
    })
    .inner
}

fn settings_language_combo(
    ui: &mut egui::Ui,
    lang: Language,
    value: &mut String,
    dark: bool,
) -> bool {
    let before = value.clone();
    ui.horizontal(|ui| {
        ui.set_height(36.0);
        field_label(ui, lang.settings_label_language(), label_color(dark));
        egui::ComboBox::from_id_salt("settings_language")
            .selected_text(if value.trim().is_empty() {
                lang.lang_option_system()
            } else if value.eq_ignore_ascii_case("en-US") {
                lang.lang_option_english()
            } else {
                lang.lang_option_chinese()
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(value, String::new(), lang.lang_option_system());
                ui.selectable_value(value, "zh-CN".to_string(), lang.lang_option_chinese());
                ui.selectable_value(value, "en-US".to_string(), lang.lang_option_english());
            });
    });
    before != *value
}

fn settings_multiline_row(ui: &mut egui::Ui, label: &str, value: &mut String, dark: bool) -> bool {
    ui.horizontal(|ui| {
        field_label(ui, label, label_color(dark));
        ui.add_sized(
            [non_negative_width(ui.available_width()), 64.0],
            TextEdit::multiline(value).desired_rows(3),
        )
        .changed()
    })
    .inner
}

fn settings_checkbox_row(ui: &mut egui::Ui, label: &str, value: &mut bool, dark: bool) -> bool {
    ui.horizontal(|ui| {
        ui.set_height(34.0);
        field_label(ui, label, label_color(dark));
        ui.checkbox(value, "").changed()
    })
    .inner
}

fn settings_u16_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut u16,
    range: std::ops::RangeInclusive<u16>,
    dark: bool,
) -> bool {
    ui.horizontal(|ui| {
        ui.set_height(38.0);
        field_label(ui, label, label_color(dark));
        ui.add(egui::DragValue::new(value).range(range)).changed()
    })
    .inner
}

fn settings_loader_combo(ui: &mut egui::Ui, lang: Language, value: &mut LoaderKind) -> bool {
    let before = value.clone();
    ui.horizontal(|ui| {
        ui.set_height(36.0);
        field_label(
            ui,
            lang.settings_label_loader(),
            label_color(ui.visuals().dark_mode),
        );
        egui::ComboBox::from_id_salt("settings_loader")
            .selected_text(loader_label(value))
            .show_ui(ui, |ui| {
                ui.selectable_value(value, LoaderKind::Forge, "forge");
                ui.selectable_value(value, LoaderKind::Fabric, "fabric");
                ui.selectable_value(value, LoaderKind::NeoForge, "neoforge");
                ui.selectable_value(value, LoaderKind::Cleanroom, "cleanroom");
            });
    });
    before != *value
}

fn run_download_source_combo(
    ui: &mut egui::Ui,
    lang: Language,
    value: &mut DownloadSource,
    dark: bool,
) -> bool {
    let before = *value;
    ui.horizontal(|ui| {
        ui.set_height(36.0);
        field_label(ui, lang.run_download_source(), label_color(dark));
        egui::ComboBox::from_id_salt("run_download_source")
            .selected_text(download_source_label(lang, *value))
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    value,
                    DownloadSource::Auto,
                    download_source_label(lang, DownloadSource::Auto),
                );
                ui.selectable_value(
                    value,
                    DownloadSource::Official,
                    download_source_label(lang, DownloadSource::Official),
                );
                ui.selectable_value(
                    value,
                    DownloadSource::Bmclapi,
                    download_source_label(lang, DownloadSource::Bmclapi),
                );
            });
    });
    before != *value
}

fn run_text_row(ui: &mut egui::Ui, label: &str, value: &mut String, dark: bool) -> bool {
    settings_text_row(ui, label, value, dark)
}

fn run_u64_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut u64,
    range: std::ops::RangeInclusive<u64>,
    dark: bool,
) -> bool {
    ui.horizontal(|ui| {
        ui.set_height(38.0);
        field_label(ui, label, label_color(dark));
        ui.add(egui::DragValue::new(value).range(range)).changed()
    })
    .inner
}

fn run_jvm_args_row(ui: &mut egui::Ui, label: &str, values: &mut Vec<String>, dark: bool) -> bool {
    let mut text = values.join("\n");
    let changed = settings_multiline_row(ui, label, &mut text, dark);
    if changed {
        *values = text
            .lines()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect();
    }
    changed
}

fn settings_relauncher_combo(
    ui: &mut egui::Ui,
    lang: Language,
    value: &mut CleanroomRelauncher,
) -> bool {
    let before = *value;
    ui.horizontal(|ui| {
        ui.set_height(36.0);
        field_label(
            ui,
            lang.settings_label_relauncher(),
            label_color(ui.visuals().dark_mode),
        );
        egui::ComboBox::from_id_salt("settings_cleanroom_relauncher")
            .selected_text(relauncher_label(*value))
            .show_ui(ui, |ui| {
                ui.selectable_value(value, CleanroomRelauncher::Unofficial, "unofficial");
                ui.selectable_value(value, CleanroomRelauncher::Original, "original");
                ui.selectable_value(value, CleanroomRelauncher::Improved, "improved");
            });
    });
    before != *value
}

fn loader_label(value: &LoaderKind) -> &'static str {
    match value {
        LoaderKind::Forge => "forge",
        LoaderKind::Fabric => "fabric",
        LoaderKind::NeoForge => "neoforge",
        LoaderKind::Cleanroom => "cleanroom",
    }
}

fn download_source_label(lang: Language, value: DownloadSource) -> &'static str {
    match (lang, value) {
        (Language::ZhCn, DownloadSource::Auto) => "自动选择",
        (Language::ZhCn, DownloadSource::Official) => "官方源",
        (Language::ZhCn, DownloadSource::Bmclapi) => "BMCLAPI 镜像",
        (Language::EnUs, DownloadSource::Auto) => "Automatic",
        (Language::EnUs, DownloadSource::Official) => "Official",
        (Language::EnUs, DownloadSource::Bmclapi) => "BMCLAPI Mirror",
    }
}

fn build_target_label(lang: Language, target: BuildTarget) -> &'static str {
    match target {
        BuildTarget::Both => lang.build_target_both(),
        BuildTarget::Client => lang.build_target_client(),
        BuildTarget::Server => lang.build_target_server(),
    }
}

fn server_platform_label(lang: Language, choice: ServerPlatformChoice) -> &'static str {
    match choice {
        ServerPlatformChoice::Current => lang.server_platform_current(),
        ServerPlatformChoice::Explicit(ServerPlatform::WindowsX86_64) => {
            lang.server_platform_windows_x86_64()
        }
        ServerPlatformChoice::Explicit(ServerPlatform::LinuxX86_64) => {
            lang.server_platform_linux_x86_64()
        }
        ServerPlatformChoice::Explicit(ServerPlatform::MacosAarch64) => {
            lang.server_platform_macos_aarch64()
        }
    }
}

fn relauncher_label(value: CleanroomRelauncher) -> &'static str {
    match value {
        CleanroomRelauncher::Unofficial => "unofficial",
        CleanroomRelauncher::Original => "original",
        CleanroomRelauncher::Improved => "improved",
    }
}

fn sync_target_combo(
    ui: &mut egui::Ui,
    index: usize,
    current: SyncTarget,
    lang: Language,
) -> Option<SyncTarget> {
    let mut value = current;
    egui::ComboBox::from_id_salt(("sync_target", index))
        .selected_text(sync_target_label(lang, value))
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut value, SyncTarget::Common, lang.sync_target_common());
            ui.selectable_value(&mut value, SyncTarget::Client, lang.sync_target_client());
            ui.selectable_value(&mut value, SyncTarget::Server, lang.sync_target_server());
        });
    (value != current).then_some(value)
}

fn sync_target_label(lang: Language, target: SyncTarget) -> &'static str {
    match target {
        SyncTarget::Common => lang.sync_target_common(),
        SyncTarget::Client => lang.sync_target_client(),
        SyncTarget::Server => lang.sync_target_server(),
    }
}

fn command_title(lang: Language, command: GuiCommand) -> &'static str {
    match command {
        GuiCommand::Init => lang.cmd_title_init(),
        GuiCommand::ModList => lang.cmd_title_mod_list(),
        GuiCommand::Overrides => lang.cmd_title_overrides(),
        GuiCommand::Settings => lang.cmd_title_settings(),
        GuiCommand::Sync => lang.cmd_title_sync(),
        GuiCommand::Build => lang.cmd_title_build(),
        GuiCommand::Check => lang.cmd_title_check(),
        GuiCommand::RunClient => lang.cmd_title_run_client(),
        GuiCommand::RunServer => lang.cmd_title_run_server(),
    }
}

fn command_description(lang: Language, command: GuiCommand) -> &'static str {
    match command {
        GuiCommand::Init => lang.cmd_desc_init(),
        GuiCommand::ModList => lang.cmd_desc_mod_list(),
        GuiCommand::Overrides => lang.cmd_desc_overrides(),
        GuiCommand::Settings => lang.cmd_desc_settings(),
        GuiCommand::Sync => lang.cmd_desc_sync(),
        GuiCommand::Build => lang.cmd_desc_build(),
        GuiCommand::Check => lang.cmd_desc_check(),
        GuiCommand::RunClient => lang.cmd_desc_run_client(),
        GuiCommand::RunServer => lang.cmd_desc_run_server(),
    }
}

fn localize_error(lang: Language, err: &GuiCommandError) -> String {
    match err {
        GuiCommandError::MissingProjectDir => lang.err_missing_project_dir().to_string(),
        GuiCommandError::MissingPackZip => lang.err_missing_pack_zip().to_string(),
        GuiCommandError::MissingOutputDir => lang.err_missing_output_dir().to_string(),
        GuiCommandError::ServerPlatformForClientBuild => {
            lang.err_server_platform_for_client_build().to_string()
        }
        GuiCommandError::MissingProxyIp => lang.err_missing_proxy_ip().to_string(),
        GuiCommandError::MissingProxyPort => lang.err_missing_proxy_port().to_string(),
        GuiCommandError::GuiOnlyCommand => lang.err_gui_only_command().to_string(),
        GuiCommandError::MissingPlayerName => lang.err_missing_player_name().to_string(),
        GuiCommandError::TestRunConfigNotLoaded => {
            lang.err_test_run_config_not_loaded().to_string()
        }
        GuiCommandError::TestRunConfigProjectMismatch { loaded, current } => {
            lang.err_test_run_config_project_mismatch(loaded, current)
        }
        GuiCommandError::InvalidTestRunConfig(error) => lang.err_invalid_test_run_config(error),
        GuiCommandError::InvalidRunSyncConfig(error) => lang.err_invalid_run_sync_config(error),
        GuiCommandError::SaveRunConfiguration(error) => lang.err_save_run_configuration(error),
    }
}

fn localized_dependency_type(lang: Language, dependency_type: GuiDependencyType) -> &'static str {
    match dependency_type {
        GuiDependencyType::Mod => lang.mod_list_type_mod(),
        GuiDependencyType::ResourcePack => lang.mod_list_type_resource_pack(),
        GuiDependencyType::ShaderPack => lang.mod_list_type_shader_pack(),
    }
}

fn project_lookup_for_entry(entry: &crate::gui::mod_list::GuiModEntry) -> Option<ProjectLookup> {
    entry
        .project_id
        .map(ProjectLookup::ProjectId)
        .or_else(|| entry.slug.clone().map(ProjectLookup::Slug))
}

fn load_mod_list_thread(project_dir: PathBuf) -> Result<ModListLoadResult, String> {
    let list = GuiDependencyList::load(&project_dir).map_err(|err| err.to_string())?;
    let settings =
        load_project_settings_for_mod_list(&project_dir).map_err(|err| err.to_string())?;
    Ok(ModListLoadResult { list, settings })
}

fn search_projects_thread(
    project_dir: PathBuf,
    input: ProjectSearchInput,
    networks: GuiNetworkEngineCache,
) -> Result<Vec<ProjectCandidate>, String> {
    let config = load_project_config_for_mod_list(&project_dir).map_err(|err| err.to_string())?;
    let network = networks
        .network_engine(config.proxy.as_deref())
        .map_err(|err| err.to_string())?;
    let api = CurseForgeClient::with_network_engine(config.api_key, network)
        .map_err(|err| err.to_string())?;
    search_project_candidates(&api, &input).map_err(|err| err.to_string())
}

#[allow(clippy::too_many_arguments)]
fn query_file_candidates_thread(
    project_dir: PathBuf,
    cache: std::sync::Arc<QueryFileCandidateCache>,
    generation: u64,
    revision: u64,
    row_id: u64,
    project: ProjectLookup,
    loader: Option<String>,
    networks: GuiNetworkEngineCache,
) -> ModListFileCandidateResult {
    let result = (|| {
        let config =
            load_project_config_for_mod_list(&project_dir).map_err(|err| err.to_string())?;
        let network = networks
            .network_engine(config.proxy.as_deref())
            .map_err(|err| err.to_string())?;
        let api = CurseForgeClient::with_network_engine(config.api_key, network)
            .map_err(|err| err.to_string())?;
        let query = QueryFileCandidates {
            project: project.clone(),
            minecraft_version: config.minecraft_version,
            loader: loader.clone(),
        };
        query_file_candidates_cached(&api, cache.as_ref(), &query).map_err(|err| err.to_string())
    })();
    ModListFileCandidateResult {
        generation,
        revision,
        row_id,
        project,
        loader,
        result,
    }
}

fn query_add_project_file_candidates_thread(
    project_dir: PathBuf,
    cache: std::sync::Arc<QueryFileCandidateCache>,
    generation: u64,
    project_id: u64,
    loader: Option<String>,
    networks: GuiNetworkEngineCache,
) -> AddProjectFileCandidateResult {
    let result = (|| {
        let config =
            load_project_config_for_mod_list(&project_dir).map_err(|err| err.to_string())?;
        let network = networks
            .network_engine(config.proxy.as_deref())
            .map_err(|err| err.to_string())?;
        let api = CurseForgeClient::with_network_engine(config.api_key, network)
            .map_err(|err| err.to_string())?;
        let query = QueryFileCandidates {
            project: ProjectLookup::ProjectId(project_id),
            minecraft_version: config.minecraft_version,
            loader: loader.clone(),
        };
        query_file_candidates_cached(&api, cache.as_ref(), &query).map_err(|err| err.to_string())
    })();
    AddProjectFileCandidateResult {
        generation,
        project_id,
        loader,
        result,
    }
}

fn load_sync_manifest_thread(
    project_dir: PathBuf,
    side: RunSide,
) -> Result<PendingSyncManifest, String> {
    let path = project_dir
        .join(".mcmpu")
        .join(side.pending_sync_file_name());
    if !path.exists() {
        return Ok(PendingSyncManifest {
            side,
            run_dir: project_dir.join("run").join(side.dir_name()),
            path,
            ignored_debug: 0,
            ignored_generated: 0,
            entries: Vec::new(),
        });
    }
    PendingSyncManifest::from_path(path).map_err(|err| err.to_string())
}

fn load_sync_window_manifest_thread(
    project_dir: PathBuf,
    side: RunSide,
) -> Result<Option<PendingSyncManifest>, String> {
    let manifest = load_sync_manifest_thread(project_dir, side)?;
    Ok((!manifest.entries.is_empty()).then_some(manifest))
}

fn merge_receivers(left: Receiver<GuiEvent>, right: Receiver<GuiEvent>) -> Receiver<GuiEvent> {
    let (sender, receiver) = mpsc::sync_channel(MAX_GUI_EVENT_QUEUE);
    forward_receiver(left, sender.clone());
    forward_receiver(right, sender);
    receiver
}

fn forward_receiver(receiver: Receiver<GuiEvent>, sender: mpsc::SyncSender<GuiEvent>) {
    thread::spawn(move || {
        for event in receiver {
            if sender.send(event).is_err() {
                break;
            }
        }
    });
}

fn run_command_thread(
    spec: GuiCommandSpec,
    sender: mpsc::SyncSender<GuiEvent>,
    console: Option<SharedConsoleInput>,
    stop_signal: Option<SharedStopSignal>,
) {
    let result = run_gui_command_direct(&spec, &sender, console.as_ref(), stop_signal.as_ref());
    flush_dropped_gui_logs(&sender);
    let sync_side = match spec.task {
        GuiCommandTask::RunClient => Some(RunSide::Client),
        GuiCommandTask::RunServer => Some(RunSide::Server),
        _ => None,
    };
    if let Some(side) = sync_side {
        match load_sync_window_manifest_thread(spec.current_dir.clone(), side) {
            Ok(Some(manifest)) => {
                let _ = sender.send(GuiEvent::SyncWindowLoaded(manifest));
            }
            Ok(None) => {}
            Err(error) => {
                let _ = sender.send(GuiEvent::SyncWindowCheckFailed(error));
            }
        }
    }
    match result {
        Ok(()) => {
            let _ = sender.send(GuiEvent::Finished(0));
        }
        Err(err) => {
            let _ = sender.send(GuiEvent::Failed(err));
        }
    }
}

fn run_gui_command_direct(
    spec: &GuiCommandSpec,
    sender: &mpsc::SyncSender<GuiEvent>,
    console: Option<&SharedConsoleInput>,
    stop_signal: Option<&SharedStopSignal>,
) -> Result<(), String> {
    match &spec.task {
        GuiCommandTask::Init { zip, output } => {
            let language = spec
                .lang
                .clone()
                .unwrap_or_else(detect_gui_command_language);
            let messages = Language::from_code(&language);
            let zip = resolve_against(&spec.current_dir, zip);
            let output = resolve_against(&spec.current_dir, output);
            let env_api_key = ApiKeyResolver::env_key();
            let builtin_api_key = ApiKeyResolver::builtin_key();
            match ApiKeyResolver::resolve(
                None,
                env_api_key.as_deref(),
                builtin_api_key.as_deref().unwrap_or(""),
            ) {
                Ok(api_key) => {
                    let api = CurseForgeClient::with_proxy(api_key, spec.proxy.as_deref())
                        .map_err(|err| err.to_string())?;
                    initialize_from_zip_with_api_and_progress(
                        zip,
                        output,
                        language,
                        &api,
                        &mut |event| send_log(sender, render_init_progress(messages, event)),
                    )
                    .map_err(|err| err.to_string())?;
                }
                Err(_) => {
                    initialize_from_zip_with_progress(zip, output, language, &mut |event| {
                        send_log(sender, render_init_progress(messages, event));
                    })
                    .map_err(|err| err.to_string())?;
                }
            }
            send_log(sender, messages.init_completed().to_string());
        }
        GuiCommandTask::Build { options } => {
            let language = project_language_for_gui(&spec.current_dir);
            let config = Config::from_path(spec.current_dir.join("config.toml"))
                .map_err(|err| err.to_string())?;
            let api_key = resolve_gui_api_key(&config).map_err(|err| err.to_string())?;
            let proxy =
                resolve_gui_proxy(spec.proxy.as_deref(), &config).map_err(|err| err.to_string())?;
            let options = normalize_build_options_for_project(&spec.current_dir, options);
            let outputs = build_project_with_api_key_and_options_and_progress(
                &spec.current_dir,
                api_key,
                proxy.as_deref(),
                options,
                &mut |event| send_log(sender, render_build_progress(language, event)),
            )
            .map_err(|err| err.to_string())?;
            if let Some(client_zip) = outputs.client_zip {
                send_log(
                    sender,
                    format!("{}: {}", language.build_client(), client_zip.display()),
                );
            }
            if let Some(server_zip) = outputs.server_zip {
                send_log(
                    sender,
                    format!("{}: {}", language.build_server(), server_zip.display()),
                );
            }
        }
        GuiCommandTask::Check => {
            let language = project_language_for_gui(&spec.current_dir);
            let config = Config::from_path(spec.current_dir.join("config.toml"))
                .map_err(|err| err.to_string())?;
            let api_key = resolve_gui_api_key(&config).map_err(|err| err.to_string())?;
            let proxy =
                resolve_gui_proxy(spec.proxy.as_deref(), &config).map_err(|err| err.to_string())?;
            let report = check_project_with_api_key_and_progress(
                &spec.current_dir,
                api_key,
                proxy.as_deref(),
                &mut |event| send_log(sender, render_check_progress(language, event)),
            )
            .map_err(|err| err.to_string())?;
            send_log(
                sender,
                format!(
                    "{}: {} client entries, {} server downloads",
                    language.check_passed(),
                    report.client_manifest_entries,
                    report.server_downloads
                ),
            );
        }
        GuiCommandTask::RunClient | GuiCommandTask::RunServer => {
            let side = match spec.task {
                GuiCommandTask::RunClient => RunSide::Client,
                GuiCommandTask::RunServer => RunSide::Server,
                _ => unreachable!("run task checked above"),
            };
            let language = project_language_for_gui(&spec.current_dir);
            spec.save_run_configuration()
                .map_err(|error| error.to_string())?;
            let config = Config::from_path(spec.current_dir.join("config.toml"))
                .map_err(|err| err.to_string())?;
            let api_key = resolve_gui_api_key(&config).map_err(|err| err.to_string())?;
            let proxy =
                resolve_gui_proxy(spec.proxy.as_deref(), &config).map_err(|err| err.to_string())?;
            // The page that owns this run always supplies both handles; the never-* fallbacks keep
            // the lifecycle usable if that ever stops being true.
            let never_console = NeverConsole;
            let console: &dyn ConsoleInput = match console {
                Some(console) => console,
                None => &never_console,
            };
            let never_stop = NeverStop;
            let stop_signal: &dyn StopSignal = match stop_signal {
                Some(stop_signal) => stop_signal,
                None => &never_stop,
            };
            let plan = run_project_with_api_key_and_progress_and_stop_signal(
                &spec.current_dir,
                api_key,
                proxy.as_deref(),
                side,
                stop_signal,
                console,
                &mut |event| send_log(sender, render_run_progress(language, event)),
            )
            .map_err(|err| err.to_string())?;
            let label = match side {
                RunSide::Client => language.build_client(),
                RunSide::Server => language.build_server(),
            };
            send_log(sender, format!("{}: {}", label, plan.run_dir.display()));
        }
    }
    Ok(())
}

fn send_log(sender: &mpsc::SyncSender<GuiEvent>, line: String) {
    DROPPED_GUI_LOG_EVENTS.with(|dropped| {
        let omitted = dropped.get();
        if omitted > 0 {
            let notice = GuiEvent::Log(format!(
                "[log overflow] {omitted} lines omitted to keep the GUI responsive"
            ));
            match sender.try_send(notice) {
                Ok(()) => dropped.set(0),
                Err(TrySendError::Full(_)) => {
                    dropped.set(omitted.saturating_add(1));
                    return;
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
        if let Err(TrySendError::Full(_)) = sender.try_send(GuiEvent::Log(line)) {
            dropped.set(dropped.get().saturating_add(1));
        }
    });
}

fn flush_dropped_gui_logs(sender: &mpsc::SyncSender<GuiEvent>) {
    DROPPED_GUI_LOG_EVENTS.with(|dropped| {
        let omitted = dropped.replace(0);
        if omitted > 0 {
            let _ = sender.send(GuiEvent::Log(format!(
                "[log overflow] {omitted} lines omitted to keep the GUI responsive"
            )));
        }
    });
}

/// Appends one line to a console buffer, keeping it bounded.
fn append_to_log(log: &mut String, line: &str) {
    log.push_str(line);
    log.push('\n');
    trim_gui_log(log);
}

fn trim_gui_log(log: &mut String) {
    if log.len() <= MAX_GUI_LOG_BYTES {
        return;
    }
    let retained_bytes = MAX_GUI_LOG_BYTES.saturating_sub(GUI_LOG_TRUNCATION_NOTICE.len());
    let desired_start = log.len().saturating_sub(retained_bytes);
    let start = log
        .char_indices()
        .map(|(index, _)| index)
        .find(|&index| index >= desired_start)
        .unwrap_or(log.len());
    let tail = log[start..].to_string();
    log.clear();
    log.push_str(GUI_LOG_TRUNCATION_NOTICE);
    log.push_str(&tail);
}

fn resolve_gui_api_key(
    _config: &Config,
) -> Result<String, crate::curseforge::resolver::ResolveError> {
    let env_api_key = ApiKeyResolver::env_key();
    let builtin_api_key = ApiKeyResolver::builtin_key();
    ApiKeyResolver::resolve(
        None,
        env_api_key.as_deref(),
        builtin_api_key.as_deref().unwrap_or(""),
    )
}

fn resolve_gui_proxy(
    cli_proxy: Option<&str>,
    _config: &Config,
) -> Result<Option<String>, crate::config::ConfigError> {
    if let Some(proxy) = cli_proxy.map(str::trim).filter(|proxy| !proxy.is_empty()) {
        return Ok(Some(proxy.to_string()));
    }
    Ok(None)
}

fn settings_proxy(form: &SettingsForm) -> Option<String> {
    let host = form.proxy_ip.trim();
    let port = form.proxy_port.trim();
    (!host.is_empty() && !port.is_empty()).then(|| format!("http://{host}:{port}"))
}

fn resolve_against(base: &std::path::Path, path: &std::path::Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn detect_gui_command_language() -> String {
    let raw = ["LC_ALL", "LC_MESSAGES", "LANGUAGE", "LANG"]
        .into_iter()
        .find_map(|key| std::env::var(key).ok())
        .filter(|value| !value.trim().is_empty())
        .or_else(sys_locale::get_locale)
        .unwrap_or_default();
    if raw.to_ascii_lowercase().starts_with("zh") {
        "zh-CN".to_string()
    } else {
        "en-US".to_string()
    }
}

fn project_language_for_gui(project_dir: &std::path::Path) -> Language {
    Config::from_path(project_dir.join("config.toml"))
        .map(|config| gui_language(&config.language))
        .unwrap_or_else(|_| Language::from_code(&detect_gui_command_language()))
}

fn render_init_progress(language: Language, event: &InitProgress) -> String {
    match (language, event) {
        (Language::ZhCn, InitProgress::OpeningZip { path }) => {
            format!("[init] 打开整合包: {path}")
        }
        (Language::EnUs, InitProgress::OpeningZip { path }) => {
            format!("[init] opening pack zip: {path}")
        }
        (Language::ZhCn, InitProgress::ReadingManifest) => "[init] 读取 manifest.json".to_string(),
        (Language::EnUs, InitProgress::ReadingManifest) => {
            "[init] reading manifest.json".to_string()
        }
        (
            Language::ZhCn,
            InitProgress::ManifestLoaded {
                name,
                version,
                file_count,
            },
        ) => format!("[init] 已读取: {name} {version}, CurseForge 条目 {file_count} 个"),
        (
            Language::EnUs,
            InitProgress::ManifestLoaded {
                name,
                version,
                file_count,
            },
        ) => format!("[init] loaded: {name} {version}, {file_count} CurseForge entries"),
        (
            Language::ZhCn,
            InitProgress::ResolvedFile {
                completed,
                total,
                project_id,
                file_id,
            },
        ) => format!("[init] 已解析 {completed}/{total}: {project_id}/{file_id}"),
        (
            Language::EnUs,
            InitProgress::ResolvedFile {
                completed,
                total,
                project_id,
                file_id,
            },
        ) => format!("[init] resolved {completed}/{total}: {project_id}/{file_id}"),
        (Language::ZhCn, InitProgress::WritingProjectFiles { output_dir }) => {
            format!("[init] 写入项目文件: {output_dir}")
        }
        (Language::EnUs, InitProgress::WritingProjectFiles { output_dir }) => {
            format!("[init] writing project files: {output_dir}")
        }
        (Language::ZhCn, InitProgress::ExtractingOverrides { overrides_dir }) => {
            format!("[init] 导入 overrides: {overrides_dir}")
        }
        (Language::EnUs, InitProgress::ExtractingOverrides { overrides_dir }) => {
            format!("[init] extracting overrides: {overrides_dir}")
        }
        (Language::ZhCn, InitProgress::Finished { output_dir }) => {
            format!("[init] 初始化文件已写入: {output_dir}")
        }
        (Language::EnUs, InitProgress::Finished { output_dir }) => {
            format!("[init] project initialized at: {output_dir}")
        }
    }
}

fn render_check_progress(language: Language, event: &BuildProgress) -> String {
    render_build_like_progress("check", language, event)
}

fn render_build_progress(language: Language, event: &BuildProgress) -> String {
    render_build_like_progress("build", language, event)
}

fn render_build_like_progress(prefix: &str, language: Language, event: &BuildProgress) -> String {
    match (language, event) {
        (Language::ZhCn, BuildProgress::ReadingProject { project_dir }) => {
            format!("[{prefix}] 读取项目: {project_dir}")
        }
        (Language::EnUs, BuildProgress::ReadingProject { project_dir }) => {
            format!("[{prefix}] reading project: {project_dir}")
        }
        (
            Language::ZhCn,
            BuildProgress::DependencyListsLoaded {
                common,
                client,
                server,
                resourcepacks,
            },
        ) => {
            format!(
                "[{prefix}] 已读取依赖清单: common={common}, client={client}, server={server}, resourcepacks={resourcepacks}"
            )
        }
        (
            Language::EnUs,
            BuildProgress::DependencyListsLoaded {
                common,
                client,
                server,
                resourcepacks,
            },
        ) => {
            format!(
                "[{prefix}] dependency lists loaded: common={common}, client={client}, server={server}, resourcepacks={resourcepacks}"
            )
        }
        (Language::ZhCn, BuildProgress::CleanroomSupplementsApplied { added }) => {
            format!("[{prefix}] Cleanroom 隐式依赖补充: {added} 个")
        }
        (Language::EnUs, BuildProgress::CleanroomSupplementsApplied { added }) => {
            format!("[{prefix}] Cleanroom implicit dependencies added: {added}")
        }
        (Language::ZhCn, BuildProgress::ResolvingDependencies { total }) => {
            format!("[{prefix}] 开始并发解析 CurseForge 依赖: {total} 个")
        }
        (Language::EnUs, BuildProgress::ResolvingDependencies { total }) => {
            format!("[{prefix}] resolving CurseForge dependencies concurrently: {total}")
        }
        (
            Language::ZhCn,
            BuildProgress::ResolvedDependency {
                completed,
                total,
                project_id,
                file_id,
                slug,
            },
        ) => format!(
            "[{prefix}] 已解析 {completed}/{total}: {}",
            dependency_label(*project_id, *file_id, slug.as_deref())
        ),
        (
            Language::EnUs,
            BuildProgress::ResolvedDependency {
                completed,
                total,
                project_id,
                file_id,
                slug,
            },
        ) => format!(
            "[{prefix}] resolved {completed}/{total}: {}",
            dependency_label(*project_id, *file_id, slug.as_deref())
        ),
        (Language::ZhCn, BuildProgress::PreparingServerInstallerAssets) => {
            format!("[{prefix}] 准备服务端安装器资产")
        }
        (Language::EnUs, BuildProgress::PreparingServerInstallerAssets) => {
            format!("[{prefix}] preparing server installer assets")
        }
        (Language::ZhCn, BuildProgress::ServerInstallerAssetsReady) => {
            format!("[{prefix}] 服务端安装器资产准备完成")
        }
        (Language::EnUs, BuildProgress::ServerInstallerAssetsReady) => {
            format!("[{prefix}] server installer assets ready")
        }
        (Language::ZhCn, BuildProgress::PreparingOutput { output_dir }) => {
            format!("[{prefix}] 准备输出目录: {output_dir}")
        }
        (Language::EnUs, BuildProgress::PreparingOutput { output_dir }) => {
            format!("[{prefix}] preparing output directory: {output_dir}")
        }
        (Language::ZhCn, BuildProgress::WritingClientZip { path }) => {
            format!("[{prefix}] 写入客户端包: {path}")
        }
        (Language::EnUs, BuildProgress::WritingClientZip { path }) => {
            format!("[{prefix}] writing client package: {path}")
        }
        (Language::ZhCn, BuildProgress::WritingServerZip { path }) => {
            format!("[{prefix}] 写入服务端包: {path}")
        }
        (Language::EnUs, BuildProgress::WritingServerZip { path }) => {
            format!("[{prefix}] writing server package: {path}")
        }
        (
            Language::ZhCn,
            BuildProgress::Finished {
                client_zip,
                server_zip,
            },
        ) => format!(
            "[{prefix}] 构建完成: {}",
            finished_build_artifacts(language, client_zip, server_zip)
        ),
        (
            Language::EnUs,
            BuildProgress::Finished {
                client_zip,
                server_zip,
            },
        ) => format!(
            "[{prefix}] completed: {}",
            finished_build_artifacts(language, client_zip, server_zip)
        ),
    }
}

fn finished_build_artifacts(language: Language, client_zip: &str, server_zip: &str) -> String {
    let mut artifacts = Vec::with_capacity(2);
    if !client_zip.is_empty() {
        artifacts.push(format!("{}: {client_zip}", language.build_client()));
    }
    if !server_zip.is_empty() {
        artifacts.push(format!("{}: {server_zip}", language.build_server()));
    }
    artifacts.join(", ")
}

fn render_run_progress(language: Language, event: &RunProgress) -> String {
    match (language, event) {
        (Language::ZhCn, RunProgress::ResolvingProject { side, project_dir }) => format!(
            "[run] 解析{}项目: {}",
            run_side_label(language, *side),
            project_dir.display()
        ),
        (Language::EnUs, RunProgress::ResolvingProject { side, project_dir }) => format!(
            "[run] resolving {} project: {}",
            run_side_label(language, *side),
            project_dir.display()
        ),
        (Language::ZhCn, RunProgress::RebuildingRunDirectory { side, run_dir }) => format!(
            "[run] 重建{}运行目录: {}",
            run_side_label(language, *side),
            run_dir.display()
        ),
        (Language::EnUs, RunProgress::RebuildingRunDirectory { side, run_dir }) => format!(
            "[run] rebuilding {} run directory: {}",
            run_side_label(language, *side),
            run_dir.display()
        ),
        (Language::ZhCn, RunProgress::CopyingOverrides { side }) => {
            format!(
                "[run] 复制{} overrides 覆盖层",
                run_side_label(language, *side)
            )
        }
        (Language::EnUs, RunProgress::CopyingOverrides { side }) => {
            format!(
                "[run] copying {} overrides layers",
                run_side_label(language, *side)
            )
        }
        (Language::ZhCn, RunProgress::CopyingDebug { side }) => {
            format!(
                "[run] 复制{}仅运行时 debug 覆盖层",
                run_side_label(language, *side)
            )
        }
        (Language::EnUs, RunProgress::CopyingDebug { side }) => {
            format!(
                "[run] copying {} runtime-only debug layers",
                run_side_label(language, *side)
            )
        }
        (Language::ZhCn, RunProgress::EnsuringSyncConfig { side, path }) => format!(
            "[run] 准备{}同步配置: {}",
            run_side_label(language, *side),
            path.display()
        ),
        (Language::EnUs, RunProgress::EnsuringSyncConfig { side, path }) => format!(
            "[run] ensuring {} sync config: {}",
            run_side_label(language, *side),
            path.display()
        ),
        (Language::ZhCn, RunProgress::RunManifestWritten { file_count }) => {
            format!("[run] 运行清单已写入: {file_count} 个文件")
        }
        (Language::EnUs, RunProgress::RunManifestWritten { file_count }) => {
            format!("[run] run manifest written: {file_count} files")
        }
        (Language::ZhCn, RunProgress::DownloadingFiles { total }) => {
            format!("[run] 检查/下载运行文件: {total} 个")
        }
        (Language::EnUs, RunProgress::DownloadingFiles { total }) => {
            format!("[run] checking/downloading run files: {total}")
        }
        (Language::ZhCn, RunProgress::VerifyingFiles { total }) => {
            format!("[run] 校验本地运行文件与缓存: {total} 个")
        }
        (Language::EnUs, RunProgress::VerifyingFiles { total }) => {
            format!("[run] verifying local run files and cache: {total}")
        }
        (Language::ZhCn, RunProgress::FileVerificationProgress { checked, total }) => {
            format!("[run] 本地校验 {checked}/{total}")
        }
        (Language::EnUs, RunProgress::FileVerificationProgress { checked, total }) => {
            format!("[run] local verification {checked}/{total}")
        }
        (
            Language::ZhCn,
            RunProgress::RunFilesChecked {
                reused,
                cached,
                downloading,
                download_bytes,
            },
        ) => format!(
            "[run] 本地校验完成: 复用 {reused}，缓存 {cached}，待下载 {downloading} ({})",
            format_run_bytes(*download_bytes)
        ),
        (
            Language::EnUs,
            RunProgress::RunFilesChecked {
                reused,
                cached,
                downloading,
                download_bytes,
            },
        ) => format!(
            "[run] local verification complete: {reused} reused, {cached} cached, {downloading} to download ({})",
            format_run_bytes(*download_bytes)
        ),
        (Language::ZhCn, RunProgress::DownloadBatchStarted { total, total_bytes }) => format!(
            "[run] 已提交并发下载: {total} 个文件，共 {}",
            format_run_bytes(*total_bytes)
        ),
        (Language::EnUs, RunProgress::DownloadBatchStarted { total, total_bytes }) => format!(
            "[run] concurrent download dispatched: {total} files, {} total",
            format_run_bytes(*total_bytes)
        ),
        (
            Language::ZhCn,
            RunProgress::FileDownloadStarted {
                started,
                total,
                path,
            },
        ) => format!("[run] 开始下载 {started}/{total}: {path}"),
        (
            Language::EnUs,
            RunProgress::FileDownloadStarted {
                started,
                total,
                path,
            },
        ) => format!("[run] download started {started}/{total}: {path}"),
        (
            Language::ZhCn,
            RunProgress::DownloadProgress {
                completed,
                total,
                transferred_bytes,
                total_bytes,
                bytes_per_second,
                active_requests,
            },
        ) => format!(
            "[run] 下载进度 {completed}/{total}: {}/{}，{}/s，活动请求 {active_requests}",
            format_run_bytes(*transferred_bytes),
            format_run_bytes(*total_bytes),
            format_run_bytes(*bytes_per_second)
        ),
        (
            Language::EnUs,
            RunProgress::DownloadProgress {
                completed,
                total,
                transferred_bytes,
                total_bytes,
                bytes_per_second,
                active_requests,
            },
        ) => format!(
            "[run] download progress {completed}/{total}: {}/{}, {}/s, {active_requests} active requests",
            format_run_bytes(*transferred_bytes),
            format_run_bytes(*total_bytes),
            format_run_bytes(*bytes_per_second)
        ),
        (
            Language::ZhCn,
            RunProgress::FileReady {
                completed,
                total,
                path,
                source,
            },
        ) => format!(
            "[run] 文件就绪 {completed}/{total} ({}): {path}",
            run_file_source_label(Language::ZhCn, *source)
        ),
        (
            Language::EnUs,
            RunProgress::FileReady {
                completed,
                total,
                path,
                source,
            },
        ) => format!(
            "[run] file ready {completed}/{total} ({}): {path}",
            run_file_source_label(Language::EnUs, *source)
        ),
        (Language::ZhCn, RunProgress::ResolvingJava { major }) => {
            format!("[run] 查找 Java {major}")
        }
        (Language::EnUs, RunProgress::ResolvingJava { major }) => {
            format!("[run] resolving Java {major}")
        }
        (Language::ZhCn, RunProgress::JavaResolved { path }) => {
            format!("[run] Java 已找到: {}", path.display())
        }
        (Language::EnUs, RunProgress::JavaResolved { path }) => {
            format!("[run] Java resolved: {}", path.display())
        }
        (Language::ZhCn, RunProgress::DownloadingLoaderInstaller { side, url, target }) => format!(
            "[run] 下载{} Loader 安装器: {url} -> {}",
            run_side_label(language, *side),
            target.display()
        ),
        (Language::EnUs, RunProgress::DownloadingLoaderInstaller { side, url, target }) => format!(
            "[run] downloading {} loader installer: {url} -> {}",
            run_side_label(language, *side),
            target.display()
        ),
        (Language::ZhCn, RunProgress::LoaderInstallerDownloaded { side, target }) => format!(
            "[run] {} Loader 安装器已下载并校验: {}",
            run_side_label(language, *side),
            target.display()
        ),
        (Language::EnUs, RunProgress::LoaderInstallerDownloaded { side, target }) => format!(
            "[run] {} loader installer downloaded and verified: {}",
            run_side_label(language, *side),
            target.display()
        ),
        (Language::ZhCn, RunProgress::RunningLoaderInstaller { side, loader }) => format!(
            "[run] 运行{} {} Loader 安装器",
            run_side_label(language, *side),
            loader_label(loader)
        ),
        (Language::EnUs, RunProgress::RunningLoaderInstaller { side, loader }) => format!(
            "[run] running {} {} loader installer",
            run_side_label(language, *side),
            loader_label(loader)
        ),
        (language, RunProgress::LoaderInstallerOutput { stream, line, .. }) => {
            render_runtime_install_output(language, *stream, line)
        }
        (Language::ZhCn, RunProgress::InstallingLoaderLibraries { side, total }) => format!(
            "[run] 并行下载{} Loader 库: {total} 个",
            run_side_label(language, *side)
        ),
        (Language::EnUs, RunProgress::InstallingLoaderLibraries { side, total }) => format!(
            "[run] downloading {} loader libraries in parallel: {total}",
            run_side_label(language, *side)
        ),
        (
            Language::ZhCn,
            RunProgress::LoaderLibraryReady {
                side,
                completed,
                total,
                path,
            },
        ) => format!(
            "[run] {} Loader 库 {completed}/{total}: {path}",
            run_side_label(language, *side)
        ),
        (
            Language::EnUs,
            RunProgress::LoaderLibraryReady {
                side,
                completed,
                total,
                path,
            },
        ) => format!(
            "[run] {} loader library {completed}/{total}: {path}",
            run_side_label(language, *side)
        ),
        (Language::ZhCn, RunProgress::PreparingRuntimeArtifacts { side, total }) => format!(
            "[run] 准备{}游戏库与资源: {total} 个",
            run_side_label(language, *side)
        ),
        (Language::EnUs, RunProgress::PreparingRuntimeArtifacts { side, total }) => format!(
            "[run] preparing {} libraries and assets: {total}",
            run_side_label(language, *side)
        ),
        (Language::ZhCn, RunProgress::RuntimeArtifactTotalUpdated { side, total }) => format!(
            "[run] {}游戏库与资源总数: {total}",
            run_side_label(language, *side)
        ),
        (Language::EnUs, RunProgress::RuntimeArtifactTotalUpdated { side, total }) => format!(
            "[run] {} library and asset total: {total}",
            run_side_label(language, *side)
        ),
        (
            Language::ZhCn,
            RunProgress::RuntimeArtifactReady {
                completed,
                total,
                path,
                source,
                ..
            },
        ) => format!(
            "[run] 游戏文件 {completed}/{total} ({}): {path}",
            artifact_install_source_label(language, *source)
        ),
        (
            Language::EnUs,
            RunProgress::RuntimeArtifactReady {
                completed,
                total,
                path,
                source,
                ..
            },
        ) => format!(
            "[run] game file {completed}/{total} ({}): {path}",
            artifact_install_source_label(language, *source)
        ),
        (
            Language::ZhCn,
            RunProgress::RuntimeArtifactDownloadProgress {
                transferred_bytes,
                total_bytes,
                active_requests,
                ..
            },
        ) => format!(
            "[run] 下载游戏文件: {}/{}，活动请求 {active_requests}",
            format_run_bytes(*transferred_bytes),
            format_run_bytes(*total_bytes)
        ),
        (
            Language::EnUs,
            RunProgress::RuntimeArtifactDownloadProgress {
                transferred_bytes,
                total_bytes,
                active_requests,
                ..
            },
        ) => format!(
            "[run] downloading game files: {}/{} with {active_requests} active request(s)",
            format_run_bytes(*transferred_bytes),
            format_run_bytes(*total_bytes)
        ),
        (Language::ZhCn, RunProgress::Launching { command }) => {
            format!("[run] 启动进程: {command}")
        }
        (Language::EnUs, RunProgress::Launching { command }) => {
            format!("[run] launching process: {command}")
        }
        (_, RunProgress::MinecraftOutput { stream, line, .. }) => {
            format!("[minecraft {}] {line}", process_stream_label(*stream))
        }
        (Language::ZhCn, RunProgress::ServerRunConfigurationWritten { changed }) => {
            format!("[run] 已准备服务端运行配置: {}", changed.join(", "))
        }
        (Language::EnUs, RunProgress::ServerRunConfigurationWritten { changed }) => format!(
            "[run] prepared server run configuration: {}",
            changed.join(", ")
        ),
        (Language::ZhCn, RunProgress::PendingSyncWritten { path, entries }) => format!(
            "[run] 已写入待处理同步清单: {} ({} 个变更)。请在弹出的同步处理窗口确认。",
            path.display(),
            entries
        ),
        (Language::EnUs, RunProgress::PendingSyncWritten { path, entries }) => format!(
            "[run] pending sync manifest written: {} ({} changes). Review it in the Sync window.",
            path.display(),
            entries
        ),
        (Language::ZhCn, RunProgress::PendingSyncFailed { path, error }) => format!(
            "[run] 待处理同步扫描失败，原清单未被替换: {}: {error}",
            path.display()
        ),
        (Language::EnUs, RunProgress::PendingSyncFailed { path, error }) => format!(
            "[run] pending sync scan failed; the previous manifest was not replaced: {}: {error}",
            path.display()
        ),
    }
}

fn installer_stream_label(
    stream: crate::project::runtime_install::RuntimeInstallStream,
) -> &'static str {
    match stream {
        crate::project::runtime_install::RuntimeInstallStream::Stdout => "stdout",
        crate::project::runtime_install::RuntimeInstallStream::Stderr => "stderr",
    }
}

fn process_stream_label(stream: ProcessOutputStream) -> &'static str {
    match stream {
        ProcessOutputStream::Stdout => "stdout",
        ProcessOutputStream::Stderr => "stderr",
    }
}

fn artifact_install_source_label(
    language: Language,
    source: crate::project::artifact_install::ArtifactInstallSource,
) -> &'static str {
    match (language, source) {
        (Language::ZhCn, crate::project::artifact_install::ArtifactInstallSource::TargetHit) => {
            language.artifact_source_target_reused()
        }
        (Language::ZhCn, crate::project::artifact_install::ArtifactInstallSource::CacheHit) => {
            language.artifact_source_cache_hit()
        }
        (Language::ZhCn, crate::project::artifact_install::ArtifactInstallSource::Downloaded) => {
            language.artifact_source_downloaded()
        }
        (Language::EnUs, crate::project::artifact_install::ArtifactInstallSource::TargetHit) => {
            language.artifact_source_target_reused()
        }
        (Language::EnUs, crate::project::artifact_install::ArtifactInstallSource::CacheHit) => {
            language.artifact_source_cache_hit()
        }
        (Language::EnUs, crate::project::artifact_install::ArtifactInstallSource::Downloaded) => {
            language.artifact_source_downloaded()
        }
    }
}

fn render_runtime_install_output(
    language: Language,
    stream: crate::project::runtime_install::RuntimeInstallStream,
    line: &str,
) -> String {
    if let Some(cache_entry) = line.strip_prefix("[cache] reused ")
        && let Some((description, path)) = cache_entry.split_once(": ")
    {
        return format!(
            "[run] {}",
            language.runtime_install_cache_reused(description, path)
        );
    }
    format!("[installer {}] {line}", installer_stream_label(stream))
}

fn dependency_label(project_id: Option<u64>, file_id: Option<u64>, slug: Option<&str>) -> String {
    match (project_id, file_id, slug) {
        (Some(project_id), Some(file_id), Some(slug)) => {
            format!("{project_id}/{file_id} ({slug})")
        }
        (Some(project_id), Some(file_id), None) => format!("{project_id}/{file_id}"),
        (_, _, Some(slug)) => slug.to_string(),
        (Some(project_id), None, _) => project_id.to_string(),
        _ => "unlocked dependency".to_string(),
    }
}

fn run_side_label(language: Language, side: RunSide) -> &'static str {
    match (language, side) {
        (Language::ZhCn, RunSide::Client) => "客户端",
        (Language::ZhCn, RunSide::Server) => "服务端",
        (Language::EnUs, RunSide::Client) => "client",
        (Language::EnUs, RunSide::Server) => "server",
    }
}

fn run_file_source_label(language: Language, source: RunFileReadySource) -> &'static str {
    match (language, source) {
        (Language::ZhCn, RunFileReadySource::Reused) => "已复用",
        (Language::ZhCn, RunFileReadySource::Cache) => "来自缓存",
        (Language::ZhCn, RunFileReadySource::Gradle) => "来自 Gradle 缓存",
        (Language::ZhCn, RunFileReadySource::Downloaded) => "已下载",
        (Language::EnUs, RunFileReadySource::Reused) => "reused",
        (Language::EnUs, RunFileReadySource::Cache) => "cache",
        (Language::EnUs, RunFileReadySource::Gradle) => "gradle cache",
        (Language::EnUs, RunFileReadySource::Downloaded) => "downloaded",
    }
}

fn format_run_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn gui_language(selection: &str) -> Language {
    if !selection.trim().is_empty() {
        return Language::from_code(selection);
    }
    let raw = ["LC_ALL", "LC_MESSAGES", "LANGUAGE", "LANG"]
        .into_iter()
        .find_map(|key| std::env::var(key).ok())
        .filter(|value| !value.trim().is_empty())
        .or_else(sys_locale::get_locale)
        .unwrap_or_else(|| "zh-CN".to_string());
    Language::from_code(&raw)
}

const LABEL_COL_WIDTH: f32 = 172.0;

fn field_label(ui: &mut egui::Ui, text: &str, color: Color32) {
    ui.allocate_ui_with_layout(
        egui::vec2(LABEL_COL_WIDTH, 20.0),
        egui::Layout::right_to_left(egui::Align::Center),
        |ui| {
            ui.add(egui::Label::new(RichText::new(text).size(13.0).color(color)).truncate());
        },
    );
    ui.add_space(10.0);
}

/// Opens one overrides file with the platform's default handler.
fn open_override_file(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        shell_open(path, None)
    }
    #[cfg(target_os = "macos")]
    {
        let mut command = std::process::Command::new("open");
        command.arg(path);
        spawn_detached(command)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(path);
        spawn_detached(command)
    }
}

/// Reveals one overrides file in the platform file manager, creating nothing.
fn reveal_override_file(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        // Explorer selects an existing file once given the item's own path, and opens a directory
        // when handed the directory itself.
        if path.is_dir() {
            shell_open(path, None)
        } else {
            shell_open(
                Path::new("explorer.exe"),
                Some(&format!("/select,\"{}\"", path.display())),
            )
        }
    }
    #[cfg(target_os = "macos")]
    {
        let mut command = std::process::Command::new("open");
        command.arg("-R").arg(path);
        spawn_detached(command)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(path.parent().unwrap_or(path));
        spawn_detached(command)
    }
}

/// Hands a path to the Windows shell through `ShellExecuteW`.
///
/// The shell API resolves the registered handler itself, so it works without a console, without
/// `cmd.exe`, and independently of any inherited standard handles.
#[cfg(target_os = "windows")]
fn shell_open(target: &Path, parameters: Option<&str>) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;

    fn wide(value: &std::ffi::OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    let operation = wide(std::ffi::OsStr::new("open"));
    let file = wide(target.as_os_str());
    let parameters = parameters.map(|value| wide(std::ffi::OsStr::new(value)));
    // SAFETY: every pointer references a NUL-terminated buffer that outlives the call, the
    // optional parameter buffer is only read when it is present, and the shell treats the window
    // handle as nullable.
    let result = unsafe {
        windows_sys::Win32::UI::Shell::ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            parameters
                .as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr()),
            std::ptr::null(),
            windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        )
    } as isize;
    // Values above 32 report success; anything at or below it is an SE_ERR_* code.
    if result > 32 {
        Ok(())
    } else {
        Err(format!("{} (shell error {result})", target.display()))
    }
}

/// Starts a detached handler process without wiring the parent's standard streams into it.
#[cfg(not(target_os = "windows"))]
fn spawn_detached(mut command: std::process::Command) -> Result<(), String> {
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn path_text_width(available_width: f32) -> f32 {
    non_negative_width(available_width - 76.0)
}

fn non_negative_width(width: f32) -> f32 {
    width.max(0.0)
}

const COMMAND_LOG_MIN_HEIGHT: f32 = 160.0;
/// Vertical space the console body frame spends on its own margins.
const LOG_BODY_VERTICAL_MARGIN: f32 = 20.0;
/// Space the console input row occupies, including the gap above it.
const CONSOLE_INPUT_ROW_HEIGHT: f32 = 34.0;
const COMMAND_FORM_LOG_GAP: f32 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq)]
struct CommandPageLayout {
    form_max_height: f32,
    gap: f32,
}

impl CommandPageLayout {
    fn new(available: f32) -> Self {
        let available = available_height(available);
        let reserved_log = available.min(COMMAND_LOG_MIN_HEIGHT);
        let remaining = available - reserved_log;
        let gap = remaining.min(COMMAND_FORM_LOG_GAP);
        Self {
            form_max_height: available_height(remaining - gap),
            gap,
        }
    }
}

fn available_height(height: f32) -> f32 {
    if height.is_finite() {
        height.max(0.0)
    } else {
        0.0
    }
}

/// Creates the shared vertical scroll policy used by every GUI surface.
pub fn vertical_scroll_area() -> egui::ScrollArea {
    egui::ScrollArea::vertical().scroll_source(egui::scroll_area::ScrollSource {
        scroll_bar: true,
        drag: egui::scroll_area::DragScroll::Never,
        mouse_wheel: true,
    })
}

/// Moves one console scroll offset by a wheel delta, never past the top of the content.
///
/// The sign matches egui's own wheel handling: a positive delta scrolls up, which shrinks the
/// offset. The upper bound is left to egui, which clamps the offset to the content length.
fn console_offset_after_wheel(offset: f32, wheel: f32) -> f32 {
    (offset - wheel).max(0.0)
}

fn toggle_row(ui: &mut egui::Ui, label: &str, value: &mut bool, label_color: Color32) {
    ui.horizontal(|ui| {
        ui.set_height(34.0);
        field_label(ui, label, label_color);
        ui.checkbox(value, "");
    });
}

fn page_bg(dark: bool) -> Color32 {
    if dark {
        Color32::from_gray(20)
    } else {
        Color32::from_gray(242)
    }
}

fn sidebar_bg(dark: bool) -> Color32 {
    if dark {
        Color32::from_gray(26)
    } else {
        Color32::from_gray(228)
    }
}

fn panel_bg(dark: bool) -> Color32 {
    if dark {
        Color32::from_gray(34)
    } else {
        Color32::WHITE
    }
}

const LOG_BG: Color32 = Color32::from_rgb(30, 30, 30);
const LOG_HEADER_BG: Color32 = Color32::from_rgb(37, 37, 38);
const LOG_TEXT: Color32 = Color32::from_rgb(212, 212, 212);
const LOG_HEADER_TEXT: Color32 = Color32::from_rgb(150, 156, 160);

fn border_color(dark: bool) -> Color32 {
    if dark {
        Color32::from_gray(52)
    } else {
        Color32::from_rgb(218, 224, 232)
    }
}

fn label_color(dark: bool) -> Color32 {
    if dark {
        Color32::from_gray(160)
    } else {
        Color32::from_rgb(90, 105, 120)
    }
}

fn text_muted(dark: bool) -> Color32 {
    if dark {
        Color32::from_gray(150)
    } else {
        Color32::from_rgb(90, 105, 120)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use crate::config::LoaderKind;
    use crate::gui::command::GuiCommand;
    use crate::gui::loader_versions::{
        LoaderVersionCandidateState, LoaderVersionCandidates, LoaderVersionQuery,
    };
    use crate::gui::mod_list::{
        AddProjectDraft, FileCandidate, GuiDependencyList, GuiDependencyType, GuiModEntry,
        ManifestList, ProjectCandidate, ProjectLookup, ProjectSearchInput,
    };
    use crate::gui::settings::SettingsForm;
    use crate::i18n::Language;
    use crate::project::artifact_install::ArtifactInstallSource;
    use crate::project::run::{
        PendingSyncChange, PendingSyncEntry, PendingSyncManifest, RunProgress, RunSide,
        SharedConsoleInput, SharedStopSignal, SyncTarget,
    };
    use crate::project::runtime_install::RuntimeInstallStream;

    #[test]
    fn path_text_width_never_goes_negative() {
        assert_eq!(super::path_text_width(46.4), 0.0);
        assert_eq!(super::path_text_width(160.0), 84.0);
    }

    #[test]
    fn wheel_input_moves_the_console_offset_against_the_scroll_direction() {
        // Scrolling up (positive wheel) pulls the content down, and never past its top.
        assert_eq!(super::console_offset_after_wheel(500.0, 40.0), 460.0);
        assert_eq!(super::console_offset_after_wheel(20.0, 40.0), 0.0);
        // Scrolling down moves towards the end of the log.
        assert_eq!(super::console_offset_after_wheel(500.0, -40.0), 540.0);
        assert_eq!(super::console_offset_after_wheel(500.0, 0.0), 500.0);
    }

    /// Renders one tree as indented lines, so a test can check both shape and order.
    fn tree_lines(nodes: &[super::OverrideTreeNode]) -> Vec<String> {
        fn walk(nodes: &[super::OverrideTreeNode], depth: usize, lines: &mut Vec<String>) {
            for node in nodes {
                match node {
                    super::OverrideTreeNode::Folder { name, children, .. } => {
                        lines.push(format!("{}{name}/", "  ".repeat(depth)));
                        walk(children, depth + 1, lines);
                    }
                    super::OverrideTreeNode::File { name, .. } => {
                        lines.push(format!("{}{name}", "  ".repeat(depth)));
                    }
                }
            }
        }
        let mut lines = Vec::new();
        walk(nodes, 0, &mut lines);
        lines
    }

    #[test]
    fn override_tree_groups_files_and_follows_the_filters() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path().join("overrides/common");
        std::fs::create_dir_all(root.join("config/deep")).unwrap();
        std::fs::write(root.join("config/deep/settings.cfg"), b"deep").unwrap();
        std::fs::write(root.join("config/alpha.cfg"), b"alpha").unwrap();
        std::fs::write(root.join("mods.toml"), b"root").unwrap();
        let catalog = super::OverrideCatalog::scan(project.path()).unwrap();

        let tree = super::override_tree(&catalog, "", super::OverrideScopeFilter::All).unwrap();
        assert_eq!(
            tree_lines(&tree),
            [
                "config/",
                "  deep/",
                "    settings.cfg",
                "  alpha.cfg",
                "mods.toml"
            ]
        );

        // A text filter keeps the folders that lead to a match.
        let tree =
            super::override_tree(&catalog, "settings", super::OverrideScopeFilter::All).unwrap();
        assert_eq!(
            tree_lines(&tree),
            ["config/", "  deep/", "    settings.cfg"]
        );

        // Nothing matching means nothing to draw, and the scope filter drops whole entries.
        assert!(
            super::override_tree(&catalog, "nothing-matches", super::OverrideScopeFilter::All)
                .is_none()
        );
        assert!(super::override_tree(&catalog, "", super::OverrideScopeFilter::Client).is_none());
    }

    #[test]
    fn deleting_an_override_drops_its_row_and_the_cached_tree() {
        let project = tempfile::tempdir().unwrap();
        let file = project.path().join("overrides/common/config/only.cfg");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"only").unwrap();

        let mut app = super::McmpuGuiApp::default();
        app.overrides.catalog = Some(super::OverrideCatalog::scan(project.path()).unwrap());
        super::refresh_override_tree(&mut app.overrides);
        assert_eq!(
            tree_lines(app.overrides.tree.as_deref().unwrap()),
            ["config/", "  only.cfg"]
        );

        let revision = app.overrides.revision;
        assert_eq!(
            app.delete_override_entry(project.path(), 0).unwrap(),
            "config/only.cfg"
        );
        assert!(!file.exists());
        assert!(app.overrides.catalog.as_ref().unwrap().entries.is_empty());
        // The cached tree no longer matches the catalog, so the next frame rebuilds it.
        assert_ne!(app.overrides.revision, revision);
        super::refresh_override_tree(&mut app.overrides);
        assert!(app.overrides.tree.is_none());
    }

    #[test]
    fn automatic_sync_window_load_only_returns_nonempty_manifests() {
        let project = tempfile::tempdir().unwrap();
        assert!(
            super::load_sync_window_manifest_thread(project.path().to_path_buf(), RunSide::Client)
                .unwrap()
                .is_none()
        );

        let path = project.path().join(".mcmpu/pending-sync-client.json");
        let mut manifest = PendingSyncManifest {
            side: RunSide::Client,
            run_dir: project.path().join("run/client"),
            path,
            ignored_debug: 0,
            ignored_generated: 0,
            entries: Vec::new(),
        };
        manifest.write().unwrap();
        assert!(
            super::load_sync_window_manifest_thread(project.path().to_path_buf(), RunSide::Client)
                .unwrap()
                .is_none()
        );

        manifest.entries.push(PendingSyncEntry {
            path: "config/changed.cfg".to_string(),
            change: PendingSyncChange::Modified,
            source: Some(crate::project::run::RunSourceKind::OverridesClient),
            target: SyncTarget::Client,
            apply: false,
        });
        manifest.write().unwrap();
        let loaded =
            super::load_sync_window_manifest_thread(project.path().to_path_buf(), RunSide::Client)
                .unwrap()
                .unwrap();
        assert_eq!(loaded.entries, manifest.entries);
    }

    #[test]
    fn gui_log_tail_is_utf8_safe_and_byte_bounded() {
        let mut log = "起".repeat(super::MAX_GUI_LOG_BYTES);
        log.push_str("结束");

        super::trim_gui_log(&mut log);

        assert!(log.len() <= super::MAX_GUI_LOG_BYTES);
        assert!(log.starts_with(super::GUI_LOG_TRUNCATION_NOTICE));
        assert!(log.ends_with("结束"));
    }

    #[test]
    fn gui_event_drain_limits_each_frame_without_dropping_queued_logs() {
        let mut app = super::McmpuGuiApp::default();
        let (sender, receiver) = mpsc::channel();
        for index in 0..(super::MAX_GUI_EVENTS_PER_FRAME + 4) {
            sender
                .send(super::GuiEvent::Log(format!("event-{index}")))
                .unwrap();
        }
        drop(sender);
        app.receiver = Some(receiver);

        app.drain_events();
        assert_eq!(app.log.lines().count(), super::MAX_GUI_EVENTS_PER_FRAME + 1);
        assert!(app.receiver.is_some());

        app.drain_events();
        assert_eq!(app.log.lines().count(), super::MAX_GUI_EVENTS_PER_FRAME + 5);
    }

    #[test]
    fn run_page_loads_editable_defaults_without_logging_configuration_errors() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("overrides/common/config")).unwrap();
        std::fs::create_dir_all(project.path().join("run/client")).unwrap();
        std::fs::write(
            crate::config::TestRunConfig::path(project.path()),
            "player_name = [",
        )
        .unwrap();
        std::fs::write(
            project.path().join("run/client/.mcmpu-sync.toml"),
            "include_paths = [",
        )
        .unwrap();
        let mut app = super::McmpuGuiApp::default();
        app.form.command = GuiCommand::RunClient;
        app.form.use_custom_project_dir = true;
        app.form.project_dir = project.path().to_path_buf();
        let original_log = app.log.clone();

        app.load_run_configuration_if_needed();

        assert_eq!(app.form.test_run, crate::config::TestRunConfig::default());
        assert_eq!(
            app.form.test_run_project_dir.as_deref(),
            Some(project.path())
        );
        assert_eq!(app.form.client_sync_include_paths, "config");
        assert_eq!(app.log, original_log);
        assert_eq!(app.status, app.current_lang().run_test_config_loaded());
    }

    #[test]
    fn run_page_reports_test_run_and_sync_io_errors_without_reusing_old_values() {
        let test_run_project = tempfile::tempdir().unwrap();
        std::fs::create_dir(crate::config::TestRunConfig::path(test_run_project.path())).unwrap();
        let expected_test_run_error =
            crate::config::TestRunConfig::from_project_for_gui(test_run_project.path())
                .unwrap_err()
                .to_string();
        let mut app = super::McmpuGuiApp::default();
        app.form.command = GuiCommand::RunClient;
        app.form.use_custom_project_dir = true;
        app.form.project_dir = test_run_project.path().to_path_buf();
        app.form.test_run.player_name = "stale".to_string();
        app.form.test_run_project_dir = Some(std::path::PathBuf::from("stale-project"));
        app.form.client_sync_include_paths = "stale".to_string();

        app.load_run_configuration_if_needed();

        assert_eq!(app.status, expected_test_run_error);
        assert_eq!(app.form.test_run, crate::config::TestRunConfig::default());
        assert!(app.form.test_run_project_dir.is_none());
        assert!(app.form.client_sync_include_paths.is_empty());

        let sync_project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(sync_project.path().join("overrides")).unwrap();
        std::fs::write(
            sync_project.path().join("overrides/common"),
            b"not a directory",
        )
        .unwrap();
        let expected_sync_error = crate::project::run::sync_config_for_project_for_gui(
            sync_project.path(),
            RunSide::Client,
        )
        .unwrap_err()
        .to_string();
        app.form.project_dir = sync_project.path().to_path_buf();
        app.form.client_sync_include_paths = "stale".to_string();

        app.load_run_configuration_if_needed();

        assert_eq!(app.status, expected_sync_error);
        assert_eq!(
            app.form.test_run_project_dir.as_deref(),
            Some(sync_project.path())
        );
        assert!(app.form.client_sync_include_paths.is_empty());
    }

    #[test]
    fn mod_list_project_settings_use_tolerant_defaults_and_replace_previous_project_state() {
        let configured_project = tempfile::tempdir().unwrap();
        let mut configured = settings_form("0.16.14", "");
        configured.loader = LoaderKind::Fabric;
        configured.save(configured_project.path()).unwrap();
        let malformed_project = tempfile::tempdir().unwrap();
        std::fs::write(malformed_project.path().join("config.toml"), "language = [").unwrap();
        let mut app = super::McmpuGuiApp::default();

        app.apply_mod_list_loaded(super::load_mod_list_thread(
            configured_project.path().to_path_buf(),
        ));
        assert_eq!(app.mod_list.minecraft_version.as_deref(), Some("1.12.2"));
        assert_eq!(app.mod_list.config_loader.as_deref(), Some("fabric"));

        app.apply_mod_list_loaded(super::load_mod_list_thread(
            malformed_project.path().to_path_buf(),
        ));
        assert_eq!(app.mod_list.minecraft_version.as_deref(), Some(""));
        assert_eq!(app.mod_list.config_loader.as_deref(), Some("forge"));

        app.apply_mod_list_loaded(Err("manifest I/O failed".to_string()));
        assert!(app.mod_list.minecraft_version.is_none());
        assert!(app.mod_list.config_loader.is_none());
    }

    #[test]
    fn mod_list_load_failure_clears_all_project_bound_state() {
        let mut app = super::McmpuGuiApp::default();
        let project = ProjectCandidate {
            project_id: 42,
            slug: "stale-project".to_string(),
            name: "Stale Project".to_string(),
            authors: vec!["Author".to_string()],
            dependency_type: GuiDependencyType::Mod,
        };
        app.mod_list.list = Some(GuiDependencyList::from_entries(vec![test_entry(
            "Stale", 42,
        )]));
        app.mod_list.dirty = true;
        app.mod_list.loading = true;
        app.mod_list.saving = true;
        app.mod_list.project_search_input = "stale".to_string();
        app.mod_list.project_search_results = vec![project.clone()];
        app.mod_list.project_search_error = Some("stale error".to_string());
        app.mod_list.project_search_loading = true;
        app.mod_list.project_search_deadline = Some(std::time::Instant::now());
        app.mod_list.project_search_cache.insert(
            ProjectSearchInput::Keywords("stale".to_string()),
            vec![project.clone()],
        );
        app.mod_list.querying_files = true;
        app.mod_list.querying_add_files = true;
        app.mod_list.row_ids = vec![99];
        app.mod_list.next_row_id = 100;
        app.mod_list.file_candidate_row_id = Some(99);
        let file = FileCandidate {
            file_id: 1001,
            file_name: "stale.jar".to_string(),
            display_name: Some("Stale".to_string()),
            game_versions: vec!["1.12.2".to_string()],
            loader: Some("forge".to_string()),
        };
        app.mod_list.file_candidates = vec![file.clone()];
        app.mod_list.add_file_candidates = Some(vec![file]);
        app.mod_list.search = "stale list filter".to_string();
        app.mod_list.minecraft_version = Some("1.12.2".to_string());
        app.mod_list.config_loader = Some("forge".to_string());
        app.mod_list.add_draft = Some(AddProjectDraft::from_candidate(project, Some("forge")));
        let previous_revision = app.mod_list.revision;
        let previous_search_generation = app.mod_list.project_search_generation;
        let previous_file_generation = app.mod_list.file_candidate_generation;
        let previous_add_generation = app.mod_list.add_file_candidate_generation;

        app.apply_mod_list_loaded(Err("new project could not be loaded".to_string()));

        assert!(!app.mod_list.loading);
        assert!(!app.mod_list.saving);
        assert!(app.mod_list.list.is_none());
        assert!(!app.mod_list.dirty);
        assert!(app.mod_list.row_ids.is_empty());
        assert_eq!(app.mod_list.next_row_id, 1);
        assert!(app.mod_list.project_search_input.is_empty());
        assert!(app.mod_list.project_search_results.is_empty());
        assert!(app.mod_list.project_search_error.is_none());
        assert!(!app.mod_list.project_search_loading);
        assert!(app.mod_list.project_search_deadline.is_none());
        assert!(app.mod_list.project_search_cache.is_empty());
        assert!(!app.mod_list.querying_files);
        assert!(!app.mod_list.querying_add_files);
        assert!(app.mod_list.file_candidate_row_id.is_none());
        assert!(app.mod_list.file_candidates.is_empty());
        assert!(app.mod_list.add_file_candidates.is_none());
        assert!(app.mod_list.search.is_empty());
        assert!(app.mod_list.minecraft_version.is_none());
        assert!(app.mod_list.config_loader.is_none());
        assert!(app.mod_list.add_draft.is_none());
        assert_ne!(app.mod_list.revision, previous_revision);
        assert_ne!(
            app.mod_list.project_search_generation,
            previous_search_generation
        );
        assert_ne!(
            app.mod_list.file_candidate_generation,
            previous_file_generation
        );
        assert_ne!(
            app.mod_list.add_file_candidate_generation,
            previous_add_generation
        );
        assert_eq!(app.mod_list.message, "new project could not be loaded");
    }

    #[test]
    fn gui_log_producer_does_not_block_when_event_queue_is_full() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        sender
            .send(super::GuiEvent::Log("already queued".to_string()))
            .unwrap();
        let (completed_sender, completed_receiver) = mpsc::channel();

        std::thread::spawn(move || {
            super::send_log(&sender, "must not block".to_string());
            completed_sender.send(()).unwrap();
        });

        completed_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("a full GUI log queue must not block the run process");
    }

    #[test]
    fn gui_run_log_distinguishes_target_cache_and_download_sources() {
        for (source, expected) in [
            (ArtifactInstallSource::TargetHit, "运行目录命中"),
            (ArtifactInstallSource::CacheHit, "缓存命中"),
            (ArtifactInstallSource::Downloaded, "已下载"),
        ] {
            let rendered = super::render_run_progress(
                Language::ZhCn,
                &RunProgress::RuntimeArtifactReady {
                    side: RunSide::Client,
                    completed: 1,
                    total: 1,
                    path: "libraries/example.jar".to_string(),
                    source,
                },
            );
            assert_eq!(
                rendered,
                format!("[run] 游戏文件 1/1 ({expected}): libraries/example.jar")
            );
        }
        assert_eq!(
            super::artifact_install_source_label(Language::EnUs, ArtifactInstallSource::TargetHit),
            "target reused"
        );
    }

    #[test]
    fn gui_run_log_localizes_loader_and_metadata_cache_events() {
        let loader = super::render_run_progress(
            Language::ZhCn,
            &RunProgress::LoaderInstallerOutput {
                side: RunSide::Client,
                stream: RuntimeInstallStream::Stdout,
                line: "[cache] reused validated client loader installation: C:\\cache\\client.json"
                    .to_string(),
            },
        );
        let metadata = super::render_run_progress(
            Language::ZhCn,
            &RunProgress::LoaderInstallerOutput {
                side: RunSide::Client,
                stream: RuntimeInstallStream::Stdout,
                line: "[cache] reused Mojang version manifest: C:\\cache\\version_manifest_v2.json"
                    .to_string(),
            },
        );

        assert_eq!(
            loader,
            "[run] 复用运行安装缓存（已验证的客户端 Loader 安装）：C:\\cache\\client.json"
        );
        assert_eq!(
            metadata,
            "[run] 复用运行安装缓存（Mojang 版本清单缓存）：C:\\cache\\version_manifest_v2.json"
        );
    }

    #[test]
    fn run_console_mode_belongs_to_the_side_whose_run_is_in_progress() {
        // Only a side with an in-progress run shows its console; the page of the other side keeps
        // its own state, so both can be used at the same time.
        let mut app = super::McmpuGuiApp::default();
        app.form.command = GuiCommand::RunClient;
        assert!(!app.run_console_mode());

        let (sender, receiver) = mpsc::channel::<super::GuiEvent>();
        app.client_run = Some(super::SideRun {
            receiver,
            stop_signal: SharedStopSignal::default(),
            console_input: SharedConsoleInput::default(),
        });
        drop(sender);
        assert!(app.run_console_mode());

        // The server page stays a form while only the client runs.
        app.form.command = GuiCommand::RunServer;
        assert!(!app.run_console_mode());
        app.form.command = GuiCommand::RunClient;
        assert!(app.run_console_mode());

        app.form.command = GuiCommand::Build;
        assert!(!app.run_console_mode());
    }

    #[test]
    fn side_runs_complete_independently_and_only_their_own_console_closes() {
        for event in [
            super::GuiEvent::Finished(0),
            super::GuiEvent::Failed("failed".into()),
        ] {
            let mut app = super::McmpuGuiApp::default();
            app.form.command = GuiCommand::RunClient;
            let (sender, receiver) = mpsc::channel();
            sender.send(event).unwrap();
            drop(sender);
            app.client_run = Some(super::SideRun {
                receiver,
                stop_signal: SharedStopSignal::default(),
                console_input: SharedConsoleInput::default(),
            });
            assert!(app.run_console_mode());
            // A run must not trap the user on one page while it is still producing output.
            assert!(app.navigation_enabled());

            app.drain_events();

            assert!(app.client_run.is_none());
            assert!(!app.run_console_mode());
            assert!(app.navigation_enabled());
        }
    }

    #[test]
    fn console_opens_collapsed_and_follows_the_runs_it_belongs_to() {
        let mut app = super::McmpuGuiApp::default();
        // The page starts collapsed: nothing is running, so there is nothing to watch.
        assert!(!app.console_expanded);

        // A side that is still running keeps the console expanded.
        let (sender, receiver) = mpsc::channel();
        app.console_expanded = true;
        app.set_side_run(
            RunSide::Client,
            Some(super::SideRun {
                receiver,
                stop_signal: SharedStopSignal::default(),
                console_input: SharedConsoleInput::default(),
            }),
        );
        app.collapse_console_when_idle();
        assert!(app.console_expanded);

        // The run ending collapses it again, without any manual toggle.
        sender.send(super::GuiEvent::Finished(0)).unwrap();
        drop(sender);
        app.drain_side_events(RunSide::Client);
        assert!(app.client_run.is_none());
        assert!(!app.console_expanded);
    }

    #[test]
    fn command_page_layout_never_produces_negative_or_non_finite_heights() {
        for available in [
            f32::NEG_INFINITY,
            -100.0,
            0.0,
            100.0,
            160.0,
            165.0,
            640.0,
            f32::INFINITY,
            f32::NAN,
        ] {
            let layout = super::CommandPageLayout::new(available);
            assert!(layout.form_max_height.is_finite());
            assert!(layout.form_max_height >= 0.0);
            assert!(layout.gap.is_finite());
            assert!(layout.gap >= 0.0);
            assert!(layout.form_max_height + layout.gap <= super::available_height(available));
        }

        let constrained = super::CommandPageLayout::new(640.0);
        assert_eq!(constrained.form_max_height, 470.0);
        assert_eq!(constrained.gap, super::COMMAND_FORM_LOG_GAP);
    }

    #[test]
    fn project_search_discards_stale_results_and_caches_current_results() {
        let mut app = super::McmpuGuiApp::default();
        app.mod_list.project_search_input = "first".to_string();
        app.schedule_project_search();
        let stale_generation = app.mod_list.project_search_generation;

        app.mod_list.project_search_input = "second".to_string();
        app.schedule_project_search();
        let current_generation = app.mod_list.project_search_generation;
        let project = ProjectCandidate {
            project_id: 42,
            slug: "second-project".to_string(),
            name: "Second Project".to_string(),
            authors: vec!["Test Author".to_string()],
            dependency_type: GuiDependencyType::Mod,
        };

        app.apply_projects_searched(
            stale_generation,
            ProjectSearchInput::Keywords("first".to_string()),
            Ok(vec![project.clone()]),
        );
        assert!(app.mod_list.project_search_results.is_empty());

        app.apply_projects_searched(
            current_generation,
            ProjectSearchInput::Keywords("second".to_string()),
            Ok(vec![project.clone()]),
        );

        assert_eq!(app.mod_list.project_search_results, vec![project.clone()]);
        assert_eq!(
            app.mod_list
                .project_search_cache
                .get(&ProjectSearchInput::Keywords("second".to_string())),
            Some(&vec![project])
        );
    }

    #[test]
    fn deleting_a_row_invalidates_pending_file_candidates_and_keeps_row_ids_aligned() {
        let mut app = super::McmpuGuiApp::default();
        app.mod_list.list = Some(GuiDependencyList::from_entries(vec![
            test_entry("First", 10),
            test_entry("Second", 20),
        ]));
        app.reset_mod_list_row_ids();
        let deleted_row_id = app.mod_list.row_ids[0];
        let pending_generation = app.mod_list.file_candidate_generation;
        let pending_revision = app.mod_list.revision;

        app.delete_mod_list_row(0, deleted_row_id);
        app.apply_file_candidates_queried(super::ModListFileCandidateResult {
            generation: pending_generation,
            revision: pending_revision,
            row_id: deleted_row_id,
            project: ProjectLookup::ProjectId(10),
            loader: Some("forge".to_string()),
            result: Ok(Vec::new()),
        });

        assert_eq!(app.mod_list.list.unwrap().entries[0].name, "Second");
        assert_eq!(app.mod_list.row_ids.len(), 1);
        assert_ne!(app.mod_list.row_ids[0], deleted_row_id);
        assert!(app.mod_list.file_candidates.is_empty());
        assert!(app.mod_list.file_candidate_row_id.is_none());
    }

    #[test]
    fn stale_save_completion_keeps_newer_mod_list_and_settings_changes_dirty() {
        let mut app = super::McmpuGuiApp::default();
        app.mod_list.revision = 2;
        app.mark_mod_list_dirty();
        app.apply_mod_list_saved(2, Ok(()));
        assert!(app.mod_list.dirty);

        app.settings.revision = 4;
        app.mark_settings_dirty();
        app.apply_settings_saved(4, Ok(()));
        assert!(app.settings.dirty);
    }

    #[test]
    fn settings_loading_keeps_configured_versions_idle_until_selector_requests_them() {
        let mut app = super::McmpuGuiApp::default();
        let form = settings_form("0.6.2-alpha", "14.23.5.2860");

        app.apply_settings_loaded(Ok(form));

        let form = app.settings.form.as_ref().unwrap();
        assert_eq!(form.loader_version, "0.6.2-alpha");
        assert_eq!(form.forge_compat_version, "14.23.5.2860");
        assert_eq!(
            app.settings.loader_versions_state,
            LoaderVersionCandidateState::Idle
        );
        assert!(app.settings.loader_versions.is_empty());
        assert!(app.settings.forge_compat_versions.is_empty());

        let query = LoaderVersionQuery {
            minecraft_version: "1.12.2".to_string(),
            loader: LoaderKind::Cleanroom,
        };
        app.settings.loader_versions_cache.insert(
            query.clone(),
            LoaderVersionCandidates {
                query,
                loader_versions: vec!["0.6.3-alpha".to_string()],
                forge_compat_versions: vec!["14.23.5.2860".to_string()],
            },
        );

        app.query_settings_loader_versions();

        assert_eq!(
            app.settings.loader_versions_state,
            LoaderVersionCandidateState::Ready
        );
        assert_eq!(
            app.settings.loader_versions,
            vec!["0.6.3-alpha".to_string()]
        );
        assert_eq!(
            app.settings.form.as_ref().unwrap().loader_version,
            "0.6.2-alpha"
        );
        assert_eq!(
            app.settings.form.as_ref().unwrap().forge_compat_version,
            "14.23.5.2860"
        );
    }

    #[test]
    fn settings_selector_request_intent_only_occurs_for_idle_or_failed_states() {
        assert!(super::should_request_loader_versions(
            &LoaderVersionCandidateState::Idle,
            true
        ));
        assert!(super::should_request_loader_versions(
            &LoaderVersionCandidateState::Failed("offline".to_string()),
            true
        ));
        assert!(!super::should_request_loader_versions(
            &LoaderVersionCandidateState::Loading,
            true
        ));
        assert!(!super::should_request_loader_versions(
            &LoaderVersionCandidateState::Ready,
            true
        ));
        assert!(!super::should_request_loader_versions(
            &LoaderVersionCandidateState::Idle,
            false
        ));
    }

    fn settings_form(loader_version: &str, forge_compat_version: &str) -> SettingsForm {
        SettingsForm {
            language: "zh-CN".to_string(),
            pack_name: "test".to_string(),
            pack_version: "1.0.0".to_string(),
            authors: "tester".to_string(),
            minecraft_version: "1.12.2".to_string(),
            loader: LoaderKind::Cleanroom,
            loader_version: loader_version.to_string(),
            forge_compat_version: forge_compat_version.to_string(),
            proxy_ip: String::new(),
            proxy_port: String::new(),
            output_dir: "dist".to_string(),
            java_version_major: 8,
            client_min_memory: "1G".to_string(),
            client_max_memory: "2G".to_string(),
            client_extra_args: String::new(),
            server_min_memory: "1G".to_string(),
            server_max_memory: "2G".to_string(),
            server_extra_args: String::new(),
            cleanroom_enabled: true,
            cleanroom_use_scalar: false,
            cleanroom_scalar_version: "2".to_string(),
            cleanroom_relauncher: Default::default(),
        }
    }

    fn test_entry(name: &str, project_id: u64) -> GuiModEntry {
        GuiModEntry {
            manifest: ManifestList::Common,
            name: name.to_string(),
            dependency_type: GuiDependencyType::Mod,
            loader: Some("forge".to_string()),
            project_id: Some(project_id),
            file_id: Some(project_id + 1000),
            auto_update: false,
            slug: Some(name.to_ascii_lowercase()),
            required: true,
        }
    }
}
