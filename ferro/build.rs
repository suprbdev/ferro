fn main() {
    println!("cargo::rustc-check-cfg=cfg(ferro_asan)");
    println!("cargo::rustc-check-cfg=cfg(ferro_sanitized)");
    println!("cargo::rerun-if-changed=build.rs");
    let sanitizers = std::env::var("CARGO_CFG_SANITIZE").unwrap_or_default();
    if !sanitizers.is_empty() {
        println!("cargo::rustc-cfg=ferro_sanitized");
    }
    if sanitizers
        .split(',')
        .any(|sanitizer| sanitizer == "address")
    {
        println!("cargo::rustc-cfg=ferro_asan");
    }
}
