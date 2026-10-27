use std::ffi::OsString;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::config::TestRunConfig;
use crate::project::build::{BuildOptions, BuildTarget, ServerPlatform, ServerPlatformChoice};
use crate::project::run::{RunSide, SyncConfig, write_sync_config_for_project};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppLaunchMode {
    Gui,
    Cli,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuiCommand {
    Init,
    ModList,
    Overrides,
    Settings,
    Sync,
    Build,
    Check,
    RunClient,
    RunServer,
}

impl GuiCommand {
    pub fn cli_name(self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::ModList => "modList",
            Self::Overrides => "overrides",
            Self::Settings => "settings",
            Self::Sync => "sync",
            Self::Build => "build",
            Self::Check => "check",
            Self::RunClient => "runClient",
            Self::RunServer => "runServer",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::ModList => "Mod List",
            Self::Overrides => "Overrides",
            Self::Settings => "Settings",
            Self::Sync => "Sync",
            Self::Build => "build",
            Self::Check => "check",
            Self::RunClient => "runClient",
            Self::RunServer => "runServer",
        }
    }

    pub fn all() -> [Self; 8] {
        [
            Self::Init,
            Self::ModList,
            Self::Overrides,
            Self::Settings,
            Self::Build,
            Self::Check,
            Self::RunClient,
            Self::RunServer,
        ]
    }
}

/// Keeps the hidden server platform selection valid when the build target changes.
///
/// Client-only builds have no server platform field. Resetting a previous explicit choice at the
/// state transition prevents it from becoming a later validation error.
pub fn normalize_build_server_platform(
    target: BuildTarget,
    server_platform: ServerPlatformChoice,
) -> ServerPlatformChoice {
    match target {
        BuildTarget::Client => ServerPlatformChoice::Current,
        BuildTarget::Server | BuildTarget::Both => server_platform,
    }
}

