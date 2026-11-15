//! Errors raised by the compile and sandbox layers.
//!
//! Every failure carries a **stable machine code** (`HBOT_*`). The IDE, the agent
//! loop and the dead-letter queue switch on [`CodeError::code`], never on the
//! human-readable text, so messages can stay helpful without becoming an API.

use std::fmt;
use std::path::PathBuf;

/// Stable machine-readable error codes.
pub mod code {
    /// TypeScript parse/strip/codegen failure.
    pub const COMPILE: &str = "HBOT_COMPILE";
    /// Bare module specifier (`import x from "lodash"`).
    pub const BARE_IMPORT: &str = "HBOT_BARE_IMPORT";
    /// Relative import that escapes the project root (`../../etc/passwd`).
    pub const IMPORT_ESCAPE: &str = "HBOT_IMPORT_ESCAPE";
    /// Relative import that does not name a file inside the project.
    pub const MODULE_NOT_FOUND: &str = "HBOT_MODULE_NOT_FOUND";
    /// Syntax the sandbox refuses by contract (`eval`, `new Function`, `require`,
    /// dynamic `import()`).
    pub const FORBIDDEN_SYNTAX: &str = "HBOT_FORBIDDEN_SYNTAX";
    /// `src/main.ts` is missing.
    pub const NO_ENTRY: &str = "HBOT_NO_ENTRY";
    /// Filesystem failure while reading the project.
    pub const IO: &str = "HBOT_IO";
    /// Uncaught JavaScript exception; `stack` is already remapped to TS positions.
    pub const JS_EXCEPTION: &str = "HBOT_JS_EXCEPTION";
    /// The handler ran past its wall-clock budget and was interrupted.
    pub const TIMEOUT: &str = "HBOT_TIMEOUT";
    /// The isolate hit its heap ceiling.
    pub const MEMORY_LIMIT: &str = "HBOT_MEMORY_LIMIT";
    /// The isolate thread died or its command channel closed.
    pub const SANDBOX_DEAD: &str = "HBOT_SANDBOX_DEAD";
    /// `hbot.json` is missing from the project root.
    pub const MANIFEST_MISSING: &str = "HBOT_MANIFEST_MISSING";
    /// `hbot.json` is present but invalid: bad JSON, unknown field, out-of-range
    /// limit, entry outside the project, … Both [`super::CodeError::ManifestSyntax`]
    /// and [`super::CodeError::ManifestInvalid`] answer this code — from the
    /// caller's point of view they are the same class of failure (the manifest
    /// must be fixed by the author), and the message carries the line/column.
    pub const MANIFEST_INVALID: &str = "HBOT_MANIFEST_INVALID";
    /// `scaffold` refused to overwrite existing files, or could not create them.
    pub const SCAFFOLD: &str = "HBOT_SCAFFOLD";
}

/// A position in the *TypeScript* source (1-based line/column).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourcePos {
    /// Project-relative module name, `/`-separated, e.g. `src/lib.ts`.
    pub file: String,
    /// 1-based line in the TypeScript source.
    pub line: u32,
    /// 1-based column in the TypeScript source.
    pub column: u32,
}

impl fmt::Display for SourcePos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.file, self.line, self.column)
    }
}

/// An optional source position, rendered as ` at file:line:column`.
///
/// A newtype rather than `Option<SourcePos>` so [`thiserror`] can render the
/// variant messages without a hand-written `Display` for the whole error enum.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ErrorLocation(Option<SourcePos>);

impl ErrorLocation {
    /// No known position (e.g. the module loader only knows the specifier).
    #[must_use]
    pub fn unknown() -> Self {
        Self(None)
    }

    /// A known position in a TypeScript file.
    #[must_use]
    pub fn at(position: SourcePos) -> Self {
        Self(Some(position))
    }

    /// The position, when known.
    #[must_use]
    pub fn position(&self) -> Option<&SourcePos> {
        self.0.as_ref()
    }
}

impl fmt::Display for ErrorLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Some(position) => write!(f, " at {position}"),
            None => Ok(()),
        }
    }
}

/// Everything that can go wrong between "a project on disk" and "a bot reply".
#[derive(Debug, thiserror::Error)]
pub enum CodeError {
    /// The TypeScript compiler front end rejected a file.
    #[error("HBOT_COMPILE: {file}:{line}:{column}: {message}")]
    Compile {
        /// Project-relative file name.
        file: String,
        /// 1-based line.
        line: u32,
        /// 1-based column.
        column: u32,
        /// Diagnostic text from SWC.
        message: String,
    },

