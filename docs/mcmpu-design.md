# `mcmpu` MC 整合包开发环境工具设计文档

## Summary
`mcmpu` 是一个 Rust CLI 工具，用于维护 CurseForge Minecraft 整合包项目。

它支持：
- 从 CurseForge 整合包 zip 初始化项目。
- 拆分 mod/resource pack/shader pack 清单。
- 生成客户端 CurseForge 导入包。
- 按指定平台生成服务端安装包，包含数据清单和一个已验证的匹配 MCServerDownloadTool 安装器。
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
- `mcmpu build --target client|server|both`
- `mcmpu build --server-platform current|windows-x86_64|linux-x86_64|macos-aarch64`
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

非空 manifest 初始化只要求 CurseForge 项目与文件元数据可正常解析，不以 `download_url` 是否存在决定初始化成功。服务端构建根据最终文件元数据在 `server-install.json` 中生成显式下载模式：存在直接 URL 时使用认证 ForgeCDN 自动下载，缺失或空白时标记为手动下载并保留 CurseForge 文件页。MCServerDownloadTool 先并发处理 automatic 项，再汇总报告缺失或 SHA1 不符的 manual 项；全部文件有效后才安装 loader。发布服务端安装不得根据 file ID 推导 CDN 路径或使用 CurseMaven。

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
language = "" # empty means follow the system locale

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

[java]
version_major = 8

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

项目根目录本地文件 `test-run.toml`（不进入 Git 或构建产物）：

```toml
player_name = "Player"
download_source = "auto"
sync_interval_seconds = 30
debug_extra_jvm_args = []
java_path = ""
```

`config.toml` 不保存 CurseForge API key。旧配置中的 `[curseforge]` 或 `curseforge_api_key` 会 fail fast，并提示迁移到 `--curseforge-api-key`、运行时环境变量或官方发布构建的编译期内置 key。

`[network].proxy_ip` 与 `[network].proxy_port` 同时为空/0 时不强制代理。代理解析优先级为命令行 `--proxy <url>`、config 显式值、大小写环境变量 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY`（遵循 `NO_PROXY`）、Windows 当前用户静态代理或 PAC/WPAD、直连。配置文件代理会拼接为 `http://<proxy_ip>:<proxy_port>`；只配置其中一项为非法配置。

`[java].version_major`、`[java.server].min_memory`、`[java.server].max_memory` 与 `[java.server].extra_args` 会写入 `server-install.json`。MCServerDownloadTool 按明确配置的 Java 大版本发现并验证候选，让用户按序号选择；安装完成后把所选 Java 的绝对路径和 JVM 设置写入当前系统的纯启动脚本。

`test-run.toml` 控制本地 `runClient` / `runServer` 运行流程：
- `player_name` 只用于客户端离线登录，仅在 GUI 的 `runClient` 表单显示并写入客户端启动计划；服务端表单不显示该字段。
- `download_source` 控制运行时下载源，默认 `auto`。
- `[java].version_major` 记录构建和本地运行共同使用的期望 Java 主版本。
- `sync_interval_seconds` 供运行目录同步工具使用。
- `debug_extra_jvm_args` 追加到对应端侧 `[java.client]` / `[java.server]` JVM 参数之后。
- `java_path` 记录本地运行优先使用的 Java 二进制路径。

GUI 配置文件职责固定如下：
- “设置”页只读取和保存 `config.toml`，不显示或修改 `test-run.toml`。
- `runClient` / `runServer` 表单读取项目根目录的 `test-run.toml`，编辑当前端适用的本地运行字段，并在启动进程前保存。
- 两个运行表单分别编辑 `run/client/.mcmpu-sync.toml` 与 `run/server/.mcmpu-sync.toml` 的同步白名单。
- pending-sync 的新增、修改、删除确认仍由独立“同步处理”窗口负责，不嵌入设置页或运行表单。

GUI 设置页按 Minecraft 版本和 Loader 后台获取版本候选。`loader_version` 必须通过下拉选择；`forge_compat_version` 仅在 Cleanroom 下显示并选择，生成 CurseForge 客户端 manifest 时使用该 Forge 兼容版本，Cleanroom 自身安装仍使用 `loader_version`。

Cleanroom 候选读取官方 releases Maven metadata；`-alpha` 属于 release 候选，名称含 `build` 的内部构建条目不显示。Cleanroom 当前仅接受 Minecraft 1.12.2。Forge 兼容版本独立来自 Forge 官方 1.12.2 版本列表，不从 Cleanroom Relauncher 的开发依赖版本猜测映射。

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
- download URL 无法获取时仍保留项目与文件坐标；服务端 build 将对应项标记为手动下载。

