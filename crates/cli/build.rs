// Embeds a build id so a rebuilt CLI detects and restarts a daemon started by an older build.
// It changes only when code that ends up in the daemon changes, so unrelated rebuilds (e.g. the
// desktop app) do not force a daemon restart.
fn main() {
    for dir in ["src", "../core/src", "../store/src", "../engine/src", "../../Cargo.lock"] {
        println!("cargo:rerun-if-changed={dir}");
    }
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=XSSH_BUILD_ID={t}");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("../../assets/icon/xssh.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}
