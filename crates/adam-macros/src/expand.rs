//! The expansion of `#[tool]`, as a pure function over token streams.
//!
//! [`expand`] never panics and never touches the compiler: a mistake in the
//! input comes back as `compile_error!` tokens, with the message and the span
//! the user should see. Everything the macro decides is therefore testable
//! here with `proc_macro2` alone.
//!
//! What it generates is the contract of `docs/authoring.md`: the function is
//! kept (so it can be unit-tested directly), a unit struct implements `Tool`,
//! and the arguments become one struct that derives `Deserialize` and
//! `JsonSchema` through the `__private` paths of the crate named by
//! `#[tool(crate = ...)]` (default `::adam`).

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote, quote_spanned};
use syn::ext::IdentExt as _;
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned as _;
use syn::{
    Attribute, Error, Expr, FnArg, GenericArgument, Ident, Item, ItemFn, Lit, LitStr, Meta, Pat,
    Path, PathArguments, Result, ReturnType, Safety, Token, Type,
};

/// Expand `#[tool(attr)] item`.
pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    try_expand(attr, item).unwrap_or_else(|e| e.to_compile_error())
}

fn try_expand(attr: TokenStream, item: TokenStream) -> Result<TokenStream> {
    let opts: Options = syn::parse2(attr)?;
    let func = parse_fn(item)?;
    let tool = analyse(&func, opts)?;
    Ok(tool.generate(&func))
}

// ---------------------------------------------------------------- options

/// What is written inside `#[tool(...)]`.
#[derive(Default)]
struct Options {
    name: Option<LitStr>,
    type_name: Option<Ident>,
    strict: Option<Span>,
    classify: bool,
    krate: Option<Path>,
}

const OPTIONS: &str = "name, type, strict, classify, crate";

impl Parse for Options {
    fn parse(input: ParseStream) -> Result<Self> {
        let mut opts = Options::default();
        while !input.is_empty() {
            // `type` and `crate` are keywords, so a plain `Ident` would refuse them.
            let key = Ident::parse_any(input)?;
            let seen = match key.to_string().as_str() {
                "name" => {
                    input.parse::<Token![=]>()?;
                    opts.name.replace(input.parse()?).is_some()
                }
                "type" => {
                    input.parse::<Token![=]>()?;
                    opts.type_name.replace(input.parse()?).is_some()
                }
                "crate" => {
                    input.parse::<Token![=]>()?;
                    opts.krate.replace(Path::parse_mod_style(input)?).is_some()
                }
                "strict" => opts.strict.replace(key.span()).is_some(),
                "classify" => std::mem::replace(&mut opts.classify, true),
                other => {
                    return Err(Error::new(
                        key.span(),
                        format!("unknown `#[tool]` option `{other}`; expected one of: {OPTIONS}"),
                    ));
                }
            };
            if seen {
                return Err(Error::new(
                    key.span(),
                    format!("`{key}` is given twice in `#[tool]`"),
                ));
            }
            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(opts)
    }
}

// ------------------------------------------------------------------ input

fn parse_fn(item: TokenStream) -> Result<ItemFn> {
    const GOES_ON: &str = "`#[tool]` goes on an `async fn`";
    let Some(first) = item.clone().into_iter().next() else {
        return Err(Error::new(Span::call_site(), GOES_ON));
    };
    match syn::parse2::<Item>(item)? {
        Item::Fn(func) => Ok(func),
        _ => Err(Error::new(first.span(), GOES_ON)),
    }
}

/// Collects every mistake so that one compile shows them all.
#[derive(Default)]
struct Errors(Option<Error>);

impl Errors {
    fn push(&mut self, span: Span, message: impl std::fmt::Display) {
        let e = Error::new(span, message);
        match &mut self.0 {
            Some(all) => all.combine(e),
            None => self.0 = Some(e),
        }
    }

