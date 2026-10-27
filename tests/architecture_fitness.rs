// Rendered from tools-create-mcp-repo/rust-harness at 9a6d487.
// Do not edit directly; update the canonical harness and re-render.

//! Portable architecture fitness tests for Rust MCP servers.
//!
//! These checks enforce the generic parts of the fleet architecture contract:
//! pure modules stay free of direct I/O imports, only composition wiring imports
//! `*_adapter` modules, and detached async tasks preserve their tracing span.

use std::path::{Path, PathBuf};

use syn::visit::Visit;

const SRC_DIR: &str = "src";
const WIRING_FILES: &[&str] = &[
    "main.rs",
    "wiring.rs",
    "server.rs",
    "http_api.rs",
    "timer.rs",
];
const IO_MODULES: &[&[&str]] = &[
    &["std", "env"],
    &["std", "fs"],
    &["std", "net"],
    &["std", "process"],
    &["tokio", "fs"],
    &["tokio", "net"],
    &["tokio", "process"],
    &["reqwest"],
];
const SPAWN_FN_NAMES: &[&str] = &["spawn", "spawn_local"];
const RUNTIME_SPAWN_QUALIFIERS: &[&str] = &["tokio", "task", "rt", "actix_rt"];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rust_source_files() -> Vec<PathBuf> {
    walkdir::WalkDir::new(repo_root().join(SRC_DIR))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
        .collect()
}

fn relative(path: &Path) -> String {
    path.strip_prefix(repo_root())
        .unwrap_or(path)
        .display()
        .to_string()
}

fn is_io_boundary(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    WIRING_FILES.contains(&name) || name == "config.rs" || name.ends_with("_adapter.rs")
}

fn use_tree_paths(tree: &syn::UseTree, prefix: &[String]) -> Vec<Vec<String>> {
    match tree {
        syn::UseTree::Path(path) => {
            let mut next_prefix = prefix.to_vec();
            next_prefix.push(path.ident.to_string());
            use_tree_paths(&path.tree, &next_prefix)
        }
        syn::UseTree::Name(name) => {
            let mut path = prefix.to_vec();
            path.push(name.ident.to_string());
            vec![path]
        }
        syn::UseTree::Rename(rename) => {
            let mut path = prefix.to_vec();
            path.push(rename.ident.to_string());
            vec![path]
        }
        syn::UseTree::Group(group) => group
            .items
            .iter()
            .flat_map(|item| use_tree_paths(item, prefix))
            .collect(),
        syn::UseTree::Glob(_) => vec![prefix.to_vec()],
    }
}

fn matches_io_module(segments: &[String]) -> bool {
    IO_MODULES.iter().any(|prefix| {
        segments.len() >= prefix.len()
            && segments
                .iter()
                .zip(prefix.iter())
                .all(|(segment, expected)| segment == expected)
    })
}

fn imports_adapter(segments: &[String]) -> bool {
    segments.iter().any(|segment| segment.ends_with("_adapter"))
}

struct ImportVisitor {
    io_imports: Vec<String>,
    adapter_imports: Vec<String>,
}

impl<'ast> Visit<'ast> for ImportVisitor {
    fn visit_item_use(&mut self, node: &'ast syn::ItemUse) {
        for segments in use_tree_paths(&node.tree, &[]) {
            if matches_io_module(&segments) {
                self.io_imports.push(segments.join("::"));
            }
            if imports_adapter(&segments) {
                self.adapter_imports.push(segments.join("::"));
            }
        }
        syn::visit::visit_item_use(self, node);
    }
}

#[test]
fn recognizes_grouped_io_and_adapter_imports() {
    let parsed = syn::parse_file(
        "use std::{fs, process}; use crate::weather_adapter::{WeatherClient, WeatherConfig};",
    )
    .unwrap_or_else(|error| panic!("fixture is not valid Rust: {error}"));
    let mut visitor = ImportVisitor {
        io_imports: Vec::new(),
        adapter_imports: Vec::new(),
    };
    visitor.visit_file(&parsed);

    assert_eq!(visitor.io_imports, vec!["std::fs", "std::process"]);
    assert_eq!(
        visitor.adapter_imports,
        vec![
            "crate::weather_adapter::WeatherClient",
            "crate::weather_adapter::WeatherConfig",
        ]
    );
}