    /// Only relative specifiers and the built-in `hbot` module are importable.
    #[error(
        "HBOT_BARE_IMPORT: bare module specifier '{specifier}' is not allowed; \
         use a relative import ('./x') or the built-in 'hbot' module{at}"
    )]
    BareImport {
        /// The offending specifier.
        specifier: String,
        /// Where in the TypeScript source the import sits, when known.
        at: ErrorLocation,
    },

    /// A relative import tried to leave the project directory.
    #[error(
        "HBOT_IMPORT_ESCAPE: import '{specifier}' from '{base}' resolves outside the project root{at}"
    )]
    ImportEscape {
        /// The offending specifier.
        specifier: String,
        /// The importing module (empty string for the entry point).
        base: String,
        /// Where in the TypeScript source the import sits, when known.
        at: ErrorLocation,
    },

    /// A relative import did not name an existing module.
    #[error(
        "HBOT_MODULE_NOT_FOUND: cannot resolve '{specifier}' from '{base}'{at} (tried: {tried})"
    )]
    ModuleNotFound {
        /// The offending specifier.
        specifier: String,
        /// The importing module.
        base: String,
        /// Candidate paths that were probed.
        tried: String,
        /// Where in the TypeScript source the import sits, when known.
        at: ErrorLocation,
    },

    /// The project uses syntax the sandbox contract forbids.
    #[error(
        "HBOT_FORBIDDEN_SYNTAX: {file}:{line}:{column}: {what} is not available in the sandbox"
    )]
    ForbiddenSyntax {
        /// Project-relative file name.
        file: String,
        /// 1-based line.
        line: u32,
        /// 1-based column.
        column: u32,
        /// What was used, e.g. `eval()`.
        what: String,
    },

    /// `src/main.ts` does not exist.
    #[error("HBOT_NO_ENTRY: project has no entry module '{path}'")]
    NoEntry {
        /// Expected entry path, project-relative.
        path: String,
    },

    /// Reading the project tree failed.
    #[error("HBOT_IO: {path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// `hbot.json` does not exist in the project root.
    #[error("HBOT_MANIFEST_MISSING: no manifest at {path}")]
    ManifestMissing {
        /// Expected path of `hbot.json`.
        path: PathBuf,
    },

    /// `hbot.json` is not valid JSON.
    ///
    /// Kept separate from [`Self::ManifestInvalid`] only so the message can carry
    /// the parser's `line:column`; both answer [`code::MANIFEST_INVALID`].
    #[error("HBOT_MANIFEST_INVALID: {path}:{line}:{column}: {message}")]
    ManifestSyntax {
        /// Manifest path.
        path: PathBuf,
        /// 1-based line of the JSON syntax error.
        line: u32,
        /// 1-based column of the JSON syntax error.
        column: u32,
        /// Parser message.
        message: String,
    },

    /// `hbot.json` parsed but violates the manifest contract.
    ///
    /// Covers unknown/missing fields, an entry outside the project or absent on
    /// disk, and limits above the host hard caps. Out-of-range limits are a
    /// **hard error**, never silently clamped: a project that asks for 512 MB
    /// must be told it cannot have it, not quietly run with 32 MB
    /// (`docs/02-architecture.md` §6.1).
    #[error("HBOT_MANIFEST_INVALID: {path}: {message}")]
    ManifestInvalid {
        /// Manifest path.
        path: PathBuf,
        /// What is wrong, in the author's terms.
        message: String,
    },

    /// `scaffold` refused to overwrite a file, or could not write the skeleton.
    #[error("HBOT_SCAFFOLD: {path}: {message}")]
    Scaffold {
        /// File (or directory) the scaffold stopped at.
        path: PathBuf,
        /// Why it stopped.
        message: String,
    },

    /// The isolate died while running the handler.
    #[error("HBOT_SANDBOX_DEAD: {message}")]
    SandboxDead {
        /// Why the worker is gone.
        message: String,
    },

    /// JavaScript threw; `stack` has already been remapped to TS positions.
    #[error("HBOT_JS_EXCEPTION: {message}")]
    JsException {
        /// `Error.message` of the caught value.
        message: String,
        /// `Error.stack`, with generated positions rewritten to TS positions.
        stack: Option<String>,
    },

    /// The handler exceeded its wall-clock budget and was interrupted.
    #[error("HBOT_TIMEOUT: handler exceeded the {budget_ms} ms budget and was interrupted")]
    Timeout {
        /// The budget in milliseconds.
        budget_ms: u64,
    },

    /// The isolate hit its heap ceiling.
    #[error("HBOT_MEMORY_LIMIT: isolate exceeded its {limit_bytes} byte heap limit")]
    MemoryLimit {
        /// Configured heap limit in bytes.
        limit_bytes: u64,
    },
}