Cleanroom 额外规则：
- 构建 mod 表时自动补缺一个 Cleanroom relauncher，使用最新可用文件，仅客户端。
- Cleanroom relauncher 三个变体互斥：`cleanroom-relauncher`、`cleanroom-relauncher-unofficial`、`improved-cleanroom-relauncher`。用户清单里已有任一变体时，不再自动注入 relauncher，以用户版本为准。
- 构建 mod 表时自动补缺 `fugue`，`project_id = 1005815`，使用最新可用文件，通用端。用户清单已有 `fugue` 时不再注入。
- 若 `[cleanroom].use_scalar = true`，按 `scalar_version` 追加 Scalar 依赖。
- 用户清单已有任一已知 Scalar 坐标或 `scalar` slug 时，不再自动注入 Scalar，以用户版本为准。
- Cleanroom 自动追加依赖需要走同一套 CurseForge 元数据解析、SHA1 校验和 required dependency 检查。
- 客户端构建必须从最终 resolved 客户端条目中识别出且只识别出一个 Relauncher 变体，不能只依据 `[cleanroom].relauncher`；零个、多个或同一条目的 PID/slug 指向不同变体时直接失败。
- Original（project `1214490`）与 Improved（project `1336201`）生成 `overrides/config/relauncher.json`，`selectedVersion`/`latestVersion` 使用 `[pack].loader_version`，并关闭在线自动更新。
- Unofficial（project `1214906`）生成 Forge 1.12 CFG `overrides/config/cleanroom_relauncher.cfg`；其上游没有版本字段，因此通过真实 `Use Local MMC Pack=true` 和 `Local MMC Pack Path=cleanroom/cleanroom-<loader_version>.zip` 固定版本，不得伪造版本字段。
- Unofficial 的本地包只能从 `https://repo.cleanroommc.com/releases/com/cleanroommc/cleanroom/<version>/cleanroom-<version>.zip` 下载，必须读取并校验发布方 `.sha1`。下载所得 ZIP 不得重打包，原始字节以 Stored 方式写入 `overrides/cleanroom/cleanroom-<version>.zip`。
- MMC ZIP 必须在根目录包含 `mmc-pack.json` 和 `patches/net.minecraft.json`、`patches/net.minecraftforge.json`、`patches/org.lwjgl3.json`；`mmc-pack.json` 中唯一 `uid=net.minecraftforge` 的 `component.version` 以及 Forge patch 中唯一 `com.cleanroommc:cleanroom` 库版本都必须等于 `[pack].loader_version`。
- MMC ZIP 缓存在项目 `.mcmpu/cache/cleanroom/`。每次构建都重新读取 Maven `.sha1`；缓存 SHA1 或结构/版本校验失败时重新下载，下载结果仍不合法则立即失败。
- 三种配置的 JVM 参数均由 `[java.client]` 生成：`-Xms<min_memory> -Xmx<max_memory>` 后追加 `extra_args`。额外参数不得再次指定 `-Xms`/`-Xmx`。每个数组元素可包含多个用普通空白分隔的 JVM 参数，CRL 配置生成时会按空白规范化为独立参数；不支持引号或转义语法，也不在 build 阶段验证具体 JVM 是否支持这些参数。
- 若 `overrides/common` 或 `overrides/client` 已存在对应配置或本地 MMC 目标文件，build/check 失败，不覆盖用户文件。服务端 zip 不生成任何 Relauncher 配置或本地 MMC 包。

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
`[build].output_dir` 只接受项目内相对路径；CLI 显式 `mcmpu build --output <dir>` 可由用户指定任意目录。包名和版本在用于 ZIP 文件名之前按跨平台单组件规则校验，拒绝路径分隔符、绝对/父目录、ADS、尾点空格、Windows 设备名及其 Unicode 别名。

构建目标默认 `both`，服务端平台默认 `current`。`current` 只按 mcmpu 实际构建主机严格解析为 `windows-x86_64`、`linux-x86_64` 或 `macos-aarch64`；无法识别时直接失败。`client` 目标禁止指定服务端平台。

产物规则：

```text
client: dist/<name>.zip
server: dist/<name>-<version>-server-<platform>.zip
both:   以上两个产物
```

客户端 zip：
- CurseForge 标准 `manifest.json`
- `modlist.html`
- `overrides/`
- `README*`、`LICENSE*`、`NOTICE*`、`CHANGELOG*`
- Cleanroom 包按最终 Relauncher 变体生成 `overrides/config/relauncher.json` 或 `overrides/config/cleanroom_relauncher.cfg`

