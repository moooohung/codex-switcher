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
    if let Err(error) = run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    codex_switcher_lib::claude::start_monitor();
    let host = std::env::var("CODEX_SWITCHER_WEB_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = std::env::var("CODEX_SWITCHER_WEB_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(3210);

    codex_switcher_lib::web::run_lan_server(&host, port)
}
