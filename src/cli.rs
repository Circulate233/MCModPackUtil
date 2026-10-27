use std::env;
use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};

use crate::config::Config;
use crate::curseforge::client::CurseForgeClient;
use crate::curseforge::resolver::ApiKeyResolver;
use crate::i18n::Language;
use crate::project::build::{
    BuildOptions, BuildProgress, BuildTarget, ServerPlatform, ServerPlatformChoice,
    build_project_with_api_key_and_options_and_progress,
};
use crate::project::check::check_project_with_api_key_and_progress;
use crate::project::init::{
    InitProgress, initialize_from_zip_with_api_and_progress, initialize_from_zip_with_progress,
};
use crate::project::run::{
    ProcessOutputStream, RunFileReadySource, RunProgress, RunSide, SharedConsoleInput,
    install_cli_ctrl_c_signal, run_project_with_api_key_and_progress_and_stop_signal,
};

#[derive(Debug, Parser)]
#[command(
    name = "mcmpu",
    version = crate::BUILD_VERSION,
    about = "Maintain CurseForge Minecraft modpack projects / 维护 CurseForge Minecraft 整合包项目",
    long_about = "Maintain CurseForge Minecraft modpack projects / 维护 CurseForge Minecraft 整合包项目\n\n\
Commands that read a project (build, check, runClient, runServer) work on the project in the \
current directory and expect config.toml there; init is the command that creates one.\n\
读取项目的命令（build、check、runClient、runServer）作用于当前目录的项目并要求存在 \
config.toml；init 才是创建项目的命令。"
)]
pub struct Cli {
    /// Output language of this run, for example zh-CN or en-US. Only `init` uses it today;
    /// every other command reads the language from config.toml, falling back to the system
    /// locale.
    /// 本次运行的输出语言，例如 zh-CN 或 en-US。目前仅 `init` 使用；其他命令从 config.toml
    /// 读取语言，缺失时回退到系统区域设置。
    #[arg(long, global = true, value_name = "LANG")]
    pub lang: Option<String>,
    /// CurseForge API key. Takes precedence over the MCMODPACKUTIL_CURSEFORGE_API_KEY,
    /// CURSEFORGE_API_KEY, and CF_API_KEY environment variables, and over a key embedded at
    /// build time.
    /// CurseForge API 密钥。优先于 MCMODPACKUTIL_CURSEFORGE_API_KEY、CURSEFORGE_API_KEY、
    /// CF_API_KEY 环境变量以及构建时内嵌的密钥。
    #[arg(long, global = true, value_name = "KEY")]
    pub curseforge_api_key: Option<String>,
    /// Network proxy URL such as http://127.0.0.1:7890. Takes precedence over the proxy
    /// configured in config.toml.
    /// 网络代理地址，例如 http://127.0.0.1:7890。优先于 config.toml 中配置的代理。
    #[arg(long, global = true, value_name = "URL")]
    pub proxy: Option<String>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(
        about = "Create a project from a CurseForge modpack zip / 从 CurseForge 整合包 zip 创建项目",
        long_about = "Create a project from a CurseForge modpack zip / 从 CurseForge 整合包 zip 创建项目\n\n\
Unpacks the modpack into a project: config.toml, test-run.toml, the dependency manifests \
(common.json, client.json, server.json, resourcepacks.json), and the overrides layers. This is \
the only command that creates a project; `--output` selects the target directory. Files the \
manifest does not assign to a side are written to common.json, and the report of those files is \
printed.\n\
将整合包解包为项目：config.toml、test-run.toml、依赖清单（common.json、client.json、\
server.json、resourcepacks.json）与 overrides 覆盖层。这是唯一会创建项目的命令；\
`--output` 指定目标目录。清单未标明端侧的文件会写入 common.json，并打印对应的报告。"
    )]
    Init {
        /// Path to the CurseForge modpack zip.
        /// CurseForge 整合包 zip 的路径。
        #[arg(value_name = "ZIP")]
        zip: PathBuf,
        /// Directory that receives the project.
        /// 接收项目的目录。
        #[arg(long, default_value = ".", value_name = "DIR")]
        output: PathBuf,
    },
    #[command(
        about = "Build client and/or server packages / 构建客户端和/或服务端包",
        long_about = "Build client and/or server packages / 构建客户端和/或服务端包\n\n\
Writes the selected distribution archives into the configured output directory, or into \
`--output` when given. Run this in the project directory.\n\
将选定的分发压缩包写入配置的输出目录，或使用 `--output` 指定的目录。请在项目目录中运行。"
    )]
    Build {
        /// Which packages to produce.
        /// 要生成的包。
        #[arg(long, value_enum, default_value_t = CliBuildTarget::Both, value_name = "TARGET")]
        target: CliBuildTarget,
        /// Server installer platform. Rejected together with `--target client`.
        /// 服务端安装器平台。与 `--target client` 同时使用会被拒绝。
        #[arg(long, value_enum, value_name = "PLATFORM")]
        server_platform: Option<CliServerPlatform>,
        /// Directory that receives the archives, overriding config.toml.
        /// 接收压缩包的目录，覆盖 config.toml 中的设置。
        #[arg(long, value_name = "DIR")]
        output: Option<PathBuf>,
    },
    #[command(
        about = "Validate project configuration and dependencies / 校验项目配置与依赖",
        long_about = "Validate project configuration and dependencies / 校验项目配置与依赖\n\n\
Resolves every dependency against CurseForge and reports the counts, without writing any \
archive. Run this in the project directory.\n\
向 CurseForge 解析全部依赖并报告数量，不写入任何压缩包。请在项目目录中运行。"
    )]
    Check,
    #[command(
        name = "runClient",
        alias = "run-client",
        about = "Start the client and supervise it until it exits / 启动客户端并监督其运行至退出",
        long_about = "Start the client and supervise it until it exits / 启动客户端并监督其运行至退出\n\n\
Prepares run/client (manifests, overrides and debug layers, loader installation, libraries, \
assets), launches Minecraft, and streams its console output. Console input is forwarded to the \
game, and Ctrl+C stops it. The command returns once the process has exited and the pending-sync \
scan has finished.\n\
准备 run/client（清单、覆盖层与 debug 层、加载器安装、库与资源），启动 Minecraft 并转发其\
控制台输出。控制台输入会转发给游戏，Ctrl+C 用于停止。进程退出且待同步扫描完成后命令才返回。"
    )]
    RunClient,
    #[command(
        name = "runServer",
        alias = "run-server",
        about = "Start the server and supervise it until it exits / 启动服务端并监督其运行至退出",
        long_about = "Start the server and supervise it until it exits / 启动服务端并监督其运行至退出\n\n\
Prepares run/server (manifests, overrides and debug layers, loader installation, libraries), \
launches the dedicated server, and streams its console output. Console input is forwarded to \
the server, and Ctrl+C stops it. On the first run the eula.txt and server.properties files are \
filled in so the server can start unattended.\n\
准备 run/server（清单、覆盖层与 debug 层、加载器安装、库），启动专用服务端并转发其控制台\
输出。控制台输入会转发给服务端，Ctrl+C 用于停止。首次运行时自动填写 eula.txt 与 \
server.properties，使服务端可无人值守启动。"
    )]
    RunServer,
    #[command(
        about = "Open the graphical interface / 打开图形界面",
        long_about = "Open the graphical interface / 打开图形界面\n\n\
Starts the desktop application, which covers every command plus the mod list, overrides tree, \
and sync review pages.\n\
启动桌面应用，覆盖全部命令，并提供 mod 列表、文件目录与同步审查页面。"
    )]
    Gui,
}

