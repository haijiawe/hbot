//! Step-by-step S1 probe: prints each stage to stderr (unbuffered) so a hang is
//! attributable to one call rather than "the bench stopped".
//!
//! ```text
//! cargo run -q -p hbot-code --example s1_probe
//! ```

use std::io::Write as _;

use hbot_code::{Limits, Sandbox, compile_project};

fn step(label: &str) {
    let mut err = std::io::stderr();
    let _ = writeln!(err, "step: {label}");
    let _ = err.flush();
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::temp_dir().join("hbot-s1-probe");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src"))?;
    std::fs::write(
        root.join("src/main.ts"),
        "import { defineBot, reply } from 'hbot';\n\
         \n\
         export default defineBot({\n\
         \x20 onMessage: async (ctx) => {\n\
         \x20   await reply(`echo: ${ctx.text}`);\n\
         \x20 },\n\
         });\n",
    )?;
    step("project written");

    let compiled = compile_project(&root)?;
    step("compiled");

    let sandbox = Sandbox::spawn(compiled, Limits::default())?;
    step("spawned");

    let event = serde_json::json!({
        "type": "message",
        "text": "hi",
        "chatId": "g1",
        "userId": "u1",
    });
    let outcome = sandbox.dispatch(&event)?;
    step("dispatched");
    let mut err = std::io::stderr();
    let _ = writeln!(err, "replies: {:?}", outcome.replies);
    let _ = err.flush();

    let memory = sandbox.memory()?;
    step(&format!("memory: used={} KiB", memory.used_bytes / 1024));

    step("dropping sandbox");
    drop(sandbox);
    step("done");
    Ok(())
}
