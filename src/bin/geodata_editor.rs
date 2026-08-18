use std::process::ExitCode;

fn main() -> ExitCode {
    match geodata_editor::editor::run_cli(std::env::args_os()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
