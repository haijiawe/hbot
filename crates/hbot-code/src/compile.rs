//! TypeScript → JavaScript compilation for bot projects.
//!
//! The sandbox never runs a bundler and never runs `tsc`. Each `src/**/*.ts`
//! file is parsed by SWC, type-only syntax is stripped, and the result is fed to
//! QuickJS as an ES module. That keeps `docs/02-architecture.md` §6.2 honest:
//! *the file you write is the module that runs*, one module per file.
//!
//! Two things are produced per file:
//!
//! * the emitted JavaScript, and
//! * a **line map** from generated lines back to TypeScript lines, so a thrown
//!   `Error` in the isolate can be reported at the line the author wrote.
//!
//! Compilation is cached by content hash. The cache is a plain
//! [`std::collections::HashMap`] owned by [`Compiler`]; persistence across
//! process restarts is deliberately out of scope for S1 (see
//! `docs/spikes/S1-quickjs.md`).

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use swc_common::{FileName, GLOBALS, Globals, Mark, SourceMap, Span, Spanned, sync::Lrc};
use swc_ecma_ast::{CallExpr, Callee, EsVersion, Expr, ModuleDecl, NewExpr};
use swc_ecma_codegen::{Emitter, text_writer::JsWriter};
use swc_ecma_parser::{Syntax, TsSyntax, parse_file_as_program};
use swc_ecma_transforms_base::{fixer::fixer, resolver};
use swc_ecma_transforms_typescript::strip;
use swc_ecma_visit::{Visit, VisitWith};

use crate::error::{CodeError, ErrorLocation};
use crate::modules;

/// Where the entry module lives, relative to the project root.
pub const ENTRY_MODULE: &str = "src/main.ts";

/// Source extensions that get compiled.
const SOURCE_EXTENSIONS: [&str; 3] = [".ts", ".js", ".mjs"];

/// Which build to produce.
///
/// Release is what a bot runs with in production: no probes, no overhead.
/// Debug adds one `__hbot_trace` probe in front of every statement so a
/// debugger can stop on a TypeScript line (`docs/02-architecture.md` §6.5).
/// The two builds are otherwise identical, which is why they share one code
/// path and differ only in [`crate::instrument`] being applied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum BuildMode {
    /// No instrumentation.
    #[default]
    Release,
    /// Every statement gets a probe; a `fileId → path` table is attached.
    Debug,
}

impl BuildMode {
    /// Whether this build carries debug probes.
    #[must_use]
    pub fn is_debug(self) -> bool {
        matches!(self, Self::Debug)
    }
}

/// The `fileId → project-relative path` table baked into a debug build.
///
/// Probes carry only a number, never a string: a handler with a thousand
/// statements would otherwise allocate a thousand path constants. The host
/// looks the path up here, once, when a breakpoint actually fires.
#[derive(Clone, Debug, Default)]
pub struct DebugInfo {
    files: Vec<String>,
}

impl DebugInfo {
    /// The path for a probe's `fileId`.
    #[must_use]
    pub fn path(&self, file_id: u32) -> Option<&str> {
        usize::try_from(file_id)
            .ok()
            .and_then(|index| self.files.get(index))
            .map(String::as_str)
    }

    /// The id assigned to a module, for tests and for the debugger UI.
    #[must_use]
    pub fn id_of(&self, name: &str) -> Option<u32> {
        self.files
            .iter()
            .position(|file| file == name)
            .and_then(|index| u32::try_from(index).ok())
    }

    /// Every path, indexed by id.
    #[must_use]
    pub fn files(&self) -> &[String] {
        &self.files
    }

    /// Number of instrumented files.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether there is nothing to look up.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// One generated line mapped back to the TypeScript it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineMapping {
    /// 1-based line in the emitted JavaScript.
    pub generated_line: u32,
    /// 1-based line in the TypeScript source.
    pub source_line: u32,
    /// 1-based column in the TypeScript source.
    pub source_column: u32,
}

