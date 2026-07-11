use std::ffi::OsString;
use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppLaunchMode {
    Gui,
    Cli,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuiCommand {
    Init,
    ModList,
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
            Self::Settings => "Settings",
            Self::Sync => "Sync",
            Self::Build => "build",
            Self::Check => "check",
            Self::RunClient => "runClient",
            Self::RunServer => "runServer",
        }
    }

    pub fn all() -> [Self; 7] {
        [
            Self::Init,
            Self::ModList,
            Self::Settings,
            Self::Build,
            Self::Check,
            Self::RunClient,
            Self::RunServer,
        ]
    }
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
    pub lang: String,
    pub proxy: ProxyConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuiCommandSpec {
    pub current_dir: PathBuf,
    pub args: Vec<String>,
    pub envs: Vec<(String, String)>,
    pub task: GuiCommandTask,
    pub lang: Option<String>,
    pub proxy: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuiCommandTask {
    Init { zip: PathBuf, output: PathBuf },
    Build,
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
            lang: String::new(),
            proxy: ProxyConfig::default(),
        }
    }
}

impl CommandForm {
    pub fn to_command_spec(&self) -> Result<GuiCommandSpec, GuiCommandError> {
        if matches!(
            self.command,
            GuiCommand::ModList | GuiCommand::Settings | GuiCommand::Sync
        ) {
            return Err(GuiCommandError::GuiOnlyCommand);
        }
        let project_dir = if self.use_custom_project_dir {
            required_path(&self.project_dir, GuiCommandError::MissingProjectDir)?
        } else {
            PathBuf::from(".")
        };
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
            GuiCommand::Build => GuiCommandTask::Build,
            GuiCommand::Check => GuiCommandTask::Check,
            GuiCommand::RunClient => GuiCommandTask::RunClient,
            GuiCommand::RunServer => GuiCommandTask::RunServer,
            GuiCommand::ModList | GuiCommand::Settings | GuiCommand::Sync => {
                unreachable!("GUI-only command returned")
            }
        };

        Ok(GuiCommandSpec {
            current_dir: project_dir,
            args,
            envs: Vec::new(),
            task,
            lang: (!self.lang.trim().is_empty()).then(|| self.lang.trim().to_string()),
            proxy,
        })
    }
}

impl GuiCommandSpec {
    pub fn display_command(&self) -> String {
        let mut parts = vec!["mcmpu".to_string()];
        for arg in &self.args {
            parts.push(shell_display_arg(arg));
        }
        parts.join(" ")
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
