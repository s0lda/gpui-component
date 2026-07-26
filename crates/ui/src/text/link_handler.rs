//! An application-level hook for links clicked inside rendered text.
//!
//! Every link in a `TextView` used to go straight to [`gpui::App::open_url`], which
//! on Linux shells out to `xdg-open` — so the *desktop's* file association decided
//! what opened, and an application had no way to say "a `file://` link is a source
//! file, open it in the editor the user picked". There is no hook in gpui for this
//! (the platform's `open_url` is not swappable), so it has to live here.
//!
//! An app installs a handler with [`set_link_handler`]; returning `true` means "I
//! opened it". Anything the handler declines — and every link when no handler is
//! installed — falls through to `open_url` exactly as before, so this is additive:
//! a consumer that never calls `set_link_handler` sees no behaviour change.

use std::cell::RefCell;

type LinkHandler = Box<dyn Fn(&str) -> bool + 'static>;

thread_local! {
    static HANDLER: RefCell<Option<LinkHandler>> = const { RefCell::new(None) };
}

/// Install the handler consulted before a clicked link is handed to the platform.
///
/// Return `true` from `handler` to claim the URL, `false` to let it fall through to
/// [`gpui::App::open_url`]. Installing replaces any previous handler. Per-thread, and
/// the UI runs on one thread, so this is the application's single handler in practice.
pub fn set_link_handler(handler: impl Fn(&str) -> bool + 'static) {
    HANDLER.with(|h| *h.borrow_mut() = Some(Box::new(handler)));
}

/// Open `url`: the installed handler first, the platform if it declines.
///
/// The handler is moved out of the cell for the duration of the call so a handler
/// that itself opens a link (or reinstalls itself) cannot panic on a double borrow.
pub(crate) fn open_link(url: &str, cx: &mut gpui::App) {
    let handler = HANDLER.with(|h| h.borrow_mut().take());
    let handled = handler.as_ref().is_some_and(|f| f(url));
    HANDLER.with(|h| {
        // Only restore if nothing was installed while we were out, so a handler that
        // replaced itself mid-call keeps the NEW one.
        let mut slot = h.borrow_mut();
        if slot.is_none() {
            *slot = handler;
        }
    });
    if !handled {
        cx.open_url(url);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    /// A handler sees the URL and can claim it.
    #[test]
    fn an_installed_handler_is_consulted() {
        let seen: Rc<Cell<bool>> = Rc::default();
        let flag = seen.clone();
        set_link_handler(move |url| {
            flag.set(url == "file:///tmp/a.rs");
            true
        });
        let handler = HANDLER.with(|h| h.borrow_mut().take());
        assert!(handler.as_ref().unwrap()("file:///tmp/a.rs"));
        assert!(seen.get());
    }

    /// Declining leaves the URL for the platform — the fall-through that keeps
    /// `https://` links going to the browser.
    #[test]
    fn a_declining_handler_leaves_the_url_alone() {
        set_link_handler(|url| url.starts_with("file://"));
        let handler = HANDLER.with(|h| h.borrow_mut().take());
        let f = handler.as_ref().unwrap();
        assert!(!f("https://example.com"));
        assert!(f("file:///tmp/a.rs"));
    }
}
