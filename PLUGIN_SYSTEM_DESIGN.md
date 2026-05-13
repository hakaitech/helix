# Helix Plugin System — Design Proposal (v0.3)

> v0.3 fixes two correctness bugs in v0.2 (`register_dynamic_hook` is
> payload-less; `FromStr for MappableCommand` runs before `Application::new`),
> and replaces hand-waving with concrete code at the load-bearing points.
>
> v0.2 was right that the design should be small. v0.3 keeps the footprint
> but stops glossing the hard parts.

---

## 1. Cuts and additions vs v0.2

**Restored (v0.2 was wrong to cut):**
- `MappableCommand::Plugin { name, doc }` variant. Required by `FromStr`
  resolution order (proof below). Saves ~30 lines of TOML loading rewrite
  that v0.2 implicitly required and didn't acknowledge.

**Replaced:**
- v0.2: bridge plugin event subscriptions through `register_dynamic_hook`.
  → That API fires `Fn() -> Result<()>` and discards the event payload
  (`helix-event/src/hook.rs:37`, parameter literally `_event`). Useless for
  the events plugins actually care about.
  v0.3: register a typed `register_hook!` per event type at host init, route
  each typed hook through a curated payload view into the script engine.

**Kept:**
- One module (`helix-term/src/plugin/`), no new crates.
- Cargo feature `steel`, opt-in, default build byte-identical to vanilla.
- Steel adapter is the only Steel-aware code in the tree.
- Synchronous calls only; long work uses `Jobs`.

---

## 2. Init order — proven, not assumed

From `helix-term/src/main.rs:128–154`:

```text
Config::load_default()          ── parses config.toml AND keymaps
                                   (Keymaps deserialize hits MappableCommand::FromStr)
user_lang_loader(...)
Application::new(args, config, lang_loader)   ── *only here* can we own
                                                  plugin state
app.run(&mut events)
```

So at the moment `MappableCommand::from_str("plugin:foo")` runs, no plugin
host has been constructed and no plugin script has executed. There are
exactly three ways out:

| Option | Cost |
| --- | --- |
| A. Pre-init Steel inside `Config::load_default` | Steel runs before logging is set up, no config visible to scripts, hard reload. Bad. |
| B. Defer keymap deserialization — two-phase config | Invasive: serde-derive split, every consumer of `Config` learns about the phase. |
| C. **Plugin variant in `MappableCommand`, lazy resolution at execute time** | One new enum arm, one new `FromStr` branch, one new `execute` match arm. ~40 LOC. |

C wins. The variant exists for one reason only: the keymap parser must
accept `plugin:foo` without knowing whether `foo` is real.

```rust
// helix-term/src/commands.rs — additions only

pub enum MappableCommand {
    Typable { name: String, args: String, doc: String },
    Static  { name: &'static str, fun: fn(&mut Context), doc: &'static str },
    Macro   { name: String, keys: Vec<KeyEvent> },
    /// Plugin-registered command. Resolved at execute time via the
    /// ScriptingHost. `doc` is a placeholder until the plugin loads.
    Plugin  { name: String, doc: String },
}

impl std::str::FromStr for MappableCommand {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(name) = s.strip_prefix("plugin:") {
            ensure!(!name.is_empty(), "expected plugin command name after 'plugin:'");
            return Ok(MappableCommand::Plugin {
                name: name.to_string(),
                doc: format!("(plugin command: {name})"),
            });
        }
        // ... existing ':', '@', and static lookup branches unchanged
    }
}

impl MappableCommand {
    pub fn execute(&self, cx: &mut Context) {
        match &self {
            // ... existing arms ...
            Self::Plugin { name, .. } => {
                #[cfg(feature = "steel")]
                if let Err(e) = crate::plugin::ScriptingHost::call_plugin_command(name, cx) {
                    cx.editor.set_error(format!("plugin '{name}': {e}"));
                }
                #[cfg(not(feature = "steel"))]
                cx.editor.set_error(format!("plugin support disabled at build time"));
            }
        }
    }
}
```

User binding in `config.toml`:
```toml
[keys.normal]
"space p" = "plugin:myplugin.do-thing"
```

The `plugin:` prefix is the namespace boundary. No collision with built-ins
(no built-in command starts with `plugin:`).