/// CLI-facing selection for the archive set created by `build`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CliBuildTarget {
    Client,
    Server,
    Both,
}

impl From<CliBuildTarget> for BuildTarget {
    fn from(value: CliBuildTarget) -> Self {
        match value {
            CliBuildTarget::Client => Self::Client,
            CliBuildTarget::Server => Self::Server,
            CliBuildTarget::Both => Self::Both,
        }
    }
}

/// CLI-facing server installer target. The absent value deliberately means `current`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CliServerPlatform {
    Current,
    #[value(name = "windows-x86_64")]
    WindowsX86_64,
    #[value(name = "linux-x86_64")]
    LinuxX86_64,
    #[value(name = "macos-aarch64")]
    MacosAarch64,
}

impl CliServerPlatform {
    fn into_choice(self) -> ServerPlatformChoice {
        match self {
            Self::Current => ServerPlatformChoice::Current,
            Self::WindowsX86_64 => ServerPlatformChoice::Explicit(ServerPlatform::WindowsX86_64),
            Self::LinuxX86_64 => ServerPlatformChoice::Explicit(ServerPlatform::LinuxX86_64),
            Self::MacosAarch64 => ServerPlatformChoice::Explicit(ServerPlatform::MacosAarch64),
        }
    }
}

fn build_options_from_cli(
    target: CliBuildTarget,
    server_platform: Option<CliServerPlatform>,
    output: Option<PathBuf>,
) -> anyhow::Result<BuildOptions> {
    if target == CliBuildTarget::Client && server_platform.is_some() {
        anyhow::bail!("--server-platform cannot be used with --target client");
    }
    Ok(BuildOptions {
        target: target.into(),
        server_platform: server_platform
            .map(CliServerPlatform::into_choice)
            .unwrap_or(ServerPlatformChoice::Current),
        output_dir_override: output,
    })
}

pub fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init { zip, output } => {
            run_init(zip, output, cli.lang, cli.curseforge_api_key, cli.proxy)
        }
        Command::Build {
            target,
            server_platform,
            output,
        } => run_build(
            cli.curseforge_api_key,
            cli.proxy,
            build_options_from_cli(target, server_platform, output)?,
        ),
        Command::Check => run_check(cli.curseforge_api_key, cli.proxy),
        Command::RunClient => run_project(RunSide::Client, cli.curseforge_api_key, cli.proxy),
        Command::RunServer => run_project(RunSide::Server, cli.curseforge_api_key, cli.proxy),
        Command::Gui => crate::gui::run_gui().map_err(anyhow::Error::from),
    }
}

