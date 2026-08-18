use std::path::Path;

fn main() {
    println!("cargo:rerun-if-env-changed=TENON_RELEASE_TAR");
    println!("cargo:rerun-if-env-changed=TENON_RELEASE_VERSION");
    let out = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("payload.rs");
    let version = std::env::var("TENON_RELEASE_VERSION").unwrap_or_else(|_| "0.1.0".to_string());
    let body = match std::env::var("TENON_RELEASE_TAR") {
        Ok(tar) if !tar.is_empty() => {
            let tar = std::fs::canonicalize(&tar)
                .unwrap_or_else(|error| panic!("TENON_RELEASE_TAR {tar}: {error}"));
            let tar = tar.display().to_string();
            println!("cargo:rerun-if-changed={tar}");
            format!(
                "pub const PAYLOAD: Option<&[u8]> = Some(include_bytes!({tar:?}));\n\
                 pub const VERSION: &str = {version:?};\n"
            )
        }
        _ => format!(
            "pub const PAYLOAD: Option<&[u8]> = None;\npub const VERSION: &str = {version:?};\n"
        ),
    };
    std::fs::write(&out, body).expect("write payload.rs");
}