---

## 3. Event subscription — typed hooks, real payloads

The event bridge needs payload access. `register_dynamic_hook` is the wrong
tool — it's a payload-less notification. The right tool is the typed
`register_hook!` macro, one call per event type, at plugin host startup.

```rust
// helix-term/src/plugin/events.rs

use helix_event::register_hook;
use helix_view::events::*;
use crate::events::*;

/// Called once during ScriptingHost::init. Registers a permanent typed hook
/// for every event the plugin system supports. Each hook funnels its event
/// into ScriptingHost::dispatch, which marshals a curated payload view and
/// fires whatever script callbacks are registered for that event name.
///
/// These typed hooks live forever — they survive :plugin-reload because
/// they target ScriptingHost (a singleton), not any individual engine instance.
pub(super) fn install_typed_hooks() {
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        super::ScriptingHost::dispatch_event(
            "DocumentDidOpen",
            EventView::DocumentDidOpen { doc: event.doc },
            event.editor,
        )
    });
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        super::ScriptingHost::dispatch_event_doc(
            "DocumentDidChange",
            EventView::DocumentDidChange {
                doc_id: event.doc.id(),
                changes_summary: summarise(event.changes),
            },
            event.doc,
        )
    });
    register_hook!(move |event: &mut SelectionDidChange<'_>| { /* ... */ });
    register_hook!(move |event: &mut OnModeSwitch<'_, '_>| { /* ... */ });
    register_hook!(move |event: &mut PostCommand<'_, '_>| { /* ... */ });
    register_hook!(move |event: &mut DocumentDidClose<'_>| { /* ... */ });
    register_hook!(move |event: &mut LanguageServerInitialized<'_>| { /* ... */ });
    register_hook!(move |event: &mut ConfigDidChange<'_>| { /* ... */ });
}

/// Curated, owned (or 'static-ifiable) payload values exposed to scripts.
/// We never hand a script a raw `&mut Editor` — the editor is reached via
/// the thread-local scope set inside `dispatch_event`.
pub enum EventView {
    DocumentDidOpen { doc: helix_view::DocumentId },
    DocumentDidChange { doc_id: helix_view::DocumentId, changes_summary: ChangesSummary },
    // ... one variant per supported event
}
```

Plugin author writes Scheme:
```scheme
(register-hook "DocumentDidChange"
  (lambda (event)
    (helix.log (string-append "doc " (number->string (event-doc-id event)) " changed"))))
```

What happens on dispatch:
1. Helix code calls `helix_event::dispatch(DocumentDidChange { ... })`.
2. Our typed hook runs (synchronously, with `&mut event`).
3. The hook calls `ScriptingHost::dispatch_event_doc("DocumentDidChange", view, event.doc)`.
4. `ScriptingHost` enters the thread-local scope and invokes every script
   callback registered against `"DocumentDidChange"`.
5. Each callback runs as a single Steel function call.
6. On callback error, log + continue (we never want a misbehaving plugin to
   wedge the editor — different from a typable command error).

---

## 4. Editor-reborrow thread-local

Steel's `Engine` is `'static`; the editor reference is not. The only sound
bridge is a scoped thread-local. The unsafe is real but bounded.

```rust
// helix-term/src/plugin/scope.rs — entire file

use std::cell::Cell;
use helix_view::Editor;

thread_local! {
    static EDITOR: Cell<Option<*mut Editor>> = const { Cell::new(None) };
}

/// Park `editor` in the thread-local for the synchronous duration of `f`,
/// then restore the previous value. The borrow checker can't model this,
/// hence the raw pointer. Soundness rests on three properties:
///   1. `f` is synchronous — we restore the slot before returning.
///   2. We snapshot the previous slot on entry and restore it on exit, so
///      nested `scope` calls compose.
///   3. `with_editor` panics if the slot is empty — reentrant or out-of-scope
///      calls fail loudly instead of aliasing.
pub(super) fn scope<R>(editor: &mut Editor, f: impl FnOnce() -> R) -> R {
    let ptr: *mut Editor = editor;
    let prev = EDITOR.with(|c| c.replace(Some(ptr)));
    let result = f();
    EDITOR.with(|c| c.set(prev));
    result
}

