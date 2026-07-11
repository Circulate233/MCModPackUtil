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
  - 校验 required dependency、download URL 和 SHA1。
  - 输出 CurseForge 客户端导入 zip。
  - 输出可运行服务端 zip，包含 `server-files.json`、`start.bat`、`start.sh`、`mcmpu-download.ps1`、展开后的 overrides 文件、resourcepacks 占位目录和顶层文档文件。
  - 构建时 minify JSON，PNG 使用 Rust 内置无损压缩。
- `start.sh` 支持 Linux `sha1sum` 和 macOS `shasum -a 1`。
- 启动脚本安装 loader 后会自动定位 `server.jar`、`fabric-server-launch.jar`、`forge-*.jar`、`neoforge-*.jar`、`cleanroom-*.jar`、`minecraft_server*.jar`。
- CLI 已接入 `build` 子命令。
- 实现 `mcmpu check` 项目 dry-run 校验：
  - 读取四个依赖清单。
  - 解析并锁定依赖。
  - 校验 required dependency、download URL、SHA1 和 loader 安装计划。
  - 不写入 dist zip。
- 构建和检查阶段的依赖解析使用并行迭代。
- build 写 zip 时，普通文件使用流式拷贝；JSON/PNG 按单文件读入并优化。
- init 解析 CurseForge project/file 元数据时使用并行迭代，并通过 stderr 输出阶段日志和已完成解析数量。
- 网络代理可通过全局 `--proxy <url>` 或 `config.toml` 的 `[network].proxy_ip` + `[network].proxy_port` 配置；为空时不强制代理。
- 语言检测读取 `LC_ALL`、`LC_MESSAGES`、`LANGUAGE`、`LANG`，再回退系统 locale。
- `config.toml` 新增 `[java.client]` 与 `[java.server]` 内存/JVM 参数配置；build 服务端脚本使用 `[java.server]`，runClient/runServer 分别使用对应端侧配置并追加 `[testRun].debug_extra_jvm_args`。
- 服务端启动脚本 Java 路径使用 `CHANGE_ME_JAVA_PATH` 占位符，未替换时显式报错退出。
- CLI help 使用中英双语描述；init/build/check 报告按 `language` 输出基础本地化文案。
- `--lang` 和 `--curseforge-api-key` 支持放在子命令前或子命令后。
- 本地化只用于 `mcmpu` 终端输出；服务端启动脚本运行文案固定英文，EULA 由 Minecraft 服务端自身处理。
- build 服务端包使用 CurseForge `download_url` 和 `x-api-key` header 下载文件；CurseMaven 仅用于 `runClient` / `runServer` 本地运行目录。
- CLI 已接入 `runClient` / `runServer` 命令，并保留 `run-client` / `run-server` alias；命令按 build/check 相同规则读取 `config.toml`、CurseForge API key 和 proxy 后调用 run 模块入口。
- README 和设计文档已补充 `runClient` / `runServer`、`[testRun]`、`debug/` 运行目录文件和 CurseMaven 命名规则。
- 实现 `mcmpu runClient` / `mcmpu runServer` 本地运行路径：
  - 重建 `mods`、`resourcepacks`、`shaderpacks`、`libraries`、`versions`、`assets` 生成目录。
  - 保留存档、日志、截图等运行数据。
  - 复制 `overrides/common`、端侧 overrides、`debug/common` 和端侧 debug。
  - 生成 `.mcmpu-sync.toml`、`.mcmpu-run-files.json` 与 `.mcmpu-run-sources.json`。
  - 运行时文件使用 CurseMaven URL 和 `<sort-prefix><slug>-<project_id>-<file_id>.<extension>` 落盘命名。
  - `.mcmpu/cache` 命中时校验 size/SHA1；损坏或缺失时重新下载。
  - runtime 解析不依赖 CurseForge `downloadUrl`，避免第三方下载禁用影响 run 文件下载。
  - Cleanroom runtime 路径不注入 Cleanroom relauncher 变体，仍保留 Fugue/Scalar 等非 relauncher 补缺规则。
  - 支持 `[testRun].java_path`、PATH、常见系统路径、项目 `.mcmpu/java/<major>/` 的 Java 查找顺序。
  - 提供 Zulu Java 下载落盘入口 `.mcmpu/java/<major>/zulu.zip`。
  - 客户端启动命令包含离线用户名、稳定离线 UUID 和固定 offline access token。
  - 服务端启动命令使用 `java -jar server.jar nogui`。
