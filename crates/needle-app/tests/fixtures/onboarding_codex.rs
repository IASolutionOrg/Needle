use std::env;

fn main() {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    match arguments.first().map(String::as_str) {
        Some("--version") => println!("codex-cli 0.144.0"),
        Some("app-server") if arguments.iter().any(|argument| argument == "--help") => {
            println!("--listen <URL>\n--strict-config\n--experimental");
        }
        Some("app-server")
            if arguments.get(1).map(String::as_str) == Some("generate-json-schema") =>
        {
            println!("--experimental");
        }
        _ => {}
    }
}
