use std::{env, fs, path::Path};

fn main() {
    for path in ["wifi.local.toml", "remote.local.rs", "remote.example.rs"] {
        println!("cargo:rerun-if-changed={path}");
    }
    let out = env::var("OUT_DIR").unwrap();
    let config = fs::read_to_string("wifi.local.toml")
        .expect("Create wifi.local.toml using wifi.example.toml");
    // Do not display parser errors: they may contain credential values.
    let config: toml::Value = toml::from_str(&config)
        .unwrap_or_else(|_| panic!("Invalid TOML in wifi.local.toml (values omitted)"));
    for key in ["ssid", "password"] {
        let value = config
            .get(key)
            .and_then(toml::Value::as_str)
            .unwrap_or_else(|| panic!("Missing string field {key} in wifi.local.toml"));
        if (key == "ssid" && (value.is_empty() || value.len() > 32))
            || (key == "password" && value.len() > 64)
        {
            panic!("Invalid length for {key} in wifi.local.toml (value omitted)");
        }
        // Debug-format strings to escape them as Rust literals, never as code.
        fs::write(
            format!("{out}/{key}.rs"),
            format!("pub const {}: &str = {value:?};\n", key.to_uppercase()),
        )
        .unwrap();
    }
    let remote = if Path::new("remote.local.rs").exists() {
        "remote.local.rs"
    } else {
        "remote.example.rs"
    };
    fs::copy(remote, format!("{out}/remote.rs")).expect("Cannot copy remote configuration");
}