/// Borrow the editor from inside a script call. Panics if called outside
/// of `scope` — that would be a host bug. Single-threaded by construction
/// (each thread has its own `EDITOR`).
pub(super) fn with_editor<R>(f: impl FnOnce(&mut Editor) -> R) -> R {
    EDITOR.with(|c| {
        let ptr = c.get().expect("with_editor called outside scope");
        // SAFETY: scope() guarantees ptr is a live `&mut Editor` for the
        // synchronous duration of `f`. The pointer is restored to its
        // previous value before scope returns, so no `&mut Editor` can
        // outlive its source borrow. The thread-local makes aliasing
        // across threads impossible.
        f(unsafe { &mut *ptr })
    })
}
```

This is ~40 lines, private to the module, and the unsafe block has a
written soundness argument. Every Steel binding that reads or mutates the
editor goes through `with_editor`. No other unsafe in the plugin subsystem.

---

## 5. ScriptingHost — concrete

The host is a singleton because the typed event hooks installed in
`install_typed_hooks` need `'static` reachability to it. `Application` owning
it would force the hook closures to capture an `Arc<Mutex<Application>>`,
which is worse than a focused global.

```rust
// helix-term/src/plugin/mod.rs

use std::sync::OnceLock;
use parking_lot::RwLock;
use anyhow::Result;

mod scope;
mod events;
#[cfg(feature = "steel")]
mod steel;

#[cfg(feature = "steel")]
type Engine = steel::SteelEngine;
#[cfg(not(feature = "steel"))]
type Engine = NullEngine;  // empty stub so the rest compiles

static HOST: OnceLock<RwLock<ScriptingHost>> = OnceLock::new();

pub struct ScriptingHost {
    engine: Option<Engine>,
    /// Plugin-registered typable commands. Looked up by `:cmd` and by
    /// `MappableCommand::Plugin { name }` execution.
    typables: hashbrown::HashMap<String, PluginTypable>,
}

pub struct PluginTypable {
    pub name: String,
    pub doc: String,
    /// Engine-specific callable handle.
    pub callable: CallableHandle,
}

pub struct CallableHandle(pub u64);  // engine-local opaque id

impl ScriptingHost {
    /// One-time install. Called from Application::new BEFORE the main loop,
    /// AFTER config is loaded.
    pub fn init(config: &crate::config::Config) -> Result<()> {
        let Some(plug_cfg) = &config.plugins else { return Ok(()); };

        events::install_typed_hooks();
        let host = Self {
            engine: Some(Engine::new(&plug_cfg.runtime, plug_cfg.init.as_deref())?),
            typables: Default::default(),
        };
        HOST.set(RwLock::new(host)).map_err(|_| anyhow::anyhow!("host already init"))?;
        Ok(())
    }

    /// Invoked from MappableCommand::Plugin::execute.
    pub fn call_plugin_command(name: &str, cx: &mut crate::commands::Context) -> Result<()> {
        let host = HOST.get().ok_or_else(|| anyhow::anyhow!("no plugin host"))?;
        let host = host.read();
        let typable = host.typables.get(name)
            .ok_or_else(|| anyhow::anyhow!("plugin command not registered"))?;
        let callable = typable.callable.0;
        drop(host);  // release before calling into the engine (may register more typables)

        scope::scope(cx.editor, || {
            HOST.get().unwrap().write().engine.as_mut().unwrap()
                .call_handle(CallableHandle(callable), &[])
                .map(|_| ())
        })
    }

    /// Called from typed event hooks installed in events::install_typed_hooks.
    pub(super) fn dispatch_event(
        name: &'static str,
        view: events::EventView,
        editor: &mut helix_view::Editor,
    ) -> Result<()> {
        let Some(host) = HOST.get() else { return Ok(()); };
        scope::scope(editor, || {
            host.write().engine.as_mut().unwrap()
                .dispatch_event(name, view)
        })
    }

    /// `:plugin-reload` typable command target.
    pub fn reload(config: &crate::config::Config) -> Result<()> {
        let Some(host) = HOST.get() else { return ScriptingHost::init(config); };
        let Some(plug_cfg) = &config.plugins else { return Ok(()); };
        let mut host = host.write();
        host.engine = None;  // drop old engine; releases all Steel state
        host.typables.clear();
        host.engine = Some(Engine::new(&plug_cfg.runtime, plug_cfg.init.as_deref())?);
        Ok(())
    }
}
```

