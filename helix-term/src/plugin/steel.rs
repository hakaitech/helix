//! Steel adapter for the plugin host.
//!
//! This file is the entire scope of "Steel-aware code" in the tree. Every
//! Rust function exposed to script-land is registered here; every Steel
//! call that crosses back into Helix routes through [`super::scope`].
//!
//! Slice 1 added the logging bindings (`helix.status`, `helix.error`,
//! `helix.log`). Slice 2 (this) adds command registration:
//! `helix.register-command` plus the slotmap that stores script-side
//! callables and the drain mechanism that surfaces them to the host.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use slotmap::{new_key_type, Key, KeyData, SlotMap};
use steel::rvals::SteelVal;
use steel::steel_vm::engine::Engine;
// `register_fn` is supplied by a trait that lives in a separate module.
// Importing it brings the method into scope on `Engine`.
use steel::steel_vm::register_fn::RegisterFn;

use super::events::SUPPORTED_EVENTS;
use super::scope::with_editor;
use super::PluginCommand;

new_key_type! { struct CallableKey; }

/// State shared between the engine struct and the closures it installs in
/// Steel. Each `Arc<Mutex<_>>` is one logical channel of communication:
///
/// * `callables` — script-side closures keyed by an opaque id. Populated by
///   `helix.register-command`; called by `call_command`.
/// * `pending_commands` — newly registered commands waiting to be surfaced
///   to [`super::ScriptingHost`]. Drained once per init/reload via
///   [`SteelEngine::drain_pending_commands`].
/// * `command_keys` — name → key index so the host can resolve a typable
///   name back to the callable it should invoke.
#[derive(Default)]
struct SharedState {
    callables: SlotMap<CallableKey, SteelVal>,
    pending_commands: Vec<PluginCommand>,
    command_keys: HashMap<String, CallableKey>,
    /// Script-registered event hooks, keyed by event name. Multiple hooks
    /// per event are supported; they fire in registration order.
    event_hooks: HashMap<String, Vec<CallableKey>>,
}

pub struct SteelEngine {
    inner: Engine,
    state: Arc<Mutex<SharedState>>,
}

impl SteelEngine {
    pub fn new() -> Result<Self> {
        let mut inner = Engine::new();
        let state = Arc::new(Mutex::new(SharedState::default()));
        register_logging_bindings(&mut inner);
        register_command_bindings(&mut inner, &state);
        register_hook_bindings(&mut inner, &state);
        register_document_bindings(&mut inner);
        register_state_bindings(&mut inner);
        Ok(Self { inner, state })
    }

