//! Source-level instrumentation for **debug** builds (`docs/02-architecture.md`
//! §6.5).
//!
//! QuickJS has no breakpoint API, so the only way to stop on a TypeScript line
//! is to ask the runtime: every statement gets a cheap probe in front of it.
//! Release builds never see this module — [`crate::compile::BuildMode::Release`]
//! goes straight from `strip` to codegen.
//!
//! # Shape of an instrumented statement
//!
//! ```js
//! if (__hbot_dbg && __hbot_trace(3, 12, 7)) {
//!     __hbot_pause(3, 12, 7, function () {
//!         var __hbot_vars = {};
//!         try { __hbot_vars["a"] = a; } catch (__hbot_e) {}
//!         return __hbot_vars;
//!     });
//! }
//! <original statement, untouched>
//! ```
//!
//! * The original statement is **not** moved into a block and **not** wrapped:
//!   wrapping `let`/`const`/`function` declarations in a block would change
//!   their scope, so the probe is spliced in as a *sibling* in front of it. The
//!   original node keeps its span, so the line map from
//!   [`crate::compile`] still points at the TypeScript line it always did.
//! * `__hbot_dbg` is a `globalThis` boolean that the isolate sets to `true` only
//!   for an event dispatched through
//!   [`crate::Sandbox::dispatch_with_debug`]. When it is `false` the whole probe
//!   costs one global read, which is the ~1× overhead a debug build pays when it
//!   is *not* being debugged.
//! * `__hbot_trace` is the O(1) host fast path: it answers "should this
//!   statement stop?" without touching user values.
//! * The locals closure is only allocated when `__hbot_trace` said yes, so
//!   marshalling variables costs nothing on the statements that never stop.
//!
//! # Deliberate coverage limits (all documented, none silent)
//!
//! * **Module top-level statements are not instrumented.** They run during
//!   `bootstrap`, before there is a handler to stop in and before any
//!   [`crate::Sandbox`] caller is waiting for a pause; instrumenting them would
//!   only add cost to startup. The `onMessage` body and everything it calls is
//!   covered.
//! * **A bare single-statement body is covered by its parent statement.**
//!   `for (const x of xs) f(x);` probes the `for` (that is the statement holding
//!   the line), not `f(x)`. Loops and branches written with braces get a probe
//!   on every statement inside the braces.
//! * Directives (`"use strict";`) and empty statements (`;`) are skipped: they
//!   are no-ops, and a probe in front of a directive would turn it into a plain
//!   string expression.
//! * Names visible to the locals closure are a **superset** approximation: the
//!   scope walk collects every binding of the enclosing functions (and the
//!   enclosing blocks' `var`/`let`/`const`) rather than a per-statement view. A
//!   name that is not yet initialised (temporal dead zone) throws inside its own
//!   `try`, so it is simply omitted — it can never break the handler.

use std::collections::BTreeSet;

use swc_common::{BytePos, SourceMap, Span, Spanned, SyntaxContext};
use swc_ecma_ast::{
    ArrowExpr, ArrowFunctionBody, AssignExpr, AssignOp, AssignTarget, BinExpr, BinaryOp,
    BindingIdent, BlockStmt, CallExpr, Callee, CatchClause, ClassDecl, ClassExpr, ComputedPropName,
    Expr, ExprOrSpread, ExprStmt, FnDecl, FnExpr, Function, FunctionBody, Ident, IfStmt, Lit,
    MemberExpr, MemberProp, Number, ObjectLit, Pat, Program, SimpleAssignTarget, Stmt, Str,
    TryStmt, VarDecl, VarDeclKind, VarDeclarator,
};
use swc_ecma_visit::{Visit, VisitMut, VisitMutWith, VisitWith};