#[test]
fn direct_io_imports_are_confined_to_boundaries() {
    let mut violations = Vec::new();
    for path in rust_source_files() {
        if is_io_boundary(&path) {
            continue;
        }
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{} could not be read: {error}", path.display()));
        let parsed = syn::parse_file(&source)
            .unwrap_or_else(|error| panic!("{} is not valid Rust: {error}", path.display()));
        let mut visitor = ImportVisitor {
            io_imports: Vec::new(),
            adapter_imports: Vec::new(),
        };
        visitor.visit_file(&parsed);
        for import in visitor.io_imports {
            violations.push(format!("{}: `use {import}::...`", relative(&path)));
        }
    }
    assert!(
        violations.is_empty(),
        "pure-module violation: direct I/O imports are allowed only in main.rs, wiring.rs, \
         config.rs, or a *_adapter.rs module. Move the I/O behind an adapter and pass data \
         through a trait or function parameter:\n{}",
        violations.join("\n")
    );
}

#[test]
fn only_wiring_imports_adapters() {
    let mut violations = Vec::new();
    for path in rust_source_files() {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if WIRING_FILES.contains(&name) {
            continue;
        }
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{} could not be read: {error}", path.display()));
        let parsed = syn::parse_file(&source)
            .unwrap_or_else(|error| panic!("{} is not valid Rust: {error}", path.display()));
        let mut visitor = ImportVisitor {
            io_imports: Vec::new(),
            adapter_imports: Vec::new(),
        };
        visitor.visit_file(&parsed);
        for import in visitor.adapter_imports {
            violations.push(format!("{}: `use {import}::...`", relative(&path)));
        }
    }
    assert!(
        violations.is_empty(),
        "layering violation: only main.rs or wiring.rs may import a *_adapter module. \
         Define a trait in the pure module and inject the adapter from composition wiring:\n{}",
        violations.join("\n")
    );
}

fn is_runtime_spawn(call: &syn::ExprCall) -> bool {
    let syn::Expr::Path(path) = call.func.as_ref() else {
        return false;
    };
    let segments = &path.path.segments;
    let Some(last) = segments.last() else {
        return false;
    };
    if !SPAWN_FN_NAMES.contains(&last.ident.to_string().as_str()) {
        return false;
    }
    segments.len() == 1
        || segments.get(segments.len() - 2).is_some_and(|segment| {
            RUNTIME_SPAWN_QUALIFIERS.contains(&segment.ident.to_string().as_str())
        })
}

struct SpawnVisitor {
    violations: Vec<String>,
}

impl<'ast> Visit<'ast> for SpawnVisitor {
    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        let instrumented = node
            .args
            .first()
            .is_some_and(|argument| matches!(argument, syn::Expr::MethodCall(call) if call.method == "instrument"));
        if is_runtime_spawn(node) && !instrumented {
            self.violations
                .push("detached spawn without `.instrument(span)` on its future".to_string());
        }
        syn::visit::visit_expr_call(self, node);
    }
}

#[test]
fn detached_tasks_propagate_the_tracing_span() {
    let mut violations = Vec::new();
    for path in rust_source_files() {
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{} could not be read: {error}", path.display()));
        let parsed = syn::parse_file(&source)
            .unwrap_or_else(|error| panic!("{} is not valid Rust: {error}", path.display()));
        let mut visitor = SpawnVisitor {
            violations: Vec::new(),
        };
        visitor.visit_file(&parsed);
        violations.extend(
            visitor
                .violations
                .into_iter()
                .map(|violation| format!("{}: {violation}", relative(&path))),
        );
    }
    assert!(
        violations.is_empty(),
        "trace-propagation violation: detached tasks lose the current span and orphan their \
         telemetry. Wrap the complete spawned future with `.instrument(task_span)`:\n{}",
        violations.join("\n")
    );
}
