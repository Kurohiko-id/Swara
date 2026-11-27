#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use swara::{admin, app, log, settings, stats};

fn main() {
    // The elevated task inherits a PATH its user can edit; a DLL loaded by bare name must come
    // from System32 only. Swara ships no DLLs of its own.
    // SAFETY: takes only a flag value.
    let _ = unsafe {
        windows::Win32::System::LibraryLoader::SetDefaultDllDirectories(
            windows::Win32::System::LibraryLoader::LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    // Release builds abort on panic, which would otherwise leave no trace of the cause.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::init();
        log::line(&format!("panic: {info}"));
        default_hook(info);
    }));
    let mode = std::env::args().nth(1);
    let code = match mode.as_deref() {
        Some("--settings") => {
            log::init();
            settings::run()
        }
        Some(flag @ ("--admin-on" | "--admin-off")) => {
            log::init();
            admin::switch_mode(flag == "--admin-on")
        }
        // The core sets up logging later, once it knows it is the only instance.
        other => launch_core(other == Some("--from-task")),
    };
    if code != 0 {
        // Give an error report queued during a failed start a moment to go out.
        stats::flush(std::time::Duration::from_secs(3));
    }
    std::process::exit(code);
}

/// Runs the core here, unless admin mode applies and the elevated task should run it instead.
fn launch_core(via_task: bool) -> i32 {
    let mode_on = admin::config_on_disk().run_as_admin;
    // Skip the known-folder lookup entirely while the mode is off.
    let in_pf =
        mode_on && std::env::current_exe().is_ok_and(|exe| admin::exe_under_program_files(&exe));
    if !admin::should_hand_off(mode_on, admin::running_elevated(), via_task, in_pf) {
        return app::run();
    }
    let Err(e) = admin::start_task() else {
        return 0;
    };
    log::init();
    log::line(&format!(
        "admin: task start failed, continuing unelevated: {e}"
    ));
    app::run()
}