fn run_init(
    zip: PathBuf,
    output: PathBuf,
    cli_lang: Option<String>,
    cli_api_key: Option<String>,
    cli_proxy: Option<String>,
) -> anyhow::Result<()> {
    let language = cli_lang.unwrap_or_else(detect_language);
    let messages = Language::from_code(&language);
    let env_api_key = ApiKeyResolver::env_key();
    let builtin_api_key = ApiKeyResolver::builtin_key();
    match ApiKeyResolver::resolve(
        cli_api_key.as_deref(),
        env_api_key.as_deref(),
        builtin_api_key.as_deref().unwrap_or(""),
    ) {
        Ok(api_key) => {
            let api = CurseForgeClient::with_proxy(api_key, cli_proxy.as_deref())?;
            initialize_from_zip_with_api_and_progress(zip, output, language, &api, &mut |event| {
                log_init_progress(messages, event)
            })?;
        }
        Err(_) => initialize_from_zip_with_progress(zip, output, language, &mut |event| {
            log_init_progress(messages, event)
        })?,
    }
    println!("{}", messages.init_completed());
    Ok(())
}

fn log_init_progress(language: Language, event: &InitProgress) {
    match (language, event) {
        (Language::ZhCn, InitProgress::OpeningZip { path }) => {
            eprintln!("[init] 打开整合包: {path}");
        }
        (Language::EnUs, InitProgress::OpeningZip { path }) => {
            eprintln!("[init] opening pack zip: {path}");
        }
        (Language::ZhCn, InitProgress::ReadingManifest) => {
            eprintln!("[init] 读取 manifest.json");
        }
        (Language::EnUs, InitProgress::ReadingManifest) => {
            eprintln!("[init] reading manifest.json");
        }
        (
            Language::ZhCn,
            InitProgress::ManifestLoaded {
                name,
                version,
                file_count,
            },
        ) => {
            eprintln!("[init] 已读取: {name} {version}, CurseForge 条目 {file_count} 个");
        }
        (
            Language::EnUs,
            InitProgress::ManifestLoaded {
                name,
                version,
                file_count,
            },
        ) => {
            eprintln!("[init] loaded: {name} {version}, {file_count} CurseForge entries");
        }
        (
            Language::ZhCn,
            InitProgress::ResolvedFile {
                completed,
                total,
                project_id,
                file_id,
            },
        ) => {
            eprintln!("[init] 已解析 {completed}/{total}: {project_id}/{file_id}");
        }
        (
            Language::EnUs,
            InitProgress::ResolvedFile {
                completed,
                total,
                project_id,
                file_id,
            },
        ) => {
            eprintln!("[init] resolved {completed}/{total}: {project_id}/{file_id}");
        }
        (Language::ZhCn, InitProgress::WritingProjectFiles { output_dir }) => {
            eprintln!("[init] 写入项目文件: {output_dir}");
        }
        (Language::EnUs, InitProgress::WritingProjectFiles { output_dir }) => {
            eprintln!("[init] writing project files: {output_dir}");
        }
        (Language::ZhCn, InitProgress::ExtractingOverrides { overrides_dir }) => {
            eprintln!("[init] 导入 overrides: {overrides_dir}");
        }
        (Language::EnUs, InitProgress::ExtractingOverrides { overrides_dir }) => {
            eprintln!("[init] extracting overrides: {overrides_dir}");
        }
        (Language::ZhCn, InitProgress::Finished { output_dir }) => {
            eprintln!("[init] 初始化文件已写入: {output_dir}");
        }
        (Language::EnUs, InitProgress::Finished { output_dir }) => {
            eprintln!("[init] project initialized at: {output_dir}");
        }
    }
}

fn run_check(cli_api_key: Option<String>, cli_proxy: Option<String>) -> anyhow::Result<()> {
    let language = project_language(".");
    let config = Config::from_path("config.toml")?;
    let api_key = resolve_api_key(cli_api_key, &config)?;
    let proxy = resolve_proxy(cli_proxy.as_deref(), &config)?;
    let report =
        check_project_with_api_key_and_progress(".", api_key, proxy.as_deref(), &mut |event| {
            log_check_progress(language, event);
        })?;
    println!(
        "{}: {} client entries, {} server downloads",
        language.check_passed(),
        report.client_manifest_entries,
        report.server_downloads
    );
    Ok(())
}

