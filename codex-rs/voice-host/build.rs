fn main() {
    // Cargo's copied-helper integration tests use the same private package
    // layout as Bazel's native_link inputs, without inherited loader paths.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu")
    {
        println!("cargo:rustc-link-arg-bin=codex-voice-host=-Wl,-rpath,$ORIGIN/../lib");
    }
}
