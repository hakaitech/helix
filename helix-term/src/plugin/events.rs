//! Bridge between Helix's typed event system (`helix-event`) and the
//! plugin host's script-side hook registry.
//!
//! For each supported event type we install a single typed `register_hook!`
//! closure once at host init. That closure parks the editor reference into
//! [`super::scope`] and calls [`ScriptingHost::dispatch_event`], which
//! looks up the script-side callables registered against that event name
//! and invokes each one with zero arguments. Scripts read whatever state
//! they need (current document, mode, ...) via separate query bindings —
//! see `helix.current-*` in `steel.rs`.
//!
//! Why typed hooks and not `register_dynamic_hook`?
//! `register_dynamic_hook` calls `Fn() -> Result<()>` — it discards the
//! event payload (see `helix-event/src/hook.rs::new_dynamic`). Useless for
//! anything that wants editor access on dispatch.
//!
//! ## What is intentionally *not* installed
//!
//! - `DocumentDidChange` / `SelectionDidChange` — payload is
//!   `&mut Document`, not `&mut Editor`. Plugging these in needs a second
//!   TLS slot for the document; punted to a follow-up slice so this slice
//!   stays small.

use helix_event::register_hook;
use helix_view::events::{
    ConfigDidChange, DiagnosticsDidChange, DocumentDidClose, DocumentDidOpen, DocumentFocusLost,
    LanguageServerExited, LanguageServerInitialized,
};

use crate::events::{OnModeSwitch, PostCommand, PostInsertChar};

use super::scope::scope;
use super::ScriptingHost;

/// Install one typed hook per supported event. Called once from
/// `ScriptingHost::init`. Each hook is `'static + Send + Sync` (captures
/// nothing) so it can live in the helix-event registry for the rest of the
/// process lifetime — including across `:plugin-reload`, since it routes
/// through the ScriptingHost singleton rather than any specific engine
/// instance.
pub(super) fn install_typed_hooks() {
    // Events whose payload carries `editor: &mut Editor` directly.
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        scope(event.editor, || ScriptingHost::fire_hooks("DocumentDidOpen"));
        Ok(())
    });
    register_hook!(move |event: &mut DocumentDidClose<'_>| {
        scope(event.editor, || ScriptingHost::fire_hooks("DocumentDidClose"));
        Ok(())
    });
    register_hook!(move |event: &mut DocumentFocusLost<'_>| {
        scope(event.editor, || ScriptingHost::fire_hooks("DocumentFocusLost"));
        Ok(())
    });
    register_hook!(move |event: &mut DiagnosticsDidChange<'_>| {
        scope(event.editor, || {
            ScriptingHost::fire_hooks("DiagnosticsDidChange")
        });
        Ok(())
    });
    register_hook!(move |event: &mut LanguageServerInitialized<'_>| {
        scope(event.editor, || {
            ScriptingHost::fire_hooks("LanguageServerInitialized")
        });
        Ok(())
    });
    register_hook!(move |event: &mut LanguageServerExited<'_>| {
        scope(event.editor, || {
            ScriptingHost::fire_hooks("LanguageServerExited")
        });
        Ok(())
    });
    register_hook!(move |event: &mut ConfigDidChange<'_>| {
        scope(event.editor, || ScriptingHost::fire_hooks("ConfigDidChange"));
        Ok(())
    });

    // Events whose payload carries `cx: &mut commands::Context`.
    register_hook!(move |event: &mut OnModeSwitch<'_, '_>| {
        scope(event.cx.editor, || ScriptingHost::fire_hooks("OnModeSwitch"));
        Ok(())
    });
    register_hook!(move |event: &mut PostCommand<'_, '_>| {
        scope(event.cx.editor, || ScriptingHost::fire_hooks("PostCommand"));
        Ok(())
    });
    register_hook!(move |event: &mut PostInsertChar<'_, '_>| {
        scope(event.cx.editor, || ScriptingHost::fire_hooks("PostInsertChar"));
        Ok(())
    });
}

/// Names of all events the plugin host is willing to subscribe to. Used by
/// `helix.register-hook` to reject unknown event names at registration
/// time rather than letting a typo silently never fire.
pub(super) const SUPPORTED_EVENTS: &[&str] = &[
    "DocumentDidOpen",
    "DocumentDidClose",
    "DocumentFocusLost",
    "DiagnosticsDidChange",
    "LanguageServerInitialized",
    "LanguageServerExited",
    "ConfigDidChange",
    "OnModeSwitch",
    "PostCommand",
    "PostInsertChar",
];
