use std::process::ExitCode;

fn main() -> ExitCode {
    let port = match daw_ai::parse_port(std::env::args().skip(1)) {
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
