use std::process::ExitCode;

/// The product's name: the one line in the `NAME` file at the repository's root. Nothing else
/// spells it out, so changing that file renames the product.
const NAME: &str = include_str!("../../../NAME").trim_ascii();

fn main() -> ExitCode {
    let name = NAME;
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "version") => {
            println!("{name} {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("usage: {name} <command>\n\n{name} runs virtual machines. No commands exist yet.");
            ExitCode::from(2)
        }
    }
}
