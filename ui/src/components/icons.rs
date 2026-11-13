//! Inline, stroked `currentColor` icons, so each inherits the colour of the
//! text around it and the theme needs no second set.

use yew::prelude::*;

fn icon(children: Html) -> Html {
    html! {
        <svg class="icon" viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor"
            stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"
            focusable="false">
            { children }
        </svg>
    }
}

pub fn close() -> Html {
    icon(html! { <>
        <line x1="6" y1="6" x2="18" y2="18" />
        <line x1="18" y1="6" x2="6" y2="18" />
    </> })
}

pub fn external() -> Html {
    icon(html! { <>
        <path d="M14 4h6v6" />
        <line x1="10" y1="14" x2="20" y2="4" />
        <path d="M19 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V6a1 1 0 0 1 1-1h5" />
    </> })
}

/// Claude's spark, in its own colour: Claude's avatar wherever it authors.
pub fn claude() -> Html {
    html! {
        <svg class="icon icon--claude" viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="#d97757"
            stroke-width="2.6" stroke-linecap="round" aria-hidden="true" focusable="false">
        <line x1="12.0" y1="9.8" x2="12.0" y2="2.0" />
        <line x1="13.1" y1="10.1" x2="16.3" y2="4.6" />
        <line x1="13.9" y1="10.9" x2="20.3" y2="7.2" />
        <line x1="14.2" y1="12.0" x2="20.2" y2="12.0" />
        <line x1="13.9" y1="13.1" x2="20.7" y2="17.0" />
        <line x1="13.1" y1="13.9" x2="16.4" y2="19.6" />
        <line x1="12.0" y1="14.2" x2="12.0" y2="21.4" />
        <line x1="10.9" y1="13.9" x2="7.8" y2="19.3" />
        <line x1="10.1" y1="13.1" x2="3.5" y2="16.9" />
        <line x1="9.8" y1="12.0" x2="3.4" y2="12.0" />
        <line x1="10.1" y1="10.9" x2="4.0" y2="7.4" />
        <line x1="10.9" y1="10.1" x2="7.8" y2="4.8" />
        </svg>
    }
}

pub fn lock() -> Html {
    icon(html! { <>
        <rect x="5" y="11" width="14" height="10" rx="2" />
        <path d="M8 11V7a4 4 0 0 1 8 0v4" />
    </> })
}

pub fn check() -> Html {
    icon(html! { <polyline points="5 12 10 17 19 7" /> })
}

/// Points right; turned a quarter down by the stylesheet when its section is open.
pub fn chevron() -> Html {
    icon(html! { <polyline points="9 6 15 12 9 18" /> })
}

pub fn clock() -> Html {
    icon(html! { <>
        <circle cx="12" cy="12" r="9" />
        <polyline points="12 7 12 12 15 14" />
    </> })
}

pub fn stopwatch() -> Html {
    icon(html! { <>
        <circle cx="12" cy="14" r="7" />
        <polyline points="12 10 12 14 14.5 15.5" />
        <line x1="10" y1="3" x2="14" y2="3" />
        <line x1="12" y1="3" x2="12" y2="7" />
        <line x1="18" y1="7" x2="19.5" y2="5.5" />
    </> })
}

pub fn pause() -> Html {
    icon(html! { <>
        <line x1="9" y1="5" x2="9" y2="19" />
        <line x1="15" y1="5" x2="15" y2="19" />
    </> })
}

pub fn heart() -> Html {
    icon(html! { <path d="M3 12h4l2-5 4 10 2-5h6" /> })
}

pub fn alert() -> Html {
    icon(html! { <>
        <path d="M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z" />
        <line x1="12" y1="9" x2="12" y2="13" />
        <line x1="12" y1="17" x2="12.01" y2="17" />
    </> })
}

pub fn refresh() -> Html {
    icon(html! { <>
        <path d="M20 11a8 8 0 0 0-14.9-3.9L4 9" />
        <path d="M4 4v5h5" />
        <path d="M4 13a8 8 0 0 0 14.9 3.9L20 15" />
        <path d="M20 20v-5h-5" />
    </> })
}

pub fn key() -> Html {
    icon(html! { <>
        <circle cx="8" cy="15" r="4" />
        <path d="M11 12l9-9M17 6l3 3M15 8l2 2" />
    </> })
}

pub fn plus() -> Html {
    icon(html! { <>
        <line x1="12" y1="5" x2="12" y2="19" />
        <line x1="5" y1="12" x2="19" y2="12" />
    </> })
}

/// A change that creates something: a circled plus.
pub fn create() -> Html {
    icon(html! { <>
        <circle cx="12" cy="12" r="9" />
        <line x1="12" y1="8" x2="12" y2="16" />
        <line x1="8" y1="12" x2="16" y2="12" />
    </> })
}

/// A change that modifies something in place: a circled pencil stroke.
pub fn modify() -> Html {
    icon(html! { <>
        <circle cx="12" cy="12" r="9" />
        <path d="M9 15l1-3 4.5-4.5 2 2L12 14z" />
    </> })
}

/// A change that removes something: a circled minus.
pub fn remove() -> Html {
    icon(html! { <>
        <circle cx="12" cy="12" r="9" />
        <line x1="8" y1="12" x2="16" y2="12" />
    </> })
}

/// A step that cannot run: a circled cross.
pub fn blocked() -> Html {
    icon(html! { <>
        <circle cx="12" cy="12" r="9" />
        <line x1="9" y1="9" x2="15" y2="15" />
        <line x1="15" y1="9" x2="9" y2="15" />
    </> })
}

/// A host-specific value (a fact placeholder).
pub fn host() -> Html {
    icon(html! { <>
        <rect x="3" y="4" width="18" height="7" rx="1.5" />
        <rect x="3" y="13" width="18" height="7" rx="1.5" />
        <line x1="7" y1="7.5" x2="7.01" y2="7.5" />
        <line x1="7" y1="16.5" x2="7.01" y2="16.5" />
    </> })
}

pub fn copy() -> Html {
    icon(html! { <>
        <rect x="9" y="9" width="12" height="12" rx="2" />
        <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" />
    </> })
}

pub fn trace() -> Html {
    icon(html! { <>
        <line x1="4" y1="6" x2="14" y2="6" />
        <line x1="8" y1="12" x2="20" y2="12" />
        <line x1="11" y1="18" x2="17" y2="18" />
    </> })
}

/// The mark: a buckle, two loops through a bar.
pub fn logo() -> Html {
    html! {
        <svg class="logo" viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor"
            stroke-width="2" stroke-linecap="round" aria-hidden="true" focusable="false">
            <rect x="3" y="6" width="18" height="12" rx="3" />
            <line x1="12" y1="3" x2="12" y2="21" />
        </svg>
    }
}

/// An operator's action.
pub fn bolt() -> Html {
    icon(html! { <polygon points="13 2 4 14 11 14 10 22 19 10 12 10 13 2" /> })
}
