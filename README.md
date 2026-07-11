# MCModPackUtil / mcmpu

## English

`mcmpu` is a Rust command-line tool for maintaining CurseForge Minecraft modpack projects.

It can initialize a project from a packaged CurseForge modpack, split dependencies into client, server, common, and resource-pack lists, then build both a CurseForge client import zip and a runnable server zip.

### Features

- Import CurseForge modpack zips with `manifest.json` and `overrides/`.
- Resolve CurseForge project/file metadata with a CLI, environment, or official-build API key.
- Maintain `config.toml`, `common.json`, `client.json`, `server.json`, and `resourcepacks.json`.
- Build client packages with `manifest.json`, `modlist.html`, and merged overrides.
- Build server packages with native `start.bat` and `start.sh` scripts that download files from the CurseForge CDN with an `x-api-key` header.
- Prepare and launch local `run/client` and `run/server` directories with CurseMaven-managed files.
- Support Forge, Fabric, NeoForge, and Cleanroom 1.12.2 server installation plans.
- Minify JSON and losslessly optimize PNG files during package creation.
- Provide Chinese and English CLI text.

### Commands

```bash
mcmpu init path/to/modpack.zip
mcmpu check
mcmpu build
mcmpu runClient
mcmpu runServer
mcmpu gui
```

Double-clicking `mcmpu.exe` without command-line arguments opens the graphical command runner. The GUI exposes the same commands as the CLI, shows per-command parameter fields, keeps CurseForge API keys out of the interface, and streams command output into the log panel. A static visual preview is available at `docs/gui-mockup.html`.

`runClient` and `runServer` also accept the aliases `run-client` and `run-server`. They read `config.toml`, resolve the CurseForge API key and proxy with the same precedence as `build` and `check`, rebuild generated run folders, download runtime files through CurseMaven, launch Java, and write a pending sync manifest for GUI review instead of changing `overrides/` automatically.

Release archives are named with version and platform, for example `mcmpu-0.1.0-windows-x86_64.zip`. Inside the archive the executable name is stable: `mcmpu.exe` on Windows and `mcmpu` on Unix-like systems. The Windows archive also includes `mcmpu.bat`; add the extracted directory to `PATH` if you want to run `mcmpu init ...` without `.\` or `.exe`.

CurseForge API key lookup order:

1. `--curseforge-api-key <key>`
2. `MCMODPACKUTIL_CURSEFORGE_API_KEY`, `CURSEFORGE_API_KEY`, or `CF_API_KEY`
3. Official release builds may embed a CurseForge key at compile time.

`config.toml` does not store CurseForge API keys. Existing `[curseforge]` or `curseforge_api_key` entries are rejected with a migration error. GUI project lookup uses the runtime environment key or the official-build embedded key.

Proxy configuration is optional. Use `--proxy <url>` or `[network].proxy_ip` plus `[network].proxy_port` in `config.toml`; when both are empty, normal system/environment networking behavior is used.

The `[testRun]` section controls local run behavior. `player_name` is carried into the launch plan, `download_source` selects the runtime download source, `java_version_major` records the expected Java major version, `sync_interval_seconds` is used by sync tooling, `java_path` records the preferred Java binary, and `debug_extra_jvm_args` is appended to the side-specific `[java.client]` or `[java.server]` JVM arguments.

Files under `debug/common`, `debug/client`, and `debug/server` are copied into local run directories for development-only configs, scripts, saves, or other files used by `runClient` / `runServer`.

Run sync is handled by the GUI Sync page. The run setup writes `.mcmpu-run-sources.json` inside the run directory, then process exit writes `.mcmpu/pending-sync-client.json` or `.mcmpu/pending-sync-server.json`. The GUI lists added, modified, and deleted files; debug-origin files are never synced, and deletions require explicit confirmation.

Runtime mod, resource-pack, and shader-pack downloads use CurseMaven coordinates. Generated file names follow `<original sort prefix><slug>-<project_id>-<file_id>.<extension>`, preserving leading sort prefixes such as `!` or `+++`, and download URLs use `https://www.cursemaven.com/curse/maven/<slug>-<project_id>/<file_id>/<slug>-<project_id>-<file_id>.<extension>`.

Built server packages do not use CurseMaven. Their `server-files.json` records CurseForge `download_url` values, and the generated scripts pass the build-time API key through the `x-api-key` request header. Server scripts intentionally contain a Java path placeholder. Edit `start.bat` or `start.sh` and replace `CHANGE_ME_JAVA_PATH`; if it is not changed, the script exits with an explicit error. Server JVM memory and extra arguments come from `[java.server]`. EULA handling is left to the Minecraft server itself.

### License

MIT

## 中文

`mcmpu` 是一个用于维护 CurseForge Minecraft 整合包项目的 Rust 命令行工具。

