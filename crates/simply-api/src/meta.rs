//! The generated method registry: enough metadata to lint an api surface
//! (everything documented, every field described) and to generate docs
//! from the trait instead of writing them by hand.

/// One method parameter, in declaration order.
#[derive(Clone, Copy)]
pub struct ParamMeta {
    pub name: &'static str,
    /// `Option<...>` in the trait — may be omitted on the wire.
    pub optional: bool,
    /// The parameter's whole doc comment (its first paragraph is the
    /// schema's `description`).
    pub doc: &'static str,
}

/// One trait method, as `#[api_service]` recorded it.
#[derive(Clone, Copy)]
pub struct MethodMeta {
    /// Wire name: the MCP tool + dispatch name (`#[api(name = "...")]`
    /// override, else the method ident).
    pub name: &'static str,
    /// The trait method ident.
    pub rust_name: &'static str,
    /// The doc comment's first paragraph — the MCP tool description.
    pub description: &'static str,
    /// The whole doc comment — the long form, for `help` and the guide.
    pub help: &'static str,
    /// `#[api(tier = "...")]`: which slice of the tool surface advertises
    /// this method. Empty only for `#[api(no_tool)]` methods.
    pub tier: &'static str,
    /// False for `#[api(no_tool)]` methods (still dispatchable/callable).
    pub has_tool: bool,
    /// The parameters in declaration order (schema property maps are
    /// alphabetical; this keeps docs in the trait's order).
    pub params: &'static [ParamMeta],
    /// JSON schema of the generated params struct (draft 2020-12, the same
    /// dialect rmcp serves).
    pub params_schema: fn() -> serde_json::Value,
    /// JSON schema of the `ApiResult<T>` inner type.
    pub output_schema: fn() -> serde_json::Value,
    /// `T` in `ApiResult<T>`, as written in the trait.
    pub output_type: &'static str,
    /// True when the tool result is plain text (`String` / `()`), not
    /// structured JSON.
    pub returns_text: bool,
    /// True for `#[api(media)]` methods: the tool result is rich content
    /// blocks (text summary + image/audio) rendered by the return type's
    /// `simply_api::mcp::MediaContent` impl, not structured JSON.
    pub returns_media: bool,
}

impl MethodMeta {
    /// What the tool returns, in a phrase — the guide's and `help`'s
    /// "Returns …" line.
    pub fn returns(&self) -> String {
        if self.returns_media {
            format!(
                "rich MCP content: a text summary plus the payload itself as an \
                 image/audio content block (`{}` on the typed path)",
                self.output_type
            )
        } else if self.returns_text {
            "confirmation text".to_owned()
        } else if self.output_type == "serde_json::Value" {
            "the patch document (JSON)".to_owned()
        } else {
            format!("`{}` (structured JSON)", self.output_type)
        }
    }

    /// The tool's whole reference as markdown — heading, the long-form
    /// doc, every parameter with its doc, what it returns. One renderer,
    /// so the generated guide and the `help` tool cannot disagree.
    pub fn reference(&self) -> String {
        let mut out = format!("### `{}`\n\n{}\n", self.name, self.help);
        if !self.params.is_empty() {
            out.push('\n');
        }
        for param in self.params {
            let optional = if param.optional { " *(optional)*" } else { "" };
            out.push_str(&format!("- `{}`{optional} — {}\n", param.name, param.doc));
        }
        out.push_str(&format!("\nReturns {}.\n", self.returns()));
        out
    }
}

/// The whole service: what `{TRAIT}_META` points at.
pub struct ServiceMeta {
    pub service: &'static str,
    pub methods: &'static [MethodMeta],
}

impl ServiceMeta {
    pub fn method(&self, name: &str) -> Option<&MethodMeta> {
        self.methods.iter().find(|m| m.name == name)
    }

    /// How many methods surface as MCP tools.
    pub fn tool_count(&self) -> usize {
        self.methods.iter().filter(|m| m.has_tool).count()
    }

    /// The tool methods, in declaration order.
    pub fn tools(&self) -> impl Iterator<Item = &MethodMeta> {
        self.methods.iter().filter(|m| m.has_tool)
    }

    /// The tool methods of one tier, in declaration order.
    pub fn tier(&self, tier: &str) -> impl Iterator<Item = &MethodMeta> + '_ {
        let tier = tier.to_owned();
        self.tools().filter(move |m| m.tier == tier)
    }

    /// The `help` tool's answer: one tool's [`MethodMeta::reference`], or —
    /// with no name — the index: every tool by tier with its one-line
    /// description. An unknown name is an error naming what there is.
    pub fn help(&self, tool: Option<&str>, tiers: &[&str]) -> Result<String, String> {
        match tool {
            Some(name) => {
                self.method(name).filter(|m| m.has_tool).map(MethodMeta::reference).ok_or_else(
                    || {
                        let known: Vec<&str> = self.tools().map(|m| m.name).collect();
                        format!("no tool named {name:?}; tools: {}", known.join(", "))
                    },
                )
            }
            None => {
                let mut out = String::new();
                for tier in tiers {
                    out.push_str(&format!("## {tier}\n\n"));
                    for m in self.tier(tier) {
                        out.push_str(&format!("- `{}` — {}\n", m.name, m.description));
                    }
                    out.push('\n');
                }
                Ok(out)
            }
        }
    }

    /// Registry lint: every method documented, every params field carrying
    /// a schemars description. Returns human-readable violations (empty ==
    /// clean) so a test can `assert!(lint().is_empty(), ...)`.
    pub fn lint(&self) -> Vec<String> {
        let mut issues = Vec::new();
        for method in self.methods {
            if method.description.trim().is_empty() {
                issues.push(format!(
                    "{}::{} has no doc comment (it becomes the tool description)",
                    self.service, method.rust_name
                ));
            }
            let schema = (method.params_schema)();
            let properties = schema.get("properties").and_then(|p| p.as_object());
            for (field, field_schema) in properties.into_iter().flatten() {
                let described = field_schema
                    .get("description")
                    .and_then(|d| d.as_str())
                    .is_some_and(|d| !d.trim().is_empty());
                if !described {
                    issues.push(format!(
                        "{}::{} param `{field}` has no description (doc-comment the parameter)",
                        self.service, method.rust_name
                    ));
                }
            }
        }
        issues
    }
}

/// JSON schema for `T` in the dialect rmcp serves (draft 2020-12) — the
/// registry's single schema source, so lints and docs agree with the wire.
pub fn schema_of<T: schemars::JsonSchema>() -> serde_json::Value {
    let generator = schemars::generate::SchemaSettings::draft2020_12().into_generator();
    serde_json::to_value(generator.into_root_schema_for::<T>()).expect("schema serializes")
}
