fn main() -> Result<(), Box<dyn std::error::Error>> {
    // protoc is vendored so that no system package is required to build Buckle.
    // SAFETY: build scripts run single-threaded before any other code in this process.
    unsafe { std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?) };
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        // Messages carrying secret values or tokens get hand-written Debug
        // impls (src/lib.rs) that print fingerprints only.
        .skip_debug([
            ".buckle.v1.SecretValue",
            ".buckle.v1.SecretBundle",
            ".buckle.v1.Welcome",
            ".buckle.v1.Assign",
            ".buckle.v1.ClusterTarget",
            ".buckle.v1.Hello",
            ".buckle.v1.HostUpdate",
        ])
        .compile_protos(&["proto/buckle/v1/fleet.proto"], &["proto"])?;
    println!("cargo:rerun-if-changed=proto/buckle/v1/fleet.proto");
    Ok(())
}
