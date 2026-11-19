//! Thread-local re-entrancy guards for Smithay seat handles.
//!
//! Smithay stores every seat handle behind a plain `std::sync::Mutex`:
//!
//! * `Seat::get_keyboard`, [`Seat::get_pointer`](smithay::input::Seat::get_pointer) and
//!   `Seat::get_touch` share one `SeatRc::inner` mutex,
//! * `KeyboardHandle` / `PointerHandle` / `TouchHandle` each own a second, non-reentrant
//!   internal mutex that Smithay holds while it invokes *compositor callbacks*
//!   (`SeatHandler::focus_changed`, `SeatHandler::cursor_image`, `PointerGrab::*`),
//! * `Seat::text_input` / `Seat::input_method` allocate their handle through the seat's
//!   `UserDataMap` and the handles own another `Mutex` that Smithay holds while it invokes
//!   `InputMethodHandler::{new_popup,dismiss_popup,popup_repositioned}`.
//!
//! Relocking any of those mutexes from inside its own callback wedges the single event-loop
//! thread forever (komawm/koma#37, #54, #64). Rust cannot express "already borrowed" for these
//! types, so Koma tracks ownership itself:
//!
//! * [`acquire`] is used by the `KomaState::kb` / `ptr` / `touch` / `text_input` / `input_method`
//!   helpers. It fails soft (returns `None` and logs both call sites) when the same handle kind
//!   is already owned on this thread.
//! * [`callback`] marks a scope where Smithay *already* holds a handle mutex while calling into
//!   Koma. It never fails, because nested callback entry (for example `cursor_image` from inside
//!   a pointer grab) is normal.
//!
//! Cost on the healthy path: one thread-local read, one comparison, one store. No allocation and
//! no logging. The instrumentation only becomes observable when a re-entrant acquisition is
//! attempted, which is precisely the state that used to hang the compositor.

use std::{cell::RefCell, fmt, marker::PhantomData, ops::Deref, panic::Location};

use tracing::error;

/// A Smithay seat handle whose backing `Mutex` is not reentrant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeatHandle {
    Keyboard,
    Pointer,
    Touch,
    TextInput,
    InputMethod,
}

impl SeatHandle {
    /// Number of tracked handle kinds (one thread-local depth slot each).
    pub const COUNT: usize = 5;

    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Keyboard => 0,
            Self::Pointer => 1,
            Self::Touch => 2,
            Self::TextInput => 3,
            Self::InputMethod => 4,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Keyboard => "keyboard",
            Self::Pointer => "pointer",
            Self::Touch => "touch",
            Self::TextInput => "text-input",
            Self::InputMethod => "input-method",
        }
    }
}

#[derive(Clone, Copy)]
struct Slot {
    /// How many nested scopes of this handle kind the current thread owns.
    depth: u32,
    /// Call site that owns the outermost scope, for the re-entrancy diagnostic.
    site: Option<Location<'static>>,
}

struct Registry {
    slots: [Slot; SeatHandle::COUNT],
}

impl Registry {
    const fn new() -> Self {
        Self {
            slots: [Slot {
                depth: 0,
                site: None,
            }; SeatHandle::COUNT],
        }
    }
}

thread_local! {
    static REGISTRY: RefCell<Registry> = const { RefCell::new(Registry::new()) };
}

/// RAII ownership marker for one seat handle kind on the current thread.
pub struct SeatScope {
    kind: SeatHandle,
}

impl Drop for SeatScope {
    fn drop(&mut self) {
        REGISTRY.with(|cell| {
            let mut registry = cell.borrow_mut();
            let slot = &mut registry.slots[self.kind.index()];
            slot.depth = slot.depth.saturating_sub(1);
            if slot.depth == 0 {
                slot.site = None;
            }
        });
    }
}

/// Declares that this thread is about to touch `kind`.
///
/// Returns `None` when the same handle kind is already owned by this thread. The caller must
/// then skip the work instead of blocking on a mutex that will never be released: the outer
/// owner is the only frame that can finish, and it is waiting for us.
#[track_caller]
#[must_use]
pub fn acquire(kind: SeatHandle) -> Option<SeatScope> {
    try_acquire(kind, *Location::caller())
}

fn try_acquire(kind: SeatHandle, site: Location<'static>) -> Option<SeatScope> {
    let outer = REGISTRY.with(|cell| {
        let mut registry = cell.borrow_mut();
        let slot = &mut registry.slots[kind.index()];
        if slot.depth > 0 {
            slot.site
        } else {
            slot.depth = 1;
            slot.site = Some(site);
            None
        }
    });

    match outer {
        None => Some(SeatScope { kind }),
        Some(outer) => {
            error!(
                handle = kind.label(),
                outer_call_site = %CallSite(Some(outer)),
                inner_call_site = %CallSite(Some(site)),
                "seat handle re-entered on the same thread; skipping nested acquisition instead \
                 of deadlocking the event loop"
            );
            None
        }
    }
}

/// Marks a scope where Smithay already holds a handle mutex while calling into Koma.
///
/// Nested entry is normal (`cursor_image` is delivered from inside a pointer grab), so this
/// never fails. It only makes later [`acquire`] calls of the same kind fail soft.
#[must_use]
pub fn callback(kind: SeatHandle) -> SeatScope {
    REGISTRY.with(|cell| {
        let mut registry = cell.borrow_mut();
        let slot = &mut registry.slots[kind.index()];
        slot.depth = slot.depth.saturating_add(1);
        if slot.site.is_none() {
            slot.site = Some(*Location::caller());
        }
    });
    SeatScope { kind }
}