/// Global flag flipped by the isolate for one debug-dispatched event.
pub(crate) const DEBUG_FLAG: &str = "__hbot_dbg";
/// Host fast path: `(fileId, line, column) -> bool`.
pub(crate) const TRACE_FN: &str = "__hbot_trace";
/// Host slow path: `(fileId, line, column, localsThunk) -> void`.
pub(crate) const PAUSE_FN: &str = "__hbot_pause";

/// Receiver that collects the locals for the scope walk inside the thunk.
const VARS_BINDING: &str = "__hbot_vars";
/// Catch binding used by the per-variable guard.
const CATCH_BINDING: &str = "__hbot_e";

/// Upper bound on how many names one locals closure captures.
///
/// The closure is emitted at every statement, so an unbounded list would bloat
/// debug builds quadratically in function size. 32 covers ordinary handlers;
/// beyond that the UI simply shows fewer variables (never wrong ones).
const MAX_CAPTURED_LOCALS: usize = 32;

/// Names that are either not variables or are context-dependent enough that
/// capturing them would be misleading.
const SKIPPED_NAMES: [&str; 6] = ["arguments", "this", "eval", "require", "undefined", "NaN"];

/// Instrument every statement of `program` that will run with a handler on the
/// stack.
///
/// `source_file_id` is the index the compiler assigned this module in
/// [`crate::compile::DebugInfo`]; it is baked into every probe as a number so
/// the host can map it back to a path without sending a string per statement.
///
/// Returns the number of probes inserted (used by tests and by the compile
/// statistics in debug logs).
pub(crate) fn instrument_program(
    program: &mut Program,
    cm: &SourceMap,
    source_file_id: u32,
) -> usize {
    let mut instrumenter = Instrumenter {
        cm,
        source_file_id,
        scopes: Vec::new(),
        probes: 0,
    };
    program.visit_mut_with(&mut instrumenter);
    instrumenter.probes
}

struct Instrumenter<'a> {
    cm: &'a SourceMap,
    source_file_id: u32,
    /// One entry per enclosing function (outermost first), each holding the
    /// names declared anywhere inside it.
    scopes: Vec<BTreeSet<String>>,
    probes: usize,
}

impl Instrumenter<'_> {
    /// Source position (1-based line, 1-based column) of a statement.
    ///
    /// `None` means the node has no real position — a synthesized node from the
    /// TypeScript strip pass, or a source map miss. Such statements are left
    /// alone: a probe with a made-up line would let a breakpoint fire in the
    /// wrong place, which is worse than not stopping at all.
    fn source_position(&self, span: Span) -> Option<(u32, u32)> {
        if span.lo() == BytePos(0) {
            return None;
        }
        let location = self.cm.try_lookup_char_pos(span.lo()).ok()?;
        let line = u32::try_from(location.line).ok()?;
        let column = u32::try_from(location.col_display).ok()?.saturating_add(1);
        Some((line, column))
    }

    /// The names a locals closure emitted here should try to read.
    ///
    /// Union of every enclosing function's bindings, innermost last, so an inner
    /// binding overwrites the outer one in the produced object.
    fn visible_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for scope in &self.scopes {
            for name in scope {
                if !seen.insert(name.as_str()) {
                    continue;
                }
                names.push(name.clone());
            }
        }
        names.retain(|name| !is_skipped(name));
        // Keep the *innermost* names when trimming: they are the ones a person
        // looking at this statement expects to see.
        if names.len() > MAX_CAPTURED_LOCALS {
            names.drain(..names.len() - MAX_CAPTURED_LOCALS);
        }
        names
    }

    /// Build the probe statement for `stmt`, or `None` when the statement should
    /// not be probed.
    fn probe_for(&self, stmt: &Stmt) -> Option<Stmt> {
        if !should_probe(stmt) {
            return None;
        }
        let span = stmt.span();
        let (line, column) = self.source_position(span)?;
        let names = self.visible_names();
        Some(self.build_probe(span, line, column, &names))
    }

    fn build_probe(&self, span: Span, line: u32, column: u32, names: &[String]) -> Stmt {
        let file = self.source_file_id;
        let locals = function_expr(span, locals_thunk(span, names));
        let pause = call(
            PAUSE_FN,
            vec![
                number_expr(f64::from(file), span),
                number_expr(f64::from(line), span),
                number_expr(f64::from(column), span),
                locals,
            ],
            span,
        );
        let trace = call(
            TRACE_FN,
            vec![
                number_expr(f64::from(file), span),
                number_expr(f64::from(line), span),
                number_expr(f64::from(column), span),
            ],
            span,
        );
        let enabled = ident_expr(DEBUG_FLAG, span);
        let test = Expr::Bin(BinExpr {
            span,
            op: BinaryOp::LogicalAnd,
            left: Box::new(enabled),
            right: Box::new(trace),
        });
        Stmt::If(IfStmt {
            span,
            test: Box::new(test),
            cons: Box::new(block(vec![expr_stmt(pause, span)], span)),
            alt: None,
        })
    }

    fn push_scope<'p>(&mut self, params: impl Iterator<Item = &'p Pat>, body: Option<&[Stmt]>) {
        let mut collector = NameCollector::default();
        for param in params {
            param.visit_with(&mut collector);
        }
        if let Some(body) = body {
            for stmt in body {
                stmt.visit_with(&mut collector);
            }
        }
        self.scopes.push(collector.names);
    }
}

