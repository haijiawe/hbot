//! `hbot-code` — the bot code sandbox: TypeScript in, QuickJS out.
//!
//! Duties (see `docs/03-directory.md` §"hbot-code" and `docs/02-architecture.md` §6):
//!
//! * [`compile`] — walk `src/**/*.ts`, strip types with SWC, keep a line map
//!   back to the TypeScript source, cache by content hash. No bundler.
//! * [`modules`] — resolve module specifiers. Only `hbot` and project-relative
//!   `./`/`../` imports exist; every other bare specifier is `HBOT_BARE_IMPORT`.
//! * [`manifest`] — parse `hbot.json` (the project's name, entry point, platform
//!   and sandbox budget) and scaffold a new project.
//! * [`dts`] — generate the project's `hbot.d.ts` from the same contract the
//!   runtime exposes; drift between the two is a test failure.
//! * [`runtime`] — one QuickJS isolate (`rquickjs`, `!Send`) on its own thread,
//!   with `log`/`reply`/`sleep`/`http.get` bridged through promises, plus memory
//!   and wall-clock limits.
//! * [`permissions`] — the single authorization judgement for
//!   `hbot.json`'s `permissions.http`; [`http`] holds the transport seam and the
//!   `http.get` pipeline.
//! * [`error`] — one error type with stable `HBOT_*` machine codes.
//!
//! The intended flow is:
//!
//! ```no_run
//! use std::path::Path;
//!
//! let root = Path::new(r"C:\bots\echo");
//! let manifest = hbot_code::manifest::load(root)?;
//! let project = hbot_code::compile_project_with_entry(root, &manifest.entry_name())?;
//! let sandbox = hbot_code::Sandbox::spawn(project, manifest.sandbox_limits())?;
//! let outcome = sandbox.dispatch(&serde_json::json!({ "text": "hi" }))?;
//! println!("{:?}", outcome.replies);
//! # Ok::<(), hbot_code::CodeError>(())
//! ```
//!
//! S1 status: relative-import multi-file projects work end to end; `vendor/**`
//! and lifecycle hooks (`onStart`/`onSchedule`) are out of scope (see
//! `docs/spikes/S1-quickjs.md`). `hbot.d.ts` declares exactly what the runtime
//! implements — nothing more (`docs/02-architecture.md` §6.1 status table).

pub mod compile;
pub mod device;
mod dispatch;
pub mod dts;
pub mod error;
pub mod http;
mod instrument;
pub mod limits;
pub mod manifest;
pub mod modules;
pub mod permissions;
pub mod runtime;

pub use compile::{
    BuildMode, CompileStats, CompiledModule, CompiledProject, Compiler, DebugInfo, ENTRY_MODULE,
    LineMapping, compile_project, compile_project_debug, compile_project_with_entry,
    compile_project_with_entry_debug,
};
pub use device::{
    DeviceFailure, DeviceHandles, MAX_CLIPBOARD_BYTES, MAX_INPUT_TEXT_BYTES, MAX_SCREEN_BYTES,
    ScreenFrame,
};
pub use dispatch::EventFailure;
pub use error::{CodeError, ErrorLocation, SourcePos};
pub use hbot_core::debug::{Breakpoints, DebugAction, DebugFrame, DebugScope, DebugSink, PausedAt};
pub use http::{HttpConfig, HttpRequest, HttpResponse, HttpTransport, TransportError};
pub use limits::{DEFAULT_HANDLER_TIMEOUT, DEFAULT_MEMORY_BYTES, Limits};
pub use manifest::{
    DevicePermissions, Manifest, ManifestLimits, Permissions, PlatformSpec, ScaffoldReport,
};
pub use permissions::{PermissionError, authorize};
pub use runtime::{EventOutcome, HostOutput, MemoryReport, Sandbox};
