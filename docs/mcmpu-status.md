# `mcmpu` 当前执行状态

更新时间：2026-07-09 Asia/Shanghai

## 当前目标
继续按 `docs/mcmpu-design.md` 完善 `mcmpu`。本文件记录当前执行状态，作为后续恢复上下文的权威入口。

## 已完成
- 在主仓库 `F:\new\MCModPackUtil` 直接工作。
- 初始化 Rust binary crate：`mcmpu`。
- `Cargo.toml` 已写入基础元数据、MIT license、README 路径和主要依赖。
- 生成 `Cargo.lock`。
- 写入 MIT `LICENSE`。
- 写入中英双语 `README.md`。
- 写入设计文档：`docs/mcmpu-design.md`。
- 写入第一批 TDD 测试：`tests/core_tests.rs`。
- 完成首轮 RED 验证：运行 `cargo test`，测试按预期失败。
- 创建 `src/lib.rs` 和第一批离线核心模块。
- 完成第一批 GREEN 验证：`tests/core_tests.rs` 11 个测试全部通过。
- 写入 CurseForge resolver 测试：`tests/resolver_tests.rs`。
- 实现 `src/curseforge/resolver.rs`。
- 完成第二批 GREEN 验证：完整 `cargo test` 通过，合计 16 个集成测试通过。
- 写入 CurseForge HTTP client 测试：`tests/curseforge_client_tests.rs`。
- 实现 `src/curseforge/client.rs`。
- 完成第三批 GREEN 验证：完整 `cargo test` 通过，合计 19 个集成测试通过。
- 写入 CLI 测试：`tests/cli_tests.rs`。
- 实现 `src/cli.rs`。
- 更新 `src/main.rs`，接入 CLI 运行入口。
- 实现 `mcmpu check` 的基础配置读取和 API key 校验。
- 完成第四批 GREEN 验证：完整 `cargo test` 通过，合计 22 个集成测试通过。
- 写入 init 离线基础测试：`tests/init_tests.rs`。
- 实现 `src/project/init.rs` 和 `src/project/mod.rs`。
- 实现空依赖 manifest 的初始化：生成 config、四个清单、overrides/common、overrides/client、overrides/server。
- 完成第五批 GREEN 验证：完整 `cargo test` 通过，合计 25 个集成测试通过。
- 移除 `src/config.rs` 中用于测试的参考包硬编码样例，测试改用 `tests/support/mod.rs` 的泛化样例构造。
- 将文档中的配置示例改为泛化 `ExamplePack-cleanroom`，避免固化用户提供的参考包信息。
- 写入 `mcmpu init <zip>` CLI 测试。
- 将 `project::init::initialize_from_zip` 接入 CLI。
- 完成第六批 GREEN 验证：`cargo fmt --check` 通过，完整 `cargo test` 通过，合计 26 个集成测试通过。
- 在 `docs/mcmpu-design.md` 补充 Cleanroom 自动追加依赖规则：
  - `cleanroom-relauncher-unofficial`，project `1214906`，最新文件，仅客户端。
  - `fugue`，project `1005815`，最新文件，通用端。
  - `[cleanroom].use_scalar` 和 `[cleanroom].scalar_version`。
  - Scalar 2 使用精确坐标 project `1235372`、file `6842491`。
  - Scalar 3 使用 project `1012168` 最新文件。
  - Scalar 版本仅允许 2 或 3。
- 补充 Cleanroom relauncher 变体规则：
  - `[cleanroom].relauncher = "unofficial" | "original" | "improved"`。
  - 默认使用 `cleanroom-relauncher-unofficial`。
  - `cleanroom-relauncher` project `1214490`、`cleanroom-relauncher-unofficial` project `1214906`、`improved-cleanroom-relauncher` project `1336201` 三者互斥。
  - 用户清单已有任一 relauncher 变体时不再注入，以用户版本为准。
- 实现 `src/project/cleanroom.rs`：
  - Cleanroom relauncher/fugue/Scalar 补缺规则。
  - relauncher 变体配置。
  - Scalar 版本校验。
  - 已有依赖 project id/slug 去重。
