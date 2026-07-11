use std::env;
use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::curseforge::client::CurseForgeClient;
use crate::curseforge::resolver::ApiKeyResolver;
use crate::i18n::Language;
use crate::project::build::{BuildProgress, build_project_with_api_key_and_progress};
use crate::project::check::check_project_with_api_key_and_progress;
use crate::project::init::{
    InitProgress, initialize_from_zip_with_api_and_progress, initialize_from_zip_with_progress,
};
use crate::project::run::{RunProgress, RunSide, run_project_with_api_key_and_progress};

#[derive(Debug, Parser)]
#[command(
    name = "mcmpu",
    version,
    about = "Maintain CurseForge Minecraft modpack projects / 维护 CurseForge Minecraft 整合包项目"
)]
pub struct Cli {
    #[arg(long, global = true)]
    pub lang: Option<String>,
    #[arg(long, global = true)]
    pub curseforge_api_key: Option<String>,
    #[arg(long, global = true)]
    pub proxy: Option<String>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(
        about = "Initialize a project from a CurseForge modpack zip / 从 CurseForge 整合包 zip 初始化项目"
    )]
    Init {
        zip: PathBuf,
        #[arg(long, default_value = ".")]
        output: PathBuf,
    },
    #[command(about = "Build client and server packages / 构建客户端与服务端包")]
    Build,
    #[command(about = "Validate project configuration and dependencies / 校验项目配置与依赖")]
    Check,
    #[command(
        name = "runClient",
        alias = "run-client",
        about = "Prepare the client run directory / 准备客户端运行目录"
    )]
    RunClient,
    #[command(
        name = "runServer",
        alias = "run-server",
        about = "Prepare the server run directory / 准备服务端运行目录"
    )]
    RunServer,
    #[command(about = "Open the graphical command runner / 打开图形命令执行器")]
    Gui,
}

pub fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init { zip, output } => {
            run_init(zip, output, cli.lang, cli.curseforge_api_key, cli.proxy)
        }
        Command::Build => run_build(cli.curseforge_api_key, cli.proxy),
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
            eprintln!("[check] 完成: {client_zip}, {server_zip}");
        }
        (
            Language::EnUs,
            BuildProgress::Finished {
                client_zip,
                server_zip,
            },
        ) => {
            eprintln!("[check] completed: {client_zip}, {server_zip}");
        }
    }
}

fn run_build(cli_api_key: Option<String>, cli_proxy: Option<String>) -> anyhow::Result<()> {
    let language = project_language(".");
    let config = Config::from_path("config.toml")?;
    let api_key = resolve_api_key(cli_api_key, &config)?;
    let proxy = resolve_proxy(cli_proxy.as_deref(), &config)?;
    let outputs =
        build_project_with_api_key_and_progress(".", api_key, proxy.as_deref(), &mut |event| {
            log_build_progress(language, event);
        })?;
    println!(
        "{}: {}",
        language.build_client(),
        outputs.client_zip.display()
    );
    println!(
        "{}: {}",
        language.build_server(),
        outputs.server_zip.display()
    );
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
            eprintln!("[build] 构建完成: {client_zip}, {server_zip}");
        }
        (
            Language::EnUs,
            BuildProgress::Finished {
                client_zip,
                server_zip,
            },
        ) => {
            eprintln!("[build] build completed: {client_zip}, {server_zip}");
        }
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

fn run_project(
    side: RunSide,
    cli_api_key: Option<String>,
    cli_proxy: Option<String>,
) -> anyhow::Result<()> {
    let language = project_language(".");
    let config = Config::from_path("config.toml")?;
    let api_key = resolve_api_key(cli_api_key, &config)?;
    let proxy = resolve_proxy(cli_proxy.as_deref(), &config)?;
    let plan = run_project_with_api_key_and_progress(
        ".",
        api_key,
        proxy.as_deref(),
        side,
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
                "[run] 复制{} overrides/debug 文件",
                run_side_label(language, *side)
            );
        }
        (Language::EnUs, RunProgress::CopyingOverrides { side }) => {
            eprintln!(
                "[run] copying {} overrides/debug files",
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
        (
            Language::ZhCn,
            RunProgress::FileReady {
                completed,
                total,
                path,
            },
        ) => {
            eprintln!("[run] 文件就绪 {completed}/{total}: {path}");
        }
        (
            Language::EnUs,
            RunProgress::FileReady {
                completed,
                total,
                path,
            },
        ) => {
            eprintln!("[run] file ready {completed}/{total}: {path}");
        }
        (Language::ZhCn, RunProgress::ResolvingJava { major }) => {
            eprintln!("[run] 查找 Java {major}");
        }
        (Language::EnUs, RunProgress::ResolvingJava { major }) => {
            eprintln!("[run] resolving Java {major}");
        }
        (Language::ZhCn, RunProgress::Launching { command }) => {
            eprintln!("[run] 启动进程: {command}");
        }
        (Language::EnUs, RunProgress::Launching { command }) => {
            eprintln!("[run] launching process: {command}");
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
        .context("failed to resolve network proxy")
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
        .map(|config| Language::from_code(&config.language))
        .unwrap_or_else(|_| Language::from_code(&detect_language()))
}
