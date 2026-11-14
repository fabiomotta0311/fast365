fn main() {
    println!("cargo:rerun-if-changed=assets/fast365.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/fast365.ico")
            .compile()
            .expect("failed to embed Windows icon");
    }
}