- Cleanroom 补缺规则只在 `mcmpu build` / `mcmpu check` 构建 mod 表时隐式执行。
- `mcmpu init` 只拆分输入 manifest 中已有依赖，不向 `common.json` / `client.json` 写入 Cleanroom relauncher、fugue 或 Scalar。
- 实现非空 manifest 初始化：
  - `initialize_from_zip_with_api` 使用 `CurseForgeApi` 解析 project/file 元数据。
  - 按端侧拆分 `common.json`、`client.json`、`server.json`、`resourcepacks.json`。
  - 端侧判断优先使用具体文件 `Client/Server` 标签；文件缺失时回退 CurseForge 项目级端侧元数据；仍缺失时聚合同项目同 MC/loader 文件列表中的 `Client/Server` 标签推断；再缺失时聚合同项目全量文件列表中的 `Client/Server` 标签推断；仍无法判断时进入缺失端侧处理。
  - 清单 JSON 不写 `side` 和 `authors`；`side` 由所在清单文件决定，`authors` 在 build 解析 CurseForge 项目元数据时重新获取。
  - 非空 manifest 缺 API key 时 fail fast。
  - CurseForge 文件缺少端侧标签时，默认按双端写入 `common.json`，并写入 `side-inference-report.txt` 报告。
- 实现 `mcmpu build`：
  - 读取 config 和四个依赖清单。
  - 解析 slug/project/file 坐标并锁定最终 project/file。
  - 自动补缺 Cleanroom relauncher/fugue/Scalar，并尊重用户已有版本。
  - 校验 required dependency 和 SHA1；服务端缺失 download URL 的文件写为 manual 下载项。
  - 输出 CurseForge 客户端导入 zip。
  - 服务端构建目标拆分正在实施：`server` 与 `both` 将输出一个按选定平台命名、只含匹配 MCServerDownloadTool 安装器的服务端 zip；`client` 不解析服务端依赖或准备安装器。
  - 构建时 minify JSON，PNG 使用 Rust 内置无损压缩。
- 服务端包中的 `overrides/common` 先展开，`overrides/server` 后展开并覆盖同路径文件；产物根目录不保留额外 `overrides/` 包裹层。
- 服务端构建会查询 GitHub Releases API，按 SemVer 选择最高稳定 `v` 前缀 tag，并完整校验 Release 元数据与三平台 index 契约；随后仅下载、缓存和嵌入所选目标平台资产。缓存按 `.mcmpu/cache/server-installer/<tag>` 隔离，同 tag 并发复用，新 tag 使用新缓存。
- MCServerDownloadTool 负责 Java 发现与选择、并发下载、manual 缺失文件报告、loader 安装与产物验证、当前系统纯启动脚本生成和幂等修复。
- CLI 已接入 `build` 子命令。
- 实现 `mcmpu check` 项目 dry-run 校验：
  - 读取四个依赖清单。
  - 解析并锁定依赖。
  - 校验 required dependency、SHA1 和 loader 安装计划；服务端缺失 download URL 不阻止检查。
  - 不写入 dist zip。
