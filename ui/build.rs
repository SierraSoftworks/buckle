//! Writes the fonts yewi is set in (Montserrat, Inter, Fira Code) into
//! `fonts/`, which the `post_build` hook in `Trunk.toml` stages into the
//! bundle. They are generated rather than committed, so the directory is
//! ignored.

fn main() {
    // Re-run when the generated directory goes missing, not only when this
    // script changes.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=fonts");
    yewi_fonts::write_to("fonts").expect("the font files should be writable to ui/fonts");
}
