//! Import rules for bot projects.
//!
//! `AGENTS.md` rule 9 and `docs/02-architecture.md` §6.1 say the only legal
//! import forms are
//!
//! * the built-in module `hbot`, and
//! * a relative path (`./x`, `../y`) that stays inside the project root.
//!
//! Everything else is a [`CodeError::BareImport`]. The check is deliberately
//! split into a pure layer ([`classify`], [`resolve_module`]) with no filesystem
//! and no QuickJS dependency, so both the compiler (compile-time gate) and the
//! QuickJS `Resolver` (load-time gate) can share it.

use crate::error::{CodeError, ErrorLocation};

/// The single bare specifier the sandbox resolves itself.
pub const HOST_MODULE: &str = "hbot";

/// Internal module id used for the synthesized host module.
pub const HOST_MODULE_ID: &str = "hbot:host";

/// Extensions the loader will consider, in probe order.
const EXTENSIONS: [&str; 3] = [".ts", ".js", ".mjs"];

/// What a specifier names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpecifierKind {
    /// The built-in `hbot` module.
    Host,
    /// A project-relative module.
    Relative,
}

/// Decide which of the two legal kinds a specifier is, or reject it.
///
/// `base` is the importing module (empty for the entry point) and is only used
/// to make the error message actionable.
///
/// # Errors
///
/// Returns [`CodeError::BareImport`] for anything that is not `hbot` and not
/// `./`/`../`-relative, and [`CodeError::ImportEscape`] for absolute paths
/// (`/etc/passwd`, `C:\Windows`, `\\server\share`).
pub fn classify(base: &str, specifier: &str) -> Result<SpecifierKind, CodeError> {
    if specifier == HOST_MODULE {
        return Ok(SpecifierKind::Host);
    }

    if is_absolute(specifier) {
        return Err(CodeError::ImportEscape {
            specifier: specifier.to_owned(),
            base: base.to_owned(),
            at: ErrorLocation::unknown(),
        });
    }

    if specifier.starts_with("./") || specifier.starts_with("../") {
        return Ok(SpecifierKind::Relative);
    }

    Err(CodeError::BareImport {
        specifier: specifier.to_owned(),
        at: ErrorLocation::unknown(),
    })
}

/// Resolve `specifier` against `base` and return the canonical project-relative
/// module name (`src/lib.ts`, `hbot:host`, ...).
///
/// `exists` is the module table lookup, which keeps this function free of `std::fs`.
///
/// # Errors
///
/// [`CodeError::BareImport`], [`CodeError::ImportEscape`], or
/// [`CodeError::ModuleNotFound`].
pub fn resolve_module<F>(base: &str, specifier: &str, exists: F) -> Result<String, CodeError>
where
    F: Fn(&str) -> bool,
{
    match classify(base, specifier)? {
        SpecifierKind::Host => Ok(HOST_MODULE_ID.to_owned()),
        SpecifierKind::Relative => {
            let directory = base.rsplit_once('/').map_or("", |(dir, _)| dir);
            let joined = if directory.is_empty() {
                specifier.to_owned()
            } else {
                format!("{directory}/{specifier}")
            };
            let normalized = normalize(&joined, base, specifier)?;
            let candidates = candidates(&normalized);

            for candidate in &candidates {
                if exists(candidate) {
                    return Ok(candidate.clone());
                }
            }

            Err(CodeError::ModuleNotFound {
                specifier: specifier.to_owned(),
                base: base.to_owned(),
                tried: candidates.join(", "),
                at: ErrorLocation::unknown(),
            })
        }
    }
}

/// Lexically normalize `a/b/../c` into `a/c`, refusing to climb above the root.
fn normalize(joined: &str, base: &str, specifier: &str) -> Result<String, CodeError> {
    let mut segments: Vec<&str> = Vec::new();
    for segment in joined.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.pop().is_none() {
                    return Err(CodeError::ImportEscape {
                        specifier: specifier.to_owned(),
                        base: base.to_owned(),
                        at: ErrorLocation::unknown(),
                    });
                }
            }
            other => segments.push(other),
        }
    }

    if segments.is_empty() {
        return Err(CodeError::ImportEscape {
            specifier: specifier.to_owned(),
            base: base.to_owned(),
            at: ErrorLocation::unknown(),
        });
    }

    Ok(segments.join("/"))
}