- 构建和检查阶段的依赖解析使用并行迭代。
- build 会为实际选择的客户端和/或服务端生成安全 entry 表，拒绝链接/reparse、非普通文件、跨平台路径冲突、`runtime/**` 与安装器保留路径，并限制每包 100,000 项、单文件 8 GiB、总未压缩 16 GiB。
- 普通文件流式写入；128 MiB 以内 JSON/PNG 保留优化，超出后流式处理。两包写入同目录临时文件并 flush/sync 后成对事务发布，第二包失败会回滚上一对产物。
- 服务端安装器缓存具有同进程等待和跨进程文件锁；安装器以路径模型返回，Stored ZIP 从同一打开句柄流式复制并校验 size/SHA-256，不再将约 21 MB 资产读入常驻内存。
- init 解析 CurseForge project/file 元数据时使用并行迭代，并通过 stderr 输出阶段日志和已完成解析数量。
- 网络代理可通过全局 `--proxy <url>` 或 `config.toml` 的 `[network].proxy_ip` + `[network].proxy_port` 配置；未显式配置时依次读取大小写环境变量、Windows 当前用户静态系统代理或 PAC/WPAD，最终回退直连。
- 语言检测读取 `LC_ALL`、`LC_MESSAGES`、`LANGUAGE`、`LANG`，再回退系统 locale。
- `config.toml` 保留 `[java.client]` 与 `[java.server]` 内存/JVM 参数配置；本地运行参数迁移到项目根目录且由 Git 忽略的 `test-run.toml`，不进入构建配置。
- GUI“设置”页只编辑 `config.toml`；`runClient` / `runServer` 表单编辑项目根目录的 `test-run.toml`，并在启动前保存。`player_name` 仅在客户端表单显示。
- GUI 性能检查开关只影响当前一次运行，每次默认开启，不持久化到 `config.toml` 或 `test-run.toml`。
- 服务端要求的 Java 大版本来自 `[java].version_major`，内存与额外 JVM 参数来自 `[java.server]`，均写入 `server-install.json`；安装器验证并记录用户选择的 Java。
- 服务端 manifest 的 Forge/NeoForge/Cleanroom/Fabric loader installer 使用同源 Maven `.sha1` 边车信息，安装器校验后执行并验证精确输出产物。
- CLI help 使用中英双语描述；init/build/check 报告按 `language` 输出基础本地化文案。
- `--lang` 和 `--curseforge-api-key` 支持放在子命令前或子命令后。
- `mcmpu` 与 MCServerDownloadTool 都提供中英终端文案；生成的服务端启动脚本只负责启动。EULA 由 Minecraft 服务端自身处理。
- build 服务端包的 automatic 项使用 ForgeCDN 和 `x-api-key` header，manual 项由安装器聚合校验并提供文件页；CurseMaven 仅用于本地 `runClient` / `runServer` 开发流程。
- CLI 已接入 `runClient` / `runServer` 命令，并保留 `run-client` / `run-server` alias；命令按 build/check 相同规则读取 `config.toml`、CurseForge API key 和 proxy 后调用 run 模块入口。
- README 和设计文档已补充 `runClient` / `runServer`、本地调试配置、`debug/` 运行目录文件和 CurseMaven 开发下载边界。
- 实现 `mcmpu runClient` / `mcmpu runServer` 本地运行路径：
  - 重建 `mods`、`resourcepacks`、`shaderpacks`、`libraries`、`versions`、`assets` 生成目录。
  - 保留存档、日志、截图等运行数据。
  - 分两个阶段复制 overrides 与独立的 runtime-only `debug/` 层；debug 不属于 overrides，且不参与构建或同步回写。
  - 生成 `.mcmpu-sync.toml`、`.mcmpu-run-files.json` 与 `.mcmpu-run-sources.json`。
  - 运行时文件使用仅供本地开发的 CurseMaven 路径和 `<sort-prefix><slug>-<project_id>-<file_id>.<extension>` 落盘命名。
  - `.mcmpu/cache` 命中时校验 size/SHA1；损坏或缺失时重新下载。
  - runtime 解析不依赖 CurseForge `downloadUrl` 字段，使用项目与文件坐标构造 CurseMaven 开发下载地址。
  - Cleanroom runtime 路径不注入 Cleanroom relauncher 变体，仍保留 Fugue/Scalar 等非 relauncher 补缺规则。
  - 支持本地调试配置的 `java_path`、PATH、常见系统路径、项目 `.mcmpu/java/<major>/` 的 Java 查找顺序。
  - 提供 Zulu Java 下载落盘入口 `.mcmpu/java/<major>/zulu.zip`。
  - 客户端启动命令包含离线用户名、稳定离线 UUID 和固定 offline access token。
  - 服务端启动命令使用 `java -jar server.jar nogui`。
