//! The binary is a thin shim. The command surface lives in the library,
//! so it is testable in process.

use std::process::ExitCode;

fn main() -> ExitCode {
    kersh::cli::main(std::env::args_os().skip(1))
}