    fn finish(self) -> Result<()> {
        self.0.map_or(Ok(()), Err)
    }
}

/// How one parameter of the function is filled in.
enum Param {
    /// `&ToolCtx`: the call's context.
    Ctx,
    /// `State<T>`: shared state, resolved from the context.
    State(Type),
    /// An argument the model fills in: a field of the generated struct.
    Model {
        ident: Ident,
        ty: Type,
        /// `#[serde(..)]` and `#[schemars(..)]`, copied to the field.
        field_attrs: Vec<Attribute>,
        description: String,
    },
    /// `#[args] a: MyArgs`: an existing struct is the whole argument object.
    Args(Type),
}

/// What the macro knows about the function once it has been checked.
struct Analysis {
    krate: Path,
    fn_ident: Ident,
    tool_name: String,
    tool_ident: Ident,
    args_ident: Ident,
    description: String,
    params: Vec<Param>,
    strict: bool,
    classify: bool,
    return_span: Span,
}

fn analyse(func: &ItemFn, opts: Options) -> Result<Analysis> {
    let mut errors = Errors::default();
    let sig = &func.sig;

    if sig.asyncness.is_none() {
        errors.push(sig.fn_token.span, "`#[tool]` functions must be `async`");
    }
    if !sig.generics.params.is_empty() || sig.generics.where_clause.is_some() {
        let span = if sig.generics.params.is_empty() {
            sig.ident.span()
        } else {
            sig.generics.span()
        };
        errors.push(
            span,
            "`#[tool]` functions cannot be generic; model arguments are deserialized into concrete types",
        );
    }
    if sig.constness.is_some() || sig.abi.is_some() || matches!(sig.safety, Safety::Unsafe(_)) {
        errors.push(
            sig.fn_token.span,
            "`#[tool]` functions cannot be `const`, `unsafe` or `extern`",
        );
    }

    let description = normalize_doc(&doc_lines(&func.attrs));
    if description.is_empty() {
        errors.push(
            sig.ident.span(),
            "`#[tool]` needs a doc comment: it is the description the model reads",
        );
    }

    let fn_name = sig.ident.unraw().to_string();
    let (tool_name, name_span) = match &opts.name {
        Some(lit) => (lit.value(), lit.span()),
        None => (fn_name.clone(), sig.ident.span()),
    };
    if !valid_tool_name(&tool_name) {
        let hint = if opts.name.is_some() {
            ""
        } else {
            "; name the tool with `#[tool(name = \"...\")]`"
        };
        errors.push(
            name_span,
            format!("tool names must match `^[a-z][a-z0-9_]{{0,63}}$`{hint}"),
        );
    }

    let tool_ident = match &opts.type_name {
        Some(ident) => ident.clone(),
        None => {
            let camel = upper_camel(&fn_name);
            if camel.is_empty() || camel == fn_name {
                errors.push(
                    sig.ident.span(),
                    "cannot derive the tool's type name from this function name; set one with `#[tool(type = Name)]`",
                );
            }
            Ident::new(
                if camel.is_empty() { "Tool" } else { &camel },
                sig.ident.span(),
            )
        }
    };
    let args_ident = format_ident!("__{}Args", tool_ident.unraw());

    let params = analyse_params(func, &mut errors);
    let models = params
        .iter()
        .filter(|p| matches!(p, Param::Model { .. } | Param::Args(_)))
        .count();
    let has_args = params.iter().any(|p| matches!(p, Param::Args(_)));
    if has_args && models > 1 {
        errors.push(
            sig.ident.span(),
            "`#[args]` must be the only model argument",
        );
    }
    if let (true, Some(span)) = (has_args, opts.strict) {
        errors.push(
            span,
            "`strict` has no effect with `#[args]`; put `#[serde(deny_unknown_fields)]` on the struct",
        );
    }
    errors.finish()?;

    let return_span = match &sig.output {
        ReturnType::Type(_, ty) => ty.span(),
        ReturnType::Default => sig.ident.span(),
    };
    Ok(Analysis {
        krate: opts.krate.unwrap_or_else(|| syn::parse_quote!(::adam)),
        fn_ident: sig.ident.clone(),
        tool_name,
        tool_ident,
        args_ident,
        description,
        params,
        strict: opts.strict.is_some(),
        classify: opts.classify,
        return_span,
    })
}

fn analyse_params(func: &ItemFn, errors: &mut Errors) -> Vec<Param> {
    let mut params = Vec::new();
    let mut ctx_seen = false;
    for arg in &func.sig.inputs {
        let FnArg::Typed(param) = arg else {
            errors.push(
                arg.span(),
                "`#[tool]` works on free functions; put shared state in `State<T>`",
            );
            continue;
        };
        let attrs = ParamAttrs::read(&param.attrs, errors);
        let kind = match type_kind(&param.ty) {
            Ok(kind) => kind,
            Err((span, message)) => {
                errors.push(span, message);
                continue;
            }
        };
        match kind {
            TypeKind::Ctx => {
                if ctx_seen {
                    errors.push(param.ty.span(), "at most one `&ToolCtx` parameter");
                }
                ctx_seen = true;
                attrs.reject_field_attrs("a `&ToolCtx` parameter", errors);
                params.push(Param::Ctx);
            }
            TypeKind::State(inner) => {
                attrs.reject_field_attrs("a `State<T>` parameter", errors);
                params.push(Param::State(*inner));
            }
            TypeKind::Model if attrs.args.is_some() => {
                if !attrs.field_attrs.is_empty() {
                    errors.push(
                        attrs.field_attrs[0].span(),
                        "put `#[serde(..)]` and `#[schemars(..)]` on the `#[args]` struct, not on the parameter",
                    );
                }
                params.push(Param::Args((*param.ty).clone()));
            }
            TypeKind::Model => {
                let ident = match &*param.pat {
                    Pat::Ident(p) if p.by_ref.is_none() && p.subpat.is_none() => p.ident.clone(),
                    other => {
                        errors.push(
                            other.span(),
                            "`#[tool]` parameters must be plain names (`name: Type`): the name is the field the model fills in",
                        );
                        continue;
                    }
                };
                params.push(Param::Model {
                    ident,
                    ty: (*param.ty).clone(),
                    field_attrs: attrs.field_attrs,
                    description: normalize_doc(&attrs.doc),
                });
            }
        }
    }
    params
}

/// The attributes on one parameter, sorted by what the macro does with them.
struct ParamAttrs {
    doc: Vec<String>,
    /// `#[serde(..)]` and `#[schemars(..)]`.
    field_attrs: Vec<Attribute>,
    /// `#[args]`.
    args: Option<Span>,
}

impl ParamAttrs {
    fn read(attrs: &[Attribute], errors: &mut Errors) -> Self {
        let mut out = Self {
            doc: doc_lines(attrs),
            field_attrs: Vec::new(),
            args: None,
        };
        for attr in attrs {
            let path = attr.path();
            if path.is_ident("doc") || is_lint(attr) {
                // Docs are read above; lint attributes stay on the kept function.
            } else if path.is_ident("serde") || path.is_ident("schemars") {
                out.field_attrs.push(attr.clone());
            } else if path.is_ident("args") && matches!(attr.meta, Meta::Path(_)) {
                out.args = Some(attr.span());
            } else {
                errors.push(
                    attr.span(),
                    "unsupported attribute on a `#[tool]` parameter; expected a doc comment, `#[args]`, `#[serde(..)]` or `#[schemars(..)]`",
                );
            }
        }
        out
    }

