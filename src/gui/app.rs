use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, ScrollArea, Sense, Stroke, TextEdit};

use crate::config::{CleanroomRelauncher, Config, DownloadSource, LoaderKind};
use crate::curseforge::client::CurseForgeClient;
use crate::curseforge::resolver::ApiKeyResolver;
use crate::gui::command::{
    CommandForm, GuiCommand, GuiCommandError, GuiCommandSpec, GuiCommandTask,
};
use crate::gui::mod_list::{
    AddProjectDraft, FileCandidate, GuiDependencyList, GuiDependencyType, ManifestList,
    ModListProjectSettings, ProjectCandidate, ProjectLookup, ProjectSearchInput,
    QueryFileCandidateCache, QueryFileCandidates, build_curseforge_client,
    load_project_config_for_mod_list, load_project_settings_for_mod_list,
    parse_project_search_input, query_file_candidates_cached, search_project_candidates,
};
use crate::gui::settings::SettingsForm;
use crate::i18n::Language;
use crate::project::build::{BuildProgress, build_project_with_api_key_and_progress};
use crate::project::check::check_project_with_api_key_and_progress;
use crate::project::init::{
    InitProgress, initialize_from_zip_with_api_and_progress, initialize_from_zip_with_progress,
};
use crate::project::run::{
    PendingSyncChange, PendingSyncManifest, RunProgress, RunSide, SyncTarget, apply_pending_sync,
    run_project_with_api_key_and_progress,
};

const ACCENT: Color32 = Color32::from_rgb(28, 172, 134);
const ACCENT_HOVER: Color32 = Color32::from_rgb(48, 195, 155);
const ACCENT_ACTIVE: Color32 = Color32::from_rgb(18, 148, 115);

#[derive(Debug)]
pub struct McmpuGuiApp {
    form: CommandForm,
    log: String,
    status: String,
    running: bool,
    receiver: Option<Receiver<GuiEvent>>,
    mod_list: ModListPageState,
    settings: SettingsPageState,
    sync: SyncPageState,
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
    SyncLoaded(Result<PendingSyncManifest, String>),
    SyncWindowLoaded(Result<PendingSyncManifest, String>),
    SyncApplied(Result<Vec<String>, String>),
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
}

