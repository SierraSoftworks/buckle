//! Toasts: the outcome of an action, said where it will be noticed and then
//! gone. Failures stay until dismissed, because a refusal is something the
//! operator needs to read, not catch. Drawn by yewi's toast host.

use yew::prelude::*;
use yewi::{ToastMessage, Tone};

pub use yewi::ToastHost;

/// The handle pages use to report what happened.
#[derive(Clone, PartialEq)]
pub struct Toaster(yewi::Toaster);

impl Toaster {
    pub fn ok(&self, text: impl Into<String>) {
        self.0.ok(text.into());
    }

    pub fn error(&self, text: impl Into<String>) {
        self.0
            .show(ToastMessage::new(Tone::Danger, text.into()).sticky());
    }
}

#[hook]
pub fn use_toast() -> Toaster {
    Toaster(yewi::use_toast())
}
