//! Embedded scripting / plugin host for Helix.
//!
//! This module is the single integration point between Helix and any
//! embedded scripting engine. It deliberately stays small:
//!
//! * a singleton [`ScriptingHost`] that owns the engine for the editor's
//!   lifetime;
//! * a scoped thread-local ([`scope`]) that lets engine bindings reach the
//!   editor for the duration of a synchronous call;
//! * a feature-gated engine adapter ([`steel`]).
//!
//! When the `steel` Cargo feature is disabled, the entire module compiles
//! to a thin no-op so the rest of `helix-term` does not need `cfg` arms at
//! call sites — they just call `ScriptingHost::init` / `call_command` and
//! the stub returns immediately.
//!
//! See `PLUGIN_SYSTEM_DESIGN.md` at the repo root for the broader
//! architectural rationale.
#![allow(dead_code)] // many fields are only used once slice 3 lands

use std::collections::HashMap;
use std::sync::OnceLock;

use anyhow::Result;
use parking_lot::RwLock;

use crate::config::PluginsConfig;

pub(crate) mod scope;
#[cfg(feature = "steel")]
mod steel;

/// Engine alias chosen at compile time. With the `steel` feature this is the
/// concrete Steel adapter; without it, a zero-sized stub so the rest of the
/// module type-checks unchanged.
#[cfg(feature = "steel")]
type Engine = steel::SteelEngine;
#[cfg(not(feature = "steel"))]
type Engine = NullEngine;

/// Process-wide singleton. Held behind an `RwLock` so future slices (event
/// hooks installed via `register_hook!`) can route into it from `'static`
/// closures without capturing `&mut Application`. v1 only ever has a single
/// writer (the main editor thread), so the lock is uncontended in practice.
static HOST: OnceLock<RwLock<ScriptingHost>> = OnceLock::new();

pub struct ScriptingHost {
    /// `Option` so `:plugin-reload` can drop the engine cleanly without
    /// inventing a sentinel state.
    engine: Option<Engine>,
    /// Plugin-registered typable / static commands, keyed by name.
    /// Populated from script-side `helix.register-command` calls and
    /// drained from the engine after init / reload.
    typables: HashMap<String, PluginCommand>,
    /// Saved copy of the plugins config from startup. `:plugin-reload`
    /// reads this so it doesn't need access to the helix-term Config
    /// (which `compositor::Context` does not expose).
    plugins_config: Option<PluginsConfig>,
}

/// Metadata for a plugin-registered command. The actual callable lives
/// inside the engine; this struct only carries what helix-term needs to
/// surface it to the keymap / `:` completer.
#[derive(Clone, Debug)]
pub struct PluginCommand {
    pub name: String,
    pub doc: String,
}

impl ScriptingHost {
    /// Construct the host and run the user's init script. Called once from
    /// [`crate::application::Application::new`] after the editor exists but
    /// before the main loop starts.
    ///
    /// If `[plugins]` is absent from the config this is a no-op — the
    /// singleton is never installed and no Steel state is allocated.
    pub fn init(
        plugins: Option<&PluginsConfig>,
        editor: &mut helix_view::Editor,
    ) -> Result<()> {
        let Some(cfg) = plugins else { return Ok(()); };
        Self::init_inner(cfg, editor)
    }

    /// Reload the engine: drop the old one, construct a fresh one and re-run
    /// `init.scm`. Used by the `:plugin-reload` typable command.
    pub fn reload(editor: &mut helix_view::Editor) -> Result<()> {
        let Some(host) = HOST.get() else {
            anyhow::bail!("plugin host is not initialised (no [plugins] section in config?)");
        };
        let cfg = host
            .read()
            .plugins_config
            .clone()
            .ok_or_else(|| anyhow::anyhow!("plugin host has no saved config"))?;

        // Drop the old engine first so all its SteelVals release before we
        // build a new engine. This keeps peak memory low on reload.
        {
            let mut guard = host.write();
            guard.engine = None;
            guard.typables.clear();
        }

        Self::populate_inner(&cfg, editor, host)
    }

    /// List of plugin-registered command names, sorted alphabetically.
    /// Used by `:plugin-list` and (future) `:` completion.
    pub fn list_commands() -> Vec<PluginCommand> {
        let Some(host) = HOST.get() else { return Vec::new(); };
        let host = host.read();
        let mut cmds: Vec<_> = host.typables.values().cloned().collect();
        cmds.sort_by(|a, b| a.name.cmp(&b.name));
        cmds
    }

