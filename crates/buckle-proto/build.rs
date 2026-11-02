fn main() -> Result<(), Box<dyn std::error::Error>> {
    // protoc is vendored so that no system package is required to build Buckle.
    // SAFETY: build scripts run single-threaded before any other code in this process.
    unsafe { std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?) };
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["proto/buckle/v1/fleet.proto"], &["proto"])?;
    println!("cargo:rerun-if-changed=proto/buckle/v1/fleet.proto");
    Ok(())
}
