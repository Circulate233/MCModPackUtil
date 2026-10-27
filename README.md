# MCModPackUtil / mcmpu

## English

`mcmpu` is a Rust command-line tool for maintaining CurseForge Minecraft modpack projects.

It can initialize a project from a packaged CurseForge modpack, split dependencies into client, server, common, and resource-pack lists, then build a CurseForge client import zip, a runnable server zip, or both.

### Features

- Import CurseForge modpack zips with `manifest.json` and `overrides/`.
- Resolve CurseForge project/file metadata with a CLI, environment, or official-build API key.
- Maintain `config.toml`, `common.json`, `client.json`, `server.json`, and `resourcepacks.json`.
- Build client packages with `manifest.json`, `modlist.html`, and merged overrides.
- Build a server zip for one selected target platform, containing `server-install.json` plus the matching latest stable MCServerDownloadTool installer.
- Prepare and launch local `run/client` and `run/server` development directories with CurseMaven-managed files.
- Support Forge, Fabric, NeoForge, and Cleanroom 1.12.2 server installation plans.
- Cleanroom client builds preconfigure the resolved Original, Improved, or Unofficial Relauncher with the selected Cleanroom version and `[java.client]` memory/JVM arguments. Unofficial builds download the publisher's original `cleanroom-<version>.zip` plus `.sha1` from Cleanroom Maven, cache it under `.mcmpu/cache/cleanroom/`, and store it unchanged at `overrides/cleanroom/`; server archives never receive these client-only files.
- Minify JSON and losslessly optimize PNG files during package creation.
- Provide Chinese and English CLI text.

### Commands

```bash
mcmpu init path/to/modpack.zip
mcmpu check
mcmpu build
mcmpu build --target client
mcmpu build --target server --server-platform linux-x86_64
mcmpu build --target both --server-platform windows-x86_64
mcmpu build --output path/to/external/dist
mcmpu runClient
mcmpu runServer
mcmpu gui
```

Double-clicking `mcmpu.exe` without command-line arguments opens the graphical command runner. The GUI exposes the same commands as the CLI, shows per-command parameter fields, keeps CurseForge API keys out of the interface, and streams command output into the log panel. A static visual preview is available at `docs/gui-mockup.html`.

`runClient` and `runServer` also accept the aliases `run-client` and `run-server`. They read `config.toml`, resolve the CurseForge API key and proxy with the same precedence as `build` and `check`, rebuild generated run folders, acquire development runtime files through CurseMaven, launch Java, and write a pending sync manifest for GUI review instead of changing `overrides/` automatically. Ctrl+C is supervised: servers receive `stop` and have a 10-second graceful deadline before termination; clients are terminated directly. Both paths still finalize profiling and pending-sync scanning.

