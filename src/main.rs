//! The binary is a thin shim. The command surface lives in the library,
//! so it is testable in process.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    kersh::cli::run(&args)
}
