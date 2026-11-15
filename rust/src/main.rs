//! thesh binary entry point — port of `src/main.c`'s `main()`.
//!
//! All logic lives in [`thesh::run`]; this shim converts the exit
//! status into a process exit.

fn main() {
    std::process::exit(thesh::run::run_main());
}
