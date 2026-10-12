// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(target_os = "linux")]
mod linux_gtk;

fn main() {
    let arguments: Vec<String> = std::env::args().collect();
    if codex_switcher_lib::claude::handle_remote_control_helper(&arguments) {
        return;
    }
    if codex_switcher_lib::prompt_cache::handle_helper(&arguments) {
        return;
    }
    if arguments.get(1).is_some_and(|s| s == "--claude-statusline") {
        if let (Some(id), Some(directory)) = (arguments.get(2), arguments.get(3)) {
            let _ = codex_switcher_lib::claude::statusline_entry(id, directory);
        }
        return;
    }
    #[cfg(target_os = "linux")]
    for variable in ["GTK_MODULES", "GTK3_MODULES"] {
        if let Some(filtered) = std::env::var_os(variable)
            .as_deref()
            .and_then(linux_gtk::without_appmenu_module)
        {
            unsafe { std::env::set_var(variable, filtered) };
        }
    }

    codex_switcher_lib::run()
}