fn log_check_progress(language: Language, event: &BuildProgress) {
    match (language, event) {
        (Language::ZhCn, BuildProgress::ReadingProject { project_dir }) => {
            eprintln!("[check] 读取项目: {project_dir}");
        }
        (Language::EnUs, BuildProgress::ReadingProject { project_dir }) => {
            eprintln!("[check] reading project: {project_dir}");
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
            eprintln!(
                "[check] 已读取依赖清单: common={common}, client={client}, server={server}, resourcepacks={resourcepacks}"
            );
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
            eprintln!(
                "[check] dependency lists loaded: common={common}, client={client}, server={server}, resourcepacks={resourcepacks}"
            );
        }
        (Language::ZhCn, BuildProgress::CleanroomSupplementsApplied { added }) => {
            eprintln!("[check] Cleanroom 隐式依赖补充: {added} 个");
        }
        (Language::EnUs, BuildProgress::CleanroomSupplementsApplied { added }) => {
            eprintln!("[check] Cleanroom implicit dependencies added: {added}");
        }
        (Language::ZhCn, BuildProgress::ResolvingDependencies { total }) => {
            eprintln!("[check] 开始并发解析 CurseForge 依赖: {total} 个");
        }
        (Language::EnUs, BuildProgress::ResolvingDependencies { total }) => {
            eprintln!("[check] resolving CurseForge dependencies concurrently: {total}");
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
        ) => {
            eprintln!(
                "[check] 已解析 {completed}/{total}: {}",
                dependency_label(*project_id, *file_id, slug.as_deref())
            );
        }
        (
            Language::EnUs,
            BuildProgress::ResolvedDependency {
                completed,
                total,
                project_id,
                file_id,
                slug,
            },
        ) => {
            eprintln!(
                "[check] resolved {completed}/{total}: {}",
                dependency_label(*project_id, *file_id, slug.as_deref())
            );
        }
        (Language::ZhCn, BuildProgress::PreparingServerInstallerAssets) => {
            eprintln!("[check] 准备服务端安装器资产");
        }
        (Language::EnUs, BuildProgress::PreparingServerInstallerAssets) => {
            eprintln!("[check] preparing server installer assets");
        }
        (Language::ZhCn, BuildProgress::ServerInstallerAssetsReady) => {
            eprintln!("[check] 服务端安装器资产准备完成");
        }
        (Language::EnUs, BuildProgress::ServerInstallerAssetsReady) => {
            eprintln!("[check] server installer assets ready");
        }
        (Language::ZhCn, BuildProgress::PreparingOutput { output_dir }) => {
            eprintln!("[check] 准备输出目录: {output_dir}");
        }
        (Language::EnUs, BuildProgress::PreparingOutput { output_dir }) => {
            eprintln!("[check] preparing output directory: {output_dir}");
        }
        (Language::ZhCn, BuildProgress::WritingClientZip { path }) => {
            eprintln!("[check] 写入客户端包: {path}");
        }
        (Language::EnUs, BuildProgress::WritingClientZip { path }) => {
            eprintln!("[check] writing client package: {path}");
        }
        (Language::ZhCn, BuildProgress::WritingServerZip { path }) => {
            eprintln!("[check] 写入服务端包: {path}");
        }
        (Language::EnUs, BuildProgress::WritingServerZip { path }) => {
            eprintln!("[check] writing server package: {path}");
        }
        (
            Language::ZhCn,
            BuildProgress::Finished {
                client_zip,
                server_zip,
            },
        ) => {
            eprintln!(
                "[check] 完成: {}",
                finished_build_artifacts(language, client_zip, server_zip)
            );
        }
        (
            Language::EnUs,
            BuildProgress::Finished {
                client_zip,
                server_zip,
            },
        ) => {
            eprintln!(
                "[check] completed: {}",
                finished_build_artifacts(language, client_zip, server_zip)
            );
        }
    }
}

fn run_build(
    cli_api_key: Option<String>,
    cli_proxy: Option<String>,
    options: BuildOptions,
) -> anyhow::Result<()> {
    let language = project_language(".");
    let config = Config::from_path("config.toml")?;
    let api_key = resolve_api_key(cli_api_key, &config)?;
    let proxy = resolve_proxy(cli_proxy.as_deref(), &config)?;
    let outputs = build_project_with_api_key_and_options_and_progress(
        ".",
        api_key,
        proxy.as_deref(),
        options,
        &mut |event| {
            log_build_progress(language, event);
        },
    )?;
    if let Some(client_zip) = outputs.client_zip {
        println!("{}: {}", language.build_client(), client_zip.display());
    }
    if let Some(server_zip) = outputs.server_zip {
        println!("{}: {}", language.build_server(), server_zip.display());
    }
    Ok(())
}