/// Returns `true` when `kind` is already owned or inside an active Smithay callback on this thread.
#[must_use]
pub fn is_held(kind: SeatHandle) -> bool {
    REGISTRY.with(|cell| cell.borrow().slots[kind.index()].depth > 0)
}

/// Convenience alias so call sites read as `let _held = seat_guard::pointer_callback();`.
#[must_use]
pub fn keyboard_callback() -> SeatScope {
    callback(SeatHandle::Keyboard)
}

#[must_use]
pub fn pointer_callback() -> SeatScope {
    callback(SeatHandle::Pointer)
}

#[must_use]
pub fn text_input_callback() -> SeatScope {
    callback(SeatHandle::TextInput)
}

#[must_use]
pub fn input_method_callback() -> SeatScope {
    callback(SeatHandle::InputMethod)
}

/// Handle plus its owning [`SeatScope`], dropped together.
///
/// Deliberately `!Send`: the ownership bookkeeping lives in a thread-local registry, and moving
/// a guard to another thread would corrupt it.
pub struct SeatHandleGuard<T> {
    handle: T,
    _scope: SeatScope,
    _not_send: PhantomData<*const ()>,
}

impl<T> SeatHandleGuard<T> {
    #[must_use]
    pub fn new(handle: T, scope: SeatScope) -> Self {
        Self {
            handle,
            _scope: scope,
            _not_send: PhantomData,
        }
    }
}

impl<T> Deref for SeatHandleGuard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.handle
    }
}

struct CallSite(Option<Location<'static>>);

impl fmt::Display for CallSite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(location) => write!(f, "{}:{}", location.file(), location.line()),
            None => f.write_str("<unknown>"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, time::Duration};

    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(2);

    #[test]
    fn nested_acquisition_is_detected_and_does_not_deadlock() {
        let (tx, rx) = mpsc::channel::<u8>();

        std::thread::spawn(move || {
            let outer = acquire(SeatHandle::Keyboard);
            let nested = acquire(SeatHandle::Keyboard);
            // 1: outer acquired, 2: nested rejected
            tx.send(u8::from(outer.is_some()) + u8::from(nested.is_some()))
                .ok();
            drop(nested);
            // The outer scope is still owned: a second nested attempt is still rejected.
            let again_nested = acquire(SeatHandle::Keyboard);
            tx.send(u8::from(again_nested.is_none())).ok();
            drop(outer);
            // Once the owner is gone the handle is available again.
            let reopened = acquire(SeatHandle::Keyboard);
            tx.send(u8::from(reopened.is_some())).ok();
        });

        assert_eq!(
            rx.recv_timeout(TIMEOUT).expect("nested acquisition hung"),
            1,
            "outer acquisition must succeed and the nested one must fail soft"
        );
        assert_eq!(
            rx.recv_timeout(TIMEOUT).expect("second acquisition hung"),
            1,
            "nested acquisition must stay rejected while the outer scope is alive"
        );
        assert_eq!(
            rx.recv_timeout(TIMEOUT)
                .expect("acquisition after release hung"),
            1,
            "dropping the outer scope must release the handle kind"
        );
    }

    #[test]
    fn callback_scope_rejects_acquisitions_of_the_same_kind_only() {
        let (tx, rx) = mpsc::channel::<(bool, bool)>();

        std::thread::spawn(move || {
            let held = pointer_callback();
            let blocked = acquire(SeatHandle::Pointer);
            let allowed = acquire(SeatHandle::Keyboard);
            tx.send((blocked.is_none(), allowed.is_some())).ok();
            drop(allowed);
            drop(held);
        });

        assert_eq!(
            rx.recv_timeout(TIMEOUT)
                .expect("callback scope acquisition hung"),
            (true, true),
            "a callback scope must block only its own handle kind"
        );
    }

    #[test]
    fn nested_callback_entry_is_accepted() {
        let (tx, rx) = mpsc::channel::<(bool, bool)>();

        std::thread::spawn(move || {
            let outer = keyboard_callback();
            let inner = keyboard_callback();
            let blocked = acquire(SeatHandle::Keyboard);
            drop(inner);
            drop(outer);
            let reopened = acquire(SeatHandle::Keyboard);
            tx.send((blocked.is_none(), reopened.is_some())).ok();
        });

        assert_eq!(
            rx.recv_timeout(TIMEOUT)
                .expect("nested callback entry hung"),
            (true, true),
            "nested callbacks must nest silently and fully unwind"
        );
    }

    #[test]
    fn guard_derefs_to_the_wrapped_handle() {
        let guard = SeatHandleGuard::new(41_u32, acquire(SeatHandle::Touch).expect("scope"));
        assert_eq!(*guard + 1, 42);
    }

    #[test]
    fn is_held_tracks_active_acquire_and_callback_scopes() {
        assert!(!is_held(SeatHandle::Pointer));
        {
            let _acq = acquire(SeatHandle::Pointer).expect("outer pointer acquire");
            assert!(is_held(SeatHandle::Pointer));
        }
        assert!(!is_held(SeatHandle::Pointer));
        {
            let _cb = pointer_callback();
            assert!(is_held(SeatHandle::Pointer));
        }
        assert!(!is_held(SeatHandle::Pointer));
    }
}
