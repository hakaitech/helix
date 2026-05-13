//! Steel adapter for the plugin host.
//!
//! This file is the entire scope of "Steel-aware code" in the tree. Every
//! Rust function exposed to script-land is registered here; every Steel
//! call that crosses back into Helix routes through [`super::scope`].
//!
//! Slice 1 (this commit) exposes only the three logging bindings:
//! `helix.status`, `helix.error`, `helix.log`. Future slices add command
//! registration, document/selection access, and event hooks.

use std::path::Path;

use anyhow::{Context, Result};
use steel::steel_vm::engine::Engine;
// `register_fn` is supplied by a trait that lives in a separate module.
// Importing it brings the method into scope on `Engine`.
use steel::steel_vm::register_fn::RegisterFn;

use super::scope::with_editor;

pub struct SteelEngine {
    inner: Engine,
}

impl SteelEngine {
    pub fn new() -> Result<Self> {
        let mut inner = Engine::new();
        register_logging_bindings(&mut inner);
        Ok(Self { inner })
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
}

/// Register the three logging-shaped functions exposed to scripts in slice 1.
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