- Java 进程结束后不再自动写回 overrides；工具会扫描新增、修改、删除并写入 `.mcmpu/pending-sync-client.json` 或 `.mcmpu/pending-sync-server.json`。
- GUI 新增“同步处理”页面，可加载 pending 清单，勾选条目、调整目标端侧，并手动应用到 `overrides/common`、`overrides/client` 或 `overrides/server`。
- run 同步来源规则：
  - `common` 是保底层，端侧 overrides 覆盖 common。
  - `debug/` 来源永远不进入同步清单。
  - 删除项默认不应用，必须显式勾选后才删除原来源端侧文件。
- 新增 GUI 命令执行器：
  - 无参数启动 `mcmpu` 时打开 GUI；带参数时保持 CLI。
  - `mcmpu gui` 显式打开 GUI。
  - GUI 左侧选择 `init`、`Mod 列表`、`同步处理`、`build`、`check`、`runClient`、`runServer`。
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
- `src/server/scripts.rs`
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
- 服务端脚本包含下载校验，不提示或写入 EULA。
- Forge、Fabric、NeoForge、Cleanroom loader 安装计划。
- required dependency 缺失检测。

## 下一步
1. 使用真实 CurseForge API key 对真实整合包执行人工验收：
   - `mcmpu init <curseforge-pack.zip>`
   - `mcmpu build`
2. 如需发布三平台二进制，运行 GitHub Actions `CI` workflow 并下载四个 artifact。

## 风险与决策
- 用户明确要求直接在主仓库工作，后续使用主仓库路径 `F:\new\MCModPackUtil`。
- 当前第一批测试聚焦离线核心逻辑，网络解析和完整 zip 构建将在后续测试中补齐。
- 服务端包策略已锁定为可运行服务器目录包，包含原生 bat/sh 下载脚本。
- API key 不写入配置；运行解析类命令按 CLI、运行时环境变量、官方构建内置 key 读取，仍为空时直接报错。
- `LoaderKind::Cleanroom` 当前生成客户端 manifest loader 名称为 `forge`，符合 Cleanroom 对 CurseForge 1.12.2 Forge 兼容包的导入语义。
- `DependencyResolver` 当前依赖 `CurseForgeApi` trait，测试使用内存 fake，真实 HTTP client 可以独立接入。
- `CurseForgeClient` 当前已实现 project、file、slug search、file list 的基础 API 调用和 JSON 映射。
- `mcmpu check` 当前做项目 dry-run 校验，会解析依赖但不写 zip。
- CLI 当前暴露 `check`、`init`、`build`、`runClient`、`runServer` 和 `gui` 命令。
- `initialize_from_zip` 覆盖空依赖 manifest 的离线初始化；非空 manifest 需要 API key 并走 CurseForge 元数据拆分。
- 测试样例数据必须保留在 `tests/`，生产代码禁止出现参考整合包名称、作者、版本等硬编码。
- `mcmpu init <zip> --output <dir>` 当前可生成项目结构，并可用 CurseForge 元数据并行解析 manifest files。
- Cleanroom 自动追加依赖属于 mod 表构建规则，坐标集中放在专用模块，避免散落硬编码。
- Cleanroom relauncher、fugue、Scalar 只在 build/check 路径补缺，并走 CurseForge resolver 锁定最新文件和校验 download URL。
- 实网 API key 可用于临时手动验证，但不得写入测试、文档、配置或日志；当前测试继续优先使用 fake API，避免外网和密钥泄露风险。
- 构建实现采用流式 zip writer；普通文件流式拷贝，JSON/PNG 优化按单文件处理，避免整包常驻内存。
- init/build/check 依赖解析使用 Rayon 并行迭代；zip 写入保持顺序，避免压缩包结构竞争。
