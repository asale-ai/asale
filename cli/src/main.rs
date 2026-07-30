//! `asale` — the command-line front end of the asale client.
//!
//! Everything lives in the library half so the exit code is decided in one
//! place and the dispatch table can be unit-tested.

fn main() -> std::process::ExitCode {
    asale_cli::main(std::env::args().skip(1).collect())
}