    /// Invoke a plugin-registered command. Wired in from
    /// `MappableCommand::Plugin::execute`. Returns an error which the
    /// caller surfaces via `editor.set_error`.
    pub fn call_command(name: &str, cx: &mut crate::commands::Context<'_>) -> Result<()> {
        let host = HOST
            .get()
            .ok_or_else(|| anyhow::anyhow!("plugin host not initialised"))?;
        // Check existence under read lock, drop, then call under write
        // lock — keeps the read-side critical section short.
        if !host.read().typables.contains_key(name) {
            anyhow::bail!("plugin command not registered");
        }
        scope::scope(cx.editor, || {
            let mut guard = host.write();
            let engine = guard
                .engine
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("plugin engine not running"))?;
            engine.call_command(name)
        })
    }

    /// True once `init` succeeded with an engine attached. Used by future
    /// slices to short-circuit when the host is absent.
    pub fn is_active() -> bool {
        HOST.get().map(|h| h.read().engine.is_some()).unwrap_or(false)
    }

    #[cfg(feature = "steel")]
    fn init_inner(cfg: &PluginsConfig, editor: &mut helix_view::Editor) -> Result<()> {
        // Install singleton with an empty engine first so call_command and
        // friends can route correctly even before init finishes. The engine
        // is populated below.
        let cell = HOST.get_or_init(|| {
            RwLock::new(ScriptingHost {
                engine: None,
                typables: HashMap::new(),
                plugins_config: Some(cfg.clone()),
            })
        });
        Self::populate_inner(cfg, editor, cell)
    }

    #[cfg(not(feature = "steel"))]
    fn init_inner(_cfg: &PluginsConfig, _editor: &mut helix_view::Editor) -> Result<()> {
        // Plugins requested but no engine compiled in. Surface a clear log
        // line rather than silently doing nothing — this is almost always a
        // build mistake when someone sets `[plugins]` but didn't pass
        // `--features steel`.
        log::warn!(
            "plugin host: [plugins] is configured but this `hx` binary was \
             built without `--features steel`; plugin scripts will not run"
        );
        Ok(())
    }

    #[cfg(feature = "steel")]
    fn populate_inner(
        cfg: &PluginsConfig,
        editor: &mut helix_view::Editor,
        cell: &RwLock<ScriptingHost>,
    ) -> Result<()> {
        let mut engine = steel::SteelEngine::new()?;
        scope::scope(editor, || -> Result<()> {
            if let Some(init) = cfg.init.as_deref() {
                engine.run_file(init)?;
            }
            Ok(())
        })?;

        // Drain commands registered by init.scm into the host's typable
        // map. Doing the drain after run_file finishes means a script can
        // register N commands and we install them atomically.
        let registered = engine.drain_pending_commands();
        let mut guard = cell.write();
        guard.engine = Some(engine);
        guard.plugins_config = Some(cfg.clone());
        for cmd in registered {
            guard.typables.insert(cmd.name.clone(), cmd);
        }
        let count = guard.typables.len();
        drop(guard);

        log::info!(
            "plugin host: steel engine initialised, {count} command{} registered",
            if count == 1 { "" } else { "s" }
        );
        Ok(())
    }

    #[cfg(not(feature = "steel"))]
    fn populate_inner(
        _cfg: &PluginsConfig,
        _editor: &mut helix_view::Editor,
        _cell: &RwLock<ScriptingHost>,
    ) -> Result<()> {
        Ok(())
    }
}

/// Zero-sized stub used when the `steel` feature is off. Lets the rest of
/// the module reference `Engine` without conditional generics. Every method
/// is a no-op or returns an error so the public `ScriptingHost` API has
/// the same signature in both build configurations.
#[cfg(not(feature = "steel"))]
#[derive(Default)]
struct NullEngine;

#[cfg(not(feature = "steel"))]
#[allow(dead_code)]
impl NullEngine {
    fn call_command(&mut self, _name: &str) -> Result<()> {
        // Unreachable in practice — `init_inner` returns early without
        // installing HOST when the feature is off — but the method must
        // exist so `ScriptingHost::call_command` type-checks regardless of
        // feature gate.
        anyhow::bail!("plugin engine not compiled in (build with --features steel)")
    }
}