    /// Load and evaluate a Steel source file. Caller must already be inside
    /// a [`super::scope::scope`] if the script will touch the editor.
    pub fn run_file(&mut self, path: &Path) -> Result<()> {
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("reading plugin init file `{}`", path.display()))?;
        // Steel's evaluator wants `Into<Cow<'static, str>>`; passing `source`
        // by value satisfies that via `String -> Cow::Owned`. The path-bearing
        // variant attaches the file path to compile/runtime diagnostics so
        // script errors point at the user's `init.scm`, not at a synthetic
        // anonymous span.
        self.inner
            .compile_and_run_raw_program_with_path(source, path.to_path_buf())
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("steel: {e}"))
            .with_context(|| format!("evaluating plugin init file `{}`", path.display()))
    }

    /// Drain commands registered since the last call. Called by the host
    /// after `run_file` / reload to surface new typables atomically.
    pub fn drain_pending_commands(&mut self) -> Vec<PluginCommand> {
        std::mem::take(&mut self.state.lock().pending_commands)
    }

    /// Look up a registered command by its script-side name and invoke its
    /// callable with zero arguments. Caller is responsible for being inside
    /// a [`super::scope::scope`] before calling — otherwise editor bindings
    /// inside the callable will panic.
    pub fn call_command(&mut self, name: &str) -> Result<()> {
        let callable = {
            let state = self.state.lock();
            let key = state
                .command_keys
                .get(name)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("no such plugin command: {name}"))?;
            state
                .callables
                .get(key)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("plugin command callable is stale"))?
        };
        self.inner
            .call_function_with_args(callable, Vec::new())
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("plugin '{name}' raised: {e}"))
    }

    /// Human-readable summary of registered event hooks, e.g.
    /// `"DocumentDidOpen:1, OnModeSwitch:2"`. Surfaced in the init log so
    /// users can sanity-check that their hooks bound where they expected.
    pub fn hook_summary(&self) -> String {
        let state = self.state.lock();
        let mut parts: Vec<_> = state
            .event_hooks
            .iter()
            .map(|(name, keys)| format!("{name}:{}", keys.len()))
            .collect();
        parts.sort();
        parts.join(", ")
    }

    /// Return the opaque keys of every hook registered against an event
    /// name. The host iterates over these and calls `call_hook` per key.
    /// Encoded as `u64` so the host doesn't need to know about
    /// `CallableKey`.
    pub fn hook_keys(&self, event_name: &str) -> Vec<u64> {
        let state = self.state.lock();
        state
            .event_hooks
            .get(event_name)
            .map(|keys| keys.iter().map(|k| k.data().as_ffi()).collect())
            .unwrap_or_default()
    }

    /// Invoke a previously stored hook callable. The host passes a key
    /// obtained from [`Self::hook_keys`].
    pub fn call_hook(&mut self, key: u64) -> Result<()> {
        let callable = {
            let state = self.state.lock();
            let key = CallableKey::from(KeyData::from_ffi(key));
            state
                .callables
                .get(key)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("plugin hook callable is stale"))?
        };
        self.inner
            .call_function_with_args(callable, Vec::new())
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("plugin hook raised: {e}"))
    }
}

/// Register the three logging-shaped functions exposed to scripts.
///
/// Naming: `helix.<verb>` matches Steel's convention for grouped namespaces
/// in builtins. The dot is not a special character in Steel identifiers
/// (Scheme allows it) — it is part of the public name.
fn register_logging_bindings(engine: &mut Engine) {
    engine.register_fn("helix.status", |msg: String| {
        with_editor(|editor| editor.set_status(msg));
    });

    engine.register_fn("helix.error", |msg: String| {
        with_editor(|editor| editor.set_error(msg));
    });

    engine.register_fn("helix.log", |msg: String| {
        // Routes to the standard `log` crate so it lands in `~/.cache/helix/helix.log`
        // alongside the editor's own logs.
        log::info!(target: "helix::plugin", "{msg}");
    });
}

/// Register `helix.register-command`. The closure captures a clone of the
/// shared state Arc; mutation happens under the Mutex.
///
/// Signature: `(helix.register-command name doc callable)`
///   - `name`     : string — the script-side name; bound in keymaps as `plugin:<name>`
///   - `doc`      : string — short description shown in completions / help
///   - `callable` : procedure — a thunk (nullary closure) called when the
///     command fires. For slice 2 the thunk takes no args;
///     slice 3 will introduce arg-bearing event callbacks.
fn register_command_bindings(engine: &mut Engine, state: &Arc<Mutex<SharedState>>) {
    let state = Arc::clone(state);
    engine.register_fn(
        "helix.register-command",
        move |name: String, doc: String, callable: SteelVal| {
            let mut guard = state.lock();
            // If the same name is registered twice (e.g. by a reloaded
            // init.scm), the latest registration wins. We deliberately
            // leave the stale callable in `callables` until the engine is
            // dropped — Steel handles its own GC pressure.
            let key = guard.callables.insert(callable);
            guard.command_keys.insert(name.clone(), key);
            guard.pending_commands.push(PluginCommand { name, doc });
        },
    );
}