impl Default for SyncPageState {
    fn default() -> Self {
        Self {
            manifest: None,
            side: RunSide::Client,
            loading: false,
            applying: false,
            window_open: false,
            message: String::new(),
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
    settings: Option<ModListProjectSettings>,
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
            settings: SettingsPageState::default(),
            sync: SyncPageState::default(),
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
        if self.any_operation_running() {
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
                ui.label(RichText::new("0.1.0").size(11.0).color(text_muted(dark)));
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
                    !self.any_operation_running(),
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
            self.log = format!("[{}] waiting\n", command.cli_name());
            self.status = self.current_lang().status_ready().to_string();
            if command == GuiCommand::ModList {
                self.open_mod_list();
            } else if command == GuiCommand::Settings {
                self.open_settings();
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
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.inner_margin(egui::Margin {
                left: 12,
                right: 12,
                top: 12,
                bottom: 0,
            }))
            .show(ui, |ui| match self.form.command {
                GuiCommand::ModList => self.mod_list_panel(ui),
                GuiCommand::Settings => self.settings_panel(ui),
                _ => {
                    self.form_panel(ui);
                    ui.add_space(10.0);
                    let log_height = (ui.available_height() - 2.0).max(160.0);
                    self.log_panel(ui, log_height);
                }
            });
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
            });
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
                ScrollArea::vertical()
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
                        && let Some(manifest) = &mut self.sync.manifest
                    {
                        for entry in &mut manifest.entries {
                            entry.apply = matches!(
                                entry.change,
                                PendingSyncChange::Modified | PendingSyncChange::Added
                            );
                        }
                    }
                    if self
                        .accent_button(ui, lang.sync_apply(), [120.0, 30.0])
                        .clicked()
                    {
                        self.apply_sync_manifest();
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
                ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .max_height(height)
                    .show(ui, |ui| {
                        let Some(manifest) = self.sync.manifest.as_mut() else {
                            ui.label(lang.sync_empty());
                            return;
                        };
                        if manifest.entries.is_empty() {
                            ui.label(lang.sync_empty());
                            return;
                        }
                        for index in 0..manifest.entries.len() {
                            sync_row(ui, index, manifest, dark);
                        }
                    });
            });
    }

    fn settings_panel(&mut self, ui: &mut egui::Ui) {
        let dark = ui.visuals().dark_mode;
        let lang = self.current_lang();
        self.ensure_settings_loaded();
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
                ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .max_height(height)
                    .show(ui, |ui| {
                        let Some(form) = self.settings.form.as_mut() else {
                            ui.label(lang.settings_not_loaded());
                            return;
                        };
                        if settings_form_ui(ui, form, lang, dark) {
                            self.mark_settings_dirty();
                        }
                    });
            });
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
        self.mod_list.project_search_loading = true;
        let sender = self.ensure_event_sender();
        thread::spawn(move || {
            let result = search_projects_thread(project_dir, input.clone());
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
                ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
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
        let list = self.mod_list.list.get_or_insert_with(|| GuiDependencyList {
            entries: Vec::new(),
        });
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
            );
            let _ = sender.send(GuiEvent::ModListAddFileCandidatesQueried(result));
        });
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
            let result =
                apply_pending_sync(&project_dir, &mut manifest).map_err(|err| err.to_string());
            let _ = sender.send(GuiEvent::SyncApplied(result));
        });
    }

    fn ensure_event_sender(&mut self) -> mpsc::Sender<GuiEvent> {
        let (sender, receiver) = mpsc::channel();
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
                self.mod_list.list = Some(result.list);
                self.mod_list.dirty = false;
                self.mod_list.revision += 1;
                self.mod_list.file_candidate_generation += 1;
                self.mod_list.querying_files = false;
                self.mod_list.file_candidate_row_id = None;
                self.mod_list.file_candidates.clear();
                self.mod_list.file_candidate_cache.clear();
                self.reset_mod_list_row_ids();
                if let Some(settings) = result.settings {
                    self.mod_list.minecraft_version = Some(settings.minecraft_version);
                    self.mod_list.config_loader = Some(settings.loader);
                }
                self.mod_list.message = self.current_lang().mod_list_loaded().to_string();
            }
            Err(err) => {
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

    fn apply_settings_loaded(&mut self, result: Result<SettingsForm, String>) {
        self.settings.loading = false;
        match result {
            Ok(form) => {
                self.settings.form = Some(form);
                self.settings.dirty = false;
                self.settings.revision += 1;
                self.settings.message = self.current_lang().settings_loaded().to_string();
            }
            Err(err) => {
                self.settings.form = None;
                self.settings.message = err;
            }
        }
        self.status = self.settings.message.clone();
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

    fn apply_sync_applied(&mut self, result: Result<Vec<String>, String>) {
        self.sync.applying = false;
        match result {
            Ok(applied) => {
                self.sync.message =
                    format!("{}: {}", self.current_lang().sync_apply(), applied.len());
                self.load_sync_manifest();
            }
            Err(err) => {
                self.sync.message = err;
            }
        }
        self.status = self.sync.message.clone();
    }

    fn log_panel(&mut self, ui: &mut egui::Ui, height: f32) {
        let dark = ui.visuals().dark_mode;
        let lang = self.current_lang();
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
                            let dot_color = if self.running {
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
                        });
                    });
                let body_height = (ui.available_height() - 10.0).max(60.0);
                egui::Frame::NONE
                    .inner_margin(egui::Margin::symmetric(14, 10))
                    .show(ui, |ui| {
                        let v = ui.visuals_mut();
                        v.extreme_bg_color = LOG_BG;
                        v.text_edit_bg_color = Some(LOG_BG);
                        v.widgets.inactive.bg_stroke = Stroke::NONE;
                        v.widgets.hovered.bg_stroke = Stroke::NONE;
                        v.widgets.active.bg_stroke = Stroke::NONE;
                        ScrollArea::vertical()
                            .stick_to_bottom(true)
                            .auto_shrink([false, false])
                            .min_scrolled_height(body_height)
                            .max_height(body_height)
                            .show(ui, |ui| {
                                ui.add(
                                    TextEdit::multiline(&mut self.log)
                                        .font(egui::TextStyle::Monospace)
                                        .text_color(LOG_TEXT)
                                        .desired_width(f32::INFINITY)
                                        .interactive(false),
                                );
                            });
                    });
            });
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
                } else {
                    let label = lang.btn_run(self.form.command.cli_name());
                    if self.accent_button(ui, &label, [140.0, 38.0]).clicked() && !self.running {
                        self.start_command();
                    }
                }
            });
        });
    }

    fn accent_button(&self, ui: &mut egui::Ui, label: &str, min_size: [f32; 2]) -> egui::Response {
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
                !self.any_operation_running(),
                egui::Button::new(RichText::new(label).color(Color32::WHITE).strong())
                    .min_size(egui::vec2(min_size[0], min_size[1]))
                    .corner_radius(7),
            )
        })
        .inner
    }

    fn any_operation_running(&self) -> bool {
        self.running
            || self.mod_list.loading
            || self.mod_list.saving
            || self.mod_list.querying_files
            || self.settings.loading
            || self.settings.saving
            || self.sync.loading
            || self.sync.applying
    }

    fn start_command(&mut self) {
        match self.form.to_command_spec() {
            Ok(spec) => {
                self.log = format!("{}\n", spec.display_command());
                self.status = self.current_lang().status_running().to_string();
                self.running = true;
                let (sender, receiver) = mpsc::channel();
                self.receiver = Some(receiver);
                thread::spawn(move || run_command_thread(spec, sender));
            }
            Err(err) => {
                let lang = self.current_lang();
                self.status = lang.status_input_error().to_string();
                self.log
                    .push_str(&format!("{}\n", localize_error(lang, &err)));
            }
        }
    }

    fn drain_events(&mut self) {
        let Some(receiver) = self.receiver.take() else {
            return;
        };
        let lang = self.current_lang();
        let mut keep_receiver = true;
        while let Ok(event) = receiver.try_recv() {
            match event {
                GuiEvent::Log(line) => {
                    self.log.push_str(&line);
                    self.log.push('\n');
                }
                GuiEvent::Finished(code) => {
                    self.running = false;
                    self.status = lang.status_finished(code);
                    self.log.push_str(&format!("[process] exit code {code}\n"));
                    keep_receiver = false;
                }
                GuiEvent::Failed(message) => {
                    self.running = false;
                    self.status = lang.status_failed().to_string();
                    self.log.push_str(&format!("[process] {message}\n"));
                    keep_receiver = false;
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
                GuiEvent::SyncLoaded(result) => {
                    self.apply_sync_loaded(result);
                }
                GuiEvent::SyncWindowLoaded(result) => {
                    self.sync.window_open = true;
                    self.apply_sync_loaded(result);
                }
                GuiEvent::SyncApplied(result) => {
                    self.apply_sync_applied(result);
                }
            }
        }
        if keep_receiver {
            self.receiver = Some(receiver);
        }
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

fn sync_row(ui: &mut egui::Ui, index: usize, manifest: &mut PendingSyncManifest, dark: bool) {
    let entry = &mut manifest.entries[index];
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
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut entry.apply, "");
                ui.add_sized(
                    [78.0, 24.0],
                    egui::Label::new(RichText::new(entry.change.to_string()).strong()),
                );
                ui.add_sized(
                    [320.0, 24.0],
                    egui::Label::new(RichText::new(&entry.path).color(label_color(dark)))
                        .truncate(),
                );
                ui.label(
                    RichText::new(
                        entry
                            .source
                            .map(|source| format!("{source:?}"))
                            .unwrap_or_else(|| "new".to_string()),
                    )
                    .size(12.0)
                    .color(text_muted(dark)),
                );
                combo_sync_target(ui, index, &mut entry.target);
            });
        });
    ui.add_space(6.0);
}