服务端 zip：
- `server-install.json`
- 仅一个与 `--server-platform` 匹配的 `MCServerDownloadTool` 安装器
- `overrides/common` 先展开到根目录，再由 `overrides/server` 同路径文件覆盖
- `resourcepacks/`
- 顶层文档文件

服务端安装器集成：
- 仅包含服务端的构建查询 `Circulate233/MCServerDownloadTool` 的 GitHub Releases API，拒绝 draft、prerelease 与非法 tag，并按 SemVer 选择最高的稳定 `v` 前缀发布；不存在稳定版时直接失败。`client` 构建不访问服务端安装器元数据。
- GitHub Release 元数据和 `release-index.json` 仍须完整校验三平台资产集合、同仓库同 tag browser URL、受限 size 与 SHA-256，但构建只下载、缓存并嵌入所选平台的 asset。
- 索引和安装器缓存位于 `.mcmpu/cache/server-installer/<tag>`。同 tag 的有效缓存通过进程内等待、跨进程锁和严格 index 状态复用；缺失或损坏项重新下载。若 Releases API 限流，构建只会通过 latest redirect 确认同一稳定 tag 后复用完整可信缓存，不能使用未知旧缓存。每个新进程仍查询最新 tag，新发布不会复用旧 tag 缓存。
- 缓存准备同时使用进程内等待与跨进程排他锁，持锁后重检；构建保留已验证的目标安装器路径，Stored 写入从同一句柄流式复制并同步校验 size/SHA-256，不把安装器读入内存。
- 目标安装器在 zip 中使用 Stored 模式；Unix 项写入 `0755` 权限。
- 用户解压服务端 zip 后运行包内唯一安装器。安装器严格解析 `server-install.json`，发现并验证指定大版本的 Java 候选并让用户选择，共享下载引擎并发获取 loader installer 和 automatic 文件，校验 manual 文件并生成缺失文件报告，安装和验证 loader，最后原子生成当前系统的纯启动脚本与安装状态。
- 再次运行安装器时执行幂等修复：有效文件与 loader 产物复用，损坏文件重新下载；用户修改过的启动脚本不静默覆盖。
- automatic 项使用 CurseForge `download_url` 和 ForgeCDN `x-api-key` header；缺失 URL 的项目写成 manual 项并附带 CurseForge 文件页。发布服务端安装不推导 CDN 地址，也不使用 CurseMaven。
- `runClient` / `runServer` 本地开发运行允许使用 CurseMaven；该策略不进入发布服务端包。
- 启动脚本只负责用已选择的 Java 启动服务端，不下载、不校验、不再次调用安装器。
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
- 从项目根目录独立读取 `debug/common` 和当前端侧 `debug/client` 或 `debug/server`，在 overrides 之后作为仅用于本地测试运行的最终覆盖层；不得将 debug 视为 overrides 子目录、回写到 overrides 或写入任何构建包。
- 写入 `.mcmpu-run-files.json`，记录需要下载的运行时文件。
- 写入 `.mcmpu-run-sources.json`，记录运行目录文件最终来源、启动前 SHA1/size 和默认回写端侧。
- 首次创建 `.mcmpu-sync.toml`，按 overrides 顶层目录生成同步白名单，并排除 `mods`、`resourcepacks`、`shaderpacks`、`libraries`、`versions`、`assets`、`.mcmpu`。
- GUI 在对应 `runClient` / `runServer` 表单中编辑该端 `.mcmpu-sync.toml`；该白名单与项目根 `test-run.toml` 在启动前分别保存。
- `runServer` 在启动进程前确保 `eula.txt` 为 `eula=true`，并把 `server.properties` 的 `online-mode` 设为 `false`：本地运行使用固定离线 UUID 和 token，服务端必须接受离线玩家。只修正这两个键，`server.properties` 中其余设置原样保留；该步骤只作用于本地运行目录，发布服务端包与生成的启动脚本仍不写入 EULA。
- 运行时先校验 `.mcmpu/cache` 中的 size/SHA1，命中后复制到 run 目录；损坏或缺失时通过仅供本地开发使用的 CurseMaven 路径重新下载。
- 客户端使用 `test-run.toml` 的 `player_name` 生成稳定离线 UUID 和固定离线 token。
- 普通客户端和服务端 run 会分别使用 `[java.client]` / `[java.server]`，并追加本地 `debug_extra_jvm_args`；两组数组元素都按普通空白拆为独立 argv，不预检参数与当前 JVM 的兼容性。额外 `-Xms`/`-Xmx` 仍会失败。服务端按实际 loader installer 产物生成启动参数；Cleanroom 从安装器输出中定位可执行 server JAR，不猜测固定版本文件名。
- CLI Ctrl+C 由运行监督器处理：服务端发送 `stop` 并最多等待 10 秒，客户端直接终止；随后仍执行 pending-sync 收尾。
- 进程结束后扫描新增、修改、删除并写入 `.mcmpu/pending-sync-client.json` 或 `.mcmpu/pending-sync-server.json`，不自动修改 `overrides/`。
- GUI 独立“同步处理”窗口加载 pending 清单，用户确认后才写回 `overrides/common`、`overrides/client` 或 `overrides/server`；pending-sync 操作不进入运行表单。
- `common` 是保底层，端侧 overrides 覆盖 common；独立的 `debug/` 运行时覆盖层永不进入同步清单，应用同步清单时也必须拒绝任何 debug 来源条目。
- 删除项只删除其原来源端侧文件，必须由用户显式勾选应用。

