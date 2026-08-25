//! Synthetic source for the builtins in scope.
//!
//! Goto-definition on a builtin needs somewhere to jump to. The language server
//! answers with a `starlark:` URI whose contents are generated here from the
//! builtin's own documentation, so the stub a reader lands in is always the
//! signature and docstring the runtime actually has.
//!
//! Stubs are read-only and never evaluated: they exist to be parsed, so that a
//! top-level binding with the right name can be located.

use starlark::docs::{
    DocFunction, DocItem, DocMember, DocModule, DocParam, DocParams, DocProperty, DocReturn,
    DocString, DocType,
};
use starlark::syntax::{AstModule, Dialect, DialectTypes};
use starlark::typing::Ty;

/// The dialect stubs are parsed with.
///
/// Not [`Dialect::Standard`], which kiln uses for real modules, because a
/// generated signature uses syntax kiln's own dialect forbids: `/` and `*`
/// parameter separators whenever a builtin mixes positional-only and named-only
/// parameters (`str`, `schema`, `event`, `entity`, `put`, `scan` and more all
/// do), and type annotations wherever a type is known. Parsing a stub under
/// Standard fails, and a stub that does not parse has no span for
/// goto-definition to land on, so it fails silently.
///
/// Types are parse-only: a stub is never evaluated, so there is nothing to check
/// them against and no `typing` module in scope to check them with.
pub(crate) fn stub_dialect() -> Dialect {
    Dialect {
        enable_positional_only_arguments: true,
        enable_keyword_only_arguments: true,
        enable_types: DialectTypes::ParseOnly,
        ..Dialect::Standard
    }
}

/// Render one builtin as Starlark source containing a top-level binding named
/// `name`, which is what goto-definition looks for.
///
/// A rendered type is not always a legal type expression: starlark-rust renders
/// `typing.Iterable[bytes]` for some of its own builtins but will not parse an
/// index on a dotted name in type position. Rather than let those stubs fail to
/// parse, and so silently lose goto-definition, they are re-rendered without
/// their types. The types are still on the hover, which does not go through here.
pub(crate) fn render_stub(name: &str, item: &DocItem) -> String {
    let typed = render_item(name, item);
    if AstModule::parse(name, typed.clone(), &stub_dialect()).is_ok() {
        return typed;
    }
    render_item(name, &erase_types(item))
}

fn render_item(name: &str, item: &DocItem) -> String {
    match item {
        // `DocModule::render_as_code` drops the module's own name and, with it,
        // any way to jump to the namespace: it renders the members as bare
        // top-level definitions. Render the namespace shape by hand instead.
        DocItem::Module(module) => render_namespace(name, module),
        // A property renders as `_name = None`, and a type with no members
        // renders as nothing at all. Neither leaves a `name` to jump to.
        DocItem::Member(DocMember::Property(property)) => {
            render_value(name, property.docs.as_ref())
        }
        DocItem::Type(ty) if ty.members.is_empty() => render_value(name, ty.docs.as_ref()),
        other => other.render_as_code(name),
    }
}

/// A global that is a value rather than a function, as a documented binding.
///
/// The value is always `None`: a stub is never evaluated, and what matters is
/// that the name is bound at top level where goto-definition can find it.
fn render_value(name: &str, docs: Option<&DocString>) -> String {
    let mut out = String::new();
    if let Some(docs) = docs {
        let mut text = docs.summary.clone();
        if let Some(details) = &docs.details {
            text.push_str("\n\n");
            text.push_str(details);
        }
        // The docstring is arbitrary prose, and a `"""` inside one would end the
        // literal early and leave the stub unparseable.
        out.push_str("\"\"\"\n");
        out.push_str(&text.replace("\"\"\"", "'''"));
        out.push_str("\n\"\"\"\n");
    }
    out.push_str(name);
    out.push_str(" = None\n");
    out
}

/// A namespace (kiln has one, `http`) as a `struct(...)` binding over private
/// member functions.
///
/// The literal callee name `struct` is load-bearing: goto-definition on a dotted
/// expression walks exactly this shape, matching the callee by name, so `http.get`
/// lands on the `def` rather than merely on the file. `struct` is not one of
/// kiln's globals, but a stub is only ever parsed, never evaluated.
fn render_namespace(name: &str, module: &DocModule) -> String {
    let mut out = String::new();
    for (member, item) in &module.members {
        out.push_str(&render_item(&format!("_{member}"), item));
        out.push_str("\n\n");
    }
    out.push_str(name);
    out.push_str(" = struct(\n");
    for member in module.members.keys() {
        out.push_str(&format!("    {member} = _{member},\n"));
    }
    out.push_str(")\n");
    out
}

