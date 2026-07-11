# `mcmpu` MC 整合包开发环境工具设计文档

## Summary
`mcmpu` 是一个 Rust CLI 工具，用于维护 CurseForge Minecraft 整合包项目。

它支持：
- 从 CurseForge 整合包 zip 初始化项目。
- 拆分 mod/resource pack/shader pack 清单。
- 生成客户端 CurseForge 导入包。
- 生成服务端可运行包，包含首次启动下载和 loader 安装脚本。
- 准备并启动客户端和服务端本地测试运行目录。
- 支持 Windows、Linux、macOS 可执行文件发布。

## Execution State
当前执行状态记录在 `docs/mcmpu-status.md`。每次继续实现前先读取该文件，确认已完成内容、当前阻塞点、测试状态和下一步。

状态文档需要持续维护：
- 完成一组文件或行为后更新“已完成”。
- 测试失败或验证通过后更新“验证记录”。
- 改变实现顺序或范围后更新“下一步”。
- 遇到设计偏差后更新“风险与决策”。

## CLI
二进制名称：`mcmpu`

命令：
- `mcmpu init <curseforge-pack.zip>`
- `mcmpu build`
- `mcmpu check`
- `mcmpu runClient`，别名 `mcmpu run-client`
- `mcmpu runServer`，别名 `mcmpu run-server`
- `mcmpu gui`

全局参数：
- `--lang zh-CN|en-US`
- `--curseforge-api-key <key>`

读取优先级：
- CurseForge API key：命令参数、运行时环境变量 `MCMODPACKUTIL_CURSEFORGE_API_KEY` / `CURSEFORGE_API_KEY` / `CF_API_KEY`、官方发布构建可用的编译期内置 key
- 语言：命令参数、`config.toml`、系统环境

API key 为空时直接报错，并提示用户到 CurseForge console 获取 key。

无参数启动 `mcmpu` 时打开 GUI；带任意 CLI 参数时保持终端行为。GUI 使用同一 exe 作为子进程执行实际命令，参数构建规则与 CLI 一致，界面中不显示 CurseForge API key，项目查询使用运行时环境变量 key 或官方构建内置 key。GUI 视觉预览文件为 `docs/gui-mockup.html`。

## Project Files
初始化后生成：

```text
config.toml
common.json
client.json
server.json
resourcepacks.json
side-inference-report.txt
overrides/
  common/
  client/
  server/
```

`init` 规则：
- 输入 zip 必须包含 `manifest.json` 和 manifest 指向的 `overrides/`。
- `manifest.version` 必须是实际版本字符串；`${version}` 这类占位值判定为非法字段。
- 包名包含 `cleanroom` 时，`loader = "cleanroom"`，同时保存 Forge 兼容版本。
- 输入包的 `overrides/` 全部导入到 `overrides/common/`。
- `overrides/client/` 和 `overrides/server/` 创建为空目录。
- 根据 CurseForge 项目/文件元数据拆分清单。
- 端侧判断顺序：具体文件 `Client` / `Server` 标签、项目级端侧元数据、同项目同 MC/loader 文件列表的 `Client` / `Server` 标签聚合推断、同项目全量文件列表的 `Client` / `Server` 标签聚合推断。
- 缺少 client/server 端侧信息时默认按双端写入 `common.json`。
- 无法自动推测端侧的条目收集到 `side-inference-report.txt`，告知用户后续可手动迁移。

## Config Schema
`config.toml`：

```toml
language = "zh-CN"

[pack]
name = "ExamplePack-cleanroom"
version = "1.0.0"
authors = ["ExampleAuthor", "SecondAuthor"]
minecraft_version = "1.12.2"
loader = "cleanroom"
loader_version = "1.0.0"
forge_compat_version = "1.0.0"

[network]
proxy_ip = ""
proxy_port = 0

[build]
output_dir = "dist"

[testRun]
player_name = "Player"
download_source = "auto"
java_version_major = 8
sync_interval_seconds = 30
debug_extra_jvm_args = []
java_path = ""

[java.client]
min_memory = "2G"
max_memory = "4G"
extra_args = []

[java.server]
min_memory = "2G"
max_memory = "4G"
extra_args = ["-XX:+UseG1GC"]

[cleanroom]
use_scalar = false
scalar_version = 2
relauncher = "unofficial"
```