/// One compiled module.
#[derive(Clone, Debug)]
pub struct CompiledModule {
    /// Project-relative name, `/`-separated, e.g. `src/lib.ts`.
    pub name: String,
    /// Emitted JavaScript (ES module, no type syntax left).
    pub js: String,
    /// Line map, sorted by [`LineMapping::generated_line`].
    pub lines: Vec<LineMapping>,
    /// Resolved module ids this module imports, in source order.
    ///
    /// `hbot:host` appears here for `import { defineBot } from 'hbot'`. The list
    /// is the static link result, so the isolate can preload in dependency order
    /// without re-resolving, and so a missing file is a compile error rather
    /// than a hanging import at runtime.
    pub imports: Vec<String>,
}

impl CompiledModule {
    /// Translate a 1-based generated line to a TypeScript position.
    ///
    /// Falls back to the closest preceding mapping when the exact line has no
    /// entry (blank lines and generated glue produce none).
    #[must_use]
    pub fn source_position(&self, generated_line: u32) -> Option<(u32, u32)> {
        source_position(&self.lines, generated_line)
    }
}

/// Shared lookup used by both the stripped and the linked representation.
fn source_position(lines: &[LineMapping], generated_line: u32) -> Option<(u32, u32)> {
    let index = match lines.binary_search_by_key(&generated_line, |m| m.generated_line) {
        Ok(index) => index,
        Err(0) => return lines.first().map(|m| (m.source_line, m.source_column)),
        Err(index) => index - 1,
    };
    lines.get(index).map(|m| (m.source_line, m.source_column))
}

/// How long compilation took and how much the cache helped.
#[derive(Clone, Copy, Debug, Default)]
pub struct CompileStats {
    /// Number of modules in the project.
    pub files: usize,
    /// Modules served from the content-hash cache.
    pub cache_hits: usize,
    /// Modules that had to be parsed and emitted.
    pub cache_misses: usize,
    /// Wall-clock time spent in [`Compiler::compile_project`].
    pub elapsed: Duration,
    /// Total TypeScript bytes read.
    pub source_bytes: usize,
    /// Total JavaScript bytes emitted.
    pub emitted_bytes: usize,
}

/// A whole project, compiled and ready to hand to an isolate.
#[derive(Clone, Debug)]
pub struct CompiledProject {
    modules: BTreeMap<String, CompiledModule>,
    entry: String,
    stats: CompileStats,
    /// `Some` only for [`BuildMode::Debug`].
    debug: Option<DebugInfo>,
}

impl CompiledProject {
    /// All modules, keyed by project-relative name.
    #[must_use]
    pub fn modules(&self) -> &BTreeMap<String, CompiledModule> {
        &self.modules
    }

    /// The `fileId → path` table, or `None` for a release build.
    #[must_use]
    pub fn debug_info(&self) -> Option<&DebugInfo> {
        self.debug.as_ref()
    }

    /// Whether this project was built with [`BuildMode::Debug`].
    #[must_use]
    pub fn is_debug(&self) -> bool {
        self.debug.is_some()
    }

    /// Translate a 1-based generated line of `module` to a TypeScript position.
    ///
    /// This is the reverse of what [`CompiledModule::source_position`] does for a
    /// module the caller already has a handle on, and is what lets a paused
    /// call stack name the TypeScript line for *intermediate* frames (the walk
    /// only knows module names and generated lines).
    #[must_use]
    pub fn source_position(&self, module: &str, generated_line: u32) -> Option<(u32, u32)> {
        self.modules.get(module)?.source_position(generated_line)
    }

    /// The entry module name (`src/main.ts`).
    #[must_use]
    pub fn entry(&self) -> &str {
        &self.entry
    }

    /// Look up one module.
    #[must_use]
    pub fn module(&self, name: &str) -> Option<&CompiledModule> {
        self.modules.get(name)
    }

