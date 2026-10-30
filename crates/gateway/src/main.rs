mod cli;
mod hub;
mod server;

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["--version"] {
        println!("agentsflow {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let result = if args.first().is_some_and(|arg| arg == "serve") {
        match server::ServeOptions::parse(&args[1..]) {
            Some(options) => server::serve(options).await,
            None => {
                eprintln!("{}", cli::USAGE);
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