fn log_build_progress(language: Language, event: &BuildProgress) {
    match (language, event) {
        (Language::ZhCn, BuildProgress::ReadingProject { project_dir }) => {
            eprintln!("[build] 读取项目: {project_dir}");
        }
        (Language::EnUs, BuildProgress::ReadingProject { project_dir }) => {
            eprintln!("[build] reading project: {project_dir}");
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
            eprintln!(
                "[build] 已读取依赖清单: common={common}, client={client}, server={server}, resourcepacks={resourcepacks}"
            );
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
            eprintln!(
                "[build] dependency lists loaded: common={common}, client={client}, server={server}, resourcepacks={resourcepacks}"
            );
        }
        (Language::ZhCn, BuildProgress::CleanroomSupplementsApplied { added }) => {
            eprintln!("[build] Cleanroom 隐式依赖补充: {added} 个");
        }
        (Language::EnUs, BuildProgress::CleanroomSupplementsApplied { added }) => {
            eprintln!("[build] Cleanroom implicit dependencies added: {added}");
        }
        (Language::ZhCn, BuildProgress::ResolvingDependencies { total }) => {
            eprintln!("[build] 开始并发解析 CurseForge 依赖: {total} 个");
        }
        (Language::EnUs, BuildProgress::ResolvingDependencies { total }) => {
            eprintln!("[build] resolving CurseForge dependencies concurrently: {total}");
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
        ) => {
            eprintln!(
                "[build] 已解析 {completed}/{total}: {}",
                dependency_label(*project_id, *file_id, slug.as_deref())
            );
        }
        (
            Language::EnUs,
            BuildProgress::ResolvedDependency {
                completed,
                total,
                project_id,
                file_id,
                slug,
            },
        ) => {
            eprintln!(
                "[build] resolved {completed}/{total}: {}",
                dependency_label(*project_id, *file_id, slug.as_deref())
            );
        }
        (Language::ZhCn, BuildProgress::PreparingServerInstallerAssets) => {
            eprintln!("[build] 准备服务端安装器资产");
        }
        (Language::EnUs, BuildProgress::PreparingServerInstallerAssets) => {
            eprintln!("[build] preparing server installer assets");
        }
        (Language::ZhCn, BuildProgress::ServerInstallerAssetsReady) => {
            eprintln!("[build] 服务端安装器资产准备完成");
        }
        (Language::EnUs, BuildProgress::ServerInstallerAssetsReady) => {
            eprintln!("[build] server installer assets ready");
        }
        (Language::ZhCn, BuildProgress::PreparingOutput { output_dir }) => {
            eprintln!("[build] 准备输出目录: {output_dir}");
        }
        (Language::EnUs, BuildProgress::PreparingOutput { output_dir }) => {
            eprintln!("[build] preparing output directory: {output_dir}");
        }
        (Language::ZhCn, BuildProgress::WritingClientZip { path }) => {
            eprintln!("[build] 写入客户端包: {path}");
        }
        (Language::EnUs, BuildProgress::WritingClientZip { path }) => {
            eprintln!("[build] writing client package: {path}");
        }
        (Language::ZhCn, BuildProgress::WritingServerZip { path }) => {
            eprintln!("[build] 写入服务端包: {path}");
        }
        (Language::EnUs, BuildProgress::WritingServerZip { path }) => {
            eprintln!("[build] writing server package: {path}");
        }
        (
            Language::ZhCn,
            BuildProgress::Finished {
                client_zip,
                server_zip,
            },
        ) => {
            eprintln!(
                "[build] 构建完成: {}",
                finished_build_artifacts(language, client_zip, server_zip)
            );
        }
        (
            Language::EnUs,
            BuildProgress::Finished {
                client_zip,
                server_zip,
            },
        ) => {
            eprintln!(
                "[build] build completed: {}",
                finished_build_artifacts(language, client_zip, server_zip)
            );
        }
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

fn run_project(
    side: RunSide,
    cli_api_key: Option<String>,
    cli_proxy: Option<String>,
) -> anyhow::Result<()> {
    let language = project_language(".");
    let config = Config::from_path("config.toml")?;
    let api_key = resolve_api_key(cli_api_key, &config)?;
    let proxy = resolve_proxy(cli_proxy.as_deref(), &config)?;
    let stop_signal = install_cli_ctrl_c_signal()?;
    let console = SharedConsoleInput::default();
    forward_terminal_input(&console);
    let plan = run_project_with_api_key_and_progress_and_stop_signal(
        ".",
        api_key,
        proxy.as_deref(),
        side,
        &stop_signal,
        &console,
        &mut |event| log_run_progress(language, event),
    )?;
    let label = match side {
        RunSide::Client => language.build_client(),
        RunSide::Server => language.build_server(),
    };
    println!("{}: {}", label, plan.run_dir.display());
    Ok(())
}

fn log_run_progress(language: Language, event: &RunProgress) {
    match (language, event) {
        (Language::ZhCn, RunProgress::ResolvingProject { side, project_dir }) => {
            eprintln!(
                "[run] 解析{}项目: {}",
                run_side_label(language, *side),
                project_dir.display()
            );
        }
        (Language::EnUs, RunProgress::ResolvingProject { side, project_dir }) => {
            eprintln!(
                "[run] resolving {} project: {}",
                run_side_label(language, *side),
                project_dir.display()
            );
        }
        (Language::ZhCn, RunProgress::RebuildingRunDirectory { side, run_dir }) => {
            eprintln!(
                "[run] 重建{}运行目录: {}",
                run_side_label(language, *side),
                run_dir.display()
            );
        }
        (Language::EnUs, RunProgress::RebuildingRunDirectory { side, run_dir }) => {
            eprintln!(
                "[run] rebuilding {} run directory: {}",
                run_side_label(language, *side),
                run_dir.display()
            );
        }
        (Language::ZhCn, RunProgress::CopyingOverrides { side }) => {
            eprintln!(
                "[run] 复制{} overrides 覆盖层",
                run_side_label(language, *side)
            );
        }
        (Language::EnUs, RunProgress::CopyingOverrides { side }) => {
            eprintln!(
                "[run] copying {} overrides layers",
                run_side_label(language, *side)
            );
        }
        (Language::ZhCn, RunProgress::CopyingDebug { side }) => {
            eprintln!(
                "[run] 复制{}仅运行时 debug 覆盖层",
                run_side_label(language, *side)
            );
        }
        (Language::EnUs, RunProgress::CopyingDebug { side }) => {
            eprintln!(
                "[run] copying {} runtime-only debug layers",
                run_side_label(language, *side)
            );
        }
        (Language::ZhCn, RunProgress::EnsuringSyncConfig { side, path }) => {
            eprintln!(
                "[run] 准备{}同步配置: {}",
                run_side_label(language, *side),
                path.display()
            );
        }
        (Language::EnUs, RunProgress::EnsuringSyncConfig { side, path }) => {
            eprintln!(
                "[run] ensuring {} sync config: {}",
                run_side_label(language, *side),
                path.display()
            );
        }
        (Language::ZhCn, RunProgress::RunManifestWritten { file_count }) => {
            eprintln!("[run] 运行清单已写入: {file_count} 个文件");
        }
        (Language::EnUs, RunProgress::RunManifestWritten { file_count }) => {
            eprintln!("[run] run manifest written: {file_count} files");
        }
        (Language::ZhCn, RunProgress::DownloadingFiles { total }) => {
            eprintln!("[run] 检查/下载运行文件: {total} 个");
        }
        (Language::EnUs, RunProgress::DownloadingFiles { total }) => {
            eprintln!("[run] checking/downloading run files: {total}");
        }
        (Language::ZhCn, RunProgress::VerifyingFiles { total }) => {
            eprintln!("[run] 校验本地运行文件与缓存: {total} 个");
        }
        (Language::EnUs, RunProgress::VerifyingFiles { total }) => {
            eprintln!("[run] verifying local run files and cache: {total}");
        }
        (Language::ZhCn, RunProgress::FileVerificationProgress { checked, total }) => {
            eprintln!("[run] 本地校验 {checked}/{total}");
        }
        (Language::EnUs, RunProgress::FileVerificationProgress { checked, total }) => {
            eprintln!("[run] local verification {checked}/{total}");
        }
        (
            Language::ZhCn,
            RunProgress::RunFilesChecked {
                reused,
                cached,
                downloading,
                download_bytes,
            },
        ) => eprintln!(
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
        ) => eprintln!(
            "[run] local verification complete: {reused} reused, {cached} cached, {downloading} to download ({})",
            format_run_bytes(*download_bytes)
        ),
        (Language::ZhCn, RunProgress::DownloadBatchStarted { total, total_bytes }) => eprintln!(
            "[run] 已提交并发下载: {total} 个文件，共 {}",
            format_run_bytes(*total_bytes)
        ),
        (Language::EnUs, RunProgress::DownloadBatchStarted { total, total_bytes }) => eprintln!(
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
        ) => eprintln!("[run] 开始下载 {started}/{total}: {path}"),
        (
            Language::EnUs,
            RunProgress::FileDownloadStarted {
                started,
                total,
                path,
            },
        ) => eprintln!("[run] download started {started}/{total}: {path}"),
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
        ) => eprintln!(
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
        ) => eprintln!(
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
        ) => {
            eprintln!(
                "[run] 文件就绪 {completed}/{total} ({}): {path}",
                run_file_source_label(Language::ZhCn, *source)
            );
        }
        (
            Language::EnUs,
            RunProgress::FileReady {
                completed,
                total,
                path,
                source,
            },
        ) => {
            eprintln!(
                "[run] file ready {completed}/{total} ({}): {path}",
                run_file_source_label(Language::EnUs, *source)
            );
        }
        (Language::ZhCn, RunProgress::ResolvingJava { major }) => {
            eprintln!("[run] 查找 Java {major}");
        }
        (Language::EnUs, RunProgress::ResolvingJava { major }) => {
            eprintln!("[run] resolving Java {major}");
        }
        (Language::ZhCn, RunProgress::JavaResolved { path }) => {
            eprintln!("[run] Java 已找到: {}", path.display());
        }
        (Language::EnUs, RunProgress::JavaResolved { path }) => {
            eprintln!("[run] Java resolved: {}", path.display());
        }
        (Language::ZhCn, RunProgress::DownloadingLoaderInstaller { side, url, target }) => {
            eprintln!(
                "[run] 下载{} Loader 安装器: {url} -> {}",
                run_side_label(language, *side),
                target.display()
            )
        }
        (Language::EnUs, RunProgress::DownloadingLoaderInstaller { side, url, target }) => {
            eprintln!(
                "[run] downloading {} loader installer: {url} -> {}",
                run_side_label(language, *side),
                target.display()
            )
        }
        (Language::ZhCn, RunProgress::LoaderInstallerDownloaded { side, target }) => eprintln!(
            "[run] {} Loader 安装器已下载并校验: {}",
            run_side_label(language, *side),
            target.display()
        ),
        (Language::EnUs, RunProgress::LoaderInstallerDownloaded { side, target }) => eprintln!(
            "[run] {} loader installer downloaded and verified: {}",
            run_side_label(language, *side),
            target.display()
        ),
        (Language::ZhCn, RunProgress::RunningLoaderInstaller { side, loader }) => eprintln!(
            "[run] 运行{} {} Loader 安装器",
            run_side_label(language, *side),
            loader_label(loader)
        ),
        (Language::EnUs, RunProgress::RunningLoaderInstaller { side, loader }) => eprintln!(
            "[run] running {} {} loader installer",
            run_side_label(language, *side),
            loader_label(loader)
        ),
        (language, RunProgress::LoaderInstallerOutput { stream, line, .. }) => {
            eprintln!("{}", render_runtime_install_output(language, *stream, line));
        }
        (Language::ZhCn, RunProgress::InstallingLoaderLibraries { side, total }) => eprintln!(
            "[run] 并行下载{} Loader 库: {total} 个",
            run_side_label(language, *side)
        ),
        (Language::EnUs, RunProgress::InstallingLoaderLibraries { side, total }) => eprintln!(
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
        ) => eprintln!(
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
        ) => eprintln!(
            "[run] {} loader library {completed}/{total}: {path}",
            run_side_label(language, *side)
        ),
        (Language::ZhCn, RunProgress::PreparingRuntimeArtifacts { side, total }) => eprintln!(
            "[run] 准备{}游戏库与资源: {total} 个",
            run_side_label(language, *side)
        ),
        (Language::EnUs, RunProgress::PreparingRuntimeArtifacts { side, total }) => eprintln!(
            "[run] preparing {} libraries and assets: {total}",
            run_side_label(language, *side)
        ),
        (Language::ZhCn, RunProgress::RuntimeArtifactTotalUpdated { side, total }) => eprintln!(
            "[run] {}游戏库与资源总数: {total}",
            run_side_label(language, *side)
        ),
        (Language::EnUs, RunProgress::RuntimeArtifactTotalUpdated { side, total }) => eprintln!(
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
        ) => eprintln!(
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
        ) => eprintln!(
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
        ) => eprintln!(
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
        ) => eprintln!(
            "[run] downloading game files: {}/{} with {active_requests} active request(s)",
            format_run_bytes(*transferred_bytes),
            format_run_bytes(*total_bytes)
        ),
        (Language::ZhCn, RunProgress::Launching { command }) => {
            eprintln!("[run] 启动进程: {command}");
        }
        (Language::EnUs, RunProgress::Launching { command }) => {
            eprintln!("[run] launching process: {command}");
        }
        (_, RunProgress::MinecraftOutput { stream, line, .. }) => {
            eprintln!("[minecraft {}] {line}", process_stream_label(*stream));
        }
        (Language::ZhCn, RunProgress::ServerRunConfigurationWritten { changed }) => {
            eprintln!("[run] 已准备服务端运行配置: {}", changed.join(", "));
        }
        (Language::EnUs, RunProgress::ServerRunConfigurationWritten { changed }) => {
            eprintln!(
                "[run] prepared server run configuration: {}",
                changed.join(", ")
            );
        }
        (Language::ZhCn, RunProgress::PendingSyncWritten { path, entries }) => {
            eprintln!(
                "[run] 已写入待处理同步清单: {} ({} 个变更)。请在 GUI 的同步处理窗口确认。",
                path.display(),
                entries
            );
        }
        (Language::EnUs, RunProgress::PendingSyncWritten { path, entries }) => {
            eprintln!(
                "[run] pending sync manifest written: {} ({} changes). Review it in the GUI Sync window.",
                path.display(),
                entries
            );
        }
        (Language::ZhCn, RunProgress::PendingSyncFailed { path, error }) => {
            eprintln!(
                "[run] 待处理同步扫描失败，原清单未被替换: {}: {error}",
                path.display()
            );
        }
        (Language::EnUs, RunProgress::PendingSyncFailed { path, error }) => {
            eprintln!(
                "[run] pending sync scan failed; the previous manifest was not replaced: {}: {error}",
                path.display()
            );
        }
    }
}

/// Forwards lines typed into this terminal to the supervised Minecraft process.
///
/// The child gets a pipe for stdin, so without this the server console would be read-only. The
/// reader ends on EOF, which is the normal case when stdin is redirected or closed.
fn forward_terminal_input(console: &SharedConsoleInput) {
    let console = console.clone();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            match std::io::BufRead::read_line(&mut stdin.lock(), &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => console.submit(&line),
            }
        }
    });
}