运行时文件使用本地 CurseMaven 开发路径：
- 文件名：`<原文件名前置排序符><slug>-<project_id>-<file_id>.<extension>`。
- 前置排序符保留原文件名前部所有非 ASCII 字母数字字符，例如 `!`、`+++`。
- CurseMaven 只用于 `runClient` / `runServer`，不写入 `server-install.json`，也不进入发布服务端安装流程。
- 缺少 `slug`、`project_id` 或 `file_id` 时 fail fast。
- CurseForge 文件缺少 SHA1 时 fail fast。

## Optimization
构建时执行：
- JSON minify，保持字符串内容不变。
- PNG 使用 Rust 内置无损压缩。
- 失败直接报错并带日志上下文。

## Performance
### Build performance / 构建性能
- client/server overrides 和顶层文档进入统一、安全且排序确定的 entry 表；common 仅允许被当前端侧同路径覆盖，其余跨平台身份冲突全部提前失败。遍历拒绝 symlink、Windows reparse/junction、非普通文件、`runtime/**` 及安装器/manifest/启动脚本保留身份。
- 每个 ZIP 最多 100,000 个文件，单文件最多 1 GiB，总未压缩大小最多 16 GiB。普通文件流式写入；不超过 128 MiB 的 JSON/PNG 保留 minify/无损优化，超出阈值改为流式写入以限制内存。
- `both` 目标的两个 ZIP 先分别写入目标同目录临时文件并执行 `finish`、`flush`、`sync_all`，再通过备份/回滚事务成对发布。单端目标只原子发布其唯一 ZIP。
- JSON/PNG 优化的单文件内存上限为 128 MiB，超过阈值直接流式写入。
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

`mcmpu` 终端输出使用现有中英本地化。服务端安装阶段的交互由 MCServerDownloadTool 按其 `--lang`、系统 locale、英文回退顺序本地化；安装器生成的启动脚本只承载启动命令。

## Error Policy
默认 fail fast：
- 缺 API key：报错退出，并提示 CLI 参数、运行时环境变量和官方构建内置 key。
- config 中出现 `[curseforge]` 或 `curseforge_api_key`：报错退出，并提示迁移路径。
- manifest 字段非法：报错退出。
- CurseForge 项目解析失败：报错退出。
- 文件端侧缺失：默认写入 `common.json`，并生成 `side-inference-report.txt`。
- required dependency 缺失：报错退出。
- 服务端 download URL 不可用：build 在 `server-install.json` 写入带 CurseForge 文件页的 manual 项，安装器聚合报告缺失或 SHA1 不符文件后退出。
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
- 实现服务端 automatic/manual 下载模式分类。
- 实现 Cleanroom 自动追加依赖规则。

### Phase 4: Project Commands
- 实现 `mcmpu init`。
- 实现 `mcmpu check`。
- 实现 `mcmpu build`。
- 实现端侧缺失默认双端和报告输出。

### Phase 5: Packaging
- 实现客户端 zip 输出。
- 实现按目标平台输出的服务端 zip，并嵌入构建时解析到的最新稳定版匹配 MCServerDownloadTool 安装器与 `server-install.json`。
- 实现 loader 安装计划渲染。
- 实现 JSON minify 和 PNG 无损压缩。

### Phase 6: Verification and Release
- 补齐集成测试 fixture。
- 运行 `cargo fmt --check`。
- 运行 `cargo clippy -- -D warnings`。
- 运行 `cargo test`。
- 写入 GitHub Actions 三平台构建配置。

整合包仓库的 CI 应独立运行 `client`、`server/windows-x86_64`、`server/linux-x86_64` 与 `server/macos-aarch64` 四个构建任务。本仓库的工具发布 workflow 只发布 mcmpu 二进制，不绑定任何具体整合包，因此不得在其中运行整合包构建或上传 Minecraft ZIP。
