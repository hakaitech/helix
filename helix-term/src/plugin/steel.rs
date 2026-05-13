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
use slotmap::{new_key_type, SlotMap};
use steel::rvals::SteelVal;
use steel::steel_vm::engine::Engine;
// `register_fn` is supplied by a trait that lives in a separate module.
// Importing it brings the method into scope on `Engine`.
use steel::steel_vm::register_fn::RegisterFn;

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
        // Steel's call_function_with_args wants `Vec<SteelVal>`. We pass an
        // empty vector for slice 2; arg-passing comes with slice 3 once we
        // also need to marshal payload data into event hooks.
        self.inner
            .call_function_with_args(callable, Vec::new())
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("plugin '{name}' raised: {e}"))
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