/// Candidate module names for a normalized specifier, most specific first.
fn candidates(path: &str) -> Vec<String> {
    let last_segment = path.rsplit('/').next().unwrap_or(path);
    let has_extension = last_segment.contains('.');

    if has_extension {
        // `./lib.js` is how TypeScript users spell `./lib.ts`.
        if let Some(stem) = path.strip_suffix(".js") {
            return vec![path.to_owned(), format!("{stem}.ts")];
        }
        return vec![path.to_owned()];
    }

    let mut out: Vec<String> = EXTENSIONS
        .iter()
        .map(|ext| format!("{path}{ext}"))
        .collect();
    out.push(format!("{path}/index.ts"));
    out.push(path.to_owned());
    out
}

/// True for anything the OS would treat as an absolute path.
fn is_absolute(specifier: &str) -> bool {
    if specifier.starts_with('/') || specifier.starts_with('\\') {
        return true;
    }
    let bytes = specifier.as_bytes();
    // `C:/x`, `C:\x`
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table<'a>(names: &'a [&'a str]) -> impl Fn(&str) -> bool + 'a {
        move |candidate: &str| names.contains(&candidate)
    }

    /// `CodeError` wraps `std::io::Error`, so it cannot derive `PartialEq`;
    /// unwrap the success side to compare resolved module ids.
    fn resolved(result: Result<String, CodeError>) -> String {
        match result {
            Ok(value) => value,
            Err(error) => panic!("expected a resolved module, got: {error}"),
        }
    }

    #[test]
    fn bare_specifiers_are_rejected() {
        let err = classify("src/main.ts", "lodash").expect_err("bare import must fail");
        assert_eq!(err.code(), "HBOT_BARE_IMPORT");
        assert_eq!(
            err.to_string(),
            "HBOT_BARE_IMPORT: bare module specifier 'lodash' is not allowed; \
             use a relative import ('./x') or the built-in 'hbot' module"
        );

        for specifier in ["node:fs", "fs", "@scope/pkg", "lodash/fp", "hbot/extra"] {
            let err = classify("src/main.ts", specifier).expect_err("must be rejected");
            assert_eq!(err.code(), "HBOT_BARE_IMPORT", "specifier: {specifier}");
        }
    }

    #[test]
    fn host_module_is_allowed() {
        assert_eq!(
            classify("src/main.ts", "hbot").expect("hbot is the one allowed bare module"),
            SpecifierKind::Host
        );
        assert_eq!(
            resolved(resolve_module("src/main.ts", "hbot", table(&[]))),
            HOST_MODULE_ID
        );
    }

    #[test]
    fn relative_imports_resolve() {
        let exists = table(&["src/lib.ts", "src/handlers/ping.ts", "src/util/index.ts"]);
        assert_eq!(
            resolved(resolve_module("src/main.ts", "./lib", &exists)),
            "src/lib.ts"
        );
        assert_eq!(
            resolved(resolve_module("src/main.ts", "./handlers/ping.ts", &exists)),
            "src/handlers/ping.ts"
        );
        assert_eq!(
            resolved(resolve_module("src/handlers/ping.ts", "../util", &exists)),
            "src/util/index.ts"
        );
        assert_eq!(
            resolved(resolve_module("src/main.ts", "./lib.js", &exists)),
            "src/lib.ts"
        );
    }

    #[test]
    fn escaping_the_project_is_rejected() {
        let exists = table(&["src/lib.ts"]);
        let err = resolve_module("src/main.ts", "../../etc/passwd", &exists)
            .expect_err("escape must fail");
        assert_eq!(err.code(), "HBOT_IMPORT_ESCAPE");

        let err = classify("src/main.ts", "/etc/passwd").expect_err("absolute must fail");
        assert_eq!(err.code(), "HBOT_IMPORT_ESCAPE");
        let err = classify("src/main.ts", "C:/Windows/win.ini").expect_err("absolute must fail");
        assert_eq!(err.code(), "HBOT_IMPORT_ESCAPE");
    }

    #[test]
    fn missing_modules_are_reported() {
        let exists = table(&["src/lib.ts"]);
        let err = resolve_module("src/main.ts", "./nope", &exists).expect_err("must fail");
        assert_eq!(err.code(), "HBOT_MODULE_NOT_FOUND");
        let text = err.to_string();
        assert!(text.contains("./nope"), "{text}");
        assert!(text.contains("src/nope.ts"), "{text}");
    }
}