impl CodeError {
    /// The stable machine code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Compile { .. } => code::COMPILE,
            Self::BareImport { .. } => code::BARE_IMPORT,
            Self::ImportEscape { .. } => code::IMPORT_ESCAPE,
            Self::ModuleNotFound { .. } => code::MODULE_NOT_FOUND,
            Self::ForbiddenSyntax { .. } => code::FORBIDDEN_SYNTAX,
            Self::NoEntry { .. } => code::NO_ENTRY,
            Self::Io { .. } => code::IO,
            Self::SandboxDead { .. } => code::SANDBOX_DEAD,
            Self::JsException { .. } => code::JS_EXCEPTION,
            Self::Timeout { .. } => code::TIMEOUT,
            Self::MemoryLimit { .. } => code::MEMORY_LIMIT,
            Self::ManifestMissing { .. } => code::MANIFEST_MISSING,
            Self::ManifestSyntax { .. } | Self::ManifestInvalid { .. } => code::MANIFEST_INVALID,
            Self::Scaffold { .. } => code::SCAFFOLD,
        }
    }

    /// Where in the TypeScript source the error happened, when known.
    #[must_use]
    pub fn position(&self) -> Option<SourcePos> {
        match self {
            Self::Compile {
                file, line, column, ..
            }
            | Self::ForbiddenSyntax {
                file, line, column, ..
            } => Some(SourcePos {
                file: file.clone(),
                line: *line,
                column: *column,
            }),
            Self::BareImport { at, .. } => at.position().cloned(),
            Self::ManifestSyntax {
                path, line, column, ..
            } => Some(SourcePos {
                file: path.to_string_lossy().into_owned(),
                line: *line,
                column: *column,
            }),
            Self::JsException { stack, .. } => {
                stack.as_deref().and_then(parse_first_stack_position)
            }
            _ => None,
        }
    }
}

/// Best-effort: pull the first `file:line:col` out of a remapped JS stack.
fn parse_first_stack_position(stack: &str) -> Option<SourcePos> {
    for line in stack.lines() {
        let trimmed = line.trim();
        let rest = trimmed.strip_prefix("at ").unwrap_or(trimmed);
        // Skip the `fn (` frame prefix if any.
        let candidate = rest.rsplit_once('(').map_or(rest, |(_, tail)| tail);
        let candidate = candidate.trim_end_matches(')');
        let mut parts = candidate.rsplitn(3, ':');
        let column = parts.next()?.parse::<u32>().ok()?;
        let line_no = parts.next()?.parse::<u32>().ok()?;
        let file = parts.next()?.to_owned();
        if file.is_empty() {
            continue;
        }
        return Some(SourcePos {
            file,
            line: line_no,
            column,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable() {
        let err = CodeError::BareImport {
            specifier: "lodash".to_owned(),
            at: ErrorLocation::unknown(),
        };
        assert_eq!(err.code(), "HBOT_BARE_IMPORT");
        assert!(err.position().is_none());
    }

    #[test]
    fn bare_import_can_carry_a_position() {
        let err = CodeError::BareImport {
            specifier: "lodash".to_owned(),
            at: ErrorLocation::at(SourcePos {
                file: "src/main.ts".to_owned(),
                line: 3,
                column: 8,
            }),
        };
        assert!(err.to_string().contains("at src/main.ts:3:8"), "{err}");
        assert_eq!(err.position().map(|pos| pos.line), Some(3));
    }

    #[test]
    fn manifest_errors_share_one_code_but_keep_the_json_position() {
        let syntax = CodeError::ManifestSyntax {
            path: PathBuf::from("hbot.json"),
            line: 4,
            column: 9,
            message: "expected `,`".to_owned(),
        };
        let invalid = CodeError::ManifestInvalid {
            path: PathBuf::from("hbot.json"),
            message: "limits.handlerMs 超过宿主硬上限 2000".to_owned(),
        };
        assert_eq!(syntax.code(), "HBOT_MANIFEST_INVALID");
        assert_eq!(invalid.code(), "HBOT_MANIFEST_INVALID");
        assert_eq!(
            syntax.position().map(|pos| (pos.line, pos.column)),
            Some((4, 9)),
            "JSON 语法错必须能回到清单的行列"
        );
        assert!(syntax.to_string().contains("hbot.json:4:9"), "{syntax}");
    }

    #[test]
    fn missing_manifest_has_its_own_code() {
        let error = CodeError::ManifestMissing {
            path: PathBuf::from("/tmp/p/hbot.json"),
        };
        assert_eq!(error.code(), "HBOT_MANIFEST_MISSING");
        assert!(error.position().is_none());
    }

    #[test]
    fn stack_position_is_parsed() {
        let err = CodeError::JsException {
            message: "boom".to_owned(),
            stack: Some("    at onMessage (src/main.ts:4:9)\n    at src/main.ts:1:1".to_owned()),
        };
        let pos = err.position().expect("position");
        assert_eq!(pos.file, "src/main.ts");
        assert_eq!(pos.line, 4);
        assert_eq!(pos.column, 9);
    }
}