它可以从已打包的 CurseForge 整合包初始化项目，把依赖拆分为客户端、服务端、通用和资源包清单，并构建客户端 CurseForge 导入包与可运行服务端包。

### 功能

- 导入包含 `manifest.json` 和 `overrides/` 的 CurseForge 整合包 zip。
- 使用 CLI、环境变量或官方构建内置 API key 解析 CurseForge 项目和文件元数据。
- 维护 `config.toml`、`common.json`、`client.json`、`server.json`、`resourcepacks.json`。
- 构建包含 `manifest.json`、`modlist.html` 和合并覆盖文件的客户端包。
- 构建包含原生 `start.bat` 和 `start.sh` 的服务端包，脚本通过 CurseForge CDN 和 `x-api-key` header 下载文件。
- 为本地测试运行准备并启动 `run/client` 与 `run/server` 目录，运行文件由 CurseMaven 管理。
- 支持 Forge、Fabric、NeoForge 和 Cleanroom 1.12.2 服务端安装计划。
- 构建时压缩 JSON，并对 PNG 做无损优化。
- 提供中文与英文 CLI 文案。

### 命令

```bash
mcmpu init path/to/modpack.zip
mcmpu check
mcmpu build
mcmpu runClient
mcmpu runServer
mcmpu gui
```

不带命令行参数双击 `mcmpu.exe` 会打开图形命令执行器。GUI 暴露与 CLI 相同的命令，按命令显示参数输入项，界面中不显示 CurseForge API key，并把命令输出流式写入日志面板。静态视觉预览在 `docs/gui-mockup.html`。

`runClient` 与 `runServer` 同时支持 `run-client` 和 `run-server` 别名。它们读取 `config.toml`，按 `build` / `check` 相同优先级解析 CurseForge API key 和代理，重建运行目录中的生成文件夹，通过 CurseMaven 下载运行文件，启动 Java，并在退出后写入待处理同步清单，不会自动修改 `overrides/`。

发布包外层文件名包含版本和平台，例如 `mcmpu-0.1.0-windows-x86_64.zip`。压缩包内部使用稳定命令名：Windows 为 `mcmpu.exe`，类 Unix 平台为 `mcmpu`。Windows 包额外包含 `mcmpu.bat`；如果希望直接输入 `mcmpu init ...`，需要把解压目录加入 `PATH`。

CurseForge API key 读取优先级：

1. `--curseforge-api-key <key>`
2. `MCMODPACKUTIL_CURSEFORGE_API_KEY`、`CURSEFORGE_API_KEY` 或 `CF_API_KEY`
3. 官方发布构建可在编译期内置 CurseForge key。

`config.toml` 不保存 CurseForge API key。旧配置中的 `[curseforge]` 或 `curseforge_api_key` 会触发迁移错误。GUI 项目查询使用运行时环境变量 key 或官方构建内置 key。

代理配置是可选的。可以使用 `--proxy <url>`，或在 `config.toml` 中同时配置 `[network].proxy_ip` 与 `[network].proxy_port`；两者都为空时使用正常系统/环境网络行为。

`[testRun]` 用于本地运行行为。`player_name` 会进入启动计划，`download_source` 选择运行时下载源，`java_version_major` 记录期望 Java 主版本，`sync_interval_seconds` 供同步工具使用，`java_path` 记录首选 Java 二进制路径，`debug_extra_jvm_args` 会追加到对应端侧 `[java.client]` 或 `[java.server]` 的 JVM 参数之后。

`debug/common`、`debug/client`、`debug/server` 下的文件会复制到本地运行目录，用于开发期配置、脚本、存档等仅参与 `runClient` / `runServer` 的内容。

运行目录同步通过 GUI 的“同步处理”页面完成。run 准备阶段会在运行目录写入 `.mcmpu-run-sources.json`，进程退出后写入 `.mcmpu/pending-sync-client.json` 或 `.mcmpu/pending-sync-server.json`。GUI 会列出新增、修改和删除；来自 `debug/` 的文件永不同步，删除必须显式确认。

运行时 mod、资源包和光影包下载使用 CurseMaven 坐标。生成文件名规则为 `<原文件名前置排序符><slug>-<project_id>-<file_id>.<extension>`，会保留 `!`、`+++` 等前置排序符；下载 URL 规则为 `https://www.cursemaven.com/curse/maven/<slug>-<project_id>/<file_id>/<slug>-<project_id>-<file_id>.<extension>`。

构建出的服务端包不使用 CurseMaven。`server-files.json` 记录 CurseForge `download_url`，生成脚本通过 `x-api-key` 请求 header 传入构建时解析到的 API key。服务端脚本会故意包含 Java 路径占位符。请编辑 `start.bat` 或 `start.sh` 并替换 `CHANGE_ME_JAVA_PATH`；如果未替换，脚本会明确报错退出。服务端 JVM 内存和额外参数来自 `[java.server]`。EULA 由 Minecraft 服务端自身处理。

### 许可证

MIT