Note `host.engine.as_mut().unwrap()` — fine, since `None` only briefly during
reload and we never dispatch concurrently (single-threaded editor loop).

---

## 6. Steel adapter — the bindings that matter

```rust
// helix-term/src/plugin/steel.rs (sketch — concrete signatures)

use ::steel::steel_vm::engine::Engine;
use ::steel::SteelVal;
use super::{scope::with_editor, CallableHandle};

pub struct SteelEngine {
    inner: Engine,
    callables: slotmap::SlotMap<slotmap::DefaultKey, SteelVal>,
}

impl SteelEngine {
    pub fn new(runtime: &std::path::Path, init: Option<&std::path::Path>) -> anyhow::Result<Self> {
        let mut inner = Engine::new();
        let mut me = Self { inner, callables: Default::default() };
        me.register_bindings()?;
        for stdlib in &["commands.scm", "document.scm", "events.scm"] {
            me.inner.run_file(&runtime.join("plugins/steel").join(stdlib))?;
        }
        if let Some(init) = init { me.inner.run_file(init)?; }
        Ok(me)
    }

    fn register_bindings(&mut self) -> anyhow::Result<()> {
        // status / log
        self.inner.register_fn("helix.status", |s: String| {
            with_editor(|e| e.set_status(s))
        });
        self.inner.register_fn("helix.error", |s: String| {
            with_editor(|e| e.set_error(s))
        });

        // document text (read)
        self.inner.register_fn("doc/text", || -> String {
            with_editor(|e| {
                let (_, d) = current_ref!(e);
                d.text().to_string()
            })
        });

        // selection (read)
        self.inner.register_fn("sel/primary-range", || -> (usize, usize) {
            with_editor(|e| {
                let (view, doc) = current_ref!(e);
                let r = doc.selection(view.id).primary();
                (r.anchor, r.head)
            })
        });

        // insert text at primary cursor
        self.inner.register_fn("doc/insert", |text: String| -> anyhow::Result<()> {
            with_editor(|e| {
                let (view, doc) = current!(e);
                let tx = helix_core::Transaction::insert(
                    doc.text(), &doc.selection(view.id), text.into());
                doc.apply(&tx, view.id);
                Ok(())
            })
        });

        // register a typable command from script
        // Steel passes a SteelVal closure; we store it and tell ScriptingHost
        self.inner.register_fn("helix.register-command",
            |name: String, doc: String, callable: SteelVal| -> anyhow::Result<()> {
                with_editor(|_| {
                    // Direct mutation of host through HOST static; safe because we are
                    // single-threaded and inside a scope() call which already holds the
                    // editor borrow.
                    let host = super::HOST.get().expect("no host");
                    let mut host = host.write();
                    let key = /* store callable in self.callables */ todo!();
                    host.typables.insert(name.clone(), super::PluginTypable {
                        name, doc, callable: CallableHandle(key),
                    });
                    Ok(())
                })
            });

        // register an event hook from script
        self.inner.register_fn("helix.register-hook",
            |event: String, callable: SteelVal| -> anyhow::Result<()> {
                /* stash callable in self.callables under (event_name, key) */
                Ok(())
            });

        Ok(())
    }

    pub fn call_handle(&mut self, h: CallableHandle, args: &[SteelVal]) -> anyhow::Result<SteelVal> {
        let key = /* decode h.0 */ todo!();
        let func = self.callables.get(key).ok_or_else(|| anyhow::anyhow!("stale handle"))?.clone();
        self.inner.call_function_with_args(func, args.to_vec()).map_err(Into::into)
    }

    pub fn dispatch_event(&mut self, name: &str, view: super::events::EventView)
        -> anyhow::Result<()>
    {
        // Look up all hooks under this event name, marshal `view` to SteelVal,
        // call each. On individual failure, log and continue.
        Ok(())
    }
}
```

The `todo!()`s are slotmap/key plumbing — boring code, not load-bearing. The
load-bearing claims (scope, marshalling, error handling, lifecycle) are all
spelled out above.