fn loader_label(loader: &crate::config::LoaderKind) -> &'static str {
    match loader {
        crate::config::LoaderKind::Forge => "Forge",
        crate::config::LoaderKind::Fabric => "Fabric",
        crate::config::LoaderKind::NeoForge => "NeoForge",
        crate::config::LoaderKind::Cleanroom => "Cleanroom",
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

fn run_side_label(language: Language, side: RunSide) -> &'static str {
    match (language, side) {
        (Language::ZhCn, RunSide::Client) => "客户端",
        (Language::ZhCn, RunSide::Server) => "服务端",
        (Language::EnUs, RunSide::Client) => "client",
        (Language::EnUs, RunSide::Server) => "server",
    }
}

fn resolve_api_key(cli_api_key: Option<String>, config: &Config) -> anyhow::Result<String> {
    let env_api_key = ApiKeyResolver::env_key();
    let builtin_api_key = ApiKeyResolver::builtin_key();
    let _ = config;
    ApiKeyResolver::resolve(
        cli_api_key.as_deref(),
        env_api_key.as_deref(),
        builtin_api_key.as_deref().unwrap_or(""),
    )
    .context("failed to resolve CurseForge API key")
}

fn resolve_proxy(cli_proxy: Option<&str>, config: &Config) -> anyhow::Result<Option<String>> {
    if let Some(proxy) = cli_proxy.map(str::trim).filter(|proxy| !proxy.is_empty()) {
        return Ok(Some(proxy.to_string()));
    }
    config
        .network
        .proxy_url()
        .context("failed to resolve configured network proxy")
}

