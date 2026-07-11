#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    ZhCn,
    EnUs,
}

impl Language {
    pub fn from_code(code: &str) -> Self {
        if code.eq_ignore_ascii_case("zh-CN") || code.to_ascii_lowercase().starts_with("zh") {
            Self::ZhCn
        } else {
            Self::EnUs
        }
    }

    pub fn init_completed(self) -> &'static str {
        match self {
            Self::ZhCn => "初始化完成",
            Self::EnUs => "init completed",
        }
    }

    pub fn check_passed(self) -> &'static str {
        match self {
            Self::ZhCn => "检查通过",
            Self::EnUs => "check passed",
        }
    }

    pub fn build_client(self) -> &'static str {
        match self {
            Self::ZhCn => "客户端",
            Self::EnUs => "client",
        }
    }

    pub fn build_server(self) -> &'static str {
        match self {
            Self::ZhCn => "服务端",
            Self::EnUs => "server",
        }
    }

    pub fn cmd_title_init(self) -> &'static str {
        match self {
            Self::ZhCn => "初始化",
            Self::EnUs => "Init",
        }
    }

    pub fn cmd_title_build(self) -> &'static str {
        match self {
            Self::ZhCn => "构建",
            Self::EnUs => "Build",
        }
    }

    pub fn cmd_title_mod_list(self) -> &'static str {
        match self {
            Self::ZhCn => "Mod 列表",
            Self::EnUs => "Mod List",
        }
    }

    pub fn cmd_title_sync(self) -> &'static str {
        match self {
            Self::ZhCn => "同步处理",
            Self::EnUs => "Sync",
        }
    }

    pub fn cmd_title_settings(self) -> &'static str {
        match self {
            Self::ZhCn => "设置",
            Self::EnUs => "Settings",
        }
    }

    pub fn cmd_title_check(self) -> &'static str {
        match self {
            Self::ZhCn => "检查",
            Self::EnUs => "Check",
        }
    }

    pub fn cmd_title_run_client(self) -> &'static str {
        match self {
            Self::ZhCn => "运行客户端",
            Self::EnUs => "Run Client",
        }
    }

    pub fn cmd_title_run_server(self) -> &'static str {
        match self {
            Self::ZhCn => "运行服务端",
            Self::EnUs => "Run Server",
        }
    }

    pub fn cmd_desc_init(self) -> &'static str {
        match self {
            Self::ZhCn => "从 CurseForge 整合包 Zip 创建项目",
            Self::EnUs => "Create a project from a CurseForge pack zip",
        }
    }

    pub fn cmd_desc_build(self) -> &'static str {
        match self {
            Self::ZhCn => "构建客户端与服务端整合包",
            Self::EnUs => "Build client and server packages",
        }
    }

    pub fn cmd_desc_mod_list(self) -> &'static str {
        match self {
            Self::ZhCn => "编辑四个依赖清单",
            Self::EnUs => "Edit the four dependency lists",
        }
    }

    pub fn cmd_desc_sync(self) -> &'static str {
        match self {
            Self::ZhCn => "确认 run 目录变更并写回 overrides",
            Self::EnUs => "Review run directory changes and write them to overrides",
        }
    }

    pub fn cmd_desc_settings(self) -> &'static str {
        match self {
            Self::ZhCn => "编辑 config.toml 项目配置",
            Self::EnUs => "Edit config.toml project settings",
        }
    }

    pub fn cmd_desc_check(self) -> &'static str {
        match self {
            Self::ZhCn => "验证项目配置",
            Self::EnUs => "Validate project configuration",
        }
    }

    pub fn cmd_desc_run_client(self) -> &'static str {
        match self {
            Self::ZhCn => "准备并启动本地客户端",
            Self::EnUs => "Prepare and launch the local client",
        }
    }

    pub fn cmd_desc_run_server(self) -> &'static str {
        match self {
            Self::ZhCn => "准备并启动本地服务端",
            Self::EnUs => "Prepare and launch the local server",
        }
    }

    pub fn field_project_dir(self) -> &'static str {
        match self {
            Self::ZhCn => "自定义项目目录",
            Self::EnUs => "Custom Project Dir",
        }
    }

    pub fn field_pack_zip(self) -> &'static str {
        match self {
            Self::ZhCn => "整合包 Zip",
            Self::EnUs => "CurseForge Pack Zip",
        }
    }

    pub fn field_output_dir(self) -> &'static str {
        match self {
            Self::ZhCn => "自定义输出目录",
            Self::EnUs => "Custom Output Dir",
        }
    }

    pub fn field_language(self) -> &'static str {
        match self {
            Self::ZhCn => "语言",
            Self::EnUs => "Language",
        }
    }

    pub fn field_proxy(self) -> &'static str {
        match self {
            Self::ZhCn => "代理",
            Self::EnUs => "Proxy",
        }
    }

    pub fn field_proxy_ip(self) -> &'static str {
        match self {
            Self::ZhCn => "代理 IP",
            Self::EnUs => "Proxy IP",
        }
    }

    pub fn field_proxy_port(self) -> &'static str {
        match self {
            Self::ZhCn => "代理端口",
            Self::EnUs => "Proxy Port",
        }
    }

    pub fn btn_browse(self) -> &'static str {
        match self {
            Self::ZhCn => "浏览",
            Self::EnUs => "Browse",
        }
    }

    pub fn btn_run(self, cli_name: &str) -> String {
        match self {
            Self::ZhCn => format!("运行 {cli_name}"),
            Self::EnUs => format!("Run {cli_name}"),
        }
    }

    pub fn btn_save(self) -> &'static str {
        match self {
            Self::ZhCn => "保存",
            Self::EnUs => "Save",
        }
    }

    pub fn console_output(self) -> &'static str {
        match self {
            Self::ZhCn => "运行日志",
            Self::EnUs => "Console Output",
        }
    }

    pub fn status_ready(self) -> &'static str {
        match self {
            Self::ZhCn => "就绪",
            Self::EnUs => "Ready",
        }
    }

    pub fn status_running(self) -> &'static str {
        match self {
            Self::ZhCn => "运行中",
            Self::EnUs => "Running",
        }
    }

    pub fn status_finished(self, code: i32) -> String {
        match self {
            Self::ZhCn => format!("完成，退出码 {code}"),
            Self::EnUs => format!("Finished with exit code {code}"),
        }
    }

    pub fn status_failed(self) -> &'static str {
        match self {
            Self::ZhCn => "失败",
            Self::EnUs => "Failed",
        }
    }

    pub fn status_input_error(self) -> &'static str {
        match self {
            Self::ZhCn => "输入有误",
            Self::EnUs => "Input error",
        }
    }

    pub fn err_missing_project_dir(self) -> &'static str {
        match self {
            Self::ZhCn => "请填写项目目录",
            Self::EnUs => "Project directory is required",
        }
    }

    pub fn err_missing_pack_zip(self) -> &'static str {
        match self {
            Self::ZhCn => "请选择整合包 Zip 文件",
            Self::EnUs => "CurseForge pack zip is required",
        }
    }

    pub fn err_missing_output_dir(self) -> &'static str {
        match self {
            Self::ZhCn => "请填写输出目录",
            Self::EnUs => "Output directory is required",
        }
    }

    pub fn err_missing_proxy_ip(self) -> &'static str {
        match self {
            Self::ZhCn => "请填写代理 IP",
            Self::EnUs => "Proxy IP is required",
        }
    }

    pub fn err_missing_proxy_port(self) -> &'static str {
        match self {
            Self::ZhCn => "请填写代理端口",
            Self::EnUs => "Proxy port is required",
        }
    }

    pub fn err_gui_only_command(self) -> &'static str {
        match self {
            Self::ZhCn => "这是 GUI 页面，不是终端命令",
            Self::EnUs => "This is a GUI page, not a CLI command",
        }
    }

    pub fn mod_list_unsaved_block(self) -> &'static str {
        match self {
            Self::ZhCn => "Mod 列表有未保存修改，请先保存",
            Self::EnUs => "Mod list has unsaved changes; save first",
        }
    }

    pub fn mod_list_saved_with_changes(self) -> &'static str {
        match self {
            Self::ZhCn => "保存完成，但之后的修改尚未保存",
            Self::EnUs => "Saved, but newer changes are still unsaved",
        }
    }

    pub fn mod_list_row_state_error(self) -> &'static str {
        match self {
            Self::ZhCn => "Mod 列表行状态异常，已取消本次操作",
            Self::EnUs => "Mod list row state is invalid; the operation was cancelled",
        }
    }

    pub fn settings_saved_with_changes(self) -> &'static str {
        match self {
            Self::ZhCn => "保存完成，但之后的修改尚未保存",
            Self::EnUs => "Saved, but newer changes are still unsaved",
        }
    }

    pub fn mod_list_loaded(self) -> &'static str {
        match self {
            Self::ZhCn => "Mod 列表已加载",
            Self::EnUs => "Mod list loaded",
        }
    }

    pub fn mod_list_saved(self) -> &'static str {
        match self {
            Self::ZhCn => "Mod 列表已保存",
            Self::EnUs => "Mod list saved",
        }
    }

    pub fn mod_list_loading(self) -> &'static str {
        match self {
            Self::ZhCn => "正在加载 Mod 列表",
            Self::EnUs => "Loading mod list",
        }
    }

    pub fn mod_list_saving(self) -> &'static str {
        match self {
            Self::ZhCn => "正在保存 Mod 列表",
            Self::EnUs => "Saving mod list",
        }
    }

    pub fn mod_list_searching_projects(self) -> &'static str {
        match self {
            Self::ZhCn => "正在搜索 CurseForge 项目",
            Self::EnUs => "Searching CurseForge projects",
        }
    }

    pub fn mod_list_querying_files(self) -> &'static str {
        match self {
            Self::ZhCn => "正在查询文件候选",
            Self::EnUs => "Querying file candidates",
        }
    }

    pub fn mod_list_not_loaded(self) -> &'static str {
        match self {
            Self::ZhCn => "请先加载 Mod 列表",
            Self::EnUs => "Load the mod list first",
        }
    }

    pub fn mod_list_project_search(self) -> &'static str {
        match self {
            Self::ZhCn => "搜索 CurseForge 名称、slug、PID 或 URL",
            Self::EnUs => "Search CurseForge name, slug, PID, or URL",
        }
    }

    pub fn mod_list_search(self) -> &'static str {
        match self {
            Self::ZhCn => "筛选当前列表",
            Self::EnUs => "Filter current list",
        }
    }

    pub fn mod_list_no_projects(self) -> &'static str {
        match self {
            Self::ZhCn => "未找到支持的项目",
            Self::EnUs => "No supported projects found",
        }
    }

    pub fn mod_list_type_mod(self) -> &'static str {
        match self {
            Self::ZhCn => "Mod",
            Self::EnUs => "Mod",
        }
    }

    pub fn mod_list_type_resource_pack(self) -> &'static str {
        match self {
            Self::ZhCn => "资源包",
            Self::EnUs => "Resource Pack",
        }
    }

    pub fn mod_list_type_shader_pack(self) -> &'static str {
        match self {
            Self::ZhCn => "光影包",
            Self::EnUs => "Shader Pack",
        }
    }

    pub fn mod_list_delete(self) -> &'static str {
        match self {
            Self::ZhCn => "删除",
            Self::EnUs => "Delete",
        }
    }

    pub fn mod_list_files(self) -> &'static str {
        match self {
            Self::ZhCn => "文件候选",
            Self::EnUs => "Files",
        }
    }

    pub fn mod_list_group_mods(self) -> &'static str {
        match self {
            Self::ZhCn => "Mods",
            Self::EnUs => "Mods",
        }
    }

    pub fn mod_list_group_resource_packs(self) -> &'static str {
        match self {
            Self::ZhCn => "资源包",
            Self::EnUs => "Resource Packs",
        }
    }

    pub fn mod_list_group_shader_packs(self) -> &'static str {
        match self {
            Self::ZhCn => "光影包",
            Self::EnUs => "Shader Packs",
        }
    }

    pub fn mod_list_empty(self) -> &'static str {
        match self {
            Self::ZhCn => "无条目",
            Self::EnUs => "Empty",
        }
    }

    pub fn mod_list_auto(self) -> &'static str {
        match self {
            Self::ZhCn => "自动更新",
            Self::EnUs => "Auto Update",
        }
    }

    pub fn mod_list_add_dialog_title(self) -> &'static str {
        match self {
            Self::ZhCn => "添加项目",
            Self::EnUs => "Add Project",
        }
    }

    pub fn mod_list_add_dialog_confirm(self) -> &'static str {
        match self {
            Self::ZhCn => "确认添加",
            Self::EnUs => "Add",
        }
    }

    pub fn mod_list_add_dialog_cancel(self) -> &'static str {
        match self {
            Self::ZhCn => "取消",
            Self::EnUs => "Cancel",
        }
    }

    pub fn mod_list_manifest(self) -> &'static str {
        match self {
            Self::ZhCn => "所属清单",
            Self::EnUs => "Manifest",
        }
    }

    pub fn mod_list_type(self) -> &'static str {
        match self {
            Self::ZhCn => "类型",
            Self::EnUs => "Type",
        }
    }

    pub fn mod_list_loader(self) -> &'static str {
        match self {
            Self::ZhCn => "加载器",
            Self::EnUs => "Loader",
        }
    }

    pub fn mod_list_required(self) -> &'static str {
        match self {
            Self::ZhCn => "必需",
            Self::EnUs => "Required",
        }
    }

    pub fn mod_list_no_candidates(self) -> &'static str {
        match self {
            Self::ZhCn => "无候选文件",
            Self::EnUs => "No candidates",
        }
    }

    pub fn mod_list_select_file(self) -> &'static str {
        match self {
            Self::ZhCn => "选择文件",
            Self::EnUs => "Select file",
        }
    }

    pub fn mod_list_added(self, name: &str) -> String {
        match self {
            Self::ZhCn => format!("已添加 {name}"),
            Self::EnUs => format!("Added {name}"),
        }
    }

    pub fn mod_list_missing_project_locator(self) -> &'static str {
        match self {
            Self::ZhCn => "缺少项目定位信息",
            Self::EnUs => "Missing project locator",
        }
    }

    pub fn mod_list_slug_hint(self) -> &'static str {
        match self {
            Self::ZhCn => "slug",
            Self::EnUs => "slug",
        }
    }

    pub fn mod_list_project_id_hint(self) -> &'static str {
        match self {
            Self::ZhCn => "PID",
            Self::EnUs => "PID",
        }
    }

    pub fn mod_list_file_id_hint(self) -> &'static str {
        match self {
            Self::ZhCn => "FID",
            Self::EnUs => "FID",
        }
    }

    pub fn mod_list_loader_any(self) -> &'static str {
        match self {
            Self::ZhCn => "任意加载器",
            Self::EnUs => "Any loader",
        }
    }

    pub fn sync_loaded(self) -> &'static str {
        match self {
            Self::ZhCn => "同步清单已加载",
            Self::EnUs => "Sync manifest loaded",
        }
    }

    pub fn sync_empty(self) -> &'static str {
        match self {
            Self::ZhCn => "没有待处理同步项",
            Self::EnUs => "No pending sync items",
        }
    }

    pub fn sync_load(self) -> &'static str {
        match self {
            Self::ZhCn => "加载同步清单",
            Self::EnUs => "Load Sync",
        }
    }

    pub fn sync_apply(self) -> &'static str {
        match self {
            Self::ZhCn => "应用已勾选",
            Self::EnUs => "Apply Checked",
        }
    }

    pub fn sync_select_all_modified(self) -> &'static str {
        match self {
            Self::ZhCn => "勾选修改/新增",
            Self::EnUs => "Check modified/added",
        }
    }

    pub fn sync_side_client(self) -> &'static str {
        match self {
            Self::ZhCn => "客户端",
            Self::EnUs => "Client",
        }
    }

    pub fn sync_side_server(self) -> &'static str {
        match self {
            Self::ZhCn => "服务端",
            Self::EnUs => "Server",
        }
    }

    pub fn settings_loaded(self) -> &'static str {
        match self {
            Self::ZhCn => "设置已加载",
            Self::EnUs => "Settings loaded",
        }
    }

    pub fn settings_saved(self) -> &'static str {
        match self {
            Self::ZhCn => "设置已保存",
            Self::EnUs => "Settings saved",
        }
    }

    pub fn settings_loading(self) -> &'static str {
        match self {
            Self::ZhCn => "正在加载设置",
            Self::EnUs => "Loading settings",
        }
    }

    pub fn settings_saving(self) -> &'static str {
        match self {
            Self::ZhCn => "正在保存设置",
            Self::EnUs => "Saving settings",
        }
    }

    pub fn settings_not_loaded(self) -> &'static str {
        match self {
            Self::ZhCn => "请先加载设置",
            Self::EnUs => "Load settings first",
        }
    }

    pub fn settings_load(self) -> &'static str {
        match self {
            Self::ZhCn => "加载设置",
            Self::EnUs => "Load Settings",
        }
    }

    pub fn settings_unsaved_block(self) -> &'static str {
        match self {
            Self::ZhCn => "设置有未保存修改，请先保存",
            Self::EnUs => "Settings have unsaved changes; save first",
        }
    }

    pub fn settings_section_pack(self) -> &'static str {
        match self {
            Self::ZhCn => "整合包信息",
            Self::EnUs => "Pack",
        }
    }

    pub fn settings_section_network(self) -> &'static str {
        match self {
            Self::ZhCn => "网络代理",
            Self::EnUs => "Network",
        }
    }

    pub fn settings_section_build(self) -> &'static str {
        match self {
            Self::ZhCn => "构建",
            Self::EnUs => "Build",
        }
    }

    pub fn settings_section_java_client(self) -> &'static str {
        match self {
            Self::ZhCn => "客户端 Java",
            Self::EnUs => "Client Java",
        }
    }

    pub fn settings_section_java_server(self) -> &'static str {
        match self {
            Self::ZhCn => "服务端 Java",
            Self::EnUs => "Server Java",
        }
    }

    pub fn settings_section_test_run(self) -> &'static str {
        match self {
            Self::ZhCn => "开发运行",
            Self::EnUs => "Test Run",
        }
    }

    pub fn settings_section_cleanroom(self) -> &'static str {
        match self {
            Self::ZhCn => "Cleanroom",
            Self::EnUs => "Cleanroom",
        }
    }

    pub fn settings_label_language(self) -> &'static str {
        match self {
            Self::ZhCn => "界面语言",
            Self::EnUs => "Language",
        }
    }

    pub fn settings_label_pack_name(self) -> &'static str {
        match self {
            Self::ZhCn => "整合包名称",
            Self::EnUs => "Pack Name",
        }
    }

    pub fn settings_label_pack_version(self) -> &'static str {
        match self {
            Self::ZhCn => "整合包版本",
            Self::EnUs => "Pack Version",
        }
    }

    pub fn settings_label_authors(self) -> &'static str {
        match self {
            Self::ZhCn => "作者",
            Self::EnUs => "Authors",
        }
    }

    pub fn settings_label_minecraft_version(self) -> &'static str {
        match self {
            Self::ZhCn => "Minecraft 版本",
            Self::EnUs => "Minecraft Version",
        }
    }

    pub fn settings_label_loader(self) -> &'static str {
        match self {
            Self::ZhCn => "加载器",
            Self::EnUs => "Loader",
        }
    }

    pub fn settings_label_loader_version(self) -> &'static str {
        match self {
            Self::ZhCn => "加载器版本",
            Self::EnUs => "Loader Version",
        }
    }

    pub fn settings_label_forge_compat_version(self) -> &'static str {
        match self {
            Self::ZhCn => "Forge 兼容版本",
            Self::EnUs => "Forge Compatibility Version",
        }
    }

    pub fn settings_label_proxy_ip(self) -> &'static str {
        match self {
            Self::ZhCn => "代理地址",
            Self::EnUs => "Proxy Host",
        }
    }

    pub fn settings_label_proxy_port(self) -> &'static str {
        match self {
            Self::ZhCn => "代理端口",
            Self::EnUs => "Proxy Port",
        }
    }

    pub fn settings_label_output_dir(self) -> &'static str {
        match self {
            Self::ZhCn => "输出目录",
            Self::EnUs => "Output Directory",
        }
    }

    pub fn settings_label_min_memory(self) -> &'static str {
        match self {
            Self::ZhCn => "最小内存",
            Self::EnUs => "Minimum Memory",
        }
    }

    pub fn settings_label_max_memory(self) -> &'static str {
        match self {
            Self::ZhCn => "最大内存",
            Self::EnUs => "Maximum Memory",
        }
    }

    pub fn settings_label_extra_jvm_args(self) -> &'static str {
        match self {
            Self::ZhCn => "额外 JVM 参数",
            Self::EnUs => "Extra JVM Args",
        }
    }

    pub fn settings_label_player_name(self) -> &'static str {
        match self {
            Self::ZhCn => "离线玩家名",
            Self::EnUs => "Offline Player Name",
        }
    }

    pub fn settings_label_download_source(self) -> &'static str {
        match self {
            Self::ZhCn => "下载源",
            Self::EnUs => "Download Source",
        }
    }

    pub fn settings_label_java_version_major(self) -> &'static str {
        match self {
            Self::ZhCn => "Java 大版本",
            Self::EnUs => "Java Major Version",
        }
    }

    pub fn settings_label_sync_interval(self) -> &'static str {
        match self {
            Self::ZhCn => "同步检查间隔（秒）",
            Self::EnUs => "Sync Interval Seconds",
        }
    }

    pub fn settings_label_debug_jvm_args(self) -> &'static str {
        match self {
            Self::ZhCn => "调试 JVM 参数",
            Self::EnUs => "Debug JVM Args",
        }
    }

    pub fn settings_label_java_path(self) -> &'static str {
        match self {
            Self::ZhCn => "开发运行 Java 路径",
            Self::EnUs => "Test Run Java Path",
        }
    }

    pub fn settings_label_cleanroom_enabled(self) -> &'static str {
        match self {
            Self::ZhCn => "启用 Cleanroom 规则",
            Self::EnUs => "Enable Cleanroom Rules",
        }
    }

    pub fn settings_label_use_scalar(self) -> &'static str {
        match self {
            Self::ZhCn => "注入 Scalar",
            Self::EnUs => "Inject Scalar",
        }
    }

    pub fn settings_label_scalar_version(self) -> &'static str {
        match self {
            Self::ZhCn => "Scalar 版本",
            Self::EnUs => "Scalar Version",
        }
    }

    pub fn settings_label_relauncher(self) -> &'static str {
        match self {
            Self::ZhCn => "Relauncher 变体",
            Self::EnUs => "Relauncher Variant",
        }
    }

    pub fn lang_option_system(self) -> &'static str {
        match self {
            Self::ZhCn => "跟随系统",
            Self::EnUs => "System",
        }
    }
}