impl VisitMut for Instrumenter<'_> {
    /// Splice a probe in front of every statement of a statement list.
    ///
    /// Children are visited first, so the probe is never itself instrumented and
    /// nested blocks are handled before their parent.
    fn visit_mut_stmts(&mut self, stmts: &mut Vec<Stmt>) {
        let original = std::mem::take(stmts);
        let mut instrumented: Vec<Stmt> = Vec::with_capacity(original.len() * 2);
        for mut stmt in original {
            stmt.visit_mut_with(self);
            if let Some(probe) = self.probe_for(&stmt) {
                instrumented.push(probe);
                self.probes += 1;
            }
            instrumented.push(stmt);
        }
        *stmts = instrumented;
    }

    fn visit_mut_function(&mut self, node: &mut Function) {
        self.push_scope(
            node.params.iter().map(|param| &param.pat),
            node.body.as_ref().map(|body| body.stmts.as_slice()),
        );
        node.visit_mut_children_with(self);
        self.scopes.pop();
    }

    fn visit_mut_arrow_expr(&mut self, node: &mut ArrowExpr) {
        let body = match &*node.body {
            ArrowFunctionBody::FunctionBody(body) => Some(body.stmts.as_slice()),
            ArrowFunctionBody::Expr(_) => None,
        };
        self.push_scope(node.params.iter(), body);
        node.visit_mut_children_with(self);
        self.scopes.pop();
    }
}

/// A statement is probed when it can carry a line breakpoint.
fn should_probe(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Empty(_) => false,
        // A directive prologue must stay the first statement of its function,
        // and a bare string expression is a no-op anyway.
        Stmt::Expr(expr) => !matches!(&*expr.expr, Expr::Lit(Lit::Str(_))),
        Stmt::Debugger(_) => true,
        _ => true,
    }
}

fn is_skipped(name: &str) -> bool {
    if name.starts_with("__hbot") {
        return true;
    }
    if name == "Infinity" {
        return true;
    }
    SKIPPED_NAMES.contains(&name)
}

