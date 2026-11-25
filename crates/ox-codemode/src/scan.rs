//! Static scanner for Code Mode programs.
//!
//! Scanning is the authorization gate: a program is executed only if every
//! reference to the host `tools` object is a static dot path or a path built
//! from string-literal computed members. The runtime gets exactly those paths
//! and nothing else.

use std::collections::{BTreeSet, HashSet};

use anyhow::{Result, bail, ensure};
use oxc_allocator::Allocator;
use oxc_ast::ast::{Expression, IdentifierReference};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_semantic::{ReferenceId, Semantic, SemanticBuilder};

/// A permission the host must consider before running the program.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum PermissionAction {
    /// Call the named tool path (`namespace.name`).
    Tool { path: String },
}

/// What the scanner derived from a program.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    /// Fully qualified tool paths reachable from the program.
    pub tools: BTreeSet<String>,
    /// Permissions implied by those paths, in stable order.
    pub actions: Vec<PermissionAction>,
}

/// Parse and authorize `source`, failing closed on any construct that could
/// hide a tool path from static analysis.
pub fn scan(source: &str) -> Result<Scan> {
    let wrapped = wrap(source);
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, &wrapped, Default::default()).parse();
    ensure!(!parsed.fatal_error, "code has a fatal parse error");
    ensure!(
        parsed.diagnostics.is_empty(),
        "code has syntax errors: {:?}",
        parsed.diagnostics.first()
    );
    let built = SemanticBuilder::new()
        .with_check_syntax_error(true)
        .build(&parsed.program);
    ensure!(
        built.diagnostics.is_empty(),
        "code is not valid JavaScript: {:?}",
        built.diagnostics.first()
    );
    let semantic = built.semantic;

    let mut collector = ToolCollector {
        semantic: &semantic,
        tools: BTreeSet::new(),
        consumed: HashSet::new(),
    };
    walk::walk_program(&mut collector, &parsed.program);

    let mut verifier = Verifier {
        semantic: &semantic,
        consumed: &collector.consumed,
        denied: None,
    };
    walk::walk_program(&mut verifier, &parsed.program);
    if let Some(denied) = verifier.denied {
        bail!("{denied}");
    }

    let actions = collector
        .tools
        .iter()
        .map(|path| PermissionAction::Tool { path: path.clone() })
        .collect();
    Ok(Scan {
        tools: collector.tools,
        actions,
    })
}

/// Both scanner and runtime analyze the same function body, so a top-level
/// `return` and `await` mean the same thing in both places. The wrapper is
/// async: the runtime drives its promise and the QuickJS job queue together.
pub(crate) fn wrap(source: &str) -> String {
    format!("(async function () {{\n\"use strict\";\n{source}\n}})")
}

/// A tool call is only a direct `tools.ns.name(...)` or
/// `tools["ns"]["name"](...)` chain, mixing dot and string-literal members.
/// Every root `tools` reference must be consumed by exactly one such call;
/// this rules out aliases, dynamic computed access, passing the object around
/// and method extraction.
fn static_tool_path<'a>(
    expr: &'a Expression<'a>,
    semantic: &Semantic<'a>,
) -> Option<(String, ReferenceId)> {
    let mut segments = Vec::new();
    let mut current = expr;
    loop {
        match current {
            Expression::StaticMemberExpression(member) if !member.optional => {
                segments.push(member.property.name.as_str().to_owned());
                current = &member.object;
            }
            Expression::ComputedMemberExpression(member) if !member.optional => {
                let Expression::StringLiteral(key) = &member.expression else {
                    return None;
                };
                let key = key.value.as_str()?;
                if key.is_empty() {
                    return None;
                }
                segments.push(key.to_owned());
                current = &member.object;
            }
            Expression::Identifier(ident) => {
                let reference_id = ident.reference_id.get()?;
                if ident.name != "tools"
                    || semantic
                        .scoping()
                        .get_reference(reference_id)
                        .symbol_id()
                        .is_some()
                {
                    return None;
                }
                segments.reverse();
                if segments
                    .iter()
                    .any(|s| matches!(s.as_str(), "call" | "apply" | "bind"))
                {
                    return None;
                }
                return Some((segments.join("."), reference_id));
            }
            _ => return None,
        }
    }
}

fn is_unresolved(ident: &IdentifierReference<'_>, semantic: &Semantic<'_>) -> bool {
    ident
        .reference_id
        .get()
        .is_some_and(|id| semantic.scoping().get_reference(id).symbol_id().is_none())
}

struct ToolCollector<'a> {
    semantic: &'a Semantic<'a>,
    tools: BTreeSet<String>,
    consumed: HashSet<ReferenceId>,
}

impl<'a> Visit<'a> for ToolCollector<'a> {
    fn visit_call_expression(&mut self, call: &oxc_ast::ast::CallExpression<'a>) {
        if !call.optional
            && let Some((path, reference_id)) = static_tool_path(&call.callee, self.semantic)
        {
            self.tools.insert(path);
            self.consumed.insert(reference_id);
        }
        walk::walk_call_expression(self, call);
    }
}

struct Verifier<'a> {
    semantic: &'a Semantic<'a>,
    consumed: &'a HashSet<ReferenceId>,
    denied: Option<String>,
}

impl<'a> Visit<'a> for Verifier<'a> {
    fn visit_identifier_reference(&mut self, ident: &IdentifierReference<'a>) {
        if self.denied.is_some() || !is_unresolved(ident, self.semantic) {
            return;
        }
        const AMBIENT: [&str; 7] = [
            "fetch",
            "Bun",
            "require",
            "process",
            "eval",
            "Function",
            "globalThis",
        ];
        if AMBIENT.contains(&ident.name.as_str()) {
            self.denied = Some(format!("ambient global `{}` is not allowed", ident.name));
            return;
        }
        if ident.name == "tools" {
            let unconsumed = ident
                .reference_id
                .get()
                .is_some_and(|id| !self.consumed.contains(&id));
            if unconsumed {
                self.denied = Some(
                    "`tools` must be used as a direct static call like tools.ns.name(...)"
                        .to_owned(),
                );
            }
        }
    }

    fn visit_import_declaration(&mut self, _: &oxc_ast::ast::ImportDeclaration<'a>) {
        self.denied = Some("import statements are not allowed".to_owned());
    }

    fn visit_import_expression(&mut self, _: &oxc_ast::ast::ImportExpression<'a>) {
        self.denied = Some("dynamic import is not allowed".to_owned());
    }

    fn visit_export_named_declaration(&mut self, _: &oxc_ast::ast::ExportNamedDeclaration<'a>) {
        self.denied = Some("exports are not allowed".to_owned());
    }

    fn visit_export_default_declaration(&mut self, _: &oxc_ast::ast::ExportDefaultDeclaration<'a>) {
        self.denied = Some("exports are not allowed".to_owned());
    }

    fn visit_export_all_declaration(&mut self, _: &oxc_ast::ast::ExportAllDeclaration<'a>) {
        self.denied = Some("exports are not allowed".to_owned());
    }
}
