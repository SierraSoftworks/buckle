//! Browser plumbing shared by every page.

pub fn window() -> web_sys::Window {
    web_sys::window().expect("the UI runs in a browser window")
}

/// Percent-encodes a path segment or query value.
pub fn enc(value: &str) -> String {
    js_sys::encode_uri_component(value).into()
}

/// An href for a full-page navigation that keeps demo mode on: demo mode is
/// carried in the query string, which a hard navigation would otherwise drop.
pub fn demo_href(path: &str) -> String {
    if crate::fixtures::is_demo() {
        let sep = if path.contains('?') { '&' } else { '?' };
        format!("{path}{sep}demo")
    } else {
        path.to_string()
    }
}

/// A value from `localStorage`. A browser that refuses storage is treated as
/// having nothing stored.
pub fn stored(key: &str) -> Option<String> {
    window()
        .local_storage()
        .ok()
        .flatten()
        .and_then(|s| s.get_item(key).ok().flatten())
}

pub fn store(key: &str, value: Option<&str>) {
    if let Some(storage) = window().local_storage().ok().flatten() {
        let _ = match value {
            Some(v) => storage.set_item(key, v),
            None => storage.remove_item(key),
        };
    }
}

/// A value between 0 and 1 for jitter.
pub fn random() -> f64 {
    js_sys::Math::random()
}

/// Copies text to the clipboard, best effort.
pub fn copy(text: &str) {
    let _ = window().navigator().clipboard().write_text(text);
}
