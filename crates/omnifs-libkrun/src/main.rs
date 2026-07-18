fn main() {
    let result = omnifs_libkrun::Config::parse(std::env::args_os().skip(1))
        .and_then(|config| omnifs_libkrun::run(&config));
    if let Err(error) = result {
        eprintln!("omnifs-libkrun: {error}");
        std::process::exit(1);
    }
}