fn detect_language() -> String {
    let raw = ["LC_ALL", "LC_MESSAGES", "LANGUAGE", "LANG"]
        .into_iter()
        .find_map(|key| env::var(key).ok())
        .filter(|value| !value.trim().is_empty())
        .or_else(sys_locale::get_locale)
        .unwrap_or_default();
    if raw.to_ascii_lowercase().starts_with("zh") {
        "zh-CN".to_string()
    } else {
        "en-US".to_string()
    }
}

fn project_language(project_dir: &str) -> Language {
    Config::from_path(PathBuf::from(project_dir).join("config.toml"))
        .map(|config| {
            if config.language.trim().is_empty() {
                Language::from_code(&detect_language())
            } else {
                Language::from_code(&config.language)
            }
        })
        .unwrap_or_else(|_| Language::from_code(&detect_language()))
}

#[cfg(test)]
mod tests {
    use crate::config::Config;
    use crate::i18n::Language;
    use crate::project::artifact_install::ArtifactInstallSource;
    use crate::project::runtime_install::RuntimeInstallStream;

    #[test]
    fn runtime_artifact_sources_have_distinct_cli_labels() {
        assert_eq!(
            super::artifact_install_source_label(Language::ZhCn, ArtifactInstallSource::TargetHit),
            "运行目录命中"
        );
        assert_eq!(
            super::artifact_install_source_label(Language::ZhCn, ArtifactInstallSource::CacheHit),
            "缓存命中"
        );
        assert_eq!(
            super::artifact_install_source_label(Language::ZhCn, ArtifactInstallSource::Downloaded),
            "已下载"
        );
        assert_eq!(
            super::artifact_install_source_label(Language::EnUs, ArtifactInstallSource::TargetHit),
            "target reused"
        );
    }