/// Resolves a build output override from the project directory for an in-process GUI build.
///
/// A GUI command does not receive a child-process working directory, unlike the CLI. Relative
/// output paths must therefore be made project-relative before invoking the project build API.
/// Absolute output paths and all other build options are preserved.
pub fn normalize_build_options_for_project(
    project_dir: &Path,
    options: &BuildOptions,
) -> BuildOptions {
    let mut normalized = options.clone();
    normalized.output_dir_override = options.output_dir_override.as_deref().map(|output_dir| {
        if output_dir.is_absolute() {
            output_dir.to_path_buf()
        } else {
            project_dir.join(output_dir)
        }
    });
    normalized
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProxyConfig {
    pub enabled: bool,
    pub ip: String,
    pub port: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandForm {
    pub command: GuiCommand,
    pub use_custom_project_dir: bool,
    pub project_dir: PathBuf,
    pub pack_zip: PathBuf,
    pub use_custom_output_dir: bool,
    pub output_dir: PathBuf,
    /// Non-persistent archive selection for the current GUI session.
    pub build_target: BuildTarget,
    /// Non-persistent server archive platform selection for the current GUI session.
    pub build_server_platform: ServerPlatformChoice,
    pub lang: String,
    pub proxy: ProxyConfig,
    pub test_run: TestRunConfig,
    pub test_run_project_dir: Option<PathBuf>,
    pub client_sync_include_paths: String,
    pub server_sync_include_paths: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuiCommandSpec {
    pub current_dir: PathBuf,
    pub args: Vec<String>,
    pub envs: Vec<(String, String)>,
    pub task: GuiCommandTask,
    pub lang: Option<String>,
    pub proxy: Option<String>,
    pub run_test_config: Option<TestRunConfig>,
    pub run_test_config_project_dir: Option<PathBuf>,
    pub run_sync_config: Option<SyncConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuiCommandTask {
    Init { zip: PathBuf, output: PathBuf },
    Build { options: BuildOptions },
    Check,
    RunClient,
    RunServer,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum GuiCommandError {
    #[error("Project directory is required")]
    MissingProjectDir,
    #[error("CurseForge pack zip is required")]
    MissingPackZip,
    #[error("Output directory is required")]
    MissingOutputDir,
    #[error("Proxy IP is required")]
    MissingProxyIp,
    #[error("Proxy port is required")]
    MissingProxyPort,
    #[error("This is a GUI-only page")]
    GuiOnlyCommand,
    #[error("Offline player name is required for runClient")]
    MissingPlayerName,
    #[error("test-run.toml has not been loaded for the current project")]
    TestRunConfigNotLoaded,
    #[error("test-run.toml was loaded for '{loaded}', but the current project is '{current}'")]
    TestRunConfigProjectMismatch { loaded: String, current: String },
    #[error("Invalid test-run configuration: {0}")]
    InvalidTestRunConfig(String),
    #[error("Invalid run sync configuration: {0}")]
    InvalidRunSyncConfig(String),
    #[error("Failed to save run configuration: {0}")]
    SaveRunConfiguration(String),
    #[error("A server platform cannot be selected for a client-only build")]
    ServerPlatformForClientBuild,
}

impl Default for CommandForm {
    fn default() -> Self {
        Self {
            command: GuiCommand::Init,
            use_custom_project_dir: false,
            project_dir: PathBuf::new(),
            pack_zip: PathBuf::new(),
            use_custom_output_dir: false,
            output_dir: PathBuf::new(),
            build_target: BuildTarget::Both,
            build_server_platform: ServerPlatformChoice::Current,
            lang: String::new(),
            proxy: ProxyConfig::default(),
            test_run: TestRunConfig::default(),
            test_run_project_dir: None,
            client_sync_include_paths: String::new(),
            server_sync_include_paths: String::new(),
        }
    }
}

impl CommandForm {
    pub fn project_root(&self) -> Result<PathBuf, GuiCommandError> {
        if self.use_custom_project_dir {
            required_path(&self.project_dir, GuiCommandError::MissingProjectDir)
        } else {
            Ok(PathBuf::from("."))
        }
    }

    pub fn to_command_spec(&self) -> Result<GuiCommandSpec, GuiCommandError> {
        if matches!(
            self.command,
            GuiCommand::ModList | GuiCommand::Overrides | GuiCommand::Settings | GuiCommand::Sync
        ) {
            return Err(GuiCommandError::GuiOnlyCommand);
        }
        let project_dir = self.project_root()?;
        let mut args = Vec::new();
        push_optional_global(&mut args, "--lang", &self.lang);
        let proxy = if self.proxy.enabled {
            let ip = self.proxy.ip.trim();
            if ip.is_empty() {
                return Err(GuiCommandError::MissingProxyIp);
            }
            let port = self.proxy.port.trim();
            if port.is_empty() {
                return Err(GuiCommandError::MissingProxyPort);
            }
            let proxy = format!("http://{ip}:{port}");
            push_optional_global(&mut args, "--proxy", &proxy);
            Some(proxy)
        } else {
            None
        };
        args.push(self.command.cli_name().to_string());

        let task = match self.command {
            GuiCommand::Init => {
                let pack_zip = required_path(&self.pack_zip, GuiCommandError::MissingPackZip)?;
                args.push(path_arg(&pack_zip));
                let output_dir = if self.use_custom_output_dir {
                    let output_dir =
                        required_path(&self.output_dir, GuiCommandError::MissingOutputDir)?;
                    args.push("--output".to_string());
                    args.push(path_arg(&output_dir));
                    output_dir
                } else {
                    PathBuf::from(".")
                };
                GuiCommandTask::Init {
                    zip: pack_zip,
                    output: output_dir,
                }
            }
            GuiCommand::Build => {
                if self.build_target == BuildTarget::Client
                    && matches!(
                        self.build_server_platform,
                        ServerPlatformChoice::Explicit(_)
                    )
                {
                    return Err(GuiCommandError::ServerPlatformForClientBuild);
                }
                args.push("--target".to_string());
                args.push(build_target_arg(self.build_target).to_string());
                if self.build_target != BuildTarget::Client {
                    args.push("--server-platform".to_string());
                    args.push(server_platform_arg(self.build_server_platform).to_string());
                }
                if self.use_custom_output_dir {
                    let output_dir =
                        required_path(&self.output_dir, GuiCommandError::MissingOutputDir)?;
                    args.push("--output".to_string());
                    args.push(path_arg(&output_dir));
                }
                GuiCommandTask::Build {
                    options: BuildOptions {
                        target: self.build_target,
                        server_platform: self.build_server_platform,
                        output_dir_override: self
                            .use_custom_output_dir
                            .then(|| self.output_dir.clone()),
                    },
                }
            }
            GuiCommand::Check => GuiCommandTask::Check,
            GuiCommand::RunClient => GuiCommandTask::RunClient,
            GuiCommand::RunServer => GuiCommandTask::RunServer,
            GuiCommand::ModList
            | GuiCommand::Overrides
            | GuiCommand::Settings
            | GuiCommand::Sync => {
                unreachable!("GUI-only command returned")
            }
        };

        let is_run = matches!(self.command, GuiCommand::RunClient | GuiCommand::RunServer);
        let run_test_config_project_dir = if is_run {
            let loaded = self
                .test_run_project_dir
                .as_ref()
                .ok_or(GuiCommandError::TestRunConfigNotLoaded)?;
            if loaded != &project_dir {
                return Err(GuiCommandError::TestRunConfigProjectMismatch {
                    loaded: loaded.display().to_string(),
                    current: project_dir.display().to_string(),
                });
            }
            Some(loaded.clone())
        } else {
            None
        };
        let run_test_config = is_run.then(|| self.test_run.clone());
        if let Some(config) = &run_test_config {
            if self.command == GuiCommand::RunClient && config.player_name.trim().is_empty() {
                return Err(GuiCommandError::MissingPlayerName);
            }
            config
                .validate()
                .map_err(|error| GuiCommandError::InvalidTestRunConfig(error.to_string()))?;
        }

        let run_sync_config = match self.command {
            GuiCommand::RunClient => Some(sync_config_from_lines(&self.client_sync_include_paths)),
            GuiCommand::RunServer => Some(sync_config_from_lines(&self.server_sync_include_paths)),
            _ => None,
        };
        if let Some(config) = &run_sync_config {
            config
                .validate()
                .map_err(|error| GuiCommandError::InvalidRunSyncConfig(error.to_string()))?;
        }

        Ok(GuiCommandSpec {
            current_dir: project_dir,
            args,
            envs: Vec::new(),
            task,
            lang: (!self.lang.trim().is_empty()).then(|| self.lang.trim().to_string()),
            proxy,
            run_test_config,
            run_test_config_project_dir,
            run_sync_config,
        })
    }
}

/// CLI argument spelling for a typed GUI archive target.
pub fn build_target_arg(target: BuildTarget) -> &'static str {
    match target {
        BuildTarget::Client => "client",
        BuildTarget::Server => "server",
        BuildTarget::Both => "both",
    }
}

/// CLI argument spelling for a typed GUI server platform choice.
pub fn server_platform_arg(choice: ServerPlatformChoice) -> &'static str {
    match choice {
        ServerPlatformChoice::Current => "current",
        ServerPlatformChoice::Explicit(ServerPlatform::WindowsX86_64) => "windows-x86_64",
        ServerPlatformChoice::Explicit(ServerPlatform::LinuxX86_64) => "linux-x86_64",
        ServerPlatformChoice::Explicit(ServerPlatform::MacosAarch64) => "macos-aarch64",
    }
}

fn sync_config_from_lines(raw: &str) -> SyncConfig {
    let mut include_paths = raw
        .lines()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    include_paths.sort();
    include_paths.dedup();
    SyncConfig { include_paths }
}

impl GuiCommandSpec {
    pub fn display_command(&self) -> String {
        let mut parts = vec!["mcmpu".to_string()];
        for arg in &self.args {
            parts.push(shell_display_arg(arg));
        }
        parts.join(" ")
    }

    pub fn save_run_configuration(&self) -> Result<(), GuiCommandError> {
        let side = match self.task {
            GuiCommandTask::RunClient => RunSide::Client,
            GuiCommandTask::RunServer => RunSide::Server,
            _ => return Ok(()),
        };
        let test_run = self.run_test_config.as_ref().ok_or_else(|| {
            GuiCommandError::SaveRunConfiguration(
                "run command is missing test-run.toml configuration".to_string(),
            )
        })?;
        let loaded_project = self
            .run_test_config_project_dir
            .as_ref()
            .ok_or(GuiCommandError::TestRunConfigNotLoaded)?;
        if loaded_project != &self.current_dir {
            return Err(GuiCommandError::TestRunConfigProjectMismatch {
                loaded: loaded_project.display().to_string(),
                current: self.current_dir.display().to_string(),
            });
        }
        let sync = self.run_sync_config.as_ref().ok_or_else(|| {
            GuiCommandError::SaveRunConfiguration(
                "run command is missing .mcmpu-sync.toml configuration".to_string(),
            )
        })?;
        test_run
            .write_to_project(&self.current_dir)
            .map_err(|error| GuiCommandError::SaveRunConfiguration(error.to_string()))?;
        write_sync_config_for_project(&self.current_dir, side, sync)
            .map_err(|error| GuiCommandError::SaveRunConfiguration(error.to_string()))
    }
}

pub fn gui_launch_mode<I, S>(args: I) -> AppLaunchMode
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    launch_mode_from_arg_count(args.into_iter().count())
}

pub fn launch_mode_from_arg_count(count: usize) -> AppLaunchMode {
    if count <= 1 {
        AppLaunchMode::Gui
    } else {
        AppLaunchMode::Cli
    }
}

fn push_optional_global(args: &mut Vec<String>, name: &str, value: &str) {
    let value = value.trim();
    if !value.is_empty() {
        args.push(name.to_string());
        args.push(value.to_string());
    }
}

fn required_path(path: &Path, err: GuiCommandError) -> Result<PathBuf, GuiCommandError> {
    if path.as_os_str().is_empty() {
        Err(err)
    } else {
        Ok(path.to_path_buf())
    }
}

fn path_arg(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn shell_display_arg(arg: &str) -> String {
    if arg.contains(char::is_whitespace) {
        format!("\"{}\"", arg.replace('"', "\\\""))
    } else {
        arg.to_string()
    }
}