fn settings_form_ui(
    ui: &mut egui::Ui,
    form: &mut SettingsForm,
    lang: Language,
    dark: bool,
) -> bool {
    let mut dirty = false;
    settings_section(ui, lang.settings_section_pack(), dark);
    dirty |= settings_language_combo(ui, lang, &mut form.language, dark);
    dirty |= settings_text_row(
        ui,
        lang.settings_label_pack_name(),
        &mut form.pack_name,
        dark,
    );
    dirty |= settings_text_row(
        ui,
        lang.settings_label_pack_version(),
        &mut form.pack_version,
        dark,
    );
    dirty |= settings_text_row(ui, lang.settings_label_authors(), &mut form.authors, dark);
    dirty |= settings_text_row(
        ui,
        lang.settings_label_minecraft_version(),
        &mut form.minecraft_version,
        dark,
    );
    dirty |= settings_loader_combo(ui, lang, &mut form.loader);
    dirty |= settings_text_row(
        ui,
        lang.settings_label_loader_version(),
        &mut form.loader_version,
        dark,
    );
    dirty |= settings_text_row(
        ui,
        lang.settings_label_forge_compat_version(),
        &mut form.forge_compat_version,
        dark,
    );

    ui.add_space(12.0);
    settings_section(ui, lang.settings_section_network(), dark);
    dirty |= settings_text_row(ui, lang.settings_label_proxy_ip(), &mut form.proxy_ip, dark);
    dirty |= settings_text_row(
        ui,
        lang.settings_label_proxy_port(),
        &mut form.proxy_port,
        dark,
    );

    ui.add_space(12.0);
    settings_section(ui, lang.settings_section_build(), dark);
    dirty |= settings_text_row(
        ui,
        lang.settings_label_output_dir(),
        &mut form.output_dir,
        dark,
    );

    ui.add_space(12.0);
    settings_section(ui, lang.settings_section_java_client(), dark);
    dirty |= settings_text_row(
        ui,
        lang.settings_label_min_memory(),
        &mut form.client_min_memory,
        dark,
    );
    dirty |= settings_text_row(
        ui,
        lang.settings_label_max_memory(),
        &mut form.client_max_memory,
        dark,
    );
    dirty |= settings_multiline_row(
        ui,
        lang.settings_label_extra_jvm_args(),
        &mut form.client_extra_args,
        dark,
    );

    ui.add_space(12.0);
    settings_section(ui, lang.settings_section_java_server(), dark);
    dirty |= settings_text_row(
        ui,
        lang.settings_label_min_memory(),
        &mut form.server_min_memory,
        dark,
    );
    dirty |= settings_text_row(
        ui,
        lang.settings_label_max_memory(),
        &mut form.server_max_memory,
        dark,
    );
    dirty |= settings_multiline_row(
        ui,
        lang.settings_label_extra_jvm_args(),
        &mut form.server_extra_args,
        dark,
    );

    ui.add_space(12.0);
    settings_section(ui, lang.settings_section_test_run(), dark);
    dirty |= settings_text_row(
        ui,
        lang.settings_label_player_name(),
        &mut form.test_player_name,
        dark,
    );
    dirty |= settings_download_source_combo(ui, lang, &mut form.test_download_source);
    dirty |= settings_text_row(
        ui,
        lang.settings_label_java_version_major(),
        &mut form.test_java_version_major,
        dark,
    );
    dirty |= settings_text_row(
        ui,
        lang.settings_label_sync_interval(),
        &mut form.test_sync_interval_seconds,
        dark,
    );
    dirty |= settings_multiline_row(
        ui,
        lang.settings_label_debug_jvm_args(),
        &mut form.test_debug_extra_jvm_args,
        dark,
    );
    dirty |= settings_text_row(
        ui,
        lang.settings_label_java_path(),
        &mut form.test_java_path,
        dark,
    );

    ui.add_space(12.0);
    settings_section(ui, lang.settings_section_cleanroom(), dark);
    dirty |= settings_checkbox_row(
        ui,
        lang.settings_label_cleanroom_enabled(),
        &mut form.cleanroom_enabled,
        dark,
    );
    if form.cleanroom_enabled {
        dirty |= settings_checkbox_row(
            ui,
            lang.settings_label_use_scalar(),
            &mut form.cleanroom_use_scalar,
            dark,
        );
        dirty |= settings_text_row(
            ui,
            lang.settings_label_scalar_version(),
            &mut form.cleanroom_scalar_version,
            dark,
        );
        dirty |= settings_relauncher_combo(ui, lang, &mut form.cleanroom_relauncher);
    }
    dirty
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
            .selected_text(if value.eq_ignore_ascii_case("en-US") {
                "en-US"
            } else {
                "zh-CN"
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(value, "zh-CN".to_string(), "zh-CN");
                ui.selectable_value(value, "en-US".to_string(), "en-US");
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

fn settings_download_source_combo(
    ui: &mut egui::Ui,
    lang: Language,
    value: &mut DownloadSource,
) -> bool {
    let before = *value;
    ui.horizontal(|ui| {
        ui.set_height(36.0);
        field_label(
            ui,
            lang.settings_label_download_source(),
            label_color(ui.visuals().dark_mode),
        );
        egui::ComboBox::from_id_salt("settings_download_source")
            .selected_text(download_source_label(*value))
            .show_ui(ui, |ui| {
                ui.selectable_value(value, DownloadSource::Auto, "auto");
                ui.selectable_value(value, DownloadSource::Official, "official");
                ui.selectable_value(value, DownloadSource::Bmclapi, "bmclapi");
            });
    });
    before != *value
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

fn download_source_label(value: DownloadSource) -> &'static str {
    match value {
        DownloadSource::Auto => "auto",
        DownloadSource::Official => "official",
        DownloadSource::Bmclapi => "bmclapi",
    }
}

fn relauncher_label(value: CleanroomRelauncher) -> &'static str {
    match value {
        CleanroomRelauncher::Unofficial => "unofficial",
        CleanroomRelauncher::Original => "original",
        CleanroomRelauncher::Improved => "improved",
    }
}

fn combo_sync_target(ui: &mut egui::Ui, index: usize, value: &mut SyncTarget) {
    egui::ComboBox::from_id_salt(("sync_target", index))
        .selected_text(value.dir_name())
        .show_ui(ui, |ui| {
            ui.selectable_value(value, SyncTarget::Common, "common");
            ui.selectable_value(value, SyncTarget::Client, "client");
            ui.selectable_value(value, SyncTarget::Server, "server");
        });
}

fn command_title(lang: Language, command: GuiCommand) -> &'static str {
    match command {
        GuiCommand::Init => lang.cmd_title_init(),
        GuiCommand::ModList => lang.cmd_title_mod_list(),
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
        GuiCommandError::MissingProxyIp => lang.err_missing_proxy_ip().to_string(),
        GuiCommandError::MissingProxyPort => lang.err_missing_proxy_port().to_string(),
        GuiCommandError::GuiOnlyCommand => lang.err_gui_only_command().to_string(),
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
    let settings = load_project_settings_for_mod_list(&project_dir).ok();
    Ok(ModListLoadResult { list, settings })
}

fn search_projects_thread(
    project_dir: PathBuf,
    input: ProjectSearchInput,
) -> Result<Vec<ProjectCandidate>, String> {
    let config = load_project_config_for_mod_list(&project_dir).map_err(|err| err.to_string())?;
    let api = build_curseforge_client(&config).map_err(|err| err.to_string())?;
    search_project_candidates(&api, &input).map_err(|err| err.to_string())
}

fn query_file_candidates_thread(
    project_dir: PathBuf,
    cache: std::sync::Arc<QueryFileCandidateCache>,
    generation: u64,
    revision: u64,
    row_id: u64,
    project: ProjectLookup,
    loader: Option<String>,
) -> ModListFileCandidateResult {
    let result = (|| {
        let config =
            load_project_config_for_mod_list(&project_dir).map_err(|err| err.to_string())?;
        let api = build_curseforge_client(&config).map_err(|err| err.to_string())?;
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
) -> AddProjectFileCandidateResult {
    let result = (|| {
        let config =
            load_project_config_for_mod_list(&project_dir).map_err(|err| err.to_string())?;
        let api = build_curseforge_client(&config).map_err(|err| err.to_string())?;
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

fn merge_receivers(left: Receiver<GuiEvent>, right: Receiver<GuiEvent>) -> Receiver<GuiEvent> {
    let (sender, receiver) = mpsc::channel();
    forward_receiver(left, sender.clone());
    forward_receiver(right, sender);
    receiver
}

fn forward_receiver(receiver: Receiver<GuiEvent>, sender: mpsc::Sender<GuiEvent>) {
    thread::spawn(move || {
        for event in receiver {
            if sender.send(event).is_err() {
                break;
            }
        }
    });
}

fn run_command_thread(spec: GuiCommandSpec, sender: mpsc::Sender<GuiEvent>) {
    let result = run_gui_command_direct(&spec, &sender);
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
    sender: &mpsc::Sender<GuiEvent>,
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
        GuiCommandTask::Build => {
            let language = project_language_for_gui(&spec.current_dir);
            let config = Config::from_path(spec.current_dir.join("config.toml"))
                .map_err(|err| err.to_string())?;
            let api_key = resolve_gui_api_key(&config).map_err(|err| err.to_string())?;
            let proxy =
                resolve_gui_proxy(spec.proxy.as_deref(), &config).map_err(|err| err.to_string())?;
            let outputs = build_project_with_api_key_and_progress(
                &spec.current_dir,
                api_key,
                proxy.as_deref(),
                &mut |event| send_log(sender, render_build_progress(language, event)),
            )
            .map_err(|err| err.to_string())?;
            send_log(
                sender,
                format!(
                    "{}: {}",
                    language.build_client(),
                    outputs.client_zip.display()
                ),
            );
            send_log(
                sender,
                format!(
                    "{}: {}",
                    language.build_server(),
                    outputs.server_zip.display()
                ),
            );
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
            let config = Config::from_path(spec.current_dir.join("config.toml"))
                .map_err(|err| err.to_string())?;
            let api_key = resolve_gui_api_key(&config).map_err(|err| err.to_string())?;
            let proxy =
                resolve_gui_proxy(spec.proxy.as_deref(), &config).map_err(|err| err.to_string())?;
            let plan = run_project_with_api_key_and_progress(
                &spec.current_dir,
                api_key,
                proxy.as_deref(),
                side,
                &mut |event| {
                    send_log(sender, render_run_progress(language, event));
                    if let RunProgress::PendingSyncWritten { path, .. } = event {
                        let result =
                            PendingSyncManifest::from_path(path).map_err(|err| err.to_string());
                        let _ = sender.send(GuiEvent::SyncWindowLoaded(result));
                    }
                },
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

fn send_log(sender: &mpsc::Sender<GuiEvent>, line: String) {
    let _ = sender.send(GuiEvent::Log(line));
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
    config: &Config,
) -> Result<Option<String>, crate::config::ConfigError> {
    if let Some(proxy) = cli_proxy.map(str::trim).filter(|proxy| !proxy.is_empty()) {
        return Ok(Some(proxy.to_string()));
    }
    config.network.proxy_url()
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
        .map(|config| Language::from_code(&config.language))
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
        ) => format!("[{prefix}] 构建完成: {client_zip}, {server_zip}"),
        (
            Language::EnUs,
            BuildProgress::Finished {
                client_zip,
                server_zip,
            },
        ) => format!("[{prefix}] completed: {client_zip}, {server_zip}"),
    }
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
                "[run] 复制{} overrides/debug 文件",
                run_side_label(language, *side)
            )
        }
        (Language::EnUs, RunProgress::CopyingOverrides { side }) => {
            format!(
                "[run] copying {} overrides/debug files",
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
        (
            Language::ZhCn,
            RunProgress::FileReady {
                completed,
                total,
                path,
            },
        ) => format!("[run] 文件就绪 {completed}/{total}: {path}"),
        (
            Language::EnUs,
            RunProgress::FileReady {
                completed,
                total,
                path,
            },
        ) => format!("[run] file ready {completed}/{total}: {path}"),
        (Language::ZhCn, RunProgress::ResolvingJava { major }) => {
            format!("[run] 查找 Java {major}")
        }
        (Language::EnUs, RunProgress::ResolvingJava { major }) => {
            format!("[run] resolving Java {major}")
        }
        (Language::ZhCn, RunProgress::Launching { command }) => {
            format!("[run] 启动进程: {command}")
        }
        (Language::EnUs, RunProgress::Launching { command }) => {
            format!("[run] launching process: {command}")
        }
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
    }
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

fn path_text_width(available_width: f32) -> f32 {
    non_negative_width(available_width - 76.0)
}

fn non_negative_width(width: f32) -> f32 {
    width.max(0.0)
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
    use crate::gui::mod_list::{
        GuiDependencyList, GuiDependencyType, GuiModEntry, ManifestList, ProjectCandidate,
        ProjectLookup, ProjectSearchInput,
    };

    #[test]
    fn path_text_width_never_goes_negative() {
        assert_eq!(super::path_text_width(46.4), 0.0);
        assert_eq!(super::path_text_width(160.0), 84.0);
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
        app.mod_list.list = Some(GuiDependencyList {
            entries: vec![test_entry("First", 10), test_entry("Second", 20)],
        });
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
