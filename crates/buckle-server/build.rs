//! Makes sure `ui/dist` exists so that `include_dir!` compiles in a checkout
//! where the UI has not been built with Trunk yet. A placeholder page tells
//! the operator how to build the real one.

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let dist = manifest.join("../../ui/dist");
    if let Err(e) = std::fs::create_dir_all(&dist) {
        println!("cargo:warning=could not create {}: {e}", dist.display());
        return;
    }
    let index = dist.join("index.html");
    if !index.exists() {
        let placeholder = "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><title>Buckle</title></head>\n<body><h1>Buckle</h1><p>The web UI was not built into this binary. Run <code>trunk build --release</code> in <code>ui/</code> and rebuild the server.</p></body></html>\n";
        if let Err(e) = std::fs::write(&index, placeholder) {
            println!("cargo:warning=could not write {}: {e}", index.display());
        }
    }
    println!("cargo:rerun-if-changed=../../ui/dist");
}