/// The same documentation with every type replaced by `Any`, which
/// `render_as_code` omits entirely, leaving a signature with no annotations.
fn erase_types(item: &DocItem) -> DocItem {
    match item {
        DocItem::Module(module) => DocItem::Module(erase_module(module)),
        DocItem::Type(ty) => DocItem::Type(DocType {
            docs: ty.docs.clone(),
            members: ty
                .members
                .iter()
                .map(|(name, member)| (name.clone(), erase_member(member)))
                .collect(),
            ty: Ty::any(),
            constructor: ty.constructor.as_ref().map(erase_function),
        }),
        DocItem::Member(member) => DocItem::Member(erase_member(member)),
    }
}

fn erase_module(module: &DocModule) -> DocModule {
    DocModule {
        docs: module.docs.clone(),
        members: module
            .members
            .iter()
            .map(|(name, item)| (name.clone(), erase_types(item)))
            .collect(),
    }
}

fn erase_member(member: &DocMember) -> DocMember {
    match member {
        DocMember::Property(property) => DocMember::Property(DocProperty {
            docs: property.docs.clone(),
            typ: Ty::any(),
        }),
        DocMember::Function(function) => DocMember::Function(erase_function(function)),
    }
}

fn erase_function(function: &DocFunction) -> DocFunction {
    let erase = |param: &DocParam| DocParam {
        name: param.name.clone(),
        docs: param.docs.clone(),
        typ: Ty::any(),
        default_value: param.default_value.clone(),
    };
    DocFunction {
        docs: function.docs.clone(),
        params: DocParams {
            pos_only: function.params.pos_only.iter().map(erase).collect(),
            pos_or_named: function.params.pos_or_named.iter().map(erase).collect(),
            args: function.params.args.as_ref().map(erase),
            named_only: function.params.named_only.iter().map(erase).collect(),
            kwargs: function.params.kwargs.as_ref().map(erase),
        },
        ret: DocReturn {
            docs: function.ret.docs.clone(),
            typ: Ty::any(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::starlark_builtins;

    /// The rendered namespace has to keep the exact shape goto-definition walks:
    /// a top-level `name = struct(member = _member, ...)` over top-level defs.
    #[test]
    fn a_namespace_renders_as_a_struct_over_its_members() {
        let docs = starlark_builtins::effect_globals().documentation();
        let http = docs.members.get("http").expect("the http namespace");
        let rendered = render_stub("http", http);

        assert!(rendered.contains("def _get("), "{rendered}");
        assert!(rendered.contains("def _post("), "{rendered}");
        assert!(rendered.contains("http = struct("), "{rendered}");
        assert!(rendered.contains("    get = _get,"), "{rendered}");

        AstModule::parse("http.star", rendered.clone(), &stub_dialect())
            .unwrap_or_else(|err| panic!("stub does not parse: {err}\n---\n{rendered}"));
    }

    /// The reason `stub_dialect` exists: `str` mixes positional-only and
    /// named-only parameters, so its signature needs both separators.
    #[test]
    fn a_stub_with_parameter_separators_needs_the_stub_dialect() {
        let docs = starlark_builtins::globals().documentation();
        let item = docs.members.get("str").expect("str");
        let rendered = render_stub("str", item);

        assert!(
            rendered.contains('/') && rendered.contains('*'),
            "{rendered}"
        );
        assert!(
            AstModule::parse("str.star", rendered.clone(), &Dialect::Standard).is_err(),
            "expected the standard dialect to reject this stub: {rendered}"
        );
        AstModule::parse("str.star", rendered.clone(), &stub_dialect())
            .unwrap_or_else(|err| panic!("stub does not parse: {err}\n---\n{rendered}"));
    }

    /// `bytes` is the case the fallback exists for: starlark-rust renders its
    /// `elems` return type as `typing.Iterable[bytes]`, which its own parser
    /// rejects in type position.
    #[test]
    fn a_stub_whose_types_do_not_parse_is_rendered_without_them() {
        let docs = starlark_builtins::globals().documentation();
        let item = docs.members.get("bytes").expect("bytes");

        let typed = render_item("bytes", item);
        assert!(
            AstModule::parse("bytes.star", typed, &stub_dialect()).is_err(),
            "expected the typed rendering to be unparseable; the fallback may be dead code now"
        );

        let rendered = render_stub("bytes", item);
        assert!(!rendered.contains("typing.Iterable["), "{rendered}");
        AstModule::parse("bytes.star", rendered.clone(), &stub_dialect())
            .unwrap_or_else(|err| panic!("fallback does not parse: {err}\n---\n{rendered}"));
    }
}
