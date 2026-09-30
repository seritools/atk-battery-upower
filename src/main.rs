use std::process::ExitCode;

use atk_battery::read_battery;
use hidapi::HidApi;

fn main() -> ExitCode {
    let api = match HidApi::new() {
        Ok(api) => api,
        Err(e) => {
            eprintln!("hidapi init failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    match read_battery(&api) {
        Some(battery) => {
            println!("{battery}");
            ExitCode::SUCCESS
        }
        None => {
            eprintln!("no response from any matching interface");
            ExitCode::FAILURE
        }
    }
}
