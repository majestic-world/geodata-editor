#![cfg_attr(windows, windows_subsystem = "windows")]

use std::process::ExitCode;

use rfd::{MessageButtons, MessageDialog, MessageLevel};

fn main() -> ExitCode {
    match geodata_editor::editor::run_cli(std::env::args_os()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            MessageDialog::new()
                .set_level(MessageLevel::Error)
                .set_title("Geodata Editor")
                .set_description(error.to_string())
                .set_buttons(MessageButtons::Ok)
                .show();
            ExitCode::FAILURE
        }
    }
}
