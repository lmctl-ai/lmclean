fn main() {
    println!("cargo:rerun-if-env-changed=LMCLEAN_VERSION");
    let version =
        std::env::var("LMCLEAN_VERSION").unwrap_or_else(|_| std::env::var("CARGO_PKG_VERSION").unwrap());
    println!("cargo:rustc-env=LMCLEAN_BUILD_VERSION={version}");
}
