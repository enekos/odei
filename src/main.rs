//! odei — tiny, native coding agent for the terminal, powered by the
//! Kimi Code subscription API (https://api.kimi.com/coding) or the
//! Gemini API (https://generativelanguage.googleapis.com).

mod activity;
mod agent;
mod calls;
mod blinker;
mod cli;
mod commands;
mod compact;
mod complete;
mod config;
mod context;
mod diff;
mod eval;
mod gemini;
mod inspect;
mod markdown;
mod mentions;
mod permissions;
mod provider;
mod serve;
mod session;
mod splash;
mod theme;
mod tools;
mod ui;

use config::Config;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let workspace = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let config = Config::load(&workspace);

    let code = match args.first().map(String::as_str) {
        None => ui::run_interactive(config, None),
        Some("ask") => {
            let parsed = cli::parse_ask_args(&args[1..]).and_then(|(format, typed)| {
                cli::compose_prompt(&typed, cli::read_piped_stdin()).map(|prompt| (format, prompt))
            });
            match parsed {
                Ok((format, prompt)) => cli::ask(config, &prompt, format),
                Err(e) => {
                    eprintln!("odei: {e}");
                    2
                }
            }
        }
        Some("sessions") => cli::sessions(),
        Some("session") => match args.get(1).map(String::as_str) {
            Some("resume") => {
                let id = match args.get(2).map(String::as_str) {
                    Some("last") | None => session::latest_for_workspace(&workspace),
                    Some("--id") => args.get(3).cloned(),
                    Some(id) => Some(id.to_string()),
                };
                match id {
                    Some(id) => ui::run_interactive(config, Some(id)),
                    None => {
                        eprintln!("odei: no saved session for this workspace");
                        1
                    }
                }
            }
            _ => {
                eprintln!("usage: odei session resume [last|--id <id>|<id>]");
                2
            }
        },
        Some("serve") => {
            let flag = |name: &str| {
                args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
            };
            serve::serve(config, flag("--workspace"), flag("--resume"))
        }
        Some("eval") => eval::run(config, &args[1..]),
        Some("setup") => match cli::setup_flow(args.get(1).map(String::as_str)) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("odei: {e}");
                1
            }
        },
        Some("upgrade") => cli::upgrade(args.iter().any(|a| a == "--check")),
        Some("status") => cli::status(&config),
        Some("doctor") => cli::doctor(&config),
        Some("models") => cli::models(),
        Some("help") | Some("--help") | Some("-h") => cli::help(),
        Some("version") | Some("--version") | Some("-V") => {
            println!("odei {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Some(other) => {
            eprintln!("odei: unknown command: {other} (run `odei help`)");
            2
        }
    };
    std::process::exit(code);
}
