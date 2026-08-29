fn main() {
    if let Some(code) = fani::run_process_wrapper_if_requested() {
        std::process::exit(code);
    }
    std::process::exit(fani::cli::run());
}
