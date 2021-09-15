pub use human_errors::detailed_message;

#[cfg(unix)]
mod nix;
mod serde;
mod std_io;
mod utf8;

human_errors::error_shim!(Error);
