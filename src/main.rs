fn main() {
    #[cfg(feature = "gui")]
    let mode = mcmpu::gui::command::launch_mode_from_arg_count(std::env::args_os().count());
    #[cfg(feature = "gui")]
    let result = match mode {
        mcmpu::gui::command::AppLaunchMode::Gui => {
            detach_console_for_gui();
            mcmpu::gui::run_gui().map_err(Into::into)
        }
        mcmpu::gui::command::AppLaunchMode::Cli => mcmpu::cli::run(),
    };
    #[cfg(not(feature = "gui"))]
    let result = mcmpu::cli::run();
    if let Err(err) = result {
        eprintln!("{err:#}");
        std::process::exit(1);
    }
}

#[cfg(all(windows, feature = "gui"))]
fn detach_console_for_gui() {
    // SAFETY: FreeConsole detaches the current process from its console and has no Rust-side
    // invariants. It is only called for no-argument GUI startup, before the native GUI loop starts.
    unsafe {
        windows_sys::Win32::System::Console::FreeConsole();
    }
}

#[cfg(all(not(windows), feature = "gui"))]
fn detach_console_for_gui() {}
