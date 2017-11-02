use std::path::Path;
use std::fs::File;
use std::io::Write;
use std::env;
use std::process::Command;

static USER_AGENT_FILE_NAME: &str = "user_agent";

fn main() {
    let package_name = option_env!("CARGO_PKG_NAME").unwrap_or("exonum");
    let package_version = option_env!("CARGO_PKG_VERSION").unwrap_or("?");
    let rust_version = rust_version().unwrap_or("rust ?".to_string());
    let user_agent = format!("{} {}/{}", package_name, package_version, rust_version);

    let out_dir = env::var("OUT_DIR").expect("Unable to get OUT_DIR");
    let dest_path = Path::new(&out_dir).join(USER_AGENT_FILE_NAME);
    let mut file = File::create(dest_path).expect("Unable to create output file");
    file.write_all(user_agent.as_bytes()).expect(
        "Unable to data to file",
    );
}

fn rust_version() -> Option<String> {
    let rustc = option_env!("RUSTC").unwrap_or("rustc");

    if let Ok(output) = Command::new(rustc).arg("-V").output().map(|x| x.stdout) {
        String::from_utf8(output).ok()
    } else {
        None
    }
}