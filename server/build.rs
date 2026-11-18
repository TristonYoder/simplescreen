use std::{env, fs, path::PathBuf};

fn main() {
    let path = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("../branding.toml");
    println!("cargo:rerun-if-changed={}", path.display());
    println!("cargo:rerun-if-changed=migrations");
    let text = fs::read_to_string(&path).expect("read branding.toml");
    let doc: toml::Table = text.parse().expect("parse branding.toml");
    let get = |section: &str, key: &str| -> String {
        match doc.get(section).and_then(|s| s.get(key)) {
            Some(toml::Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => panic!("branding.toml missing {section}.{key}"),
        }
    };
    println!("cargo:rustc-env=BRAND_NAME={}", get("product", "name"));
    println!("cargo:rustc-env=BRAND_SLUG={}", get("product", "slug"));
    println!("cargo:rustc-env=BRAND_BONJOUR_TYPE={}", get("network", "bonjour_type"));
    println!("cargo:rustc-env=BRAND_DEFAULT_PORT={}", get("network", "default_port"));
}