    fn reject_field_attrs(&self, what: &str, errors: &mut Errors) {
        if let Some(attr) = self.field_attrs.first() {
            errors.push(
                attr.span(),
                format!("`#[serde]` and `#[schemars]` only apply to arguments the model fills in, not to {what}"),
            );
        }
        if let Some(span) = self.args {
            errors.push(span, format!("`#[args]` does not apply to {what}"));
        }
    }
}

fn is_lint(attr: &Attribute) -> bool {
    ["allow", "warn", "deny", "forbid", "expect"]
        .iter()
        .any(|lint| attr.path().is_ident(lint))
}

enum TypeKind {
    Ctx,
    State(Box<Type>),
    Model,
}

/// What a parameter's type says about it, or the error to report.
fn type_kind(ty: &Type) -> std::result::Result<TypeKind, (Span, &'static str)> {
    match ty {
        Type::Reference(r) => {
            if last_segment(&r.elem).is_some_and(|s| s.ident == "ToolCtx") {
                if r.mutability.is_some() {
                    return Err((
                        ty.span(),
                        "take the context as `&ToolCtx`, not `&mut ToolCtx`",
                    ));
                }
                return Ok(TypeKind::Ctx);
            }
            Err((
                ty.span(),
                "model arguments must be owned (`String`, not `&str`): they are deserialized from JSON",
            ))
        }
        Type::ImplTrait(_) => Err((
            ty.span(),
            "`#[tool]` functions cannot be generic; model arguments are deserialized into concrete types",
        )),
        Type::Path(_) => {
            let Some(segment) = last_segment(ty) else {
                return Ok(TypeKind::Model);
            };
            if segment.ident == "ToolCtx" {
                return Err((ty.span(), "take the context as `&ToolCtx`"));
            }
            if segment.ident == "State"
                && let PathArguments::AngleBracketed(args) = &segment.arguments
                && args.args.len() == 1
                && let Some(GenericArgument::Type(inner)) = args.args.first()
            {
                return Ok(TypeKind::State(Box::new(inner.clone())));
            }
            Ok(TypeKind::Model)
        }
        _ => Ok(TypeKind::Model),
    }
}

fn last_segment(ty: &Type) -> Option<&syn::PathSegment> {
    match ty {
        Type::Path(p) if p.qself.is_none() => p.path.segments.last(),
        Type::Paren(p) => last_segment(&p.elem),
        Type::Group(g) => last_segment(&g.elem),
        _ => None,
    }
}

// ------------------------------------------------------------------ names

/// `^[a-z][a-z0-9_]{0,63}$`
fn valid_tool_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// `get_weather` -> `GetWeather`.
fn upper_camel(name: &str) -> String {
    name.split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect::<String>()
            })
        })
        .collect()
}

// ------------------------------------------------------------------- docs

