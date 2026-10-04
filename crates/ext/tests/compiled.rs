//! The compiled tier: `Extension` + `ExtensionSet`.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use ext::{BaseTools, ExtError, Extension, ExtensionSet, SessionSetup};
use kernel::capability::Capability;
use kernel::middleware::{Middleware, MiddlewareSource};
use kernel::tool::{Tool, ToolContext, ToolError, ToolKind, ToolResult};
use serde_json::{Value, json};

struct Named(&'static str);

#[async_trait]
impl Tool for Named {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "test"
    }
    fn schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Stateless
    }
    fn capabilities(&self) -> Vec<Capability> {
        Vec::new()
    }
    async fn invoke(&self, _: &ToolContext<'_>, _: Value) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::Value(Value::Null))
    }
}

struct Noop;
impl Middleware for Noop {}

/// An extension with one tool and one middleware that records its config.
struct Demo {
    name: &'static str,
    tool: &'static str,
}

impl Extension for Demo {
    fn name(&self) -> &str {
        self.name
    }
    fn tools(&self, _: &SessionSetup) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(Named(self.tool))]
    }
    fn middleware_names(&self) -> Vec<String> {
        vec![format!("{}_mw", self.name)]
    }
    fn middleware(&self, name: &str, config: &Value) -> Result<Arc<dyn Middleware>, ExtError> {
        if config.get("bad").is_some() {
            return Err(ExtError::MiddlewareConfig {
                name: name.to_owned(),
                message: "bad".into(),
            });
        }
        Ok(Arc::new(Noop))
    }
}

fn setup(workdir: &Path) -> SessionSetup {
    let host = Arc::new(host::NativeHost::new(
        Arc::new(kernel::Redactor::new()),
        Arc::new(host::MapSecretSource::empty()),
    ));
    SessionSetup {
        workdir: workdir.to_path_buf(),
        sandbox: Arc::new(sandbox::NoneBackend::new(host)),
    }
}

#[test]
fn base_tools_through_the_api_match_the_sandbox_declarations() {
    let set = ExtensionSet::new().with(Arc::new(BaseTools)).unwrap();
    let decls = set.tool_decls(Path::new("/w")).unwrap();
    assert_eq!(decls, sandbox::tool_decls("/w"));
    let tools = set.tools(&setup(Path::new("/w"))).unwrap();
    let names: Vec<_> = tools.iter().map(|t| t.name()).collect();
    assert_eq!(
        names,
        ["read", "write", "edit", "bash", "run_script", "python"]
    );
}

#[test]
fn names_are_unique_and_compiled_tools_avoid_reserved_prefixes() {
    let set = ExtensionSet::new()
        .with(Arc::new(Demo {
            name: "a",
            tool: "x",
        }))
        .unwrap();
    assert!(matches!(
        set.clone().with(Arc::new(Demo { name: "a", tool: "y" })),
        Err(ExtError::DuplicateExtension(n)) if n == "a"
    ));
    let both = set
        .with(Arc::new(Demo {
            name: "b",
            tool: "x",
        }))
        .unwrap();
    assert!(matches!(
        both.tool_decls(Path::new("/w")),
        Err(ExtError::DuplicateTool(n)) if n == "x"
    ));
    for bad in ["ext.mine.x", "mcp.docs.x"] {
        let set = ExtensionSet::new()
            .with(Arc::new(Demo {
                name: "c",
                tool: bad,
            }))
            .unwrap();
        assert!(matches!(
            set.tool_decls(Path::new("/w")),
            Err(ExtError::ReservedPrefix(_))
        ));
    }
    let set = ExtensionSet::new()
        .with(Arc::new(Demo {
            name: "d",
            tool: "Bad",
        }))
        .unwrap();
    assert!(matches!(
        set.tool_decls(Path::new("/w")),
        Err(ExtError::InvalidToolName(_))
    ));
}

#[test]
fn middleware_is_built_by_name_from_its_config() {
    let set = ExtensionSet::new()
        .with(Arc::new(Demo {
            name: "a",
            tool: "x",
        }))
        .unwrap()
        .with(Arc::new(Demo {
            name: "b",
            tool: "y",
        }))
        .unwrap();
    assert_eq!(
        set.middleware_names().into_iter().collect::<Vec<_>>(),
        ["a_mw", "b_mw"]
    );
    let entry = set
        .middleware_entry("b_mw", 300, MiddlewareSource::Agent, &json!({}), None)
        .unwrap();
    assert_eq!((entry.name.as_str(), entry.priority), ("b_mw", 300));
    assert_eq!(entry.source, MiddlewareSource::Agent);
    assert!(matches!(
        set.middleware_entry("c_mw", 300, MiddlewareSource::Agent, &json!({}), None),
        Err(ExtError::UnknownMiddleware(n)) if n == "c_mw"
    ));
    assert!(matches!(
        set.middleware_entry(
            "a_mw",
            300,
            MiddlewareSource::Agent,
            &json!({"bad": 1}),
            None
        ),
        Err(ExtError::MiddlewareConfig { .. })
    ));
}

#[test]
fn two_extensions_cannot_provide_the_same_middleware() {
    struct Clash;
    impl Extension for Clash {
        fn name(&self) -> &str {
            "clash"
        }
        fn middleware_names(&self) -> Vec<String> {
            vec!["a_mw".into()]
        }
    }
    let set = ExtensionSet::new()
        .with(Arc::new(Demo {
            name: "a",
            tool: "x",
        }))
        .unwrap();
    assert!(matches!(
        set.with(Arc::new(Clash)),
        Err(ExtError::DuplicateMiddleware(n)) if n == "a_mw"
    ));
}