- Java 进程结束后不再自动写回 overrides；工具会扫描新增、修改、删除并写入 `.mcmpu/pending-sync-client.json` 或 `.mcmpu/pending-sync-server.json`。
- GUI 的 `runClient` / `runServer` 表单分别编辑 `run/client/.mcmpu-sync.toml` 与 `run/server/.mcmpu-sync.toml` 的同步白名单。
- GUI 使用独立“同步处理”窗口加载 pending 清单，勾选条目、调整目标端侧，并手动应用到 `overrides/common`、`overrides/client` 或 `overrides/server`；该操作不放入设置页或运行表单。
- run 同步来源规则：
  - `common` 是保底层，端侧 overrides 覆盖 common。
  - `debug/` 来源永远不进入同步清单。
  - 删除项默认不应用，必须显式勾选后才删除原来源端侧文件。
- 新增 GUI 命令执行器：
  - 无参数启动 `mcmpu` 时打开 GUI；带参数时保持 CLI。
  - `mcmpu gui` 显式打开 GUI。
  - GUI 左侧选择 `init`、`Mod 列表`、`build`、`check`、`runClient`、`runServer` 和“设置”；pending-sync 通过独立“同步处理”窗口处理。
  - 右侧按命令显示 project dir、pack zip、output dir、language、proxy 等参数输入。
  - 文件/目录选择使用原生对话框。
  - GUI 通过当前 exe 启动子进程执行同一 CLI 命令。
  - GUI 界面不显示 CurseForge API key；项目查询使用运行时环境变量 key 或官方构建内置 key。
  - stdout/stderr 流式显示到日志面板。
  - 视觉预览文件：`docs/gui-mockup.html`。
- 新增 `.github/workflows/ci.yml`：
  - 执行 fmt、clippy、test。
  - 构建 Windows x86_64、Linux x86_64、macOS x86_64、macOS aarch64 artifact。
- CurseForge API key 策略已调整：
  - `config.toml` 不再生成或保存 `[curseforge]` / `curseforge_api_key`。
  - 旧配置中出现 `[curseforge]` 或 `curseforge_api_key` 时 fail fast，并提示迁移到 CLI 参数、运行时环境变量或官方构建内置 key。
  - CLI key 优先级为 `--curseforge-api-key`、运行时环境变量 `MCMODPACKUTIL_CURSEFORGE_API_KEY` / `CURSEFORGE_API_KEY` / `CF_API_KEY`、编译期内置 `MCMPU_BUILTIN_CURSEFORGE_API_KEY`。
  - `build.rs` 从编译环境变量 `MCMODPACKUTIL_CURSEFORGE_API_KEY` 注入内置 key，构建日志不输出真实 key。
  - GitHub release artifact 构建任务注入仓库 secret `MCMODPACKUTIL_CURSEFORGE_API_KEY`；普通 verify job 不注入 secret。

## 当前文件状态
已新增或修改：
- `.github/workflows/ci.yml`
- `build.rs`
- `Cargo.toml`
- `Cargo.lock`
- `LICENSE`
- `README.md`
- `docs/mcmpu-design.md`
- `docs/mcmpu-status.md`
- `docs/gui-mockup.html`
- `src/main.rs`
- `src/lib.rs`
- `src/cli.rs`
- `src/config.rs`
- `src/dependency.rs`
- `src/gui/app.rs`
- `src/gui/command.rs`
- `src/gui/mod.rs`
- `src/i18n.rs`
- `src/curseforge/mod.rs`
- `src/curseforge/client.rs`
- `src/curseforge/model.rs`
- `src/curseforge/resolver.rs`
- `src/manifest/mod.rs`
- `src/manifest/curseforge_pack.rs`
- `src/manifest/generated.rs`
- `src/project/mod.rs`
- `src/project/build.rs`
- `src/project/check.rs`
- `src/project/cleanroom.rs`
- `src/project/init.rs`
- `src/project/run.rs`
- `src/server/mod.rs`
- `src/server/files.rs`
- `src/server/loader.rs`
- `src/zip/mod.rs`
- `src/zip/optimize.rs`
- `tests/core_tests.rs`
- `tests/build_tests.rs`
- `tests/check_tests.rs`
- `tests/cli_tests.rs`
- `tests/curseforge_client_tests.rs`
- `tests/gui_command_tests.rs`
- `tests/cleanroom_tests.rs`
- `tests/init_tests.rs`
- `tests/resolver_tests.rs`
- `tests/run_tests.rs`
- `tests/support/mod.rs`