    #[test]
    fn cli_localizes_loader_and_metadata_cache_events_but_preserves_installer_output() {
        let loader = super::render_runtime_install_output(
            Language::ZhCn,
            RuntimeInstallStream::Stdout,
            "[cache] reused loader installer: C:\\cache\\loader.jar",
        );
        let metadata = super::render_runtime_install_output(
            Language::EnUs,
            RuntimeInstallStream::Stdout,
            "[cache] reused Mojang version manifest: /cache/version_manifest_v2.json",
        );
        let ordinary = super::render_runtime_install_output(
            Language::ZhCn,
            RuntimeInstallStream::Stderr,
            "installer warning",
        );

        assert_eq!(
            loader,
            "[run] 复用运行安装缓存（Loader 安装器）：C:\\cache\\loader.jar"
        );
        assert_eq!(
            metadata,
            "[run] runtime install cache reused (Mojang version manifest): /cache/version_manifest_v2.json"
        );
        assert_eq!(ordinary, "[installer stderr] installer warning");
    }

    #[test]
    fn proxy_resolution_prefers_cli_over_config_and_rejects_incomplete_config() {
        let project = tempfile::tempdir().unwrap();
        let mut config =
            Config::from_path_for_settings(project.path().join("config.toml")).unwrap();
        config.network.proxy_ip = "127.0.0.1".to_string();
        config.network.proxy_port = 7897;

        assert_eq!(
            super::resolve_proxy(None, &config).unwrap().as_deref(),
            Some("http://127.0.0.1:7897")
        );
        assert_eq!(
            super::resolve_proxy(Some("http://127.0.0.1:8899"), &config)
                .unwrap()
                .as_deref(),
            Some("http://127.0.0.1:8899")
        );

        config.network.proxy_port = 0;
        assert!(super::resolve_proxy(None, &config).is_err());
    }
}