`config.toml` 不保存 CurseForge API key。旧配置中的 `[curseforge]` 或 `curseforge_api_key` 会 fail fast，并提示迁移到 `--curseforge-api-key`、运行时环境变量或官方发布构建的编译期内置 key。

`[network].proxy_ip` 与 `[network].proxy_port` 同时为空/0 时不强制代理，HTTP client 使用系统/环境默认行为；命令行 `--proxy <url>` 优先级高于 config。配置文件代理会拼接为 `http://<proxy_ip>:<proxy_port>`；只配置其中一项为非法配置。

`[java.client]` 暂时保留给客户端未来使用。`[java.server]` 会写入服务端启动脚本。脚本内 Java 路径使用占位符，用户必须手动修改；未修改时脚本启动会显式报错。

`[testRun]` 控制本地 `runClient` / `runServer` 运行流程：
- `player_name` 写入启动计划。
- `download_source` 控制运行时下载源，默认 `auto`。
- `java_version_major` 记录期望 Java 主版本。
- `sync_interval_seconds` 供运行目录同步工具使用。
- `debug_extra_jvm_args` 追加到对应端侧 `[java.client]` / `[java.server]` JVM 参数之后。
- `java_path` 记录本地运行优先使用的 Java 二进制路径。

`[cleanroom]` 仅在 `pack.loader = "cleanroom"` 时生效：
- `relauncher` 控制自动注入的 Cleanroom relauncher 变体，允许值：
  - `"unofficial"`：默认值，`cleanroom-relauncher-unofficial`，`project_id = 1214906`。
  - `"original"`：`cleanroom-relauncher`，`project_id = 1214490`。
  - `"improved"`：`improved-cleanroom-relauncher`，`project_id = 1336201`。
- `use_scalar` 控制是否向 mod 表追加 Scalar。
- `scalar_version = 2` 时追加精确坐标 `project_id = 1235372`、`file_id = 6842491`。
- `scalar_version = 3` 时追加 `project_id = 1012168` 的最新可用文件。
- 其他 `scalar_version` 值为非法参数，命令直接报错。

## Dependency Lists
四个 JSON 清单均为对象数组。

示例：

```json
[
  {
    "project_id": 521480,
    "file_id": 5811529,
    "slug": "skin-layers-3d",
    "loader": "forge",
    "type": "mod",
    "name": "Skin Layers 3D",
    "required": true
  }
]
```

字段规则：
- `project_id + file_id` 是数字锁定坐标。
- `slug` 是 CurseForge 项目 URL 最后一段，例如 `skin-layers-3d`。
- 清单移除单独 `version` 字段。
- 清单不写 `side` 字段；端侧由所在文件决定：`common.json`、`client.json`、`server.json` 或 `resourcepacks.json`。
- 清单不写 `authors` 字段；作者信息在构建 `modlist.html` 时从 CurseForge 项目元数据解析。
- 缺少数字坐标时，使用 `slug + minecraft_version + loader` 查询候选。
- 候选选择优先 stable；没有 stable 时允许 beta；alpha 需要显式 `file_id`。
- required dependency 缺失时中断，并列出缺失项目，要求用户显式加入清单。
- download URL 无法获取时中断，并列出项目与文件。

Cleanroom 额外规则：
- 构建 mod 表时自动补缺一个 Cleanroom relauncher，使用最新可用文件，仅客户端。
- Cleanroom relauncher 三个变体互斥：`cleanroom-relauncher`、`cleanroom-relauncher-unofficial`、`improved-cleanroom-relauncher`。用户清单里已有任一变体时，不再自动注入 relauncher，以用户版本为准。
- 构建 mod 表时自动补缺 `fugue`，`project_id = 1005815`，使用最新可用文件，通用端。用户清单已有 `fugue` 时不再注入。
- 若 `[cleanroom].use_scalar = true`，按 `scalar_version` 追加 Scalar 依赖。
- 用户清单已有任一已知 Scalar 坐标或 `scalar` slug 时，不再自动注入 Scalar，以用户版本为准。
- Cleanroom 自动追加依赖需要走同一套 CurseForge 解析、download URL 校验和 required dependency 检查。