当前 `src/main.rs` 已接入 CLI/GUI 分流。离线核心模块、CurseForge resolver 抽象、真实 HTTP client、Cleanroom 补缺规则、`mcmpu check` 项目 dry-run 校验、`mcmpu init <zip>`、`mcmpu build`、`mcmpu runClient`、`mcmpu runServer` 和 GUI 命令执行器已经实现。

## 当前测试状态
命令：

```bash
cargo test
```

结果：通过，合计 128 个集成测试通过，另有 1 个库内单元测试通过。

```text
tests/build_tests.rs: 11 passed; 0 failed
tests/check_tests.rs: 2 passed; 0 failed
tests/cli_tests.rs: 12 passed; 0 failed
tests/core_tests.rs: 20 passed; 0 failed
tests/curseforge_client_tests.rs: 7 passed; 0 failed
tests/cleanroom_tests.rs: 11 passed; 0 failed
tests/gui_command_tests.rs: 10 passed; 0 failed
tests/gui_mod_list_tests.rs: 11 passed; 0 failed
tests/init_tests.rs: 10 passed; 0 failed
tests/resolver_tests.rs: 8 passed; 0 failed
tests/run_tests.rs: 27 passed; 0 failed
```

本次验证命令：

```powershell
cargo test
```

结果通过；Windows 本地偶发 Cargo incremental 目录 finalizing `拒绝访问` warning，但测试退出码为 0。

格式检查：

```powershell
cargo fmt --check
```

结果：通过。

Clippy 检查：

```powershell
$env:CARGO_INCREMENTAL='0'; $env:RUSTFLAGS='-C debuginfo=0'; cargo clippy --release -- -D warnings
```

结果：通过。

Release 构建：

```powershell
$env:CARGO_INCREMENTAL='0'; $env:RUSTFLAGS='-C debuginfo=0'; cargo build --release
```

结果：通过。

补充记录：默认 GUI release build 首次因 F 盘空间不足失败；执行 `cargo clean` 清理 Cargo 生成目录，释放约 8.3 GiB 后，`cargo test --release --no-default-features`、`cargo fmt --check`、`cargo clippy --release -- -D warnings` 和默认 `cargo build --release` 均通过。

当前本机产物：

```text
target/dist/mcmpu-0.1.0-windows-x86_64.zip
```

发布策略：外层发布 zip 文件名必须明确包含版本和平台标识；包内命令名统一，Windows 包含 `mcmpu.exe` 和转发用 `mcmpu.bat`。

`target/dist/mcmpu-0.1.0-windows-x86_64/mcmpu.bat --help` 已验证可运行，命令包含 `init`、`build`、`check`。

补充验证：已搜索生产代码、测试、文档和 README 中的参考包名称、作者、示例 Forge 版本、旧测试 helper 名称和临时 API key 标记，结果无业务硬编码或密钥残留。

## 当前测试覆盖意图
`tests/core_tests.rs` 当前覆盖：
- `manifest.version = "${version}"` 判定非法。
- 包名包含 `cleanroom` 时生成 Cleanroom 配置。
- 清单对象数组读写，且条目没有 `version` 字段。
- CurseForge 文件/项目元数据映射 loader、side、type。
- CurseForge class id 映射 Mods、Resource Packs、Shader Packs。
- `modlist.html` 转义项目名和作者名。
- 客户端 manifest 使用锁定的 `project_id + file_id`。
- JSON minify 保持字符串内容。
- `server-install.json` 与安装器流程覆盖下载校验；生成的纯启动脚本不下载，且不提示或写入 EULA。
- Forge、Fabric、NeoForge、Cleanroom loader 安装计划。
- required dependency 缺失检测。

## 下一步
1. 使用真实 CurseForge API key 对真实整合包执行人工验收：
   - `mcmpu init <curseforge-pack.zip>`
   - 验收独立构建：`mcmpu build --target client`、`mcmpu build --target server --server-platform windows-x86_64` 和默认 `mcmpu build`。
