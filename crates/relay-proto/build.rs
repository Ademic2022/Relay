fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/relay.proto");
    println!("cargo:rerun-if-changed=proto/rendezvous.proto");
    println!("cargo:rerun-if-env-changed=PROTOC");
    let mut cfg = prost_build::Config::new();
    // Vendored protoc, so builds (and CI) need no system install.
    // An explicit PROTOC env var still wins.
    if std::env::var_os("PROTOC").is_none() {
        cfg.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);
    }
    cfg.bytes(["."])
        .compile_protos(&["proto/relay.proto", "proto/rendezvous.proto"], &["proto"])?;
    Ok(())
}