/// `function () { var __hbot_vars = {}; try { __hbot_vars["a"] = a; } catch
/// (__hbot_e) {} … return __hbot_vars; }`
fn locals_thunk(span: Span, names: &[String]) -> Vec<Stmt> {
    let mut body = vec![var_decl(
        VARS_BINDING,
        Expr::Object(ObjectLit {
            span,
            props: Vec::new(),
        }),
        span,
    )];
    for name in names {
        let assign = Expr::Assign(AssignExpr {
            span,
            op: AssignOp::Assign,
            left: AssignTarget::Simple(SimpleAssignTarget::Member(MemberExpr {
                span,
                obj: Box::new(ident_expr(VARS_BINDING, span)),
                prop: MemberProp::Computed(ComputedPropName {
                    span,
                    expr: Box::new(str_expr(name, span)),
                }),
            })),
            right: Box::new(ident_expr(name, span)),
        });
        body.push(Stmt::Try(Box::new(TryStmt {
            span,
            block: block_stmt(vec![expr_stmt(assign, span)], span),
            handler: Some(CatchClause {
                span,
                param: Some(Pat::Ident(BindingIdent {
                    id: ident(CATCH_BINDING, span),
                    type_ann: None,
                })),
                body: block_stmt(Vec::new(), span),
            }),
            finalizer: None,
        })));
    }
    body.push(Stmt::Return(swc_ecma_ast::ReturnStmt {
        span,
        arg: Some(Box::new(ident_expr(VARS_BINDING, span))),
    }));
    body
}

// --- AST builders -----------------------------------------------------------
//
// Every synthesized node reuses the probed statement's span. The codegen writer
// records a line mapping per emitted node, so reusing the statement's span is
// what keeps the generated line of the probe pointing at *its own* TypeScript
// line instead of at line 1 (`DUMMY_SP` maps to byte 0).

fn ident(sym: &str, span: Span) -> Ident {
    Ident::new_no_ctxt(sym.into(), span)
}

fn ident_expr(sym: &str, span: Span) -> Expr {
    Expr::Ident(ident(sym, span))
}

fn str_expr(value: &str, span: Span) -> Expr {
    let mut literal = Str::from(value);
    literal.span = span;
    Expr::Lit(Lit::Str(literal))
}

fn number_expr(value: f64, span: Span) -> Expr {
    Expr::Lit(Lit::Num(Number {
        span,
        value,
        raw: None,
    }))
}

fn expr_stmt(expr: Expr, span: Span) -> Stmt {
    Stmt::Expr(ExprStmt {
        span,
        expr: Box::new(expr),
    })
}

fn block(stmts: Vec<Stmt>, span: Span) -> Stmt {
    Stmt::Block(block_stmt(stmts, span))
}

/// The `BlockStmt` form, for the places that need a block *inside* a statement
/// (`try`/`catch` bodies) rather than as a statement of its own.
fn block_stmt(stmts: Vec<Stmt>, span: Span) -> BlockStmt {
    BlockStmt {
        span,
        ctxt: SyntaxContext::empty(),
        stmts,
    }
}

fn call(callee: &str, args: Vec<Expr>, span: Span) -> Expr {
    Expr::Call(CallExpr {
        span,
        ctxt: SyntaxContext::empty(),
        callee: Callee::Expr(Box::new(ident_expr(callee, span))),
        args: args
            .into_iter()
            .map(|expr| ExprOrSpread {
                spread: None,
                expr: Box::new(expr),
            })
            .collect(),
        type_args: None,
    })
}

fn function_expr(span: Span, body: Vec<Stmt>) -> Expr {
    Expr::Fn(FnExpr {
        ident: None,
        function: Box::new(Function {
            this_param: None,
            params: Vec::new(),
            decorators: Vec::new(),
            span,
            ctxt: SyntaxContext::empty(),
            body: Some(FunctionBody { span, stmts: body }),
            is_generator: false,
            is_async: false,
            type_params: None,
            return_type: None,
        }),
    })
}

fn var_decl(name: &str, init: Expr, span: Span) -> Stmt {
    Stmt::Decl(swc_ecma_ast::Decl::Var(Box::new(VarDecl {
        span,
        ctxt: SyntaxContext::empty(),
        kind: VarDeclKind::Var,
        declare: false,
        decls: vec![VarDeclarator {
            span,
            name: Pat::Ident(BindingIdent {
                id: ident(name, span),
                type_ann: None,
            }),
            init: Some(Box::new(init)),
            definite: false,
        }],
    })))
}

