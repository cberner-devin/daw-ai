use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments.as_slice() == ["--mcp"] {
        return match daw_ai::codex::run_mcp() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("DAW-AI MCP server stopped: {error}");
                ExitCode::FAILURE
            }
        };
    }

    let port = match daw_ai::parse_port(&arguments) {
        Ok(port) => port,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };

    match daw_ai::server::run(port) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("DAW-AI stopped: {error}");
            ExitCode::FAILURE
        }
    }
}