    /// Whether a module exists — the module-table predicate handed to
    /// [`modules::resolve_module`].
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.modules.contains_key(name)
    }

    /// Compilation statistics.
    #[must_use]
    pub fn stats(&self) -> CompileStats {
        self.stats
    }

    /// Rewrite generated positions in a QuickJS stack trace to TypeScript
    /// positions.
    ///
    /// QuickJS reports frames as `at fn (src/main.ts:7:13)`; because every
    /// module was declared to QuickJS under its project-relative name, the only
    /// thing that needs fixing is the line/column pair.
    #[must_use]
    pub fn remap_stack(&self, stack: &str) -> String {
        stack
            .lines()
            .map(|line| self.remap_stack_line(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn remap_stack_line(&self, line: &str) -> String {
        for (name, module) in &self.modules {
            let Some(at) = line.find(name.as_str()) else {
                continue;
            };
            let head = &line[..at];
            let tail = &line[at + name.len()..];
            let Some((generated_line, _column, rest)) = split_position(tail) else {
                continue;
            };
            let Some((source_line, source_column)) = module.source_position(generated_line) else {
                continue;
            };
            return format!("{head}{name}:{source_line}:{source_column}{rest}");
        }
        line.to_owned()
    }
}

/// Parse a `:line:column` suffix, returning the numbers and the untouched rest.
fn split_position(tail: &str) -> Option<(u32, u32, &str)> {
    let tail = tail.strip_prefix(':')?;
    let (line, tail) = tail.split_once(':')?;
    let line: u32 = line.parse().ok()?;
    let digit_count = tail.chars().take_while(char::is_ascii_digit).count();
    let (column, rest) = tail.split_at(digit_count);
    let column: u32 = column.parse().ok()?;
    Some((line, column, rest))
}

/// Cache key.
///
/// The content hash alone is not enough: a debug build and a release build of
/// the same bytes emit different JavaScript, and the `fileId` baked into every
/// probe depends on which project the file belongs to. Keying on all four
/// keeps a debug compile from ever serving a release module (or a module from a
/// differently ordered project).
type CacheKey = (String, u64, BuildMode, u32);

/// Owns the content-hash cache.
///
/// Reuse one `Compiler` for the lifetime of an editing session; create a fresh
/// one per cold compile. The cache never expires — entries are keyed by the hash
/// of the file content, so an edit is simply a different key.
#[derive(Debug, Default)]
pub struct Compiler {
    cache: HashMap<CacheKey, StrippedFile>,
    last_stats: CompileStats,
}

impl Compiler {
    /// Create an empty compiler.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Compile every source file under `root/src`, require `src/main.ts`, and
    /// link the import graph.
    ///
    /// # Errors
    ///
    /// * [`CodeError::Io`] when the tree cannot be read,
    /// * [`CodeError::NoEntry`] when `src/main.ts` is missing,
    /// * [`CodeError::Compile`] / [`CodeError::ForbiddenSyntax`] /
    ///   [`CodeError::BareImport`] for rejected source,
    /// * [`CodeError::ModuleNotFound`] / [`CodeError::ImportEscape`] when an
    ///   import does not link.
    pub fn compile_project(&mut self, root: &Path) -> Result<CompiledProject, CodeError> {
        self.compile_project_with_entry(root, ENTRY_MODULE)
    }

    /// Compile with an explicit [`BuildMode`], using [`ENTRY_MODULE`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::compile_project`].
    pub fn compile_project_mode(
        &mut self,
        root: &Path,
        mode: BuildMode,
    ) -> Result<CompiledProject, CodeError> {
        self.compile_project_with_entry_mode(root, ENTRY_MODULE, mode)
    }

    /// Compile every source file under `root/src` with a manifest-chosen entry.
    ///
    /// `entry` is a `/`-separated project-relative module name (see
    /// [`crate::manifest::Manifest::entry_name`]). It must exist under `root/src`
    /// and be reachable the same way any other module is; the filesystem layout is
    /// still `src/**`, only *which* file is the entry changes
    /// (`docs/06-ide.md`: 入口由 `hbot.json` 的 `entry` 指定).
    ///
    /// # Errors
    ///
    /// Same as [`Self::compile_project`]; [`CodeError::NoEntry`] names the
    /// requested `entry`.
    pub fn compile_project_with_entry(
        &mut self,
        root: &Path,
        entry: &str,
    ) -> Result<CompiledProject, CodeError> {
        self.compile_project_with_entry_mode(root, entry, BuildMode::Release)
    }

    /// Compile with an explicit [`BuildMode`].
    ///
    /// Every other entry point in this module is one of the two modes; this is
    /// the one the debugger uses. See [`Compiler::compile_project_with_entry`]
    /// for the error cases.
    ///
    /// # Errors
    ///
    /// Same as [`Self::compile_project`].
    pub fn compile_project_with_entry_mode(
        &mut self,
        root: &Path,
        entry: &str,
        mode: BuildMode,
    ) -> Result<CompiledProject, CodeError> {
        let started = Instant::now();
        let mut stats = CompileStats::default();

        let sources = collect_sources(root)?;
        if !sources.iter().any(|(name, _)| name == entry) {
            return Err(CodeError::NoEntry {
                path: entry.to_owned(),
            });
        }

        // Ids are the sorted position in the source list, so they are stable for
        // a given project and never collide with another file's probes.
        let debug = if mode.is_debug() {
            Some(DebugInfo {
                files: sources.iter().map(|(name, _)| name.clone()).collect(),
            })
        } else {
            None
        };

        let mut stripped = BTreeMap::new();
        let mut kept: HashMap<CacheKey, StrippedFile> = HashMap::new();

        for (index, (name, path)) in sources.iter().enumerate() {
            let source = std::fs::read_to_string(path).map_err(|source| CodeError::Io {
                path: path.clone(),
                source,
            })?;
            stats.source_bytes += source.len();

            let file_id = u32::try_from(index).unwrap_or(u32::MAX);
            let hash = content_hash(&source);
            let key = (name.clone(), hash, mode, file_id);
            let file = if let Some(cached) = self.cache.get(&key) {
                stats.cache_hits += 1;
                cached.clone()
            } else {
                stats.cache_misses += 1;
                strip_module(name, &source, mode, file_id)?
            };

            stats.emitted_bytes += file.js.len();
            kept.insert(key, file.clone());
            stripped.insert(name.clone(), file);
        }

        let modules = link(&stripped)?;
        self.cache = kept;
        stats.files = modules.len();
        stats.elapsed = started.elapsed();
        self.last_stats = stats;

        Ok(CompiledProject {
            modules,
            entry: entry.to_owned(),
            stats,
            debug,
        })
    }

    /// Statistics from the most recent [`Self::compile_project`] call.
    #[must_use]
    pub fn last_stats(&self) -> CompileStats {
        self.last_stats
    }
}

/// One module after type stripping but before linking.
///
/// Kept separate from [`CompiledModule`] because import resolution needs the
/// whole project: a specifier is only resolvable once every file is known.
#[derive(Clone, Debug)]
struct StrippedFile {
    js: String,
    lines: Vec<LineMapping>,
    imports: Vec<ImportSpecifier>,
}

/// A specifier exactly as written, plus where it was written.
#[derive(Clone, Debug)]
struct ImportSpecifier {
    text: String,
    line: u32,
    column: u32,
}

/// Resolve every import of every module against the compiled set.
///
/// This is where a project stops being a bag of files: `./nope` (nothing there)
/// and `../../etc` (leaves the project) are reported here, pointing at the
/// `import` statement that wrote them.
///
/// The link pass is not just diagnostics. `rquickjs`' module loader reports a
/// failed load by setting a QuickJS exception while the import promise stays
/// pending, which would hang the isolate forever (measured in S1). Every module
/// that can be reached through the entry is therefore known good *before* the
/// isolate sees it; the loader's own checks stay as defence in depth.
fn link(
    stripped: &BTreeMap<String, StrippedFile>,
) -> Result<BTreeMap<String, CompiledModule>, CodeError> {
    let exists = |candidate: &str| stripped.contains_key(candidate);
    let mut linked = BTreeMap::new();

    for (name, file) in stripped {
        let mut imports = Vec::with_capacity(file.imports.len());
        for specifier in &file.imports {
            let resolved = modules::resolve_module(name, &specifier.text, exists)
                .map_err(|error| located(error, name, specifier.line, specifier.column))?;
            imports.push(resolved);
        }

        linked.insert(
            name.clone(),
            CompiledModule {
                name: name.clone(),
                js: file.js.clone(),
                lines: file.lines.clone(),
                imports,
            },
        );
    }

    Ok(linked)
}

/// Attach the import statement's position to a link error.
fn located(error: CodeError, file: &str, line: u32, column: u32) -> CodeError {
    let at = crate::error::ErrorLocation::at(crate::error::SourcePos {
        file: file.to_owned(),
        line,
        column,
    });
    match error {
        CodeError::ImportEscape {
            specifier, base, ..
        } => CodeError::ImportEscape {
            specifier,
            base,
            at,
        },
        CodeError::ModuleNotFound {
            specifier,
            base,
            tried,
            ..
        } => CodeError::ModuleNotFound {
            specifier,
            base,
            tried,
            at,
        },
        other => other,
    }
}

/// Convenience wrapper: compile a project with a throwaway cache.
///
/// # Errors
///
/// See [`Compiler::compile_project`].
pub fn compile_project(root: &Path) -> Result<CompiledProject, CodeError> {
    Compiler::new().compile_project(root)
}

/// Convenience wrapper: compile with a throwaway cache and a manifest-chosen
/// entry.
///
/// # Errors
///
/// See [`Compiler::compile_project_with_entry`].
pub fn compile_project_with_entry(root: &Path, entry: &str) -> Result<CompiledProject, CodeError> {
    Compiler::new().compile_project_with_entry(root, entry)
}

/// Convenience wrapper: compile a **debug** project (instrumented) with a
/// throwaway cache.
///
/// This is the build the debugger runs; see [`BuildMode`].
///
/// # Errors
///
/// See [`Compiler::compile_project`].
pub fn compile_project_debug(root: &Path) -> Result<CompiledProject, CodeError> {
    Compiler::new().compile_project_mode(root, BuildMode::Debug)
}

/// Convenience wrapper: compile a **debug** project with a manifest-chosen
/// entry.
///
/// # Errors
///
/// See [`Compiler::compile_project_with_entry`].
pub fn compile_project_with_entry_debug(
    root: &Path,
    entry: &str,
) -> Result<CompiledProject, CodeError> {
    Compiler::new().compile_project_with_entry_mode(root, entry, BuildMode::Debug)
}

/// Recursively collect `root/src/**/*.{ts,js,mjs}`, excluding `.d.ts`.
fn collect_sources(root: &Path) -> Result<Vec<(String, PathBuf)>, CodeError> {
    let source_root = root.join("src");
    let mut out = Vec::new();
    walk(&source_root, root, &mut out)?;
    out.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(out)
}

fn walk(directory: &Path, root: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<(), CodeError> {
    let entries = std::fs::read_dir(directory).map_err(|source| CodeError::Io {
        path: directory.to_path_buf(),
        source,
    })?;

    for entry in entries {
        let entry = entry.map_err(|source| CodeError::Io {
            path: directory.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| CodeError::Io {
            path: path.clone(),
            source,
        })?;

        if file_type.is_dir() {
            walk(&path, root, out)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }

        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        // Type declarations describe types only; there is nothing to run.
        if name.ends_with(".d.ts") {
            continue;
        }
        if !SOURCE_EXTENSIONS
            .iter()
            .any(|extension| name.ends_with(extension))
        {
            continue;
        }

        let relative = path
            .strip_prefix(root)
            .map_err(|_| CodeError::ImportEscape {
                specifier: name.to_owned(),
                base: String::new(),
                at: ErrorLocation::unknown(),
            })?;
        let module_name = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        out.push((module_name, path));
    }

    Ok(())
}

/// Compile a single module: parse → gate forbidden syntax → scan imports →
/// strip types → (debug only) instrument → emit + line map.
fn strip_module(
    name: &str,
    source: &str,
    mode: BuildMode,
    file_id: u32,
) -> Result<StrippedFile, CodeError> {
    GLOBALS.set(&Globals::default(), || {
        strip_module_in_globals(name, source, mode, file_id)
    })
}

fn strip_module_in_globals(
    name: &str,
    source: &str,
    mode: BuildMode,
    file_id: u32,
) -> Result<StrippedFile, CodeError> {
    let cm: Lrc<SourceMap> = Default::default();
    let fm = cm.new_source_file(FileName::Custom(name.to_owned()).into(), source.to_owned());

    let mut recovered = Vec::new();
    let program = parse_file_as_program(
        &fm,
        Syntax::Typescript(TsSyntax::default()),
        EsVersion::latest(),
        None,
        &mut recovered,
    )
    .map_err(|error| compile_error(&cm, name, error.span(), &error.kind().msg()))?;

    if let Some(first) = recovered.first() {
        return Err(compile_error(&cm, name, first.span(), &first.kind().msg()));
    }

    let mut gate = ModuleGate {
        cm: &cm,
        file: name,
        error: None,
        imports: Vec::new(),
    };
    program.visit_with(&mut gate);
    if let Some(error) = gate.error.take() {
        return Err(error);
    }
    let imports = gate.imports;

    let unresolved_mark = Mark::new();
    let top_level_mark = Mark::new();
    let mut program = program
        .apply(resolver(unresolved_mark, top_level_mark, true))
        .apply(strip(unresolved_mark, top_level_mark))
        .apply(fixer(None));

    // Debug builds get a probe in front of every statement. This runs *after*
    // the type strip so the probes never have to understand TypeScript, and
    // *before* codegen so the line map is built from the instrumented tree —
    // each probe reuses its statement's span, so the map still lands on the
    // line the author wrote.
    if mode.is_debug() {
        crate::instrument::instrument_program(&mut program, &cm, file_id);
    }

    let mut buffer = Vec::new();
    let mut raw_map: Vec<(swc_common::BytePos, swc_common::LineCol)> = Vec::new();
    {
        let writer = JsWriter::new(cm.clone(), "\n", &mut buffer, Some(&mut raw_map));
        let mut emitter = Emitter {
            cfg: Default::default(),
            cm: cm.clone(),
            comments: None,
            wr: writer,
        };
        emitter
            .emit_program(&program)
            .map_err(|error| CodeError::Compile {
                file: name.to_owned(),
                line: 1,
                column: 1,
                message: format!("codegen failed: {error}"),
            })?;
    }

    let js = String::from_utf8(buffer).map_err(|error| CodeError::Compile {
        file: name.to_owned(),
        line: 1,
        column: 1,
        message: format!("codegen produced invalid UTF-8: {error}"),
    })?;

    let mut lines: Vec<LineMapping> = Vec::new();
    for (byte_pos, generated) in raw_map {
        let Ok(location) = cm.try_lookup_char_pos(byte_pos) else {
            continue;
        };
        let generated_line = generated.line.saturating_add(1);
        if lines
            .last()
            .is_some_and(|previous| previous.generated_line == generated_line)
        {
            continue;
        }
        lines.push(LineMapping {
            generated_line,
            source_line: u32::try_from(location.line).unwrap_or(u32::MAX),
            source_column: u32::try_from(location.col_display)
                .unwrap_or(0)
                .saturating_add(1),
        });
    }
    lines.sort_by_key(|mapping| mapping.generated_line);

    Ok(StrippedFile { js, lines, imports })
}

fn compile_error(cm: &SourceMap, name: &str, span: Span, message: &str) -> CodeError {
    let location = cm.lookup_char_pos(span.lo());
    CodeError::Compile {
        file: name.to_owned(),
        line: u32::try_from(location.line).unwrap_or(u32::MAX),
        column: u32::try_from(location.col_display)
            .unwrap_or(0)
            .saturating_add(1),
        message: message.to_owned(),
    }
}

/// FNV-1a 64. Not cryptographic — the cache only needs "did the bytes change".
fn content_hash(source: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in source.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Walks the pre-strip AST and rejects what the sandbox contract forbids:
/// bare imports, `eval`, `new Function`, `require`, dynamic `import()`, and
/// TypeScript `import x = require()`.
struct ModuleGate<'a> {
    cm: &'a SourceMap,
    file: &'a str,
    error: Option<CodeError>,
    imports: Vec<ImportSpecifier>,
}

impl ModuleGate<'_> {
    fn report(&mut self, span: Span, what: String) {
        if self.error.is_some() {
            return;
        }
        let location = self.cm.lookup_char_pos(span.lo());
        self.error = Some(CodeError::ForbiddenSyntax {
            file: self.file.to_owned(),
            line: u32::try_from(location.line).unwrap_or(u32::MAX),
            column: u32::try_from(location.col_display)
                .unwrap_or(0)
                .saturating_add(1),
            what,
        });
    }

    fn check_specifier(&mut self, span: Span, specifier: &str) {
        if self.error.is_some() {
            return;
        }
        let location = self.cm.lookup_char_pos(span.lo());
        let line = u32::try_from(location.line).unwrap_or(u32::MAX);
        let column = u32::try_from(location.col_display)
            .unwrap_or(0)
            .saturating_add(1);
        match modules::classify(self.file, specifier) {
            Ok(_) => {
                // Recorded in source order so the link pass can report the
                // exact `import` statement that failed to resolve.
                self.imports.push(ImportSpecifier {
                    text: specifier.to_owned(),
                    line,
                    column,
                });
            }
            Err(CodeError::BareImport { specifier, .. }) => {
                self.error = Some(CodeError::BareImport {
                    specifier,
                    at: ErrorLocation::at(crate::error::SourcePos {
                        file: self.file.to_owned(),
                        line,
                        column,
                    }),
                });
            }
            Err(other) => self.error = Some(other),
        }
    }
}

impl Visit for ModuleGate<'_> {
    fn visit_module_decl(&mut self, node: &ModuleDecl) {
        match node {
            ModuleDecl::Import(decl) => {
                // Specifiers are WTF-8 atoms; one that is not valid UTF-8 cannot
                // name a file in the project, so it is folded to a lossy string
                // and rejected by the classifier like any other non-relative
                // specifier.
                let specifier = decl.src.value.to_string_lossy();
                self.check_specifier(decl.span, &specifier);
            }
            ModuleDecl::ExportNamed(named) => {
                if let Some(source) = &named.src {
                    let specifier = source.value.to_string_lossy();
                    self.check_specifier(source.span, &specifier);
                }
            }
            ModuleDecl::ExportAll(all) => {
                let specifier = all.src.value.to_string_lossy();
                self.check_specifier(all.span, &specifier);
            }
            ModuleDecl::TsImportEquals(import) => {
                self.report(import.span, "import x = require()".to_owned());
            }
            _ => {}
        }
        node.visit_children_with(self);
    }

    fn visit_call_expr(&mut self, node: &CallExpr) {
        match &node.callee {
            Callee::Import(_) => self.report(node.span, "dynamic import()".to_owned()),
            Callee::Expr(callee) => {
                if let Expr::Ident(ident) = &**callee {
                    match ident.sym.as_str() {
                        "eval" => self.report(node.span, "eval()".to_owned()),
                        "require" => self.report(node.span, "require()".to_owned()),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        node.visit_children_with(self);
    }

    fn visit_new_expr(&mut self, node: &NewExpr) {
        if let Expr::Ident(ident) = &*node.callee {
            if ident.sym.as_str() == "Function" {
                self.report(node.span, "new Function()".to_owned());
            }
        }
        node.visit_children_with(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Release build of one module — what every non-debug caller gets.
    fn strip_release(name: &str, source: &str) -> Result<StrippedFile, CodeError> {
        strip_module(name, source, BuildMode::Release, 0)
    }

    fn temp_project(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("hbot-code-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        for (relative, content) in files {
            let path = root.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create dir");
            }
            std::fs::write(&path, content).expect("write fixture");
        }
        root
    }

    #[test]
    fn strips_types_and_keeps_lines() {
        let source = "const answer: number = 41;\n\
                      export function bump(value: number): number {\n\
                      \x20   return value + 1;\n\
                      }\n";
        let module = strip_release("src/lib.ts", source).expect("strips");
        assert!(!module.js.contains(": number"), "{}", module.js);
        assert!(module.js.contains("const answer = 41;"), "{}", module.js);
        assert_eq!(source_position(&module.lines, 1), Some((1, 1)));
        assert_eq!(source_position(&module.lines, 2), Some((2, 1)));
        // The body line keeps its TypeScript line; its column is the column of
        // the token that starts it (the source is indented), so only the line is
        // asserted exactly. Line preservation is what S1 needs: it is what makes
        // a runtime error point back at the line the author wrote.
        assert_eq!(
            source_position(&module.lines, 3).map(|(line, _)| line),
            Some(3)
        );
    }

    #[test]
    fn bare_imports_are_rejected_at_compile_time() {
        let error =
            strip_release("src/main.ts", "import _ from 'lodash';\n").expect_err("must reject");
        assert_eq!(error.code(), "HBOT_BARE_IMPORT");
    }

    #[test]
    fn eval_and_dynamic_import_are_rejected() {
        for source in [
            "export default () => eval('1');",
            "const m = import('./x'); export default m;",
            "const f = new Function('return 1'); export default f;",
            "const fs = require('fs'); export default fs;",
        ] {
            let error = strip_release("src/main.ts", source).expect_err("must reject");
            assert_eq!(error.code(), "HBOT_FORBIDDEN_SYNTAX", "source: {source}");
        }
    }

    #[test]
    fn syntax_errors_point_at_the_ts_line() {
        let error =
            strip_release("src/main.ts", "const a = 1;\nconst = ;\n").expect_err("must fail");
        assert_eq!(error.code(), "HBOT_COMPILE");
        let position = error.position().expect("position");
        assert_eq!(position.file, "src/main.ts");
        assert!(position.line >= 1);
    }

    #[test]
    fn cache_reuses_stripped_modules() {
        let root = temp_project(
            "cache",
            &[
                (
                    "src/main.ts",
                    "import { greet } from './lib';\nexport default greet;\n",
                ),
                (
                    "src/lib.ts",
                    "export const greet = (name: string) => `hi ${name}`;\n",
                ),
                ("src/types.d.ts", "declare const nope: string;\n"),
            ],
        );
        let mut compiler = Compiler::new();
        let first = compiler.compile_project(&root).expect("first compile");
        assert_eq!(first.stats().files, 2, "the .d.ts file must be skipped");
        assert_eq!(first.stats().cache_misses, 2);
        assert_eq!(first.stats().cache_hits, 0);

        let second = compiler.compile_project(&root).expect("second compile");
        assert_eq!(second.stats().cache_hits, 2);
        assert_eq!(second.stats().cache_misses, 0);
        assert_eq!(
            first.module("src/lib.ts").map(|m| m.js.as_str()),
            second.module("src/lib.ts").map(|m| m.js.as_str())
        );

        std::fs::write(
            root.join("src/lib.ts"),
            "export const greet = (name: string) => `hello ${name}`;\n",
        )
        .expect("write");
        let third = compiler.compile_project(&root).expect("third compile");
        assert_eq!(third.stats().cache_misses, 1);
        assert!(
            third
                .module("src/lib.ts")
                .is_some_and(|module| module.js.contains("hello")),
            "edited file must be recompiled"
        );
    }

    #[test]
    fn missing_entry_is_reported() {
        let root = temp_project("no-entry", &[("src/lib.ts", "export const a = 1;\n")]);
        let error = compile_project(&root).expect_err("must fail");
        assert_eq!(error.code(), "HBOT_NO_ENTRY");
    }

    #[test]
    fn a_manifest_chosen_entry_replaces_src_main_ts() {
        // `docs/06-ide.md`: the entry comes from `hbot.json`, not from a hard
        // coded `src/main.ts`. Everything under `src/**` is still compiled; only
        // the module the host imports changes.
        let root = temp_project(
            "custom-entry",
            &[
                (
                    "src/bot.ts",
                    "import { greet } from './lib';\nexport default greet('bot');\n",
                ),
                (
                    "src/lib.ts",
                    "export const greet = (name: string) => `hi ${name}`;\n",
                ),
            ],
        );

        // No `src/main.ts` anywhere: the default entry must still fail...
        assert_eq!(
            compile_project(&root).expect_err("默认入口不存在").code(),
            "HBOT_NO_ENTRY"
        );

        // ...but the manifest-chosen one compiles, links and is recorded.
        let project = compile_project_with_entry(&root, "src/bot.ts").expect("清单指定入口应编过");
        assert_eq!(project.entry(), "src/bot.ts");
        assert_eq!(project.stats().files, 2);
        assert!(project.contains("src/lib.ts"));
        assert_eq!(
            project
                .module("src/bot.ts")
                .map(|module| module.imports.as_slice()),
            Some(["src/lib.ts".to_owned()].as_slice()),
            "import 应链接到 src/lib.ts"
        );

        // A non-existent custom entry names itself in the error.
        let error = compile_project_with_entry(&root, "src/nope.ts").expect_err("应报缺入口");
        assert_eq!(error.code(), "HBOT_NO_ENTRY");
        assert!(error.to_string().contains("src/nope.ts"), "{error}");
    }

    #[test]
    fn stack_positions_are_remapped() {
        let root = temp_project(
            "remap",
            &[
                (
                    "src/main.ts",
                    "import { boom } from './lib';\nexport default boom;\n",
                ),
                (
                    "src/lib.ts",
                    "export function boom() {\n  throw new Error('x');\n}\n",
                ),
            ],
        );
        let project = compile_project(&root).expect("compiles");
        let remapped = project.remap_stack("    at boom (src/lib.ts:2:9)");
        assert!(remapped.contains("src/lib.ts:2:"), "{remapped}");
    }
}