/// Collects the binding names of one function (parameters plus everything
/// declared in its body) without descending into nested functions or classes —
/// those have their own scopes and are pushed separately as the walk enters
/// them.
#[derive(Default)]
struct NameCollector {
    names: BTreeSet<String>,
}

impl NameCollector {
    fn insert(&mut self, sym: &str) {
        if is_skipped(sym) {
            return;
        }
        self.names.insert(sym.to_owned());
    }
}

impl Visit for NameCollector {
    fn visit_binding_ident(&mut self, node: &BindingIdent) {
        self.insert(node.id.sym.as_ref());
    }

    // Nested functions get their own `NameCollector`, so stop here.
    fn visit_fn_decl(&mut self, node: &FnDecl) {
        self.insert(node.ident.sym.as_ref());
    }

    fn visit_fn_expr(&mut self, _node: &FnExpr) {}

    fn visit_arrow_expr(&mut self, _node: &ArrowExpr) {}

    fn visit_class_decl(&mut self, node: &ClassDecl) {
        self.insert(node.ident.sym.as_ref());
    }

    fn visit_class_expr(&mut self, _node: &ClassExpr) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use swc_common::{FileName, GLOBALS, Globals, Mark, SourceMap, sync::Lrc};
    use swc_ecma_ast::EsVersion;
    use swc_ecma_codegen::{Emitter, text_writer::JsWriter};
    use swc_ecma_parser::{Syntax, TsSyntax, parse_file_as_program};
    use swc_ecma_transforms_base::{fixer::fixer, resolver};

    fn instrument(source: &str) -> (String, usize) {
        GLOBALS.set(&Globals::default(), || {
            let cm: Lrc<SourceMap> = Default::default();
            let fm = cm.new_source_file(
                FileName::Custom("src/main.ts".to_owned()).into(),
                source.to_owned(),
            );
            let mut recovered = Vec::new();
            let program = parse_file_as_program(
                &fm,
                Syntax::Typescript(TsSyntax::default()),
                EsVersion::latest(),
                None,
                &mut recovered,
            )
            .expect("fixture parses");
            assert!(recovered.is_empty(), "fixture has no syntax errors");
            let unresolved = Mark::new();
            let top_level = Mark::new();
            let mut program = program
                .apply(resolver(unresolved, top_level, true))
                .apply(fixer(None));
            let probes = instrument_program(&mut program, &cm, 7);

            let mut buffer = Vec::new();
            {
                let writer = JsWriter::new(cm.clone(), "\n", &mut buffer, None);
                let mut emitter = Emitter {
                    cfg: Default::default(),
                    cm: cm.clone(),
                    comments: None,
                    wr: writer,
                };
                emitter.emit_program(&program).expect("codegen succeeds");
            }
            (String::from_utf8(buffer).expect("utf-8"), probes)
        })
    }

    #[test]
    fn every_statement_of_a_function_body_gets_a_probe() {
        let (js, probes) = instrument(
            r#"
            export function onMessage(ctx: any) {
                const a = 1;
                ctx.reply(String(a));
            }
            "#,
        );
        // Two statements in the body, both probed.
        assert_eq!(probes, 2, "{js}");
        assert_eq!(js.matches("__hbot_trace(7,").count(), 2, "{js}");
        // The probe carries the statement's own line and its file id.
        assert!(js.contains("__hbot_pause(7, 3,"), "{js}");
        assert!(js.contains("__hbot_pause(7, 4,"), "{js}");
    }

