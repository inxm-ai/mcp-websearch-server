// Rendered from tools-create-mcp-repo/rust-harness at 9a6d487.
// Do not edit directly; update the canonical harness and re-render.

//! Fitness function: every registered MCP tool returns typed, schema'd output.
//!
//! clippy cannot require a return type to *be* `Json<T>` — this scans every
//! `#[tool(...)]`-annotated method under `src/` and rejects one whose return
//! type does not mention `rmcp`'s `Json<_>` wrapper, directly or through
//! `Result<Json<_>, _>`. Zero discovered tools is itself a failure: the
//! check must never pass silently because registration moved.
//!
//! This file is convention-driven and byte-identical across repos: it always
//! scans `src/`, never a package-specific path.

use std::fs;
use std::path::{Path, PathBuf};

use syn::visit::Visit;
use syn::{File, ImplItemFn, ReturnType, Type};

const SRC_DIR: &str = "src";

fn is_tool_attribute(attribute: &syn::Attribute) -> bool {
    attribute.path().is_ident("tool")
}

/// True if `ty` is `Json<_>`, or a generic type (`Result<Json<_>, E>`,
/// `Option<Json<_>>`, ...) with `Json<_>` as one of its type arguments.
fn mentions_json_wrapper(ty: &Type) -> bool {
    let Type::Path(type_path) = ty else {
        return false;
    };
    let Some(last_segment) = type_path.path.segments.last() else {
        return false;
    };
    if last_segment.ident == "Json" {
        return true;
    }
    let syn::PathArguments::AngleBracketed(generic_args) = &last_segment.arguments else {
        return false;
    };
    generic_args.args.iter().any(|argument| match argument {
        syn::GenericArgument::Type(inner) => mentions_json_wrapper(inner),
        _ => false,
    })
}

fn suggest_result_name(tool_name: &str) -> String {
    let mut result = String::new();
    for part in tool_name.split('_') {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            result.extend(first.to_uppercase());
            result.push_str(chars.as_str());
        }
    }
    result.push_str("Result");
    result
}

struct ToolVisitor {
    file: String,
    tool_count: usize,
    violations: Vec<String>,
}

impl<'ast> Visit<'ast> for ToolVisitor {
    fn visit_impl_item_fn(&mut self, node: &'ast ImplItemFn) {
        if node.attrs.iter().any(is_tool_attribute) {
            self.tool_count += 1;
            let name = node.sig.ident.to_string();
            let returns_json = match &node.sig.output {
                ReturnType::Type(_, ty) => mentions_json_wrapper(ty),
                ReturnType::Default => false,
            };
            if !returns_json {
                self.violations.push(format!(
                    "{}: tool `{name}` does not return `Json<_>` — wrap the typed contract \
                     result in `rmcp`'s `Json` (directly, or inside `Result<Json<_>, McpError>`), \
                     e.g. declare a `{}` in the contract module and return `Json({name}_result)`",
                    self.file,
                    suggest_result_name(&name),
                ));
            }
        }
        syn::visit::visit_impl_item_fn(self, node);
    }
}

fn rust_source_files(dir: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "rs"))
        .map(walkdir::DirEntry::into_path)
        .collect()
}

/// Scans `sources` (repo-relative path -> file contents) the same way the
/// enforcing test scans the real repository; kept separate so the fixture
/// tests below never touch the filesystem.
fn check_tool_returns(sources: &[(&str, &str)]) -> Vec<String> {
    let mut violations = Vec::new();
    let mut tool_count = 0;
    for (path, source) in sources {
        let parsed: File = syn::parse_file(source)
            .unwrap_or_else(|error| panic!("{path} is not valid Rust: {error}"));
        let mut visitor = ToolVisitor {
            file: (*path).to_string(),
            tool_count: 0,
            violations: Vec::new(),
        };
        visitor.visit_file(&parsed);
        tool_count += visitor.tool_count;
        violations.extend(visitor.violations);
    }
    if tool_count == 0 {
        violations.push(format!(
            "no MCP tool registrations discovered under {SRC_DIR}/ — the checker scans for \
             `#[tool(...)]`-annotated methods; if registration moved, update SRC_DIR in \
             tests/typed_tool_returns.rs"
        ));
    }
    violations
}

// --- acceptance 1: a bare `Json<T>` return passes ---------------------------

#[test]
fn accepts_direct_json_wrapper() {
    let source = r#"
        impl Greeter {
            #[tool(description = "greet")]
            async fn greet(&self) -> Json<GreetingResult> { unimplemented!() }
        }
    "#;
    assert_eq!(
        check_tool_returns(&[("synthetic.rs", source)]),
        Vec::<String>::new()
    );
}

// --- acceptance 2: `Result<Json<T>, McpError>` passes ------------------------

#[test]
fn accepts_json_wrapper_inside_result() {
    let source = r#"
        impl Greeter {
            #[tool(description = "greet")]
            async fn greet(&self) -> Result<Json<GreetingResult>, McpError> { unimplemented!() }
        }
    "#;
    assert_eq!(
        check_tool_returns(&[("synthetic.rs", source)]),
        Vec::<String>::new()
    );
}

// --- acceptance 3: untyped returns are rejected ------------------------------

#[test]
fn rejects_untyped_returns() {
    for annotation in [
        "String",
        "Result<CallToolResult, McpError>",
        "Result<String, McpError>",
    ] {
        let source = format!(
            r#"
            impl Greeter {{
                #[tool(description = "greet")]
                async fn greet(&self) -> {annotation} {{ unimplemented!() }}
            }}
        "#
        );
        let violations = check_tool_returns(&[("synthetic.rs", &source)]);
        assert_eq!(violations.len(), 1, "`{annotation}` should be rejected");
    }
}

// --- acceptance 4: zero discovered tools fails, never passes silently -------

#[test]
fn zero_discovered_tools_fails() {
    let violations = check_tool_returns(&[(
        "synthetic.rs",
        "impl Greeter { async fn not_a_tool(&self) {} }",
    )]);
    assert_eq!(violations.len(), 1);
    assert!(violations[0].contains("no MCP tool registrations discovered"));
}

// --- the enforcing check over the repo's real source -------------------------

#[test]
fn repo_tools_return_typed_json() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let src_dir = repo_root.join(SRC_DIR);
    let files = rust_source_files(&src_dir);
    let owned: Vec<(String, String)> = files
        .iter()
        .map(|path| {
            let relative = path
                .strip_prefix(repo_root)
                .unwrap_or(path)
                .display()
                .to_string();
            let source = fs::read_to_string(path)
                .unwrap_or_else(|error| panic!("{} could not be read: {error}", path.display()));
            (relative, source)
        })
        .collect();
    let borrowed: Vec<(&str, &str)> = owned
        .iter()
        .map(|(path, source)| (path.as_str(), source.as_str()))
        .collect();
    let violations = check_tool_returns(&borrowed);
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}