---

## 7. Configuration & runtime paths

`config.toml`:
```toml
[plugins]
runtime = "~/.config/helix/runtime"   # bundled .scm files live under runtime/plugins/steel/
init    = "~/.config/helix/init.scm"  # user entry point; optional
```

If `[plugins]` is absent, `ScriptingHost::init` returns immediately and the
typed event hooks are never installed.

`~/.config/helix/init.scm` minimal:
```scheme
(require "helix/commands")
(require "helix/events")

(define (toggle-line-numbers)
  (helix.status "todo: hook into config update"))

(helix.register-command "toggle-line-numbers" "Toggle relative line numbers"
                       toggle-line-numbers)

(helix.register-hook "DocumentDidChange"
  (lambda (event)
    (helix.status "doc changed")))
```

User keymap:
```toml
[keys.normal]
"space l" = "plugin:toggle-line-numbers"
```

---

## 8. Worked example: format-on-save

End-to-end so the design is testable in your head.

`init.scm`:
```scheme
(define (run-formatter doc-id)
  (let ((path (doc/path doc-id)))
    (when (string-suffix? path ".rs")
      (let ((output (shell/run "rustfmt --emit stdout" (doc/text-of doc-id))))
        (doc/replace-all doc-id output)))))

(helix.register-hook "DocumentWillSave"
  (lambda (event)
    (run-formatter (event-doc-id event))))
```

What needs to exist for this to work:
- `DocumentWillSave` event (does not yet exist in upstream — would need to be
  added in `helix-view/src/events.rs`; one of the smallest possible upstream
  contributions to land first).
- `doc/path`, `doc/text-of`, `doc/replace-all` bindings (slice 3).
- `shell/run` binding — synchronous shell execution returning stdout. Maps to
  `std::process::Command`. Steel-side cleanly.
- `string-suffix?` is in Steel's stdlib.

This is the test: if v0.3 makes format-on-save plausible to write in a
weekend, v0.3 is the right shape.

---

## 9. Reload semantics

`:plugin-reload`:
1. `ScriptingHost::reload(&config)` is called.
2. The `engine: Option<Engine>` field is set to `None`, dropping every
   stored callable and every Steel-side state.
