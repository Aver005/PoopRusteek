// Copy this file to `src/commands/defs/<yourcmd>.rs` and rename `ExampleCommand`.
// One file per command; nothing here reaches into `App` — return a `CommandResult`
// and let `apply_command_result` (`app/keys/dispatch.rs`) perform any app-level effect.

use crate::app::AppState;
use crate::commands::{Command, CommandResult};
use crate::config::Config;

pub struct ExampleCommand; // <-- rename to <Foo>Command

impl Command for ExampleCommand {
    fn name(&self) -> &str {
        "example" // <-- your command name, WITHOUT a leading '/'
    }

    fn description(&self) -> &str {
        "One-line summary shown by /help + autocomplete" // <-- edit
    }

    fn usage(&self) -> &str {
        "/example [arg]" // <-- optional; default is "" (trait provides it)
    }

    // Exact current trait signature — do not change the parameter types.
    // Prefix a param with `_` (e.g. `_state`, `_config`) if your body doesn't use it.
    fn execute(&self, args: &str, state: &mut AppState, _config: &Config) -> CommandResult {
        let arg = args.trim(); // <-- parse `args` here

        // Editable body. Talk to the user via `state.push_system(..)`, then return
        // an appropriate `CommandResult` variant (see `commands/mod.rs` for the full
        // set — e.g. `Error(String)`, `ResetProvider`, `Quit`, or a new variant you
        // add for a richer app-level effect).
        state.push_system(format!("example command ran with: {arg:?}"));
        CommandResult::Handled
    }
}

// Registration (add ONE line in `register_defaults()` in `src/commands/mod.rs`):
//     self.register(Box::new(defs::example::ExampleCommand));
// And declare the module in `src/commands/defs/mod.rs`:
//     pub mod example;
// `/help` is generated from the live registry, so nothing else needs touching.