/// Register `helix.register-hook`. Script signature:
///   `(helix.register-hook event-name callable)`
///
/// where `event-name` is one of the strings in
/// [`super::events::SUPPORTED_EVENTS`]. Unknown event names are rejected
/// loudly via `helix.error` — a typo here would otherwise silently never
/// fire and be very confusing to debug.
fn register_hook_bindings(engine: &mut Engine, state: &Arc<Mutex<SharedState>>) {
    let state = Arc::clone(state);
    engine.register_fn(
        "helix.register-hook",
        move |event_name: String, callable: SteelVal| {
            if !SUPPORTED_EVENTS.contains(&event_name.as_str()) {
                let supported = SUPPORTED_EVENTS.join(", ");
                // We cannot return Result from a register_fn closure in a
                // way that propagates back into the Scheme error system
                // generically — easiest path is to log and surface via the
                // editor status. The script keeps running; the missing
                // registration will be obvious to the user.
                log::error!(
                    "helix.register-hook: unknown event '{event_name}'. Supported: {supported}"
                );
                with_editor(|editor| {
                    editor.set_error(format!(
                        "plugin: unknown event '{event_name}' (supported: {supported})"
                    ))
                });
                return;
            }
            let mut guard = state.lock();
            let key = guard.callables.insert(callable);
            guard.event_hooks.entry(event_name).or_default().push(key);
        },
    );
}

/// Register bindings that read or mutate the current document.
///
/// All editor accesses go through [`with_editor`] — these functions are
/// only safe to call from inside [`super::scope::scope`]; outside of one
/// they panic. That's a host-level invariant (every entry point from
/// script-land must establish a scope first).
fn register_document_bindings(engine: &mut Engine) {
    engine.register_fn("doc/text", || -> String {
        with_editor(|editor| {
            let (_, doc) = current_ref!(editor);
            doc.text().to_string()
        })
    });

    engine.register_fn("doc/path", || -> String {
        with_editor(|editor| {
            let (_, doc) = current_ref!(editor);
            doc.path()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        })
    });

    engine.register_fn("doc/line-count", || -> usize {
        with_editor(|editor| {
            let (_, doc) = current_ref!(editor);
            doc.text().len_lines()
        })
    });

    engine.register_fn("doc/insert", |text: String| {
        with_editor(|editor| {
            let (view, doc) = current!(editor);
            let tx = helix_core::Transaction::insert(
                doc.text(),
                &doc.selection(view.id).clone(),
                text.into(),
            );
            doc.apply(&tx, view.id);
        });
    });

    engine.register_fn("sel/primary-anchor", || -> usize {
        with_editor(|editor| {
            let (view, doc) = current_ref!(editor);
            doc.selection(view.id).primary().anchor
        })
    });

    engine.register_fn("sel/primary-head", || -> usize {
        with_editor(|editor| {
            let (view, doc) = current_ref!(editor);
            doc.selection(view.id).primary().head
        })
    });
}

