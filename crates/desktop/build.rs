fn main() {
    // GPUI's layout recursion needs more than the 1 MB default main-thread stack on Windows.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/STACK:8000000");
    }
    // App icon (Explorer, taskbar and window title bar).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("../../assets/icon/xssh.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}