Release archives are named with version and platform, for example `mcmpu-0.1.0-windows-x86_64.zip`. Inside the archive the executable name is stable: `mcmpu.exe` on Windows and `mcmpu` on Unix-like systems. The Windows archive also includes `mcmpu.bat`; add the extracted directory to `PATH` if you want to run `mcmpu init ...` without `.\` or `.exe`.

CurseForge API key lookup order:

1. `--curseforge-api-key <key>`
2. `MCMODPACKUTIL_CURSEFORGE_API_KEY`, `CURSEFORGE_API_KEY`, or `CF_API_KEY`
3. Official release builds may embed a CurseForge key at compile time.

`config.toml` does not store CurseForge API keys. Existing `[curseforge]` or `curseforge_api_key` entries are rejected with a migration error. GUI project lookup uses the runtime environment key or the official-build embedded key.

Proxy configuration is optional. Resolution order is `--proxy <url>`, `[network].proxy_ip` plus `[network].proxy_port` in `config.toml`, `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` and `NO_PROXY` (including lowercase forms), then on Windows the current user's static proxy or PAC/WPAD settings, and finally direct connections.

`config.toml` uses an empty `language` value to follow the system locale. The Settings page offers `跟随系统`, Chinese, and English; only an explicit language selection is persisted.

Local run settings are stored in the project-root `test-run.toml`, outside the build-facing `config.toml`. It contains `player_name`, `download_source`, `sync_interval_seconds`, `java_path`, and `debug_extra_jvm_args`; the file is ignored by Git. Java major version is a build-facing project setting in `[java].version_major`. The GUI Settings page edits `config.toml`. The `runClient` and `runServer` forms edit the applicable `test-run.toml` fields and save the file immediately before launch; `player_name` is shown only on the client form.

Before starting a server, `runServer` writes `eula.txt` with `eula=true` and sets `online-mode=false` in `server.properties`, because the local run authenticates with a fixed offline UUID and token. Only those two keys are controlled; other server properties are left untouched. Published server packages and their generated startup scripts still do not write an EULA.

Each `[java.client/server].extra_args` array element and each `debug_extra_jvm_args` element may contain multiple JVM arguments separated by ordinary whitespace. mcmpu splits them into individual process arguments and does not preflight whether a JVM version supports a flag. Additional `-Xms` or `-Xmx` arguments are rejected because memory is controlled by the dedicated memory fields.

Files under the project-root `debug/common`, `debug/client`, and `debug/server` form an independent runtime-only layer. They are applied after `overrides` when preparing `runClient` / `runServer`, but are never copied into `overrides`, synchronized back, or included in client/server build archives.

Each GUI run form edits its side-specific `run/client/.mcmpu-sync.toml` or `run/server/.mcmpu-sync.toml` include paths. Pending changes are still handled in an independent Sync window, not in the Settings or run form: process exit writes `.mcmpu/pending-sync-client.json` or `.mcmpu/pending-sync-server.json`, and the window lists added, modified, and deleted files and can filter them by path. Debug-origin files are never synced, and a deletion only becomes pending after it is confirmed in the window.

Local runtime mod, resource-pack, and shader-pack downloads use CurseMaven and remain a development-only path. Generated file names follow `<original sort prefix><slug>-<project_id>-<file_id>.<extension>`, preserving leading sort prefixes such as `!` or `+++`. This acquisition path is not serialized into `server-install.json` and is never used by published server installation.

The server zip is immediately usable as an installation directory: files from `overrides/common` are expanded into the archive root first, followed by `overrides/server`, so server-specific files take precedence without an extra `overrides/` directory. `server-install.json` keeps each CurseForge file's original filename under its mod, resource-pack, or shader-pack folder. Each archive contains `server-install.json` and exactly one installer for its selected server platform. After extraction, run that installer.

Build target defaults to `both`; server platform defaults to `current`. `--target` accepts `client`, `server`, or `both`. `--server-platform` accepts `current`, `windows-x86_64`, `linux-x86_64`, or `macos-aarch64`. `current` is resolved strictly from the mcmpu build host; unsupported hosts fail instead of falling back to a nearby platform. A `client` build must not specify `--server-platform`.

`client` writes only `<pack-name>.zip` and does not resolve server dependencies or prepare a server installer. `server` writes only `<pack-name>-<version>-server-<platform>.zip` and does not produce client-only Cleanroom/MMC files or a client archive. `both` writes both archives for the selected server platform. A server archive contains exactly one matching installer, never all three installers.

Each server-containing build validates the complete `Circulate233/MCServerDownloadTool` release index, rejects drafts, prereleases, and invalid tags, and selects the greatest stable `v`-prefixed SemVer release without trusting API ordering. It then downloads and embeds only the requested platform asset. The verified cache remains isolated by release tag under `.mcmpu/cache/server-installer/<tag>`; missing or damaged selected assets are redownloaded. If the Releases API is rate-limited, mcmpu resolves GitHub's latest-release redirect and may reuse only a complete, verified cache for that exact tag. Installer executables are stored uncompressed in the server zip; Unix executables carry mode `0755`.

For a modpack repository CI matrix, build the archives independently:

```bash
mcmpu build --target client
mcmpu build --target server --server-platform windows-x86_64
mcmpu build --target server --server-platform linux-x86_64
mcmpu build --target server --server-platform macos-aarch64
```

This repository's own GitHub Actions workflow publishes only mcmpu executables. It deliberately does not run these modpack-package commands or attach Minecraft archives to mcmpu tool releases.

Configured `[build].output_dir` values must be relative and remain inside the project. An explicit CLI `build --output` is user-authorized and may point outside it. Pack name and version are validated as portable single filename components. Build first validates one deterministic entry table for each archive, rejects links/reparse points, non-regular files, portable path collisions, `runtime/**`, and installer-reserved paths, and enforces 100,000 entries, 1 GiB per file, and 16 GiB total uncompressed data per archive. Both archives are fully written and synced to same-directory temporary files, then published as one rollback-capable transaction so a failed second archive cannot replace only half of a previous valid pair.

Installer cache preparation is serialized per cache across threads and processes. Cached executables remain files rather than in-memory buffers; each Stored ZIP entry is streamed from one open handle while its exact size and SHA-256 are verified.

`server-install.json` classifies each server file as an automatic ForgeCDN download or a manual file with its CurseForge file page. The installer discovers compatible Java installations and asks the user to select one, downloads automatic files concurrently, reports all missing or invalid manual files, installs and verifies the selected loader, generates a download-free startup script for the current system, and supports idempotent repair on later runs. The required Java major comes from `[java].version_major`; heap settings and extra JVM arguments come from `[java.server]` and are serialized into the manifest. CurseMaven is restricted to local `runClient` / `runServer` development paths and is not used by published server installation. EULA handling remains the Minecraft server's responsibility.

Initialization resolves the normal CurseForge project and file metadata but does not require a direct `download_url`. During build, missing server download URLs become explicit manual entries in `server-install.json`; MCServerDownloadTool reports their CurseForge file pages during installation.

### License

MIT

## 中文

`mcmpu` 是一个用于维护 CurseForge Minecraft 整合包项目的 Rust 命令行工具。

它可以从已打包的 CurseForge 整合包初始化项目，把依赖拆分为客户端、服务端、通用和资源包清单，并按需构建客户端 CurseForge 导入包、可运行服务端包或两者。

### 功能

- 导入包含 `manifest.json` 和 `overrides/` 的 CurseForge 整合包 zip。
- 使用 CLI、环境变量或官方构建内置 API key 解析 CurseForge 项目和文件元数据。
- 维护 `config.toml`、`common.json`、`client.json`、`server.json`、`resourcepacks.json`。
- 构建包含 `manifest.json`、`modlist.html` 和合并覆盖文件的客户端包。
- 构建面向单个目标平台的服务端 zip，其中包含 `server-install.json` 与对应平台的最新稳定版 MCServerDownloadTool 安装器。
- 为本地测试运行准备并启动 `run/client` 与 `run/server` 目录，开发期运行文件通过 CurseMaven 获取。
- 支持 Forge、Fabric、NeoForge 和 Cleanroom 1.12.2 服务端安装计划。
- Cleanroom 客户端构建会按最终解析到的 Original、Improved 或 Unofficial Relauncher 预制配置，写入所选 Cleanroom 版本以及 `[java.client]` 内存/JVM 参数。Unofficial 构建从 Cleanroom Maven 下载发布方原始 `cleanroom-<version>.zip` 和 `.sha1`，校验后缓存到 `.mcmpu/cache/cleanroom/`，并保持字节不变写入 `overrides/cleanroom/`；服务端包不会生成这些客户端文件。
- 构建时压缩 JSON，并对 PNG 做无损优化。
- 提供中文与英文 CLI 文案。

### 命令

```bash
mcmpu init path/to/modpack.zip
mcmpu check
mcmpu build
mcmpu build --target client
mcmpu build --target server --server-platform linux-x86_64
mcmpu build --target both --server-platform windows-x86_64
mcmpu build --output path/to/external/dist
mcmpu runClient
mcmpu runServer
mcmpu gui
```

不带命令行参数双击 `mcmpu.exe` 会打开图形命令执行器。GUI 暴露与 CLI 相同的命令，按命令显示参数输入项，界面中不显示 CurseForge API key，并把命令输出流式写入日志面板。静态视觉预览在 `docs/gui-mockup.html`。

`runClient` 与 `runServer` 同时支持 `run-client` 和 `run-server` 别名。它们读取 `config.toml`，按 `build` / `check` 相同优先级解析 CurseForge API key 和代理，重建运行目录中的生成文件夹，通过 CurseMaven 获取开发期运行文件，启动 Java，并在退出后写入待处理同步清单，不会自动修改 `overrides/`。CLI 的 Ctrl+C 会经过运行监督：服务端先发送 `stop`，10 秒后仍未退出才强制终止；客户端直接终止。两条路径都会完成性能报告和待同步清单收尾。

发布包外层文件名包含版本和平台，例如 `mcmpu-0.1.0-windows-x86_64.zip`。压缩包内部使用稳定命令名：Windows 为 `mcmpu.exe`，类 Unix 平台为 `mcmpu`。Windows 包额外包含 `mcmpu.bat`；如果希望直接输入 `mcmpu init ...`，需要把解压目录加入 `PATH`。

CurseForge API key 读取优先级：

1. `--curseforge-api-key <key>`
2. `MCMODPACKUTIL_CURSEFORGE_API_KEY`、`CURSEFORGE_API_KEY` 或 `CF_API_KEY`
3. 官方发布构建可在编译期内置 CurseForge key。

`config.toml` 不保存 CurseForge API key。旧配置中的 `[curseforge]` 或 `curseforge_api_key` 会触发迁移错误。GUI 项目查询使用运行时环境变量 key 或官方构建内置 key。

代理配置是可选的。解析优先级依次为 `--proxy <url>`、`config.toml` 中同时配置的 `[network].proxy_ip` 与 `[network].proxy_port`、`HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` 与 `NO_PROXY`（包括小写形式）、Windows 当前用户的静态系统代理或 PAC/WPAD，最后才直连。

`config.toml` 中 `language` 为空时跟随系统语言。GUI“设置”页提供“跟随系统”、中文和英文选项；只有明确选择的语言才会持久化。

本地运行设置独立保存在项目根目录的 `test-run.toml`，不再混入面向构建的 `config.toml`。其中包含 `player_name`、`download_source`、`sync_interval_seconds`、`java_path` 和 `debug_extra_jvm_args`；该文件默认由 Git 忽略。Java 主版本是构建项目配置，存放在 `[java].version_major`。GUI“设置”页编辑 `config.toml`；`runClient` / `runServer` 表单编辑各自适用的 `test-run.toml` 字段，并在启动前保存该文件。`player_name` 只在客户端运行表单显示。

`runServer` 在启动服务端前写入 `eula=true` 并关闭 `server.properties` 的正版验证（`online-mode=false`），因为本地运行使用固定离线 UUID 和 token。只控制这两个键，其余服务端设置保持不变。发布服务端包及其生成的启动脚本仍不写入 EULA。

`[java.client/server].extra_args` 与 `debug_extra_jvm_args` 的每个数组元素都可以包含多个以普通空白分隔的 JVM 参数。mcmpu 会将其拆成独立进程参数，不预先判断当前 JVM 是否支持某个参数；额外出现 `-Xms` 或 `-Xmx` 时仍会失败，因为内存由专用字段统一控制。

项目根目录下的 `debug/common`、`debug/client`、`debug/server` 是独立的仅运行时覆盖层。准备 `runClient` / `runServer` 时，它们会在 `overrides` 之后应用，但绝不会复制进 `overrides`、同步回写或进入客户端/服务端构建包。

GUI 的客户端/服务端运行表单可分别编辑 `run/client/.mcmpu-sync.toml` 与 `run/server/.mcmpu-sync.toml` 的同步白名单。待处理变更仍在独立“同步处理”窗口中确认，不放入“设置”页或运行表单：进程退出后写入 `.mcmpu/pending-sync-client.json` 或 `.mcmpu/pending-sync-server.json`，窗口列出新增、修改和删除并可按路径筛选；来自 `debug/` 的文件永不同步，删除项只有在窗口内确认后才会进入待应用状态。

本地运行时 mod、资源包和光影包通过 CurseMaven 获取，该路径仅用于开发。生成文件名规则为 `<原文件名前置排序符><slug>-<project_id>-<file_id>.<extension>`，会保留 `!`、`+++` 等前置排序符；这套获取方式不会写入 `server-install.json`，也不会用于发布服务端安装。

服务端 zip 解压后本身就是安装目录：`overrides/common` 先直接展开到压缩包根目录，随后展开 `overrides/server`，因此服务端专用文件覆盖通用文件，产物中没有额外的 `overrides/` 包裹层。`server-install.json` 会在 mods、资源包和光影包目录中保留 CurseForge 提供的原始文件名。每个服务端压缩包只包含一个与目标平台匹配的安装器：Windows x64、Linux x64 或 macOS Apple Silicon；用户解压后直接运行该安装器。

构建目标默认是 `both`，服务端平台默认是 `current`。`--target` 可选 `client`、`server`、`both`；`--server-platform` 可选 `current`、`windows-x86_64`、`linux-x86_64`、`macos-aarch64`。`current` 只按运行 mcmpu 的当前构建主机严格解析，无法映射到受支持平台时直接报错，不会猜测相近平台。`client` 构建不允许传入 `--server-platform`。

`client` 只输出 `<pack-name>.zip`，不会解析服务端依赖或准备服务端安装器；`server` 只输出 `<pack-name>-<version>-server-<platform>.zip`，不会生成客户端 Cleanroom/MMC 文件或客户端包；`both` 输出客户端包和所选服务端平台包。服务端 ZIP 永远只嵌入目标平台的一个安装器，不会再打入三个安装器。

所有包含服务端的构建都会完整校验 `Circulate233/MCServerDownloadTool` 的 Release 索引，排除 draft、prerelease 和非法 tag，再按语义版本选择最高稳定的 `v` 前缀发布，但只下载和嵌入所选平台资产。已验证缓存仍按 `.mcmpu/cache/server-installer/<tag>` 隔离，目标资产损坏或缺失时重新下载。Releases API 限流时，mcmpu 会解析 GitHub latest-release 重定向，并且只能复用同一 tag 的完整已验证缓存。安装器以不压缩方式写入服务端 ZIP；Unix 安装器权限为 `0755`。

整合包仓库的 CI 矩阵应分别构建四类产物：

```bash
mcmpu build --target client
mcmpu build --target server --server-platform windows-x86_64
mcmpu build --target server --server-platform linux-x86_64
mcmpu build --target server --server-platform macos-aarch64
```

本仓库自己的 GitHub Actions 只发布 mcmpu 工具可执行文件，不运行上述整合包打包命令，也不会把 Minecraft 整合包附加到 mcmpu 工具发布中。

`[build].output_dir` 必须是留在项目内的相对路径；用户显式传入的 `build --output` 可以指向项目外目录。整合包名称和版本必须是跨平台安全的单个文件名组件。构建会先建立确定性的统一 entry 表，拒绝链接/reparse point、非普通文件、跨平台路径身份冲突、`runtime/**` 和安装器保留路径，并对每个压缩包限制最多 100,000 项、单文件 8 GiB、总未压缩大小 16 GiB。客户端和服务端包都先写入同目录临时文件并完成 flush/sync，再通过带备份回滚的成对事务发布，第二个包失败不会破坏上一对有效产物或留下半套新产物。

安装器缓存对同进程线程和跨进程构建串行准备。安装器保持为缓存文件而不是读入常驻 `Vec`；写入 Stored ZIP 时从同一打开句柄流式复制并同时校验精确大小与 SHA-256。

`server-install.json` 将服务端文件区分为 ForgeCDN 自动下载项，以及附带 CurseForge 文件页的手动项。安装器会发现兼容 Java 并让用户选择，共用并发下载引擎获取自动项，汇总报告缺失或损坏的手动项，安装并验证所选 loader，按当前系统生成不包含下载逻辑的纯启动脚本；再次运行时执行幂等修复。要求的 Java 大版本来自 `[java].version_major`，堆内存和额外 JVM 参数来自 `[java.server]`，这些设置均写入 manifest。CurseMaven 仅用于本地 `runClient` / `runServer` 开发流程，发布服务端安装不使用 CurseMaven。EULA 仍由 Minecraft 服务端自身处理。

初始化仍会解析正常的 CurseForge 项目与文件元数据，但不要求存在直接 `download_url`。服务端下载 URL 缺失项会在 build 时写成 `server-install.json` 的手动项，由 MCServerDownloadTool 在安装时给出对应 CurseForge 文件页。

### 许可证

MIT
