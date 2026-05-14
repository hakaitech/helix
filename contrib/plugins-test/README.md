# Steel plugin host — end-to-end test fixture

A minimal config dir that exercises all three slices of the Steel
plugin host. Mirrors what was used to hand-verify the feature before
landing.

## Layout

```
helix/
  config.toml   # [editor] insecure=true, plugin keymap, [plugins] init
  init.scm      # registers commands and hooks across slices 1-3
```

## How to run

Build a release `hx` with the `steel` feature, then point it at this
config dir:

```sh
cargo build -p helix-term --features steel --release
XDG_CONFIG_HOME=$PWD/contrib/plugins-test \
  ./target/release/hx -v -c contrib/plugins-test/helix/config.toml \
  /tmp/scratch.txt
```

Press `<space>b` to see `:plugin-list` in the status line (slice 2 +
the registered commands from `init.scm`).
Press `<space>p` to invoke `plugin:say-hi`, which inserts `hello from
steel!` at the cursor via the `doc/insert` binding (slice 3).
Press `<space>l` to invoke `plugin:show-info`, which queries
`doc/line-count` and `helix.current-mode` and posts the result to the
status bar (slice 3).
Press `<space>r` to reload — `init.scm` re-runs against a fresh
engine.

Toggle modes (`i` / Esc) and watch `OnModeSwitch` / `PostCommand`
hooks fire via the log:

```sh
tail -f ~/.cache/helix/helix.log | grep helix::plugin
```

Expected entries:

```
[init] running init.scm
plugin host: steel engine initialised, 2 commands registered, hooks: OnModeSwitch:1, PostCommand:1
[hook PostCommand] a command was executed
[hook OnModeSwitch] now in mode insert
[hook OnModeSwitch] now in mode normal
```

## Why `insecure = true`

Without it, Helix's workspace-trust prompt blocks early keystrokes and
the test sequence gets eaten by the modal dialog. `insecure` skips
that check; only set it for explicit test/sandbox directories.
