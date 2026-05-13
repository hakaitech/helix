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
//! call sites — they just call `ScriptingHost::init` / `try_call_command`
//! and the stub returns immediately.
//!
//! See `PLUGIN_SYSTEM_DESIGN.md` at the repo root for the broader
//! architectural rationale.
#![allow(dead_code)] // many fields are only used once slices 2/3 land

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

    #[cfg(feature = "steel")]
    fn init_inner(cfg: &PluginsConfig, editor: &mut helix_view::Editor) -> Result<()> {
        let mut engine = steel::SteelEngine::new()?;
        scope::scope(editor, || {
            if let Some(init) = cfg.init.as_deref() {
                engine.run_file(init)?;
            }
            Ok::<_, anyhow::Error>(())
        })?;

        let host = ScriptingHost { engine: Some(engine) };
        HOST.set(RwLock::new(host))
            .map_err(|_| anyhow::anyhow!("ScriptingHost already initialised"))?;
        log::info!("plugin host: steel engine initialised");
        Ok(())
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

    /// True once `init` succeeded with an engine attached. Used by future
    /// slices to short-circuit when the host is absent.
    pub fn is_active() -> bool {
        HOST.get().map(|h| h.read().engine.is_some()).unwrap_or(false)
    }
}

/// Zero-sized stub used when the `steel` feature is off. Lets the rest of
/// the module reference `Engine` without conditional generics.
#[cfg(not(feature = "steel"))]
#[derive(Default)]
struct NullEngine;