支持项目类型：
- Mods
- Resource Packs
- Shader Packs

目录规则：
- Resource Packs 放入 `resourcepacks/`
- Shader Packs 放入 `shaderpacks/`
- 服务端包包含 resourcepacks
- 客户端包包含 resourcepacks 和 shaderpacks

## Build Outputs
`mcmpu build` 一次生成：

```text
dist/<name>-<version>-client.zip
dist/<name>-<version>-server.zip
```

客户端 zip：
- CurseForge 标准 `manifest.json`
- `modlist.html`
- `overrides/`
- `README*`、`LICENSE*`、`NOTICE*`、`CHANGELOG*`

服务端 zip：
- `start.bat`
- `start.sh`
- `mcmpu-download.ps1`
- `server-files.json`
- 展开后的 `overrides/common` 与 `overrides/server` 文件
- `resourcepacks/`
- 顶层文档文件

服务端首次启动脚本：
- Windows 使用 `mcmpu-download.ps1` 通过 PowerShell 下载。
- Linux/macOS 使用 `curl` 或 `wget` 下载。
- build 服务端包使用 CurseForge `download_url`，下载请求带 `x-api-key` header。
- CurseMaven 只用于 `runClient` / `runServer` 本地运行目录。
- 下载 mods/resourcepacks 并校验 SHA1。
- 安装 loader 服务端。
- 文件已存在且校验通过时跳过下载。
- 不提示或写入 EULA；首次启动由 Minecraft 服务端自身生成并处理 `eula.txt`。

## Run Outputs
`mcmpu runClient` 和 `mcmpu runServer` 读取当前目录的 `config.toml`，按 `build` / `check` 相同规则解析 CurseForge API key 和代理，解析依赖后准备本地运行目录并启动 Java：

```text
run/client/
run/server/
```

运行目录规则：
- 重建 `mods`、`resourcepacks`、`shaderpacks`、`libraries`、`versions`、`assets` 这些生成目录。
- 保留世界、日志、配置白名单以外的本地运行数据。
- 复制 `overrides/common` 和当前端侧 `overrides/client` 或 `overrides/server`。
- 复制 `debug/common` 和当前端侧 `debug/client` 或 `debug/server`，作为仅用于本地测试运行的开发文件。
- 写入 `.mcmpu-run-files.json`，记录需要下载的运行时文件。
- 写入 `.mcmpu-run-sources.json`，记录运行目录文件最终来源、启动前 SHA1/size 和默认回写端侧。
- 首次创建 `.mcmpu-sync.toml`，按 overrides 顶层目录生成同步白名单，并排除 `mods`、`resourcepacks`、`shaderpacks`、`libraries`、`versions`、`assets`、`.mcmpu`。
- 运行时先校验 `.mcmpu/cache` 中的 size/SHA1，命中后复制到 run 目录；损坏或缺失时通过 CurseMaven 重新下载。
- 客户端使用 `[testRun].player_name` 生成稳定离线 UUID 和固定离线 token。
- 服务端使用 `[java.server]` 与 `[testRun].debug_extra_jvm_args` 生成 `java -jar server.jar nogui` 启动命令。
- 进程结束后扫描新增、修改、删除并写入 `.mcmpu/pending-sync-client.json` 或 `.mcmpu/pending-sync-server.json`，不自动修改 `overrides/`。
- GUI “同步处理”页面加载 pending 清单，用户确认后才写回 `overrides/common`、`overrides/client` 或 `overrides/server`。
- `common` 是保底层，端侧 overrides 覆盖 common；`debug/` 覆盖层永不进入同步清单。
- 删除项只删除其原来源端侧文件，必须由用户显式勾选应用。