    #[test]
    fn the_original_statements_are_not_wrapped_or_moved() {
        let (js, _) = instrument(
            r#"
            export function onMessage(ctx: any) {
                let value = 0;
                if (value === 0) {
                    value = 2;
                }
                return value;
            }
            "#,
        );
        // A probe in front of `let` must not put the declaration in a new block:
        // `value` is read two statements later and would be out of scope.
        let let_line = js
            .lines()
            .find(|line| line.contains("let value = 0"))
            .expect("the declaration survived");
        assert!(let_line.trim_start().starts_with("let value = 0"), "{js}");
        assert!(js.contains("return value;"), "{js}");
    }

    #[test]
    fn directives_and_empty_statements_are_left_alone() {
        let (js, probes) = instrument(
            r#"
            export function onMessage(ctx: any) {
                "use strict";
                ;
                ctx.reply("x");
            }
            "#,
        );
        assert_eq!(probes, 1, "{js}");
        assert_eq!(js.matches("__hbot_trace(").count(), 1, "{js}");
        // The directive is still the first statement of the body.
        let body = js.split('{').nth(1).expect("body");
        assert!(body.trim_start().starts_with("\"use strict\""), "{js}");
    }

    #[test]
    fn the_locals_closure_captures_the_visible_bindings() {
        let (js, _) = instrument(
            r#"
            export function onMessage(ctx: any, extra: number) {
                const count: number = extra + 1;
                try {
                    const other = 2;
                    ctx.reply(String(count + other));
                } catch (error) {
                    ctx.reply("no");
                }
            }
            "#,
        );
        assert!(js.contains("__hbot_vars[\"ctx\"] = ctx"), "{js}");
        assert!(js.contains("__hbot_vars[\"extra\"] = extra"), "{js}");
        assert!(js.contains("__hbot_vars[\"count\"] = count"), "{js}");
        // Catch parameters are bindings too.
        assert!(js.contains("__hbot_vars[\"error\"] = error"), "{js}");
        // Each read is guarded, so an uninitialised binding is skipped instead
        // of throwing out of the handler.
        assert!(js.contains("catch (__hbot_e)"), "{js}");
        // Probe internals never capture themselves.
        assert!(!js.contains("__hbot_vars[\"__hbot_vars\"]"), "{js}");
    }

    #[test]
    fn inner_functions_do_not_leak_into_the_outer_locals() {
        let (js, probes) = instrument(
            r#"
            export function onMessage(ctx: any) {
                const outer = 1;
                const helper = () => {
                    const inner = 2;
                    return inner;
                };
                ctx.reply(String(outer + helper()));
            }
            "#,
        );
        // Three statements in `onMessage`, two in the arrow.
        assert_eq!(probes, 5, "{js}");
        // The closures emitted in `onMessage`'s own body must not capture the
        // arrow's `inner`; only the probes inside the arrow may.
        let inner_decl = js.find("const helper").expect("helper declaration");
        let outer_region = &js[..inner_decl];
        assert!(!outer_region.contains("__hbot_vars[\"inner\"]"), "{js}");
        let arrow_region = &js[inner_decl..];
        assert!(
            arrow_region.contains("__hbot_vars[\"inner\"] = inner"),
            "{js}"
        );
    }

    #[test]
    fn probes_are_attributed_to_the_statement_line() {
        let (js, _) = instrument(
            "export function onMessage(ctx: any) {\nlet a = 1;\nctx.reply(String(a));\n}\n",
        );
        // Line 2 for the declaration, line 3 for the reply — not line 1, which
        // is where a `DUMMY_SP` node would land.
        assert!(js.contains("__hbot_trace(7, 2, 1)"), "{js}");
        assert!(js.contains("__hbot_trace(7, 3, 1)"), "{js}");
    }

    #[test]
    fn module_top_level_is_not_probed() {
        let (js, probes) = instrument(
            r#"
            const moduleLevel = 1;
            export function onMessage(ctx: any) {
                ctx.reply(String(moduleLevel));
            }
            "#,
        );
        assert_eq!(probes, 1, "{js}");
        assert_eq!(js.matches("__hbot_trace(").count(), 1, "{js}");
    }
}
