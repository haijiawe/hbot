//! `hbot-code` — the bot code sandbox: TypeScript in, QuickJS out.
//!
//! Duties (see `docs/03-directory.md` §"hbot-code" and `docs/02-architecture.md` §6):
//!
//! * [`compile`] — walk `src/**/*.ts`, strip types with SWC, keep a line map
//!   back to the TypeScript source, cache by content hash. No bundler.
//! * [`modules`] — resolve module specifiers. Only `hbot` and project-relative
//!   `./`/`../` imports exist; every other bare specifier is `HBOT_BARE_IMPORT`.
//! * [`runtime`] — one QuickJS isolate (`rquickjs`, `!Send`) on its own thread,
//!   with `log`/`reply`/`sleep` bridged through promises, plus memory and
//!   wall-clock limits.
//! * [`error`] — one error type with stable `HBOT_*` machine codes.
//!
//! The intended flow is:
//!
//! ```no_run
//! use std::path::Path;
//!
//! let project = hbot_code::compile_project(Path::new(r"C:\bots\echo"))?;
//! let sandbox = hbot_code::Sandbox::spawn(project, hbot_code::Limits::default())?;
//! let outcome = sandbox.dispatch(&serde_json::json!({ "text": "hi" }))?;
//! println!("{:?}", outcome.replies);
//! # Ok::<(), hbot_code::CodeError>(())
//! ```
//!
//! S1 status: relative-import multi-file projects work end to end; `vendor/**`
//! and `onStart`/`onSchedule` are out of scope (see `docs/spikes/S1-quickjs.md`).

pub mod compile;
pub mod error;
pub mod limits;
pub mod modules;
pub mod runtime;

pub use compile::{
    CompileStats, CompiledModule, CompiledProject, Compiler, ENTRY_MODULE, LineMapping,
    compile_project,
};
pub use error::{CodeError, ErrorLocation, SourcePos};
pub use limits::{DEFAULT_HANDLER_TIMEOUT, DEFAULT_MEMORY_BYTES, Limits};
pub use runtime::{EventOutcome, HostOutput, MemoryReport, Sandbox};
