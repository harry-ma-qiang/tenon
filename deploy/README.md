# deploy — running the barebone under an OS service manager

RFC section 5.1: the barebone is "supervised by the OS service manager when installed
(systemd --user / launchd), foreground in dev". These are the two units, with two
placeholders each — `@TENON_BIN@` and `@TENON_HOME@` — that `tenon install-service` fills
in. Nothing else in the repo reads them, so editing one by hand is fine.

| File | Manager | Scope |
|---|---|---|
| `systemd/tenon.service` | systemd | user unit (`systemctl --user`), never a system unit |
| `launchd/com.tenon.base.plist` | launchd | LaunchAgent in `~/Library/LaunchAgents` |

## Install

```
tenon install-service --user            # write the unit for this binary and this home
tenon install-service --user --print    # print it instead, and write nothing
systemctl --user start tenon.service    # you start it; install-service never does
```

`install-service` resolves the running binary and `--home` (or `$TENON_HOME`, or
`~/.tenon`), renders the template for this OS, writes it to
`$XDG_CONFIG_HOME/systemd/user/tenon.service` (or `~/Library/LaunchAgents`), and then, on
Linux with `systemctl` on `PATH`, runs `systemctl --user daemon-reload` and
`systemctl --user enable tenon.service`. Without `systemctl` — a container, a machine
without systemd, macOS — it prints the two commands to run instead and exits 0. It never
starts base: which home is live and when is a human's decision.

`--user` is required (and `--print` implies it). A system unit would run base as root,
which is the opposite of what per-env privilege drop is for.

## What the unit says, and why

```
ExecStart=@TENON_BIN@ start --foreground --home @TENON_HOME@
Restart=always
KillMode=mixed
TimeoutStopSec=30
```

- **`--foreground`.** The daemonising `tenon start` re-execs itself with `setsid`, which is
  exactly what a service manager must not see: it would supervise the wrapper and lose the
  process it cares about. `Type=simple` plus `--foreground` means the pid systemd tracks is
  base itself.
- **`Restart=always`.** Base is the thing that restarts everything else; nothing restarts
  base except the OS. A `kill -9` of base leaves nodes that notice their socket closing and
  stop themselves (~1.1 s) and a sandbox container that the next boot reaps, so a restart
  is a clean boot, not a repair.
- **`KillMode=mixed`.** SIGTERM to base only, SIGKILL to the whole cgroup after
  `TimeoutStopSec`. Base's own SIGTERM path is graceful and ordered — flush the packs of
  every env, destroy each sandbox instance, stop the envs deepest-first, then the guardian
  — and it must not race a SIGTERM delivered to the nodes at the same moment. 30 s is
  generous enough for a container teardown and short enough that a wedged base still dies.
- **`Environment=TENON_HOME`.** So `tenon status`, `tenon attach` and `tenon approve` run
  by hand against the same home the service uses, without `--home`.

## Base runs no user code, and panics abort

The barebone is L0 in RFC section 10: humans ship it, agents never change it at runtime.
Base loads no plugin, evaluates no config expression and runs no model — it spawns
processes (BEAM nodes, the harness, the worker inside a sandbox), owns sockets and files,
and answers frames. Everything an agent can influence lives behind a process boundary, in
a node, a harness or a sandbox.

That is what makes `panic = "abort"` right for the release profile (`rs/Cargo.toml`): a
panic in base is a bug in code humans shipped, never a condition that some plugin provoked
and that unwinding could contain. Aborting turns it into an exit the service manager sees
and restarts from a known state, instead of a half-unwound actor that still holds the front
door. `cargo test` builds the dev profile, so the test harness still unwinds and
`#[should_panic]` still works.

## Uninstall

```
systemctl --user disable --now tenon.service && rm ~/.config/systemd/user/tenon.service
launchctl unload -w ~/Library/LaunchAgents/com.tenon.base.plist   # macOS
```
