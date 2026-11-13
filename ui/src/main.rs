//! The Buckle web UI (design §12).
//!
//! A Yew application compiled to WebAssembly with Trunk and embedded in the
//! server. It talks to `/api/v1` and nothing else, so the same bundle works
//! behind any host name.
//!
//! # Demo mode
//!
//! Appending `?demo` to any URL serves every API call from the fixtures in
//! [`fixtures`] (debug builds only), which is how the UI is reviewed without a
//! server and how the end-to-end tests drive it.

mod api;
mod app;
mod auth;
mod components;
mod fixtures;
mod hooks;
mod logic;
mod pages;
mod store;
mod util;

fn main() {
    console_error_panic_hook::set_once();
    wasm_logger::init(wasm_logger::Config::default());
    yewi::install_styles();
    yew::Renderer::<app::App>::new().render();
}