2. 如需发布三平台二进制，运行 GitHub Actions `CI` workflow 并下载四个 artifact。

## 风险与决策
- 用户明确要求直接在主仓库工作，后续使用主仓库路径 `F:\new\MCModPackUtil`。
- 当前第一批测试聚焦离线核心逻辑，网络解析和完整 zip 构建将在后续测试中补齐。
- 服务端包策略正在迁移为单平台安装目录包：服务端 ZIP 仅含数据 manifest 与构建时解析到的目标平台安装器，不预制下载脚本。默认构建保持 `both + current`。
- API key 不写入配置；运行解析类命令按 CLI、运行时环境变量、官方构建内置 key 读取，仍为空时直接报错。
- `LoaderKind::Cleanroom` 当前生成客户端 manifest loader 名称为 `forge`，符合 Cleanroom 对 CurseForge 1.12.2 Forge 兼容包的导入语义。
- `DependencyResolver` 当前依赖 `CurseForgeApi` trait，测试使用内存 fake，真实 HTTP client 可以独立接入。
- `CurseForgeClient` 当前已实现 project、file、slug search、file list 的基础 API 调用和 JSON 映射。
- `mcmpu check` 当前做项目 dry-run 校验，会解析依赖但不写 zip。
- CLI 当前暴露 `check`、`init`、`build`、`runClient`、`runServer` 和 `gui` 命令。
- CLI Ctrl+C 已接入运行监督：服务端发送 `stop` 并在 10 秒后必要时强制终止，客户端直接终止；两端都会继续生成 pending-sync 清单。
- `initialize_from_zip` 覆盖空依赖 manifest 的离线初始化；非空 manifest 需要 API key 并走 CurseForge 元数据拆分。
- 非空 manifest 初始化会解析 CurseForge 项目与文件元数据，但不会因缺失 `download_url` 失败。服务端 build 将缺失 URL 的文件写入 `server-install.json` 的 manual 项并保留 CurseForge 文件页；MCServerDownloadTool 先处理 automatic 项，再聚合报告所有缺失或 SHA1 不符的 manual 项，补齐后重跑才继续 loader 安装。发布构建不推导 CDN 路径，也不使用 CurseMaven；CurseMaven 仅限本地 `runClient` / `runServer` 开发流程。
- 测试样例数据必须保留在 `tests/`，生产代码禁止出现参考整合包名称、作者、版本等硬编码。
- `mcmpu init <zip> --output <dir>` 当前可生成项目结构，并可用 CurseForge 元数据并行解析 manifest files。
- Cleanroom 自动追加依赖属于 mod 表构建规则，坐标集中放在专用模块，避免散落硬编码。
- Cleanroom relauncher、fugue、Scalar 只在 build/check 路径补缺，并走 CurseForge resolver 锁定最新文件和校验文件元数据。
- Cleanroom 客户端 build/check 已按最终 resolved 条目严格识别唯一 Relauncher：Original/Improved 生成结构化 `config/relauncher.json`；Unofficial 生成启用 `Use Local MMC Pack` 的完整 Forge CFG，从 Cleanroom Maven 下载并校验原始 MMC ZIP，复用 `.mcmpu/cache/cleanroom/` 后以 Stored 原样写入客户端 `overrides/cleanroom/`。参数来自 `[java.client]`，每个 `extra_args` 元素按普通空白规范化；build 不验证具体 JVM 参数兼容性，但重复覆盖 `-Xms`/`-Xmx` 仍会失败。服务端不生成这些文件。
- 实网 API key 可用于临时手动验证，但不得写入测试、文档、配置或日志；当前测试继续优先使用 fake API，避免外网和密钥泄露风险。
- 构建实现采用流式 zip writer；普通文件流式拷贝，JSON/PNG 仅在单文件不超过 128 MiB 时优化，避免整包或超大单文件常驻内存。
- init/build/check 依赖解析使用 Rayon 并行迭代；zip 写入保持顺序，避免压缩包结构竞争。
