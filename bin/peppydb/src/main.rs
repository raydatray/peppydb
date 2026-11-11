//! `peppydb serve`: the peppydb server. See `server::cli`.

fn main() -> std::process::ExitCode {
    server::cli::main(std::env::args_os())
}