3. `typables` map is cleared. Existing `MappableCommand::Plugin { name }`
   instances stored in keymaps are still valid (they're just strings); the
   next time a user presses the key, the name is looked up against the new
   `typables` map.
4. A fresh `Engine` is constructed and `init.scm` is re-run.
5. Event hooks: the *typed* hooks installed by `install_typed_hooks` keep
   firing (they target `ScriptingHost::dispatch_event`, which routes to
   whichever engine is currently installed). Steel-side event callbacks are
   gone with the old engine and re-registered by the new `init.scm`.

What can go wrong:
- A user has a key bound to `plugin:foo` and the new `init.scm` no longer
  registers `foo`. → `set_error("plugin command not registered")`. Recoverable.
- An event fires during reload. → `engine` is briefly `None`; we log "engine
  not ready" and continue. Editor is unaffected.

---

## 10. Errors, by source

| Source | Outcome | Why |
| --- | --- | --- |
| Plugin host init failure | startup error printed, plugin disabled, editor still launches | Don't deny editor over a bad plugin |
| `init.scm` evaluation error | `editor.set_error(...)` after first frame, plugin disabled | Visible but recoverable |
| Typable plugin command error | `editor.set_error(...)` | Same as built-in typable failure |
| Event hook callback error | `log::error!` only, no UI surface | Hooks fire on every keystroke; status spam would be hostile |
| Steel panic | Caught by `std::panic::catch_unwind` inside the dispatch site, engine moved to `None` for the rest of the session | Plugin panic should not take down the editor |

The panic catch happens at the `ScriptingHost::dispatch_event` /
`call_plugin_command` boundary, not inside the engine. Adds ~20 lines.

---

## 11. Cargo wiring (unchanged from v0.2)

```toml
# helix-term/Cargo.toml
[features]
default = ["git"]
steel = ["dep:steel-core"]

[dependencies]
steel-core = {
    git = "https://github.com/mattwparas/steel.git",
    rev = "<pin>",
    features = ["anyhow", "sync"],
    optional = true,
}
parking_lot = { workspace = true }   # already in workspace
slotmap = { workspace = true }       # already in workspace
```

---

## 12. Diff budget

| File | Change | Approx LOC |
| --- | --- | --- |
| `helix-term/Cargo.toml` | +1 dep, +1 feature | 5 |
| `helix-term/src/plugin/mod.rs` | new | 200 |
| `helix-term/src/plugin/scope.rs` | new | 50 |
| `helix-term/src/plugin/events.rs` | new | 200 (mostly the typed-hook installer + EventView enum + marshalling) |
| `helix-term/src/plugin/steel.rs` | new, feature-gated | 600 (every binding, marshalling for every EventView arm, slotmap plumbing) |
| `helix-term/src/commands.rs` | `MappableCommand::Plugin` variant, FromStr arm, execute arm, Deserialize, PartialEq | 40 |
| `helix-term/src/commands/typed.rs` | new typable `plugin-reload`, `plugin-list` | 30 |
| `helix-term/src/application.rs` | call `ScriptingHost::init` | 5 |
| `helix-term/src/config.rs` | `Plugins` struct, `plugins: Option<Plugins>` on Config | 30 |
| `runtime/plugins/steel/{commands,document,events}.scm` | bundled stdlib | 200 |

Total: ~1,360 LOC added, ~80 modified. Still single-PR auditable.

---

## 13. Implementation slices — three, sized to be testable

**Slice 1 — Lifecycle + status.** Module skeleton, `[plugins]` config,
`ScriptingHost::init`, scope thread-local, Steel engine construction, three
bindings (`helix.status`, `helix.error`, `helix.log`). No commands, no
events. Acceptance: a Steel `init.scm` that calls `(helix.status "hello")`
shows the message on startup.

**Slice 2 — Commands.** `MappableCommand::Plugin` variant, `FromStr` arm,
`execute` arm, `helix.register-command` binding, `:plugin-reload`,
`:plugin-list`. Acceptance: `(helix.register-command "greet" "..." greet-fn)`
in `init.scm`, key bound to `"plugin:greet"`, command runs, `:plugin-reload`
reloads.

**Slice 3 — Events + editor surface.** Typed hook installer for the 8
event types, `EventView` enum, payload marshalling, `helix.register-hook`,
document/selection bindings. Acceptance: format-on-save (worked example
above) runs end-to-end.

Each slice merges behind the same `steel` feature flag. Slice 1 alone is
mergeable as "scaffolding exists, not yet useful." Slice 3 is when the
feature is worth turning on.

---

## 14. Things I am still uncertain about

These are the questions where I do not have a confident answer:

1. **`SteelVal` send/sync requirements.** Steel `register_fn` with the `sync`
   feature requires closures to be `Send + Sync`. Closures capturing
   `with_editor` (which itself touches thread-local) are `Send` only if the
   thread-local-access machinery is `Send`. `thread_local!` itself is fine,
   but I want to verify by getting it to compile before committing. **Action: prototype slice 1 to find out, not argue from first principles.**

2. **Steel `Engine` clone-on-thread semantics.** With `sync` feature, the
   engine is `Send + Sync` but internally locked. We don't share it across
   threads, but Steel's internal threading might still introduce surprises.
   **Action: read Steel's `SteelThread` once we have a build.**

3. **Catching `SteelErr` across the FFI boundary.** Steel's `anyhow` feature
   converts errors via `From<SteelErr> for anyhow::Error`. I haven't read
   the conversion to know if stack traces survive. **Action: probe once
   building.**

4. **Whether `runtime` plugin .scm files belong in the workspace or in a
   sibling repo.** Bundling makes the first-run experience work. Sibling
   repo makes the stdlib evolvable independently. Lean: bundle for now.

---

## 15. Open questions for you (down to 3)

1. **Which fork?** Local origin is `helix-editor/helix`. Add your fork as a
   remote, or `git worktree` off it from a different path?
2. **Steel pin — rev or tag?** Default to `rev` for reproducibility unless
   you prefer tags.
3. **License surface.** Steel is Apache-2.0, Helix workspace is MPL-2.0.
   Compatible at use, but a licence audit will flag it. Confirm OK for the
   fork.
