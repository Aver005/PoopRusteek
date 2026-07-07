---
name: Add a PoopRusteek Slash Command
description: Recipe for adding a new in-TUI slash command (/foo) to PoopRusteek — the Command trait, one-file-per-command layout, registration, and CommandResult routing. Use when adding or editing a slash command; not for agent tools (see pooprusteek-add-tool).
---

# Add a PoopRusteek Slash Command

Source of truth: `src/commands/mod.rs` + `src/commands/defs/`. Reference: `.memories/reference/COMMANDS.md`.

Copy `assets/command-template.rs` as a starting skeleton.

## Rules

- **One file per command** in `src/commands/defs/`, plus **one registration line** in
  `register_defaults()` in `src/commands/mod.rs`.
- `name()` must return the name **WITHOUT** a leading `/`. Dispatch (`parse_input`) strips the
  `/` before lookup, and the registry key is the bare name. A registry test
  (`no_registered_command_name_starts_with_slash`) fails the build otherwise. (`/goal` historically
  had this bug — harmless only because the extra slash was tolerated; don't repeat it.)
- A command **never mutates `App` directly**. `execute` gets `&mut AppState` + `&Config` and returns
  a `CommandResult`; app-level effects (spawning turns, touching the provider, the semantic service,
  etc.) are performed by the interpreter `apply_command_result` in `src/app/keys/dispatch.rs`,
  reached via `CommandResult`/`AppEvent`. Anything richer than editing `AppState`/`Config` needs a
  `CommandResult` variant, not a reach-in.

## The `Command` trait (`commands/mod.rs`)

```rust
pub trait Command: Send + Sync {
    fn name(&self) -> &str;                 // bare name, no leading '/'
    fn description(&self) -> &str;          // shown by /help + autocomplete
    fn usage(&self) -> &str { "" }          // optional; "/foo <arg>"
    fn execute(&self, args: &str, state: &mut AppState, config: &Config) -> CommandResult;
}
```

## `CommandResult` variants (non-exhaustive — see `commands/mod.rs`)

`Handled` · `Error(String)` · `LoadSession(String)` · `ResetProvider` · `Quit` ·
`TtlUpdate(u64)` · `ReloadMcp` · `ShowTools` · `ShowSkills` · `ToggleSkill(String,bool)` ·
`Jobs(JobCommandAction)` · `OpenWhitelist` · `OpenConfirm(ConfirmAction)` · `Sidechat(String)` ·
`NewChat` · `OpenChats` · `SpawnAgent(String)` · `OpenAgents` · `Serve(ServeAction)` ·
`Rag(RagAction)` · `Update(UpdateAction)` · … Add a new variant here when your command needs an
app-level effect, and handle it in `apply_command_result` (`app/keys/dispatch.rs`).

Handy shared helpers in `commands/mod.rs`: `with_args(args, usage, body)` (canonical
`Usage:` error when the arg is empty) and `save_config_then(&cfg, then)` (persist config, run
`then` only on a successful write).

## Minimal end-to-end example (`src/commands/defs/retry.rs`)

```rust
use crate::app::AppState;
use crate::commands::{Command, CommandResult, save_config_then};
use crate::config::Config;

pub struct RetryCommand;

impl Command for RetryCommand {
    fn name(&self) -> &str { "retry" }               // NOT "/retry"
    fn description(&self) -> &str { "Set max retries on request failure" }
    fn usage(&self) -> &str { "/retry <number|on|off|-1>" }

    fn execute(&self, args: &str, _state: &mut AppState, config: &Config) -> CommandResult {
        let n: i32 = /* parse args … */ -1;
        let mut cfg = config.clone();
        cfg.agent.max_retries = n;
        save_config_then(&cfg, || CommandResult::ResetProvider)  // effect returned, not applied here
    }
}
```

Register it (in `register_defaults`, `commands/mod.rs`):

```rust
self.register(Box::new(defs::retry::RetryCommand));
```

And declare the module (top of `commands/defs/mod.rs`). `/help` is generated from the live
registry, so nothing else needs touching. Add the module to `defs/` and you're done.