/// Register bindings that report editor state outside of any specific
/// document — useful inside event hooks where the script wants to know
/// which mode it's in / which doc fired the event.
fn register_state_bindings(engine: &mut Engine) {
    engine.register_fn("helix.current-mode", || -> String {
        with_editor(|editor| match editor.mode() {
            helix_view::document::Mode::Normal => "normal".to_string(),
            helix_view::document::Mode::Insert => "insert".to_string(),
            helix_view::document::Mode::Select => "select".to_string(),
        })
    });

    engine.register_fn("helix.current-doc-id", || -> u64 {
        with_editor(|editor| {
            let (_, doc) = current_ref!(editor);
            // DocumentId wraps NonZeroUsize. We expose it as a u64 for
            // Scheme-side identity checks ("did THIS document change?").
            // Stable for the document's lifetime.
            doc.id().to_string().parse::<u64>().unwrap_or(0)
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test for the slice 2 wiring: a script that registers two
    /// commands should surface them via drain_pending_commands with the
    /// names and docs intact. Exercises the round-trip through Steel
    /// without needing a real Editor — `helix.log` does not touch the
    /// editor scope so we can evaluate the script unscoped.
    #[test]
    fn register_command_round_trips_through_drain() {
        let mut engine = SteelEngine::new().expect("engine init");
        engine
            .inner
            .compile_and_run_raw_program(
                r#"
                (helix.register-command "greet"
                                        "Say hello"
                                        (lambda () (helix.log "greet was called")))
                (helix.register-command "farewell"
                                        "Say goodbye"
                                        (lambda () (helix.log "farewell was called")))
                "#,
            )
            .expect("script ran");

        let cmds = engine.drain_pending_commands();
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0].name, "greet");
        assert_eq!(cmds[0].doc, "Say hello");
        assert_eq!(cmds[1].name, "farewell");
        assert_eq!(cmds[1].doc, "Say goodbye");

        // A second drain on the same engine returns nothing — the queue
        // is single-shot per registration burst.
        assert!(engine.drain_pending_commands().is_empty());
    }

    /// End-to-end verification that the stored callable survives the
    /// round-trip from Steel → slotmap → call_hook → Steel. This is the
    /// critical correctness property for slice 3: the typed `register_hook!`
    /// bridge in events.rs calls `ScriptingHost::fire_hooks`, which calls
    /// `engine.call_hook(key)`. If that path is broken at any layer, hooks
    /// silently never run.
    ///
    /// The script-side closure mutates a Steel-side counter so we can
    /// observe whether the body actually ran. Reads back the counter via
    /// `(begin counter)` evaluating to the current value.
    #[test]
    fn call_hook_runs_the_stored_callable_body() {
        let mut engine = SteelEngine::new().expect("engine init");
        engine
            .inner
            .compile_and_run_raw_program(
                r#"
                (define counter 0)
                (helix.register-hook "OnModeSwitch"
                                     (lambda () (set! counter (+ counter 1))))
                "#,
            )
            .expect("setup script");

        let keys = engine.hook_keys("OnModeSwitch");
        assert_eq!(keys.len(), 1);

        engine.call_hook(keys[0]).expect("first invocation");
        engine.call_hook(keys[0]).expect("second invocation");
        engine.call_hook(keys[0]).expect("third invocation");

        // Re-evaluate `counter` to read its current value.
        let result = engine
            .inner
            .compile_and_run_raw_program("counter")
            .expect("read counter");
        let last = result.last().expect("at least one value");
        match last {
            SteelVal::IntV(n) => assert_eq!(*n, 3, "counter should be 3 after 3 fires"),
            other => panic!("expected IntV(3), got {other:?}"),
        }
    }

    /// Hook registration stores keys under the event name and exposes
    /// them via hook_keys. Unknown event names are rejected silently (we
    /// can't propagate the error through Steel's register_fn return path
    /// cleanly, but we log + status-bar it).
    #[test]
    fn register_hook_stores_callable_under_event_name() {
        let mut engine = SteelEngine::new().expect("engine init");
        engine
            .inner
            .compile_and_run_raw_program(
                r#"
                (helix.register-hook "DocumentDidOpen"
                                     (lambda () (helix.log "doc opened")))
                (helix.register-hook "DocumentDidOpen"
                                     (lambda () (helix.log "second doc-open handler")))
                (helix.register-hook "PostCommand"
                                     (lambda () (helix.log "post command")))
                "#,
            )
            .expect("script ran");

        let open_keys = engine.hook_keys("DocumentDidOpen");
        assert_eq!(
            open_keys.len(),
            2,
            "both DocumentDidOpen hooks should register"
        );
        let post_keys = engine.hook_keys("PostCommand");
        assert_eq!(post_keys.len(), 1);
        // No registrations for events the script didn't subscribe to.
        assert!(engine.hook_keys("OnModeSwitch").is_empty());
    }

    /// Re-registering the same name keeps the most recent callable and
    /// pushes a second pending entry (the host's typables HashMap will
    /// de-dup by name on the surfacing side).
    #[test]
    fn re_registration_replaces_callable() {
        let mut engine = SteelEngine::new().expect("engine init");
        engine
            .inner
            .compile_and_run_raw_program(
                r#"
                (helix.register-command "greet" "v1" (lambda () (helix.log "v1")))
                (helix.register-command "greet" "v2" (lambda () (helix.log "v2")))
                "#,
            )
            .expect("script ran");

        let state = engine.state.lock();
        // The latest binding wins in command_keys, so call_command would
        // hit v2's callable.
        let key = state.command_keys.get("greet").expect("key for greet");
        assert!(state.callables.contains_key(*key));
        // Both registrations are still in pending — the host de-dups by
        // name when surfacing them into its typables map.
        assert_eq!(state.pending_commands.len(), 2);
    }
}