运行时文件使用 CurseMaven 命名和下载规则：
- 文件名：`<原文件名前置排序符><slug>-<project_id>-<file_id>.<extension>`。
- 前置排序符保留原文件名前部所有非 ASCII 字母数字字符，例如 `!`、`+++`。
- URL：`https://www.cursemaven.com/curse/maven/<slug>-<project_id>/<file_id>/<slug>-<project_id>-<file_id>.<extension>`。
- 缺少 `slug`、`project_id` 或 `file_id` 时 fail fast。
- CurseForge 文件缺少 SHA1 时 fail fast。

## Optimization
构建时执行：
- JSON minify，保持字符串内容不变。
- PNG 使用 Rust 内置无损压缩。
- 失败直接报错并带日志上下文。

## Performance
- zip 写入使用流式 writer，普通文件直接按文件写入，避免构建整包时把全部内容常驻内存。
- JSON/PNG 优化按单文件处理，内存峰值与单个待优化文件大小相关。
- CurseForge 解析逻辑集中在 resolver/API 抽象，init/build/check 对独立依赖执行并行解析。
- 构建阶段使用锁定后的 project/file 坐标写 manifest，减少重复查询和重复解析。

## Localization
内置语言：
- `zh-CN`
- `en-US`

输出内容覆盖：
- CLI help
- 错误信息
- build/check 报告

本地化仅用于 `mcmpu` 终端输出。生成进服务端包的 `start.bat`、`start.sh` 等运行脚本文案固定使用英文。

## Error Policy
默认 fail fast：
- 缺 API key：报错退出，并提示 CLI 参数、运行时环境变量和官方构建内置 key。
- config 中出现 `[curseforge]` 或 `curseforge_api_key`：报错退出，并提示迁移路径。
- manifest 字段非法：报错退出。
- CurseForge 项目解析失败：报错退出。
- 文件端侧缺失：默认写入 `common.json`，并生成 `side-inference-report.txt`。
- required dependency 缺失：报错退出。
- download URL 不可用：报错退出。
- loader installer URL 解析失败：报错退出。
- 空 catch 块禁止出现。

## Test Plan
单元测试覆盖 manifest、config、清单、解析、HTML、脚本、压缩策略。
集成测试覆盖 init/build/check 与 client/server zip 结构。

## Implementation Phases
实施按以下阶段推进，阶段完成状态以 `docs/mcmpu-status.md` 为准。

### Phase 1: Project Foundation
- 建立 Rust crate。
- 写入 MIT `LICENSE`。
- 写入中英双语 `README.md`。
- 写入本设计文档。
- 写入第一批离线核心行为测试。

### Phase 2: Offline Core
- 创建 `src/lib.rs` 和模块树。
- 实现 `config`、`manifest`、`dependency`、`server`、`zip` 的离线逻辑。
- 让 `tests/core_tests.rs` 中无需网络的测试通过。

### Phase 3: CurseForge Resolution
- 实现 CurseForge HTTP client。
- 实现 project/file/slug 解析。
- 实现 API key 读取优先级。
- 实现 required dependency 缺失检测。
- 实现 download URL 可用性检查。
- 实现 Cleanroom 自动追加依赖规则。

### Phase 4: Project Commands
- 实现 `mcmpu init`。
- 实现 `mcmpu check`。
- 实现 `mcmpu build`。
- 实现端侧缺失默认双端和报告输出。

### Phase 5: Packaging
- 实现客户端 zip 输出。
- 实现服务端 zip 输出。
- 实现服务端 `start.bat` 和 `start.sh`。
- 实现 loader 安装计划渲染。
- 实现 JSON minify 和 PNG 无损压缩。

### Phase 6: Verification and Release
- 补齐集成测试 fixture。
- 运行 `cargo fmt --check`。
- 运行 `cargo clippy -- -D warnings`。
- 运行 `cargo test`。
- 写入 GitHub Actions 三平台构建配置。
