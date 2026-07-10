fn main() {
    if let Err(error) = herdr_tether::cli::run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}