/// The text of each `///` line (`#[doc = "..."]`), in order.
fn doc_lines(attrs: &[Attribute]) -> Vec<String> {
    attrs
        .iter()
        .filter(|attr| attr.path().is_ident("doc"))
        .filter_map(|attr| match &attr.meta {
            Meta::NameValue(nv) => match &nv.value {
                Expr::Lit(expr) => match &expr.lit {
                    Lit::Str(s) => Some(s.value()),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// A doc comment as the text the model reads.
///
/// Doc comments are hard-wrapped for the reader of the source, and a model
/// should not see the wrapping: lines of one paragraph are joined with a
/// space, and a blank line separates paragraphs (`\n\n`). Lines that Markdown
/// would not join keep their line break: list items, headings, quotes, table
/// rows, and everything inside a fenced code block (verbatim).
pub(crate) fn normalize_doc(lines: &[String]) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    let mut paragraph_break = false;
    // The previous line must stay on its own line (a fence, a heading, a table row).
    let mut previous_is_hard = false;
    for raw in lines.iter().flat_map(|l| l.split('\n')) {
        let line = raw.strip_prefix(' ').unwrap_or(raw).trim_end();
        let text = line.trim_start();
        let is_fence = text.starts_with("```") || text.starts_with("~~~");
        if is_fence || in_fence {
            if !out.is_empty() {
                out.push_str(if paragraph_break { "\n\n" } else { "\n" });
            }
            out.push_str(if is_fence { text } else { line });
            paragraph_break = false;
            previous_is_hard = true;
            if is_fence {
                in_fence = !in_fence;
            }
            continue;
        }
        if text.is_empty() {
            paragraph_break = !out.is_empty();
            continue;
        }
        if !out.is_empty() {
            if paragraph_break {
                out.push_str("\n\n");
            } else if previous_is_hard || starts_a_block(text) {
                out.push('\n');
            } else {
                out.push(' ');
            }
        }
        out.push_str(text);
        paragraph_break = false;
        previous_is_hard = text.starts_with('#') || text.starts_with('|');
    }
    out
}

/// A line that begins a Markdown block of its own (so it is not joined to the line above).
fn starts_a_block(text: &str) -> bool {
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    let numbered =
        digits > 0 && text[digits..].starts_with(['.', ')']) && text[digits + 1..].starts_with(' ');
    numbered
        || ["- ", "* ", "+ ", "> ", "#", "|"]
            .iter()
            .any(|marker| text.starts_with(marker))
}

// ------------------------------------------------------------- generation

impl Analysis {
    fn generate(&self, func: &ItemFn) -> TokenStream {
        let Self {
            krate,
            fn_ident,
            tool_name,
            tool_ident,
            args_ident,
            description,
            ..
        } = self;

        let kept = kept_fn(func);
        let docs = func.attrs.iter().filter(|a| a.path().is_ident("doc"));
        let vis = &func.vis;

        let model_fields: Vec<&Param> = self
            .params
            .iter()
            .filter(|p| matches!(p, Param::Model { .. }))
            .collect();
        let args_param = self.params.iter().find_map(|p| match p {
            Param::Args(ty) => Some(ty),
            _ => None,
        });

        // The type the model's JSON is read into.
        let (args_ty, args_struct) = match args_param {
            Some(ty) => (quote!(#ty), TokenStream::new()),
            None => (quote!(#args_ident), self.args_struct(&model_fields)),
        };
        let asserted: Vec<&Type> = match args_param {
            Some(ty) => vec![ty],
            None => model_fields
                .iter()
                .filter_map(|p| match p {
                    Param::Model { ty, .. } => Some(ty),
                    _ => None,
                })
                .collect(),
        };
        let assertions = asserted.iter().map(|ty| {
            quote_spanned! {ty.span()=> #krate::__private::assert_tool_arg::<#ty>(); }
        });

        let mut states: Vec<&Type> = Vec::new();
        for param in &self.params {
            if let Param::State(ty) = param
                && !states.iter().any(|s| same_tokens(s, ty))
            {
                states.push(ty);
            }
        }
        let required_state = (!states.is_empty()).then(|| {
            quote! {
                fn required_state(&self) -> ::std::vec::Vec<#krate::StateKey> {
                    ::std::vec![#(#krate::StateKey::of::<#states>()),*]
                }
            }
        });

        // Read the arguments, resolve the state, call the function: one call
        // argument per parameter, in the function's order.
        let mut lets = Vec::new();
        let mut call_args = Vec::new();
        for param in &self.params {
            call_args.push(match param {
                Param::Ctx => quote!(__ctx),
                Param::State(ty) => {
                    let name = format_ident!("__state{}", lets.len());
                    lets.push(quote! { let #name = __ctx.require_state::<#ty>()?; });
                    quote!(#name)
                }
                Param::Model { ident, .. } => quote!(#ident),
                Param::Args(_) => quote!(__args),
            });
        }
        let read = if args_param.is_some() {
            quote!(let __args)
        } else {
            let idents = model_fields.iter().filter_map(|p| match p {
                Param::Model { ident, .. } => Some(ident),
                _ => None,
            });
            quote!(let #args_ident { #(#idents),* })
        };

        let ret = if self.classify {
            quote_spanned! {self.return_span=> #krate::__private::classified(__result) }
        } else {
            quote_spanned! {self.return_span=>
                #krate::__private::IntoToolResult::into_tool_result(__result)
            }
        };

        quote! {
            #kept

            #(#docs)*
            #[derive(
                ::core::fmt::Debug,
                ::core::clone::Clone,
                ::core::marker::Copy,
                ::core::default::Default,
            )]
            #vis struct #tool_ident;

            #args_struct

            const _: () = { #(#assertions)* };

            #[#krate::__private::async_trait]
            impl #krate::Tool for #tool_ident {
                fn spec(&self) -> #krate::__private::ToolSpec {
                    static SPEC: ::std::sync::OnceLock<#krate::__private::ToolSpec> =
                        ::std::sync::OnceLock::new();
                    ::core::clone::Clone::clone(SPEC.get_or_init(|| {
                        #krate::__private::spec_for::<#args_ty>(#tool_name, #description)
                    }))
                }

                #required_state

                async fn call(
                    &self,
                    __ctx: &#krate::ToolCtx,
                    __arguments: #krate::__private::Value,
                ) -> ::core::result::Result<#krate::ToolOutput, #krate::ToolError> {
                    #read = match #krate::__private::parse_args::<#args_ty>(#tool_name, __arguments) {
                        ::core::result::Result::Ok(parsed) => parsed,
                        ::core::result::Result::Err(refusal) => {
                            return ::core::result::Result::Ok(refusal);
                        }
                    };
                    #(#lets)*
                    let __result = #fn_ident(#(#call_args),*).await;
                    #ret
                }
            }
        }
    }

    /// The struct the model's arguments are read into: one field per model parameter.
    fn args_struct(&self, fields: &[&Param]) -> TokenStream {
        let krate = &self.krate;
        let args_ident = &self.args_ident;
        let serde_path = LitStr::new(
            &format!("{}::__private::serde", path_text(krate)),
            Span::call_site(),
        );
        let schemars_path = LitStr::new(
            &format!("{}::__private::schemars", path_text(krate)),
            Span::call_site(),
        );
        let strict = self.strict.then(|| quote!(#[serde(deny_unknown_fields)]));
        let fields = fields.iter().filter_map(|p| match p {
            Param::Model {
                ident,
                ty,
                field_attrs,
                description,
            } => {
                let described = (!description.is_empty())
                    .then(|| quote!(#[schemars(description = #description)]));
                Some(quote! { #described #(#field_attrs)* #ident: #ty, })
            }
            _ => None,
        });
        quote! {
            #[doc(hidden)]
            #[derive(#krate::__private::serde::Deserialize, #krate::__private::schemars::JsonSchema)]
            #[serde(crate = #serde_path)]
            #[schemars(crate = #schemars_path)]
            #strict
            struct #args_ident { #(#fields)* }
        }
    }
}

/// The function as the user wrote it, minus what only the macro reads: the
/// attributes on its parameters (docs, `#[args]`, `#[serde]`) go, lint
/// attributes stay.
fn kept_fn(func: &ItemFn) -> ItemFn {
    let mut kept = func.clone();
    for arg in &mut kept.sig.inputs {
        if let FnArg::Typed(param) = arg {
            param.attrs.retain(is_lint);
        }
    }
    kept
}

fn same_tokens(a: &Type, b: &Type) -> bool {
    quote!(#a).to_string() == quote!(#b).to_string()
}

fn path_text(path: &Path) -> String {
    quote!(#path).to_string().replace(' ', "")
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::*;

    /// The expansion, with whitespace removed so that assertions do not
    /// depend on how tokens are printed.
    fn ok(attr: TokenStream, item: TokenStream) -> String {
        let out = try_expand(attr, item).expect("expands");
        // It is valid Rust, whatever else it is.
        syn::parse2::<syn::File>(out.clone()).expect("the expansion parses as items");
        out.to_string()
            .split_whitespace()
            .collect::<String>()
            .replace("doc=r\"", "doc=\"")
            .replace(",)", ")")
            .replace(",}", "}")
    }

    fn err(attr: TokenStream, item: TokenStream) -> String {
        match try_expand(attr, item) {
            Ok(out) => panic!("expected an error, got {out}"),
            Err(e) => e
                .into_iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    fn has(haystack: &str, needle: &str) {
        let needle: String = needle.split_whitespace().collect();
        assert!(
            haystack.contains(&needle),
            "missing `{needle}` in\n{haystack}"
        );
    }

    fn lacks(haystack: &str, needle: &str) {
        let needle: String = needle.split_whitespace().collect();
        assert!(
            !haystack.contains(&needle),
            "unexpected `{needle}` in\n{haystack}"
        );
    }

    #[test]
    fn a_plain_tool_keeps_the_fn_and_generates_the_struct_and_the_impl() {
        let out = ok(
            quote!(),
            quote! {
                /// Ask the person a question.
                pub async fn ask_user(
                    /// What you need to know
                    question: String,
                ) -> Result<ToolOutput, ToolError> {
                    Ok(ToolOutput::text(question))
                }
            },
        );
        // The function stays, its parameter docs do not.
        has(
            &out,
            "pubasyncfnask_user(question:String)->Result<ToolOutput,ToolError>",
        );
        lacks(&out, "Whatyouneedtoknow\"]question:String)");
        // The unit struct: visibility of the fn, the doc of the fn.
        has(&out, "pubstructAskUser;");
        has(&out, "#[doc=\"Askthepersonaquestion.\"]");
        // The arguments struct.
        has(&out, "struct__AskUserArgs{");
        has(
            &out,
            "#[schemars(description=\"Whatyouneedtoknow\")]question:String}",
        );
        has(&out, "#[serde(crate=\"::adam::__private::serde\")]");
        has(&out, "#[schemars(crate=\"::adam::__private::schemars\")]");
        lacks(&out, "deny_unknown_fields");
        // The impl.
        has(
            &out,
            "#[::adam::__private::async_trait]impl::adam::ToolforAskUser",
        );
        has(
            &out,
            "::adam::__private::spec_for::<__AskUserArgs>(\"ask_user\",\"Askthepersonaquestion.\")",
        );
        has(&out, "::std::sync::OnceLock");
        has(
            &out,
            "let__AskUserArgs{question}=match::adam::__private::parse_args::<__AskUserArgs>(\"ask_user\",__arguments)",
        );
        has(&out, "ask_user(question).await");
        has(
            &out,
            "::adam::__private::IntoToolResult::into_tool_result(__result)",
        );
        // No state declared, no `required_state`.
        lacks(&out, "required_state");
        // Arguments are checked at compile time to be something the model can fill in.
        has(&out, "::adam::__private::assert_tool_arg::<String>()");
    }

    #[test]
    fn ctx_and_state_are_resolved_not_deserialized_and_state_is_declared() {
        let out = ok(
            quote!(),
            quote! {
                /// Run a command.
                async fn run_checks(
                    env: State<ToolEnv>,
                    ctx: &ToolCtx,
                    /// Shell command
                    command: String,
                    cwd: Option<String>,
                    other: State<Other>,
                    again: State<ToolEnv>,
                ) -> Outcome { todo!() }
            },
        );
        // Fields: only the model's parameters.
        has(
            &out,
            "struct__RunChecksArgs{#[schemars(description=\"Shellcommand\")]command:String,cwd:Option<String>}",
        );
        // The state is resolved through the context, in order, and passed by position.
        has(&out, "let__state0=__ctx.require_state::<ToolEnv>()?;");
        has(&out, "let__state1=__ctx.require_state::<Other>()?;");
        has(&out, "let__state2=__ctx.require_state::<ToolEnv>()?;");
        has(
            &out,
            "run_checks(__state0,__ctx,command,cwd,__state1,__state2).await",
        );
        // Declared once per type.
        has(
            &out,
            "fnrequired_state(&self)->::std::vec::Vec<::adam::StateKey>{::std::vec![::adam::StateKey::of::<ToolEnv>(),::adam::StateKey::of::<Other>()]}",
        );
        // A private fn gives a private struct.
        has(&out, "structRunChecks;");
        lacks(&out, "pubstruct");
    }

    #[test]
    fn serde_and_schemars_attributes_on_a_parameter_reach_the_field() {
        let out = ok(
            quote!(),
            quote! {
                /// Count.
                async fn count(#[serde(default)] #[schemars(range(min = 1))] n: u32) -> String { todo!() }
            },
        );
        has(&out, "#[serde(default)]#[schemars(range(min=1))]n:u32}");
    }

    #[test]
    fn name_type_strict_and_crate_options() {
        let out = ok(
            quote!(name = "weather", type = Forecast, strict, crate = ::adam_llm_agent),
            quote! {
                /// Weather.
                pub(crate) async fn get_weather(city: String) -> String { city }
            },
        );
        has(&out, "pub(crate)structForecast;");
        has(
            &out,
            "::adam_llm_agent::__private::spec_for::<__ForecastArgs>(\"weather\",",
        );
        has(&out, "impl::adam_llm_agent::ToolforForecast");
        has(
            &out,
            "#[serde(crate=\"::adam_llm_agent::__private::serde\")]",
        );
        has(&out, "#[serde(deny_unknown_fields)]");
        has(&out, "get_weather(city).await");
    }

    #[test]
    fn a_tool_name_is_derived_from_the_fn_and_the_type_is_upper_camel() {
        let out = ok(
            quote!(),
            quote! {
                /// D.
                async fn get_the_weather_2() -> String { String::new() }
            },
        );
        has(&out, "structGetTheWeather2;");
        has(
            &out,
            "spec_for::<__GetTheWeather2Args>(\"get_the_weather_2\"",
        );
        // No parameters: an empty (but braced) arguments struct.
        has(&out, "struct__GetTheWeather2Args{}");
    }

    #[test]
    fn an_args_struct_replaces_the_generated_one() {
        let out = ok(
            quote!(),
            quote! {
                /// Search.
                async fn search(state: State<Db>, #[args] query: SearchArgs) -> String { todo!() }
            },
        );
        lacks(&out, "struct__SearchArgs");
        has(&out, "spec_for::<SearchArgs>(\"search\"");
        has(
            &out,
            "let__args=match::adam::__private::parse_args::<SearchArgs>(\"search\",__arguments)",
        );
        has(&out, "search(__state0,__args).await");
        has(&out, "assert_tool_arg::<SearchArgs>()");
        // The marker does not survive on the kept function.
        lacks(&out, "#[args]");
    }

    #[test]
    fn classify_routes_the_result_through_the_classifier() {
        let out = ok(
            quote!(classify),
            quote! {
                /// Fetch.
                async fn fetch(url: String) -> Result<String, MyErr> { todo!() }
            },
        );
        has(&out, "::adam::__private::classified(__result)");
        lacks(&out, "into_tool_result");
    }

    #[test]
    fn lint_attributes_stay_on_the_kept_parameter_only() {
        let out = ok(
            quote!(),
            quote! {
                /// D.
                async fn f(#[allow(unused)] a: String) -> String { a }
            },
        );
        has(&out, "asyncfnf(#[allow(unused)]a:String)");
        lacks(&out, "struct__FArgs{#[allow");
    }

    #[test]
    fn descriptions_are_joined_and_paragraphs_kept() {
        let lines = |text: &str| text.lines().map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(
            normalize_doc(&lines(" One two\n three.\n\n Second paragraph.")),
            "One two three.\n\nSecond paragraph."
        );
        assert_eq!(normalize_doc(&lines("")), "");
        assert_eq!(normalize_doc(&lines("\n\n  \n")), "");
        // Lists, headings and quotes keep their own line; a wrapped item joins.
        assert_eq!(
            normalize_doc(&lines(
                " Steps:\n 1. first\n    wrapped\n 2) second\n - a\n * b\n > quote\n # Head\n after"
            )),
            "Steps:\n1. first wrapped\n2) second\n- a\n* b\n> quote\n# Head\nafter"
        );
        // A number that is not a list marker is text.
        assert_eq!(normalize_doc(&lines(" In\n 2024 we")), "In 2024 we");
        // Fences are verbatim, on their own lines.
        assert_eq!(
            normalize_doc(&lines(
                " Use it:\n ```\n let a =  1;\n\n   indented\n ```\n Done."
            )),
            "Use it:\n```\nlet a =  1;\n\n  indented\n```\nDone."
        );
        // A table row does not swallow the next line.
        assert_eq!(
            normalize_doc(&lines(" | a | b |\n after")),
            "| a | b |\nafter"
        );
        // Block comments arrive as one string with newlines.
        assert_eq!(normalize_doc(&["one\n two".to_owned()]), "one two");
    }

    #[test]
    fn names_and_types() {
        assert!(valid_tool_name("ask_user"));
        assert!(valid_tool_name("a"));
        assert!(valid_tool_name(&"a".repeat(64)));
        assert!(!valid_tool_name(&"a".repeat(65)));
        for bad in ["", "Has Space", "_x", "1a", "a-b", "Ab", "é"] {
            assert!(!valid_tool_name(bad), "{bad}");
        }
        assert_eq!(upper_camel("ask_user"), "AskUser");
        assert_eq!(upper_camel("_a__b_"), "AB");
        assert_eq!(upper_camel("x2"), "X2");
        assert_eq!(upper_camel("_"), "");
    }

    // ---- the errors a user sees

    #[test]
    fn no_doc_comment() {
        let msg = err(quote!(), quote! { async fn f() -> String { todo!() } });
        assert_eq!(
            msg,
            "`#[tool]` needs a doc comment: it is the description the model reads"
        );
        // Only attributes that are not docs: still none.
        let msg = err(
            quote!(),
            quote! { #[doc(hidden)] async fn f() -> String { todo!() } },
        );
        assert!(msg.contains("needs a doc comment"));
    }

    #[test]
    fn not_async() {
        let msg = err(
            quote!(),
            quote! { /// D.
            fn f() -> String { todo!() } },
        );
        assert_eq!(msg, "`#[tool]` functions must be `async`");
    }

    #[test]
    fn generic_and_impl_trait() {
        let generic = "`#[tool]` functions cannot be generic; model arguments are deserialized into concrete types";
        assert_eq!(
            err(
                quote!(),
                quote! { /// D.
                async fn f<T>(a: T) -> String { todo!() } }
            ),
            generic
        );
        assert_eq!(
            err(
                quote!(),
                quote! { /// D.
                async fn f() -> String where String: Clone { todo!() } }
            ),
            generic
        );
        assert_eq!(
            err(
                quote!(),
                quote! { /// D.
                async fn f(a: impl Into<String>) -> String { todo!() } }
            ),
            generic
        );
    }

    #[test]
    fn const_unsafe_extern() {
        for item in [
            quote! { /// D.
            const async fn f() -> String { todo!() } },
            quote! { /// D.
            async unsafe fn f() -> String { todo!() } },
            quote! { /// D.
            async extern "C" fn f() -> String { todo!() } },
        ] {
            assert!(err(quote!(), item).contains("cannot be `const`, `unsafe` or `extern`"));
        }
    }

    #[test]
    fn self_receiver() {
        let msg = err(
            quote!(),
            quote! { /// D.
            async fn f(&self) -> String { todo!() } },
        );
        assert_eq!(
            msg,
            "`#[tool]` works on free functions; put shared state in `State<T>`"
        );
    }

    #[test]
    fn borrowed_and_mutable_ctx_arguments() {
        let msg = err(
            quote!(),
            quote! { /// D.
            async fn f(a: &str) -> String { todo!() } },
        );
        assert_eq!(
            msg,
            "model arguments must be owned (`String`, not `&str`): they are deserialized from JSON"
        );
        let msg = err(
            quote!(),
            quote! { /// D.
            async fn f(c: &mut ToolCtx) -> String { todo!() } },
        );
        assert!(msg.contains("not `&mut ToolCtx`"));
        let msg = err(
            quote!(),
            quote! { /// D.
            async fn f(c: ToolCtx) -> String { todo!() } },
        );
        assert!(msg.contains("take the context as `&ToolCtx`"));
    }

    #[test]
    fn two_contexts() {
        let msg = err(
            quote!(),
            quote! { /// D.
            async fn f(a: &ToolCtx, b: &adam::ToolCtx) -> String { todo!() } },
        );
        assert_eq!(msg, "at most one `&ToolCtx` parameter");
    }

    #[test]
    fn args_must_be_alone() {
        let msg = err(
            quote!(),
            quote! { /// D.
            async fn f(#[args] a: A, b: String) -> String { todo!() } },
        );
        assert_eq!(msg, "`#[args]` must be the only model argument");
        let msg = err(
            quote!(),
            quote! { /// D.
            async fn f(#[args] a: A, #[args] b: B) -> String { todo!() } },
        );
        assert_eq!(msg, "`#[args]` must be the only model argument");
        let msg = err(
            quote!(strict),
            quote! { /// D.
            async fn f(#[args] a: A) -> String { todo!() } },
        );
        assert!(msg.contains("`strict` has no effect with `#[args]`"));
        let msg = err(
            quote!(),
            quote! { /// D.
            async fn f(#[args] #[serde(default)] a: A) -> String { todo!() } },
        );
        assert!(msg.contains("on the `#[args]` struct"));
    }

    #[test]
    fn attributes_on_non_model_parameters() {
        let msg = err(
            quote!(),
            quote! { /// D.
            async fn f(#[serde(default)] c: &ToolCtx) -> String { todo!() } },
        );
        assert!(
            msg.contains(
                "only apply to arguments the model fills in, not to a `&ToolCtx` parameter"
            )
        );
        let msg = err(
            quote!(),
            quote! { /// D.
            async fn f(#[serde(default)] c: State<Db>) -> String { todo!() } },
        );
        assert!(msg.contains("not to a `State<T>` parameter"));
        let msg = err(
            quote!(),
            quote! { /// D.
            async fn f(#[args] c: State<Db>) -> String { todo!() } },
        );
        assert!(msg.contains("`#[args]` does not apply to a `State<T>` parameter"));
        let msg = err(
            quote!(),
            quote! { /// D.
            async fn f(#[inline] a: String) -> String { todo!() } },
        );
        assert!(msg.contains("unsupported attribute on a `#[tool]` parameter"));
        // `#[args = 1]` is not the marker.
        let msg = err(
            quote!(),
            quote! { /// D.
            async fn f(#[args = 1] a: String) -> String { todo!() } },
        );
        assert!(msg.contains("unsupported attribute"));
    }

    #[test]
    fn unsupported_patterns() {
        for item in [
            quote! { /// D.
            async fn f((a, b): (u8, u8)) -> String { todo!() } },
            quote! { /// D.
            async fn f(_: String) -> String { todo!() } },
            quote! { /// D.
            async fn f(ref a: String) -> String { todo!() } },
        ] {
            let msg = err(quote!(), item);
            assert!(msg.contains("must be plain names"), "{msg}");
        }
    }

    #[test]
    fn bad_names() {
        let msg = err(
            quote!(name = "Has Space"),
            quote! { /// D.
            async fn f() -> String { todo!() } },
        );
        assert_eq!(msg, "tool names must match `^[a-z][a-z0-9_]{0,63}$`");
        // A derived name gets a hint.
        let msg = err(
            quote!(),
            quote! { /// D.
            async fn _hidden() -> String { todo!() } },
        );
        assert!(
            msg.contains("`^[a-z][a-z0-9_]{0,63}$`; name the tool with `#[tool(name = \"...\")]`"),
            "{msg}"
        );
        // No type name can be derived, or it would be the function itself.
        let msg = err(
            quote!(),
            quote! { /// D.
            async fn Weird() -> String { todo!() } },
        );
        assert!(msg.contains("set one with `#[tool(type = Name)]`"), "{msg}");
        let msg = err(
            quote!(name = "ok"),
            quote! { /// D.
            async fn __() -> String { todo!() } },
        );
        assert!(msg.contains("cannot derive"), "{msg}");
    }

    #[test]
    fn unknown_and_repeated_options() {
        let msg = err(
            quote!(aproval = "always"),
            quote! { /// D.
            async fn f() -> String { todo!() } },
        );
        assert_eq!(
            msg,
            "unknown `#[tool]` option `aproval`; expected one of: name, type, strict, classify, crate"
        );
        for repeated in [
            quote!(name = "a", name = "b"),
            quote!(strict, strict),
            quote!(classify, classify),
            quote!(type = A, type = B),
            quote!(crate = a, crate = b),
        ] {
            let msg = err(
                repeated,
                quote! { /// D.
                async fn f() -> String { todo!() } },
            );
            assert!(msg.ends_with("is given twice in `#[tool]`"), "{msg}");
        }
        // Malformed options are syn's errors.
        assert!(
            err(
                quote!(name = 3),
                quote! { /// D.
                async fn f() {} }
            )
            .contains("expected string literal")
        );
        assert!(
            err(
                quote!(name = "a" strict),
                quote! { /// D.
                async fn f() {} }
            )
            .contains("expected `,`")
        );
    }

    #[test]
    fn not_a_function() {
        let msg = err(quote!(), quote! { pub struct Nope; });
        assert_eq!(msg, "`#[tool]` goes on an `async fn`");
        let msg = err(quote!(), quote! { struct Nope; });
        assert_eq!(msg, "`#[tool]` goes on an `async fn`");
        // Nothing at all.
        assert_eq!(err(quote!(), quote!()), "`#[tool]` goes on an `async fn`");
    }

    #[test]
    fn every_mistake_is_reported_at_once() {
        let msg = err(
            quote!(),
            quote! {
                fn f<T>(&self, a: &str) -> String { todo!() }
            },
        );
        assert_eq!(msg.lines().count(), 5, "{msg}");
    }

    #[test]
    fn expand_turns_an_error_into_compile_error_tokens() {
        let out = expand(quote!(), quote! { struct S; }).to_string();
        assert!(out.contains("compile_error"), "{out}");
        assert!(out.contains("goes on an `async fn`"), "{out}");
        let out = expand(
            quote!(what),
            quote! { /// D.
            async fn f() -> String { todo!() } },
        )
        .to_string();
        assert!(out.contains("unknown `#[tool]` option"), "{out}");
    }
}
