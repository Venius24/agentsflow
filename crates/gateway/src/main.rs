mod cli;
mod diag;
mod doctor;
mod export;
mod guard;
mod hub;
mod mail;
mod mail_mcp;
mod runtime;
mod server;
mod team;
mod telegram;
#[cfg(test)]
mod testing;
mod workflow;

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["--version"] {
        println!("agentsflow {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if args.first().is_some_and(|arg| arg == "doctor") {
        let Some(options) = doctor::Options::parse(&args[1..]) else {
            eprintln!("Usage: agentsflow doctor [--live] [--codex-bin PATH]");
            return ExitCode::from(2);
        };
        return if doctor::run(options).await {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    if args.first().is_some_and(|arg| arg == "team") {
        return match team::run_cli(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("[error] {error:#}");
                ExitCode::from(2)
            }
        };
    }
    if args.first().is_some_and(|arg| arg == "mail-mcp") {
        if args.len() != 3
            || args[1] != "--as"
            || !(args[2] == "orchestrator" || team::valid_id(&args[2]))
        {
            eprintln!("Usage: agentsflow mail-mcp --as <claude|codex|orchestrator>");
            return ExitCode::from(2);
        }
        return match mail_mcp::run(&args[2]).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("mail-mcp: {error:#}");
                ExitCode::FAILURE
            }
        };
    }
    if args.first().is_some_and(|arg| arg == "flow") {
        let command = match cli::FlowCommand::parse(&args[1..]) {
            Ok(command) => command,
            Err(error) => {
                eprintln!("[error] {error:#}");
                return ExitCode::from(2);
            }
        };
        return match command.run().await {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::FAILURE,
            Err(error) => {
                eprintln!("[error] {error:#}");
                ExitCode::FAILURE
            }
        };
    }
    let result = if args.first().is_some_and(|arg| arg == "serve") {
        match server::ServeOptions::parse_checked(&args[1..]) {
            Ok(Some(options)) => {
                let workflows = match options.workflows() {
                    Ok(workflows) => workflows,
                    Err(error) => {
                        eprintln!("[error] {error:#}");
                        return ExitCode::from(2);
                    }
                };
                server::serve(options, workflows).await
            }
            Ok(None) => {
                eprintln!("{}", cli::USAGE);
                return ExitCode::from(2);
            }
            Err(error) => {
                eprintln!("[error] {error:#}");
                return ExitCode::from(2);
            }
        }
    } else {
        match cli::parse_args(args) {
            Some(command) => cli::run(command).await,
            None => {
                eprintln!("{}", cli::USAGE);
                return ExitCode::from(2);
            }
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("[error] {error:#}");
            ExitCode::FAILURE
        }
    }
}
