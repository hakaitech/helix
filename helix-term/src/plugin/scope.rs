//! Scoped thread-local that lets engine bindings reach `&mut Editor`
//! during a synchronous call from script-land.
//!
//! Soundness rests on three properties:
//!
//! 1. **Synchronous scope.** [`scope`] takes a closure and restores the
//!    previous slot value before returning. The pointer is therefore never
//!    held across an `await` or any other suspension point.
//! 2. **Stacked entries.** The previous slot value is snapshotted on entry
//!    and restored on exit, so nested `scope(...)` calls compose without
//!    aliasing — each inner call shadows the outer pointer for its body
//!    only.
//! 3. **Loud failure on misuse.** [`with_editor`] panics if the slot is
//!    empty. The borrow checker cannot model "this pointer is live for the
//!    duration of this synchronous call," so the runtime check is what
//!    catches script-call paths that haven't been wrapped in `scope`.
//!
//! The `unsafe` block inside `with_editor` is the only unsafe code in the
//! plugin subsystem. Its reasoning is documented inline.

use std::cell::Cell;

use helix_view::Editor;

thread_local! {
    /// Pointer to a borrowed `&mut Editor`, or `None` if no script call is
    /// in flight on this thread. `Cell` instead of `RefCell` because we
    /// only ever swap whole `Option<*mut _>` values, never borrow the cell
    /// contents.
    static EDITOR: Cell<Option<*mut Editor>> = const { Cell::new(None) };
}

/// Park `editor` in the thread-local for the synchronous duration of `f`,
/// then restore the previous slot value.
pub(super) fn scope<R>(editor: &mut Editor, f: impl FnOnce() -> R) -> R {
    let ptr: *mut Editor = editor;
    let prev = EDITOR.with(|c| c.replace(Some(ptr)));
    let result = f();
    EDITOR.with(|c| c.set(prev));
    result
}

/// Borrow the editor from inside a script binding.
///
/// Panics if called outside a [`scope`]. That would mean a binding was
/// invoked from a code path that didn't park the editor first — a host
/// bug, not a script bug.
pub(super) fn with_editor<R>(f: impl FnOnce(&mut Editor) -> R) -> R {
    EDITOR.with(|c| {
        let ptr = c.get().expect(
            "plugin::scope::with_editor called outside a scope; \
             this is a host bug — every script entry point must wrap \
             its call in scope::scope",
        );
        // SAFETY: `ptr` was obtained from a `&mut Editor` passed to
        // `scope`. `scope` does not return until `f` finishes, so the
        // source borrow outlives this read. The thread-local guarantees
        // single-threaded access (each OS thread has its own slot).
        // Re-entrant calls compose because `scope` stacks via the
        // snapshot/restore pattern in `scope`.
        f(unsafe { &mut *ptr })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "outside a scope")]
    fn with_editor_panics_outside_scope() {
        with_editor(|_| {});
    }
}
