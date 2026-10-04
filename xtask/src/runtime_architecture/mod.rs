// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use proc_macro2::Span;
use quote::ToTokens;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::{Attribute, Fields, GenericArgument, Item, Meta, PathArguments, Token, Type, UseTree};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const MARKER: &str = "<!-- GENERATED INVENTORY: cargo run -p xtask --bin runtime-architecture -->";
const OUTPUT: &str = "lib/docs";

#[derive(Clone)]
struct Definition {
    name: String,
    module: String,
    file: String,
    line: usize,
    kind: String,
    condition: String,
    fields: Vec<Field>,
    alias: Option<Type>,
    parameters: Vec<(String, Option<Type>)>,
}

#[derive(Clone)]
struct Field {
    name: String,
    ty: Type,
    variant: String,
    line: usize,
    condition: String,
}

#[derive(Default)]
struct Module {
    uses: Vec<(String, String)>,
    globs: Vec<String>,
}

#[derive(Default)]
struct Model {
    modules: BTreeMap<String, Module>,
    definitions: BTreeMap<String, Definition>,
    namespaces: BTreeMap<String, Namespace>,
    sources: BTreeMap<String, String>,
    resolution_sources: BTreeMap<String, String>,
    external_crates: BTreeSet<String>,
}

#[derive(Serialize)]
struct Namespace {
    name: String,
    code_lines: usize,
    types: usize,
    files: BTreeSet<String>,
}

#[derive(Serialize)]
struct TypeInfo {
    name: String,
    kind: String,
    file: String,
    line: usize,
    condition: String,
    fields: Vec<FieldInfo>,
}

#[derive(Serialize)]
struct FieldInfo {
    name: String,
    declaration: String,
    variant: String,
    line: usize,
    condition: String,
}

#[derive(Clone, Debug, Serialize)]
struct Edge {
    from: String,
    to: String,
    field: String,
    variant: String,
    cardinality: String,
    relationship: String,
    line: usize,
    condition: String,
}

#[derive(Serialize)]
struct Report {
    format: u8,
    source_fingerprint: String,
    source_files: Vec<String>,
    types: Vec<TypeInfo>,
    edges: Vec<Edge>,
    namespaces: Vec<Namespace>,
    warnings: Vec<String>,
}

fn scoped(module: &str) -> bool {
    module == "drasi_lib"
        || module.starts_with("drasi_lib::")
        || module == "drasi_core"
        || module.starts_with("drasi_core::")
}

// Feature/platform cfgs remain visible as a conditional union. Only expressions
// provably false outside a test build are excluded.
fn production(meta: &Meta) -> Option<bool> {
    match meta {
        Meta::Path(path) if path.is_ident("test") => Some(false),
        Meta::List(list) => {
            let children = Punctuated::<Meta, Token![,]>::parse_terminated
                .parse2(list.tokens.clone())
                .ok()?;
            let values: Vec<_> = children.iter().map(production).collect();
            if list.path.is_ident("not") && values.len() == 1 {
                values[0].map(|value| !value)
            } else if list.path.is_ident("all") {
                if values.contains(&Some(false)) {
                    Some(false)
                } else if values.iter().all(|value| *value == Some(true)) {
                    Some(true)
                } else {
                    None
                }
            } else if list.path.is_ident("any") {
                if values.contains(&Some(true)) {
                    Some(true)
                } else if values.iter().all(|value| *value == Some(false)) {
                    Some(false)
                } else {
                    None
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

fn excluded(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr
                .parse_args::<Meta>()
                .is_ok_and(|meta| production(&meta) == Some(false))
    })
}

fn conditions(attrs: &[Attribute], inherited: &str) -> String {
    let mut parts = Vec::new();
    if !inherited.is_empty() {
        parts.push(inherited.to_owned());
    }
    parts.extend(
        attrs
            .iter()
            .filter(|attr| {
                attr.path().is_ident("cfg")
                    || (attr.path().is_ident("cfg_attr")
                        && attr
                            .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
                            .is_ok_and(|args| {
                                args.iter().skip(1).any(|meta| meta.path().is_ident("cfg"))
                            }))
            })
            .map(|attr| attr.meta.to_token_stream().to_string()),
    );
    parts.join(" && ")
}

fn attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(v) => &v.attrs,
        Item::Enum(v) => &v.attrs,
        Item::ExternCrate(v) => &v.attrs,
        Item::Fn(v) => &v.attrs,
        Item::ForeignMod(v) => &v.attrs,
        Item::Impl(v) => &v.attrs,
        Item::Macro(v) => &v.attrs,
        Item::Mod(v) => &v.attrs,
        Item::Static(v) => &v.attrs,
        Item::Struct(v) => &v.attrs,
        Item::Trait(v) => &v.attrs,
        Item::TraitAlias(v) => &v.attrs,
        Item::Type(v) => &v.attrs,
        Item::Union(v) => &v.attrs,
        Item::Use(v) => &v.attrs,
        _ => &[],
    }
}

fn imports(tree: &UseTree, prefix: &str, module: &mut Module) {
    match tree {
        UseTree::Path(path) => imports(&path.tree, &format!("{prefix}{}::", path.ident), module),
        UseTree::Name(name) => {
            let target = if name.ident == "self" {
                prefix.trim_end_matches("::").to_owned()
            } else {
                format!("{prefix}{}", name.ident)
            };
            let local = target.rsplit("::").next().unwrap_or(&target).to_owned();
            module.uses.push((local, target));
        }
        UseTree::Rename(rename) => module.uses.push((
            rename.rename.to_string(),
            format!("{prefix}{}", rename.ident),
        )),
        UseTree::Glob(_) => module.globs.push(prefix.trim_end_matches("::").to_owned()),
        UseTree::Group(group) => {
            for item in &group.items {
                imports(item, prefix, module);
            }
        }
    }
}

fn declared_fields(fields: &Fields, variant: &str, inherited: &str) -> Vec<Field> {
    fields
        .iter()
        .enumerate()
        .filter(|(_, field)| !excluded(&field.attrs))
        .map(|(index, field)| Field {
            name: field
                .ident
                .as_ref()
                .map_or_else(|| index.to_string(), ToString::to_string),
            ty: field.ty.clone(),
            variant: variant.into(),
            line: field.span().start().line,
            condition: conditions(&field.attrs, inherited),
        })
        .collect()
}

impl Model {
    fn read_file(&mut self, root: &Path, file: &Path, module: &str, condition: &str) -> Result<()> {
        let file = file.canonicalize()?;
        let relative = file.strip_prefix(root)?.to_string_lossy().into_owned();
        let source = fs::read_to_string(&file)?;
        let syntax = syn::parse_file(&source)
            .map_err(|error| format!("{relative}: cannot parse Rust source: {error}"))?;
        self.resolution_sources
            .insert(relative.clone(), source.clone());
        if excluded(&syntax.attrs) {
            return Ok(());
        }
        let condition = conditions(&syntax.attrs, condition);
        if scoped(module) {
            self.sources.insert(relative.clone(), source.clone());
        }
        let stem = file.file_stem().ok_or("source file has no stem")?;
        let base = file.parent().ok_or("source file has no parent")?;
        let children = if stem == "mod" || stem == "lib" {
            base.to_path_buf()
        } else {
            base.join(stem)
        };
        let mut owners = vec![module.to_owned(); source.lines().count()];
        let mut line_filter = NestedItems::default();
        line_filter.visit_file(&syntax);
        let mut removed = line_filter.removed;
        self.read_items(
            root,
            &syntax.items,
            module,
            &relative,
            base,
            &children,
            &condition,
            &mut owners,
            &mut removed,
        )?;
        // proc_macro2's lexer identifies physical lines with actual Rust tokens.
        let mut code = BTreeSet::new();
        token_lines(
            source.parse::<proc_macro2::TokenStream>()?,
            &mut code,
            &removed,
        );
        for line in &code {
            if let Some(owner) = owners
                .get(line.saturating_sub(1))
                .filter(|name| scoped(name))
            {
                let entry = self
                    .namespaces
                    .entry(owner.clone())
                    .or_insert_with(|| Namespace {
                        name: owner.clone(),
                        code_lines: 0,
                        types: 0,
                        files: BTreeSet::new(),
                    });
                entry.code_lines += 1;
                entry.files.insert(relative.clone());
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn read_items(
        &mut self,
        root: &Path,
        items: &[Item],
        module: &str,
        file: &str,
        path_base: &Path,
        children: &Path,
        inherited: &str,
        owners: &mut [String],
        removed: &mut Vec<Span>,
    ) -> Result<()> {
        self.modules.entry(module.into()).or_default();
        for item in items {
            let attributes = attrs(item);
            if excluded(attributes) {
                remove_span(item.span(), removed);
                continue;
            }
            let condition = conditions(attributes, inherited);
            match item {
                Item::Mod(child) => {
                    let child_name = format!("{module}::{}", child.ident);
                    if let Some((_, content)) = &child.content {
                        for line in child.span().start().line..=child.span().end().line {
                            if let Some(owner) = owners.get_mut(line.saturating_sub(1)) {
                                *owner = child_name.clone();
                            }
                        }
                        self.read_items(
                            root,
                            content,
                            &child_name,
                            file,
                            &children.join(child.ident.to_string()),
                            &children.join(child.ident.to_string()),
                            &condition,
                            owners,
                            removed,
                        )?;
                    } else {
                        let explicit = child.attrs.iter().find_map(|attr| {
                            if !attr.path().is_ident("path") {
                                return None;
                            }
                            match &attr.meta {
                                Meta::NameValue(value) => match &value.value {
                                    syn::Expr::Lit(syn::ExprLit {
                                        lit: syn::Lit::Str(value),
                                        ..
                                    }) => Some(value.value()),
                                    _ => None,
                                },
                                _ => None,
                            }
                        });
                        let target = match explicit {
                            Some(path) => path_base.join(path),
                            None => {
                                let plain = children.join(format!("{}.rs", child.ident));
                                let directory =
                                    children.join(child.ident.to_string()).join("mod.rs");
                                match (plain.exists(), directory.exists()) {
                                    (true, false) => plain,
                                    (false, true) => directory,
                                    _ => {
                                        return Err(format!(
                                        "cannot uniquely resolve module {child_name} from {file}"
                                    )
                                        .into())
                                    }
                                }
                            }
                        };
                        self.read_file(root, &target, &child_name, &condition)?;
                    }
                }
                Item::Use(value) => imports(
                    &value.tree,
                    "",
                    self.modules.entry(module.into()).or_default(),
                ),
                Item::Struct(_)
                | Item::Enum(_)
                | Item::Union(_)
                | Item::Trait(_)
                | Item::TraitAlias(_)
                | Item::Type(_) => self.definition(item, module, file, &condition, None)?,
                Item::Macro(value) => {
                    if value.mac.path.is_ident("macro_rules") {
                        continue;
                    }
                    let macro_name = value.mac.path.to_token_stream().to_string();
                    if matches!(
                        macro_name.as_str(),
                        "impl_from_unsigned"
                            | "impl_from_signed"
                            | "impl_from_float"
                            | "partialeq_numeric"
                            | "from_integer"
                    ) {
                        let template = items
                            .iter()
                            .find_map(|item| match item {
                                Item::Macro(definition)
                                    if definition
                                        .ident
                                        .as_ref()
                                        .is_some_and(|name| *name == macro_name) =>
                                {
                                    Some(definition.mac.tokens.clone())
                                }
                                _ => None,
                            })
                            .ok_or_else(|| {
                                format!("missing implementation macro {module}::{macro_name}")
                            })?;
                        reject_type_tokens(template)?;
                        continue;
                    }
                    if macro_name == "lazy_static" {
                        // Static declarations are values; lazy_static's synthetic
                        // marker structs are macro implementation details, not source types.
                        reject_type_tokens(value.mac.tokens.clone())?;
                        continue;
                    }
                    // These three source macros declare the eleven identifier types.
                    // Parse their actual templates rather than copying type definitions.
                    if value.mac.path.is_ident("name_type")
                        || value.mac.path.is_ident("identity_type")
                    {
                        let macro_name = value.mac.path.to_token_stream().to_string();
                        let template = items
                            .iter()
                            .find_map(|item| match item {
                                Item::Macro(definition)
                                    if definition
                                        .ident
                                        .as_ref()
                                        .is_some_and(|name| *name == macro_name) =>
                                {
                                    Some(definition.mac.tokens.clone())
                                }
                                _ => None,
                            })
                            .ok_or_else(|| {
                                format!("missing template for {module}::{macro_name}")
                            })?;
                        let first = value
                            .mac
                            .tokens
                            .clone()
                            .into_iter()
                            .next()
                            .ok_or("empty identity macro")?;
                        let name: syn::Ident = syn::parse2(first.into())?;
                        let expanded = expand_identifier_macro(template, &name)?;
                        for generated in &syn::parse_file(&expanded.to_string())?.items {
                            if matches!(generated, Item::Struct(_)) {
                                self.definition(
                                    generated,
                                    module,
                                    file,
                                    &condition,
                                    Some(value.span().start().line),
                                )?;
                            }
                        }
                    } else if scoped(module) {
                        return Err(format!("{file}:{}: unsupported item macro {}; inspect it and extend inventory coverage", value.span().start().line, value.mac.path.to_token_stream()).into());
                    }
                }
                _ => {}
            }
            let mut nested = NestedItems {
                items: Vec::new(),
                removed: Vec::new(),
            };
            match item {
                Item::Fn(function) => nested.visit_block(&function.block),
                Item::Impl(implementation) => {
                    for member in &implementation.items {
                        if let syn::ImplItem::Fn(function) = member {
                            if excluded(&function.attrs) {
                                remove_span(function.span(), &mut nested.removed);
                            } else {
                                nested.visit_block(&function.block);
                            }
                        }
                    }
                }
                _ => {}
            }
            removed.extend(nested.removed);
            for local in nested.items {
                // Rust has no importable fully qualified path for a block-local type.
                // An explicit lexical suffix prevents pretending otherwise.
                let local_module = format!("{module}::<local@{}>", local.span().start().line);
                self.modules
                    .entry(local_module.clone())
                    .or_default()
                    .globs
                    .push(module.into());
                self.definition(&local, &local_module, file, &condition, None)?;
            }
        }
        Ok(())
    }

    fn definition(
        &mut self,
        item: &Item,
        module: &str,
        file: &str,
        condition: &str,
        line: Option<usize>,
    ) -> Result<()> {
        let (name, kind, fields, alias, generics) = match item {
            Item::Struct(value) => (
                &value.ident,
                "struct",
                declared_fields(&value.fields, "", condition),
                None,
                &value.generics,
            ),
            Item::Union(value) => (
                &value.ident,
                "union",
                declared_fields(&Fields::Named(value.fields.clone()), "<union>", condition),
                None,
                &value.generics,
            ),
            Item::Enum(value) => {
                let fields = value
                    .variants
                    .iter()
                    .filter(|v| !excluded(&v.attrs))
                    .flat_map(|variant| {
                        declared_fields(
                            &variant.fields,
                            &variant.ident.to_string(),
                            &conditions(&variant.attrs, condition),
                        )
                    })
                    .collect();
                (&value.ident, "enum", fields, None, &value.generics)
            }
            Item::Trait(value) => (&value.ident, "trait", vec![], None, &value.generics),
            Item::TraitAlias(value) => (&value.ident, "trait alias", vec![], None, &value.generics),
            Item::Type(value) => (
                &value.ident,
                "type alias",
                vec![],
                Some(*value.ty.clone()),
                &value.generics,
            ),
            _ => return Ok(()),
        };
        let full = format!("{module}::{name}");
        let mut definition = Definition {
            name: full.clone(),
            module: module.into(),
            file: file.into(),
            line: line.unwrap_or_else(|| item.span().start().line),
            kind: kind.into(),
            condition: condition.into(),
            fields,
            alias,
            parameters: generics
                .type_params()
                .map(|value| (value.ident.to_string(), value.default.clone()))
                .collect(),
        };
        if let Some(line) = line {
            for field in &mut definition.fields {
                field.line = line;
            }
        }
        if self.definitions.insert(full.clone(), definition).is_some() {
            return Err(format!(
                "duplicate conditional type {full}; model alternatives explicitly"
            )
            .into());
        }
        Ok(())
    }

    fn resolve(
        &self,
        module: &str,
        path: &str,
        visiting: &mut BTreeSet<String>,
    ) -> BTreeSet<String> {
        let key = format!("{module}|{path}");
        if !visiting.insert(key.clone()) {
            return BTreeSet::new();
        }
        let result = self.resolve_inner(module, path, visiting);
        visiting.remove(&key);
        result
    }

    fn resolve_inner(
        &self,
        module: &str,
        path: &str,
        visiting: &mut BTreeSet<String>,
    ) -> BTreeSet<String> {
        let path = path.trim_start_matches("::");
        let parts: Vec<_> = path.split("::").collect();
        let root = module.split("::").next().unwrap_or(module);
        let explicit = match parts[0] {
            "crate" => Some((root.to_owned(), parts[1..].join("::"))),
            "self" => Some((module.to_owned(), parts[1..].join("::"))),
            "super" => Some((
                module.rsplit_once("::").map_or(root, |v| v.0).to_owned(),
                parts[1..].join("::"),
            )),
            "drasi_core" | "drasi_lib" if parts.len() > 1 => {
                Some((parts[0].into(), parts[1..].join("::")))
            }
            _ => None,
        };
        if let Some((scope, tail)) = explicit {
            if tail.is_empty() {
                return BTreeSet::from([scope]);
            }
            return self.resolve(&scope, &tail, visiting);
        }
        if matches!(parts[0], "std" | "core" | "alloc") || self.external_crates.contains(parts[0]) {
            return BTreeSet::from([path.into()]);
        }
        let local = format!("{module}::{}", parts[0]);
        if self.definitions.contains_key(&local) {
            return BTreeSet::from([if parts.len() == 1 {
                local
            } else {
                format!("{local}::{}", parts[1..].join("::"))
            }]);
        }
        if self.modules.contains_key(&local) {
            if parts.len() == 1 {
                return BTreeSet::from([local]);
            }
            return self.resolve(&local, &parts[1..].join("::"), visiting);
        }
        let Some(imports) = self.modules.get(module) else {
            return BTreeSet::new();
        };
        let mut found = BTreeSet::new();
        for (_, target) in imports.uses.iter().filter(|(name, _)| name == parts[0]) {
            let expanded = if parts.len() == 1 {
                target.clone()
            } else {
                format!("{target}::{}", parts[1..].join("::"))
            };
            found.extend(self.resolve(module, &expanded, visiting));
        }
        if !found.is_empty() {
            return found;
        }
        for glob in &imports.globs {
            for target in self.resolve(module, glob, visiting) {
                found.extend(self.resolve(&target, path, visiting));
            }
        }
        if found.is_empty() {
            let prelude = match path {
                "Option" => Some("std::option::Option"),
                "Result" => Some("std::result::Result"),
                "Vec" => Some("std::vec::Vec"),
                "Box" => Some("std::boxed::Box"),
                "String" => Some("std::string::String"),
                "Send" => Some("std::marker::Send"),
                "Sync" => Some("std::marker::Sync"),
                "Fn" => Some("std::ops::Fn"),
                "FnOnce" => Some("std::ops::FnOnce"),
                "FnMut" => Some("std::ops::FnMut"),
                _ => None,
            };
            if let Some(name) = prelude {
                found.insert(name.into());
            }
        }
        found
    }
}

#[derive(Default)]
struct NestedItems {
    items: Vec<Item>,
    removed: Vec<Span>,
}
impl<'ast> Visit<'ast> for NestedItems {
    fn visit_item(&mut self, item: &'ast Item) {
        if excluded(attrs(item)) {
            remove_span(item.span(), &mut self.removed);
        } else {
            if matches!(
                item,
                Item::Struct(_) | Item::Enum(_) | Item::Union(_) | Item::Type(_) | Item::Trait(_)
            ) {
                self.items.push(item.clone());
            }
            syn::visit::visit_item(self, item);
        }
    }
    fn visit_field(&mut self, field: &'ast syn::Field) {
        if excluded(&field.attrs) {
            remove_span(field.span(), &mut self.removed);
        } else {
            syn::visit::visit_field(self, field);
        }
    }
    fn visit_variant(&mut self, variant: &'ast syn::Variant) {
        if excluded(&variant.attrs) {
            remove_span(variant.span(), &mut self.removed);
        } else {
            syn::visit::visit_variant(self, variant);
        }
    }
    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if excluded(&item.attrs) {
            remove_span(item.span(), &mut self.removed);
        } else {
            syn::visit::visit_impl_item_fn(self, item);
        }
    }
    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        if excluded(&item.attrs) {
            remove_span(item.span(), &mut self.removed);
        } else {
            syn::visit::visit_trait_item_fn(self, item);
        }
    }
}

fn expand_identifier_macro(
    template: proc_macro2::TokenStream,
    name: &syn::Ident,
) -> Result<proc_macro2::TokenStream> {
    let body = template
        .into_iter()
        .find_map(|token| match token {
            proc_macro2::TokenTree::Group(group)
                if group.delimiter() == proc_macro2::Delimiter::Brace =>
            {
                Some(group.stream())
            }

            _ => None,
        })
        .ok_or("identifier macro has no expansion body")?;
    fn substitute(
        tokens: proc_macro2::TokenStream,
        name: &syn::Ident,
    ) -> Result<proc_macro2::TokenStream> {
        let mut out = proc_macro2::TokenStream::new();
        let mut iter = tokens.into_iter();
        while let Some(token) = iter.next() {
            match token {
                proc_macro2::TokenTree::Punct(punct) if punct.as_char() == '$' => {
                    match iter.next().map(|v| v.to_string()).as_deref() {
                        Some("name") => out.extend([proc_macro2::TokenTree::Ident(name.clone())]),
                        Some("doc") => out.extend([proc_macro2::TokenTree::Literal(
                            proc_macro2::Literal::string("generated identifier"),
                        )]),
                        other => {
                            return Err(
                                format!("unsupported identifier macro variable: {other:?}").into()
                            )
                        }
                    }
                }
                proc_macro2::TokenTree::Group(group) => {
                    out.extend([proc_macro2::TokenTree::Group(proc_macro2::Group::new(
                        group.delimiter(),
                        substitute(group.stream(), name)?,
                    ))])
                }
                other => out.extend([other]),
            }
        }
        Ok(out)
    }
    substitute(body, name)
}

fn reject_type_tokens(tokens: proc_macro2::TokenStream) -> Result<()> {
    for token in tokens {
        match token {
            proc_macro2::TokenTree::Ident(name)
                if matches!(name.to_string().as_str(), "struct" | "enum" | "union" | "trait" | "type") =>
                return Err("a previously non-type macro now contains a type declaration; extend extraction coverage".into()),
            proc_macro2::TokenTree::Group(group) => reject_type_tokens(group.stream())?,
            _ => {}
        }
    }
    Ok(())
}

fn remove_span(span: Span, removed: &mut Vec<Span>) {
    removed.push(span);
}
fn token_lines(tokens: proc_macro2::TokenStream, lines: &mut BTreeSet<usize>, removed: &[Span]) {
    let mut tokens = tokens.into_iter().peekable();
    while let Some(token) = tokens.next() {
        let span = token.span();
        if removed
            .iter()
            .any(|range| span.start() >= range.start() && span.end() <= range.end())
        {
            continue;
        }
        if matches!(&token, proc_macro2::TokenTree::Punct(p) if p.as_char() == '#') {
            let mut following = tokens.clone();
            if matches!(following.peek(), Some(proc_macro2::TokenTree::Punct(p)) if p.as_char() == '!')
            {
                following.next();
            }
            if matches!(following.next(), Some(proc_macro2::TokenTree::Group(group))
                if group.delimiter() == proc_macro2::Delimiter::Bracket
                    && matches!(group.stream().into_iter().next(), Some(proc_macro2::TokenTree::Ident(name)) if name == "doc"))
            {
                tokens = following;
                continue;
            }
        }
        match token {
            proc_macro2::TokenTree::Group(group) => {
                lines.insert(group.span_open().start().line);
                lines.insert(group.span_close().start().line);
                token_lines(group.stream(), lines, removed);
            }
            token => {
                lines.extend(token.span().start().line..=token.span().end().line);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Cardinality {
    min: usize,
    max: Option<usize>,
}
impl Cardinality {
    const ONE: Self = Self {
        min: 1,
        max: Some(1),
    };
    const OPTIONAL: Self = Self {
        min: 0,
        max: Some(1),
    };
    const MANY: Self = Self { min: 0, max: None };
    fn times(self, other: Self) -> Self {
        Self {
            min: self.min.saturating_mul(other.min),
            max: match (self.max, other.max) {
                (Some(0), _) | (_, Some(0)) => Some(0),
                (Some(a), Some(b)) => a.checked_mul(b),
                _ => None,
            },
        }
    }
    fn text(self) -> String {
        match self.max {
            Some(max) if max == self.min => max.to_string(),
            Some(max) => format!("{}..{max}", self.min),
            None => format!("{}..*", self.min),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn dependencies(
    model: &Model,
    owner: &Definition,
    field: &Field,
    ty: &Type,
    module: &str,
    count: Cardinality,
    modes: &[String],
    aliases: &mut BTreeSet<String>,
    edges: &mut Vec<Edge>,
    warnings: &mut BTreeSet<String>,
) {
    let recurse = |ty: &Type,
                   count,
                   modes: &[String],
                   aliases: &mut BTreeSet<String>,
                   edges: &mut Vec<Edge>,
                   warnings: &mut BTreeSet<String>| {
        dependencies(
            model, owner, field, ty, module, count, modes, aliases, edges, warnings,
        );
    };
    match ty {
        Type::Reference(reference) => {
            let mut modes = modes.to_vec();
            modes.push("borrow".into());
            recurse(&reference.elem, count, &modes, aliases, edges, warnings);
        }
        Type::Ptr(pointer) => {
            let mut modes = modes.to_vec();
            modes.push("raw pointer; liveness unknown".into());
            recurse(
                &pointer.elem,
                count.times(Cardinality::OPTIONAL),
                &modes,
                aliases,
                edges,
                warnings,
            );
        }
        Type::Slice(slice) => recurse(
            &slice.elem,
            count.times(Cardinality::MANY),
            modes,
            aliases,
            edges,
            warnings,
        ),
        Type::Array(array) => {
            let size = match &array.len {
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Int(n),
                    ..
                }) => n.base10_parse().ok(),
                _ => None,
            };
            let factor = size.map_or(Cardinality::MANY, |n| Cardinality {
                min: n,
                max: Some(n),
            });
            if size.is_none() {
                warnings.insert(format!(
                    "{} field {}: symbolic array extent {}",
                    owner.name,
                    field.name,
                    array.len.to_token_stream()
                ));
            }
            recurse(
                &array.elem,
                count.times(factor),
                modes,
                aliases,
                edges,
                warnings,
            );
        }
        Type::Tuple(tuple) => {
            for member in &tuple.elems {
                recurse(member, count, modes, aliases, edges, warnings);
            }
        }
        Type::Paren(paren) => recurse(&paren.elem, count, modes, aliases, edges, warnings),
        Type::Group(group) => recurse(&group.elem, count, modes, aliases, edges, warnings),
        Type::TraitObject(object) => {
            for bound in &object.bounds {
                if let syn::TypeParamBound::Trait(bound) = bound {
                    let path = bound
                        .path
                        .segments
                        .iter()
                        .map(|s| s.ident.to_string())
                        .collect::<Vec<_>>()
                        .join("::");
                    if matches!(path.as_str(), "Send" | "Sync") {
                        continue;
                    }
                    let mut modes = modes.to_vec();
                    modes.push("trait object; concrete implementation erased".into());
                    let bound_type = Type::Path(syn::TypePath {
                        qself: None,
                        path: bound.path.clone(),
                    });
                    recurse(&bound_type, count, &modes, aliases, edges, warnings);
                }
            }
        }
        Type::BareFn(_) => {
            // Parameter and return types are not stored object relationships.
        }
        Type::Path(path) => {
            let text = path
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect::<Vec<_>>()
                .join("::");
            let text = if text == "Self" {
                owner.name.clone()
            } else {
                text
            };
            if owner
                .parameters
                .iter()
                .any(|(name, _)| name == text.split("::").next().unwrap_or(""))
                || path.qself.is_some()
            {
                warnings.insert(format!(
                    "{} field {}: generic/associated target {} is not a concrete runtime type",
                    owner.name,
                    field.name,
                    ty.to_token_stream()
                ));
                return;
            }
            if matches!(
                text.as_str(),
                "bool"
                    | "char"
                    | "str"
                    | "u8"
                    | "u16"
                    | "u32"
                    | "u64"
                    | "u128"
                    | "usize"
                    | "i8"
                    | "i16"
                    | "i32"
                    | "i64"
                    | "i128"
                    | "isize"
                    | "f32"
                    | "f64"
            ) {
                return;
            }
            let targets = model.resolve(module, &text, &mut BTreeSet::new());
            if targets.len() != 1 {
                warnings.insert(format!(
                    "{} field {}: {} target {text}: {:?}",
                    owner.name,
                    field.name,
                    if targets.is_empty() {
                        "unresolved"
                    } else {
                        "ambiguous"
                    },
                    targets
                ));
                return;
            }
            let target = targets.first().expect("one resolved target");
            if let Some(alias) = model
                .definitions
                .get(target)
                .filter(|definition| definition.alias.is_some())
            {
                if aliases.insert(target.clone()) {
                    let mut modes = modes.to_vec();
                    modes.push(format!("alias {target}"));
                    let arguments: Vec<Type> = path
                        .path
                        .segments
                        .last()
                        .into_iter()
                        .flat_map(|s| match &s.arguments {
                            PathArguments::AngleBracketed(args) => args
                                .args
                                .iter()
                                .filter_map(|a| match a {
                                    GenericArgument::Type(ty) => Some(ty.clone()),
                                    _ => None,
                                })
                                .collect(),
                            _ => vec![],
                        })
                        .collect();
                    let mut substitutions = BTreeMap::new();
                    for (index, (parameter, default)) in alias.parameters.iter().enumerate() {
                        if let Some(argument) = arguments.get(index).or(default.as_ref()) {
                            let mut argument = argument.clone();
                            qualify_type(
                                model,
                                if index < arguments.len() {
                                    module
                                } else {
                                    &alias.module
                                },
                                &mut argument,
                            );
                            substitutions.insert(parameter.clone(), argument);
                        } else {
                            warnings.insert(format!(
                                "{} field {}: missing alias argument {target}::{parameter}",
                                owner.name, field.name
                            ));
                        }
                    }
                    let mut representation = alias.alias.clone().expect("alias");
                    struct Substitute(BTreeMap<String, Type>);
                    impl syn::visit_mut::VisitMut for Substitute {
                        fn visit_type_mut(&mut self, ty: &mut Type) {
                            if let Type::Path(path) = ty {
                                if let Some(ident) = path.path.get_ident() {
                                    if let Some(replacement) = self.0.get(&ident.to_string()) {
                                        *ty = replacement.clone();
                                        return;
                                    }
                                }
                            }
                            syn::visit_mut::visit_type_mut(self, ty);
                        }
                    }
                    syn::visit_mut::VisitMut::visit_type_mut(
                        &mut Substitute(substitutions),
                        &mut representation,
                    );
                    dependencies(
                        model,
                        owner,
                        field,
                        &representation,
                        &alias.module,
                        count,
                        &modes,
                        aliases,
                        edges,
                        warnings,
                    );
                    aliases.remove(target);
                } else {
                    warnings.insert(format!(
                        "{} field {}: recursive alias {target}",
                        owner.name, field.name
                    ));
                }
                return;
            }
            let last = target.rsplit("::").next().unwrap_or(target);
            let external = !model.definitions.contains_key(target);
            let mut next_modes = modes.to_vec();
            let known_wrapper_crate = matches!(
                target.split("::").next(),
                Some(
                    "std" | "core" | "alloc" | "tokio" | "once_cell" | "futures" | "im" | "anyhow"
                )
            );
            let factor = if external && known_wrapper_crate {
                match last {
                    "Option" | "OnceLock" | "OnceCell" => {
                        next_modes.push(last.into());
                        Some(Cardinality::OPTIONAL)
                    }
                    "Weak" => {
                        next_modes.push("weak live target".into());
                        Some(Cardinality::OPTIONAL)
                    }
                    "Arc" | "Rc" => {
                        next_modes.push("shared strong".into());
                        Some(Cardinality::ONE)
                    }
                    "Box" => {
                        next_modes.push("owned box".into());
                        Some(Cardinality::ONE)
                    }
                    "Mutex" | "RwLock" | "RefCell" | "Cell" | "Pin" | "ManuallyDrop" | "Cow" => {
                        next_modes.push(last.into());
                        Some(Cardinality::ONE)
                    }
                    "Vec" | "VecDeque" | "HashMap" | "BTreeMap" | "HashSet" | "BTreeSet"
                    | "BinaryHeap" => {
                        next_modes.push(format!("{last} entries"));
                        Some(Cardinality::MANY)
                    }
                    "Sender" | "Receiver" if target.contains("watch") => {
                        next_modes.push("watch latest value".into());
                        Some(Cardinality::ONE)
                    }
                    "Sender" | "Receiver" | "UnboundedSender" | "UnboundedReceiver" => {
                        next_modes.push("channel payload access (not exclusive ownership)".into());
                        Some(if target.contains("oneshot") {
                            Cardinality::OPTIONAL
                        } else {
                            Cardinality::MANY
                        })
                    }
                    "Result" => {
                        next_modes.push("Result alternative".into());
                        Some(Cardinality::OPTIONAL)
                    }
                    "JoinHandle" | "BoxFuture" | "LocalBoxFuture" | "Future"
                    | "FuturesUnordered" | "Abortable" | "Fn" | "FnOnce" | "FnMut" => {
                        // Futures retain erased captures, NOT their eventual Output value.
                        next_modes.push(
                            "task/future/callback handle; captures and output not inferred".into(),
                        );
                        edges.push(edge(owner, field, target, count, &next_modes));
                        return;
                    }
                    _ => None,
                }
            } else {
                None
            };
            if let Some(factor) = factor {
                if let Some(segment) = path.path.segments.last() {
                    if let PathArguments::AngleBracketed(arguments) = &segment.arguments {
                        for argument in &arguments.args {
                            if let GenericArgument::Type(argument) = argument {
                                recurse(
                                    argument,
                                    count.times(factor),
                                    &next_modes,
                                    aliases,
                                    edges,
                                    warnings,
                                );
                            }
                        }
                    }
                }
            } else {
                edges.push(edge(owner, field, target, count, modes));
                // A custom generic type is the direct stored target. Its arguments
                // are not assumed to be owned; its own definition shows its fields.
            }
        }
        Type::Never(_) => {}
        _ => {
            warnings.insert(format!(
                "{} field {}: opaque stored type {}",
                owner.name,
                field.name,
                ty.to_token_stream()
            ));
        }
    }
}

fn edge(
    owner: &Definition,
    field: &Field,
    target: &str,
    count: Cardinality,
    modes: &[String],
) -> Edge {
    Edge {
        from: owner.name.clone(),
        to: target.into(),
        field: field.name.clone(),
        variant: field.variant.clone(),
        cardinality: count.text(),
        relationship: if modes.is_empty() {
            "stored value".into()
        } else {
            modes.join(" / ")
        },
        line: field.line,
        condition: field.condition.clone(),
    }
}

fn qualify_type(model: &Model, module: &str, ty: &mut Type) {
    struct Qualify<'a> {
        model: &'a Model,
        module: &'a str,
    }
    impl syn::visit_mut::VisitMut for Qualify<'_> {
        fn visit_type_path_mut(&mut self, path: &mut syn::TypePath) {
            syn::visit_mut::visit_type_path_mut(self, path);
            let name = path
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect::<Vec<_>>()
                .join("::");
            let resolved = self.model.resolve(self.module, &name, &mut BTreeSet::new());
            if resolved.len() == 1 {
                if let Ok(mut qualified) =
                    syn::parse_str::<syn::Path>(resolved.first().expect("one target"))
                {
                    if let (Some(last), Some(original)) =
                        (qualified.segments.last_mut(), path.path.segments.last())
                    {
                        last.arguments = original.arguments.clone();
                    }
                    path.path = qualified;
                }
            }
        }
    }
    syn::visit_mut::VisitMut::visit_type_mut(&mut Qualify { model, module }, ty);
}

fn fingerprint(sources: &BTreeMap<String, String>) -> String {
    // Stable change detector, not a cryptographic integrity/security assertion.
    let mut hash = 0xcbf29ce484222325u64;
    for (path, content) in sources {
        for byte in path.bytes().chain([0]).chain(content.bytes()).chain([0]) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("fnv1a64:{hash:016x}")
}

fn build(root: &Path) -> Result<Report> {
    let mut model = Model::default();
    let metadata = std::process::Command::new("cargo")
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--offline",
        ])
        .current_dir(root)
        .output()?;
    if !metadata.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&metadata.stderr)
        )
        .into());
    }
    let metadata: serde_json::Value = serde_json::from_slice(&metadata.stdout)?;
    let packages = metadata["packages"]
        .as_array()
        .ok_or("cargo metadata has no packages")?;
    for package in packages
        .iter()
        .filter(|p| matches!(p["name"].as_str(), Some("drasi-lib" | "drasi-core")))
    {
        for dependency in package["dependencies"]
            .as_array()
            .ok_or("package has no dependencies")?
        {
            if dependency["kind"].as_str() != Some("dev") {
                let name = dependency["rename"]
                    .as_str()
                    .or_else(|| dependency["name"].as_str())
                    .ok_or("dependency has no name")?;
                model.external_crates.insert(name.replace('-', "_"));
            }
        }
    }
    for manifest in ["lib/Cargo.toml", "core/Cargo.toml"] {
        model
            .resolution_sources
            .insert(manifest.into(), fs::read_to_string(root.join(manifest))?);
    }
    model.read_file(root, &root.join("lib/src/lib.rs"), "drasi_lib", "")?;
    model.read_file(root, &root.join("core/src/lib.rs"), "drasi_core", "")?;
    let mut types = Vec::new();
    let mut edges = Vec::new();
    let mut warnings = BTreeSet::new();
    for definition in model.definitions.values().filter(|d| scoped(&d.module)) {
        let namespace = definition
            .module
            .split("::<local@")
            .next()
            .unwrap_or(&definition.module);
        if let Some(namespace) = model.namespaces.get_mut(namespace) {
            namespace.types += 1;
        }
        types.push(TypeInfo {
            name: definition.name.clone(),
            kind: definition.kind.clone(),
            file: definition.file.clone(),
            line: definition.line,
            condition: definition.condition.clone(),
            fields: definition
                .fields
                .iter()
                .map(|field| FieldInfo {
                    name: field.name.clone(),
                    declaration: field.ty.to_token_stream().to_string(),
                    variant: field.variant.clone(),
                    line: field.line,
                    condition: field.condition.clone(),
                })
                .collect(),
        });
    }
    for definition in model.definitions.values().filter(|d| scoped(&d.module)) {
        for field in &definition.fields {
            let count = if field.variant.is_empty() {
                Cardinality::ONE
            } else {
                Cardinality::OPTIONAL
            };
            dependencies(
                &model,
                definition,
                field,
                &field.ty,
                &definition.module,
                count,
                &[],
                &mut BTreeSet::new(),
                &mut edges,
                &mut warnings,
            );
        }
    }
    edges.sort_by(|a, b| {
        (&a.from, &a.to, &a.variant, &a.field).cmp(&(&b.from, &b.to, &b.variant, &b.field))
    });
    Ok(Report {
        format: 1,
        source_fingerprint: fingerprint(&model.resolution_sources),
        source_files: model.sources.keys().cloned().collect(),
        types,
        edges,
        namespaces: model.namespaces.into_values().collect(),
        warnings: warnings.into_iter().collect(),
    })
}

fn matrix(report: &Report) -> (Vec<String>, BTreeMap<(String, String), String>) {
    let names: BTreeSet<_> = report
        .types
        .iter()
        .map(|t| t.name.clone())
        .chain(report.edges.iter().map(|e| e.to.clone()))
        .collect();
    let mut cells: BTreeMap<(String, String), Vec<&Edge>> = BTreeMap::new();
    for edge in &report.edges {
        cells
            .entry((edge.from.clone(), edge.to.clone()))
            .or_default()
            .push(edge);
    }
    (
        names.into_iter().collect(),
        cells
            .into_iter()
            .map(|(key, values)| {
                let mut variants: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
                for edge in values {
                    variants
                        .entry(&edge.variant)
                        .or_default()
                        .push(&edge.cardinality);
                }
                let label = variants
                    .into_iter()
                    .map(|(variant, counts)| {
                        let count = counts.join(" + ");
                        if variant.is_empty() {
                            count
                        } else {
                            format!("{count} [{variant}]")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" OR ");
                (key, label)
            })
            .collect(),
    )
}

fn csv(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn render(report: &Report, document: &str) -> Result<BTreeMap<String, String>> {
    if document.matches(MARKER).count() != 1 {
        return Err("document must contain exactly one generated-inventory marker".into());
    }
    let json = serde_json::to_string(report)?;
    let pretty = serde_json::to_string_pretty(report)? + "\n";
    let html =
        include_str!("report.html").replace("__REPORT_DATA__", &json.replace('<', "\\u003c"));
    let prefix = document
        .split_once(MARKER)
        .map(|(prefix, _)| prefix)
        .ok_or("missing document preamble")?;
    let mut markdown = format!("{prefix}{MARKER}\n\n## Alphabetical type inventory\n\n\
        Generated from **{} source files**; **{} named types**. Source fingerprint: `{}`.\n\n\
        Names below are definition-site paths, not duplicate public re-export paths. \
        Block-local declarations use an explicit `<local@line>` lexical identifier because Rust \
        provides no importable path for them. Traits and aliases are included, but are not extra runtime objects.\n\n",
        report.source_files.len(), report.types.len(), report.source_fingerprint);
    for item in &report.types {
        markdown.push_str(&format!(
            "- `{}` — {}; [source](../../{}#L{}){}.\n",
            item.name,
            item.kind,
            item.file,
            item.line,
            if item.condition.is_empty() {
                String::new()
            } else {
                format!("; conditional: `{}`", item.condition.replace('`', "'"))
            }
        ));
    }
    markdown.push_str("\n## Namespace LOC inventory\n\nThe interactive companion colors these same **exclusive** namespace totals. Child namespaces are not double-counted.\n\n");
    markdown.push_str("| Namespace | Code lines | Named types |\n|---|---:|---:|\n");
    for namespace in &report.namespaces {
        markdown.push_str(&format!(
            "| `{}` | {} | {} |\n",
            namespace.name, namespace.code_lines, namespace.types
        ));
    }
    markdown.push_str(&format!("\n## Extraction limitations requiring review\n\n**{} explicit diagnostics.** These are also searchable in the companion and retained in the JSON. \
        An unresolved or erased target is never silently matched by its short name. \
        The field declaration remains available even when no concrete matrix edge can be inferred.\n\n",
        report.warnings.len()));
    for warning in &report.warnings {
        markdown.push_str(&format!("- `{}`\n", warning.replace('`', "'")));
    }
    let (names, cells) = matrix(report);
    let mut csv_text = format!(
        "{}\n",
        std::iter::once(csv("Dependent \\ dependency"))
            .chain(names.iter().map(|name| csv(name)))
            .collect::<Vec<_>>()
            .join(",")
    );
    for from in report.types.iter().map(|t| &t.name) {
        csv_text.push_str(&csv(from));
        for to in &names {
            csv_text.push(',');
            csv_text.push_str(&csv(cells
                .get(&(from.clone(), to.clone()))
                .map_or("", String::as_str)));
        }
        csv_text.push('\n');
    }
    Ok(BTreeMap::from([
        ("runtime-architecture.md".into(), markdown),
        ("runtime-architecture.html".into(), html),
        ("runtime-architecture.json".into(), pretty),
        ("runtime-dependencies.csv".into(), csv_text),
    ]))
}

pub fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help") {
        println!("cargo run -p xtask --bin runtime-architecture [--check]\nRegenerates the architecture document, offline HTML, JSON and full cardinality matrix.");
        return Ok(());
    }
    if args.iter().any(|a| a != "--check") {
        return Err("unknown option; use --help".into());
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("xtask has no workspace parent")?
        .canonicalize()?;
    let report = build(&root)?;
    let directory = root.join(OUTPUT);
    let document = fs::read_to_string(directory.join("runtime-architecture.md"))?;
    let outputs = render(&report, &document)?;
    let check = args.iter().any(|a| a == "--check");
    let mut stale = Vec::new();
    for (name, content) in outputs {
        let path = directory.join(&name);
        if check {
            if fs::read_to_string(&path).ok().as_ref() != Some(&content) {
                stale.push(name);
            }
        } else {
            fs::write(path, content)?;
        }
    }
    if !stale.is_empty() {
        return Err(format!("stale architecture artifacts: {}", stale.join(", ")).into());
    }
    println!(
        "{} types, {} stored relationships, {} namespaces, {} explicit extraction diagnostics; {}.",
        report.types.len(),
        report.edges.len(),
        report.namespaces.len(),
        report.warnings.len(),
        if check {
            "artifacts current"
        } else {
            "artifacts generated"
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_tests_without_hiding_feature_branches() {
        assert_eq!(
            production(&syn::parse_quote!(all(test, feature = "x"))),
            Some(false)
        );
        assert_eq!(
            production(&syn::parse_quote!(any(test, feature = "x"))),
            None
        );
        assert_eq!(production(&syn::parse_quote!(not(test))), Some(true));
    }

    #[test]
    fn follows_explicit_reexports_without_short_name_guessing() {
        let mut model = Model::default();
        model
            .modules
            .entry("drasi_lib".into())
            .or_default()
            .uses
            .push(("Chosen".into(), "crate::inner::Node".into()));
        model.modules.entry("drasi_lib::inner".into()).or_default();
        model
            .definition(
                &syn::parse_quote!(
                    struct Node;
                ),
                "drasi_lib::inner",
                "fixture.rs",
                "",
                None,
            )
            .unwrap();
        assert_eq!(
            model.resolve("drasi_lib", "Chosen", &mut BTreeSet::new()),
            BTreeSet::from(["drasi_lib::inner::Node".into()])
        );
        assert!(model
            .resolve("drasi_lib", "Node", &mut BTreeSet::new())
            .is_empty());
    }

    #[test]
    fn cardinality_distinguishes_shared_weak_optional_and_collections() {
        assert_eq!(Cardinality::ONE.times(Cardinality::OPTIONAL).text(), "0..1");
        assert_eq!(
            Cardinality::OPTIONAL.times(Cardinality::MANY).text(),
            "0..*"
        );
        assert_eq!(
            Cardinality::ONE
                .times(Cardinality {
                    min: 3,
                    max: Some(3)
                })
                .text(),
            "3"
        );
    }

    #[test]
    fn identifier_macros_use_the_source_template() {
        let template: proc_macro2::TokenStream = quote::quote! {
            ($name:ident, $doc:literal) => { #[doc = $doc] pub struct $name(std::sync::Arc<str>); };
        };
        let result = expand_identifier_macro(template, &syn::parse_quote!(NodeId)).unwrap();
        let file: syn::File = syn::parse2(result).unwrap();
        assert!(matches!(&file.items[0], Item::Struct(item) if item.ident == "NodeId"));
    }

    fn fixture(source: &str) -> Model {
        let mut model = Model::default();
        model
            .external_crates
            .extend(["tokio".into(), "futures".into()]);
        let file = syn::parse_file(source).unwrap();
        let mut owners = vec!["drasi_lib".into(); source.lines().count()];
        model
            .read_items(
                Path::new("."),
                &file.items,
                "drasi_lib",
                "fixture.rs",
                Path::new("."),
                Path::new("."),
                "",
                &mut owners,
                &mut Vec::new(),
            )
            .unwrap();
        model
    }

    fn fixture_edges(model: &Model, name: &str) -> (Vec<Edge>, BTreeSet<String>) {
        let owner = &model.definitions[&format!("drasi_lib::{name}")];
        let mut edges = Vec::new();
        let mut warnings = BTreeSet::new();
        for field in &owner.fields {
            dependencies(
                model,
                owner,
                field,
                &field.ty,
                &owner.module,
                if field.variant.is_empty() {
                    Cardinality::ONE
                } else {
                    Cardinality::OPTIONAL
                },
                &[],
                &mut BTreeSet::new(),
                &mut edges,
                &mut warnings,
            );
        }
        (edges, warnings)
    }

    #[test]
    fn stored_edges_expand_aliases_and_keep_weak_and_channel_semantics() {
        let model = fixture(
            r#"
            use std::sync::{Arc, Weak};
            struct Node;
            type Nodes<T> = Option<Vec<Arc<T>>>;
            type Updates = tokio::sync::watch::Receiver<Arc<Node>>;
            struct Owner {
                nodes: Nodes<Node>, weak: Weak<Node>, fixed: [Node; 3],
                updates: Updates, callback: fn(Node) -> Node,
                task: tokio::task::JoinHandle<Node>, self_ref: Weak<Self>
            }
        "#,
        );
        let (edges, warnings) = fixture_edges(&model, "Owner");
        assert!(warnings.is_empty(), "{warnings:?}");
        let field = |name: &str| edges.iter().find(|edge| edge.field == name).unwrap();
        assert_eq!(field("nodes").cardinality, "0..*");
        assert_eq!(field("nodes").to, "drasi_lib::Node");
        assert!(field("nodes").relationship.contains("shared strong"));
        assert_eq!(field("weak").cardinality, "0..1");
        assert!(field("weak").relationship.contains("weak live"));
        assert_eq!(field("fixed").cardinality, "3");
        assert_eq!(field("updates").cardinality, "1");
        assert!(field("updates").relationship.contains("watch latest"));
        assert_eq!(field("self_ref").to, "drasi_lib::Owner");
        assert_eq!(field("task").to, "tokio::task::JoinHandle");
        assert!(!edges.iter().any(|edge| edge.field == "callback"));
    }

    #[test]
    fn test_exclusion_inline_modules_and_block_local_types() {
        let model = fixture(
            r#"
            #[cfg(test)] mod tests { struct Hidden; }
            #[cfg(feature = "optional")] mod active { pub struct Visible; }
            fn example() { struct Local; #[cfg(test)] struct HiddenLocal; }
        "#,
        );
        assert!(!model.definitions.keys().any(|name| name.contains("Hidden")));
        assert!(model.definitions["drasi_lib::active::Visible"]
            .condition
            .contains("optional"));
        assert_eq!(
            model
                .definitions
                .keys()
                .filter(|name| name.contains("::<local@"))
                .count(),
            1
        );
    }

    #[test]
    fn enum_alternatives_are_not_independent_required_fields() {
        let model = fixture("struct Node; enum Choice { Empty, One(Node), Many(Vec<Node>) }");
        let (edges, warnings) = fixture_edges(&model, "Choice");
        assert!(warnings.is_empty());
        assert_eq!(edges.len(), 2);
        assert_eq!(
            (&edges[0].variant, edges[0].cardinality.as_str()),
            (&"One".into(), "0..1")
        );
        assert_eq!(
            (&edges[1].variant, edges[1].cardinality.as_str()),
            (&"Many".into(), "0..*")
        );
    }

    #[test]
    fn ambiguity_is_diagnosed_instead_of_picking_a_short_name() {
        let model = fixture("mod a { pub struct Node; } mod b { pub struct Node; } use a::*; use b::*; struct Owner { node: Node }");
        let (edges, warnings) = fixture_edges(&model, "Owner");
        assert!(edges.is_empty());
        assert!(warnings.iter().any(|w| w.contains("ambiguous")));
    }

    #[test]
    fn lexical_line_count_excludes_comments_docs_and_test_items() {
        let source = "// comment\n/// doc\nstruct Node; // inline\n#[cfg(test)]\nmod tests {\n struct Test;\n}\n\n";
        let syntax = syn::parse_file(source).unwrap();
        let mut visitor = NestedItems::default();
        visitor.visit_file(&syntax);
        let mut lines = BTreeSet::new();
        token_lines(source.parse().unwrap(), &mut lines, &visitor.removed);
        assert_eq!(lines.into_iter().collect::<Vec<_>>(), vec![3]);
        let mixed = "/** Documentation */ struct A;\n#[cfg(test)] struct B; struct C;\n";
        let mut visitor = NestedItems::default();
        visitor.visit_file(&syn::parse_file(mixed).unwrap());
        let mut lines = BTreeSet::new();
        token_lines(mixed.parse().unwrap(), &mut lines, &visitor.removed);
        assert_eq!(lines.into_iter().collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn complete_repository_snapshot_has_critical_owners_and_a_rectangular_matrix() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .canonicalize()
            .unwrap();
        let report = build(&root).unwrap();
        assert_eq!(
            report.types.len(),
            report.namespaces.iter().map(|n| n.types).sum::<usize>()
        );
        assert!(report.types.windows(2).all(|w| w[0].name < w[1].name));
        for (from, field, to, cardinality) in [
            (
                "drasi_lib::lib_core::DrasiLib",
                "instance_graph",
                "drasi_lib::computation::instance::InstanceGraph",
                "1",
            ),
            (
                "drasi_lib::computation::instance::InstanceGraph",
                "entry",
                "drasi_lib::computation::instance::Entry",
                "0..1",
            ),
            (
                "drasi_lib::computation::v1::graph::ComputationGraph",
                "components",
                "drasi_lib::computation::v1::graph::controller::InstanceSlot",
                "0..*",
            ),
            (
                "drasi_lib::component_graph::graph::ComponentGraph",
                "runtime",
                "drasi_lib::computation::runtime::Runtime",
                "0..1",
            ),
            (
                "drasi_core::computation::query_adapter::ComputationQuery",
                "inner",
                "drasi_core::query::evaluator::QueryEvaluator",
                "1",
            ),
        ] {
            assert!(
                report.edges.iter().any(|e| e.from == from
                    && e.field == field
                    && e.to == to
                    && e.cardinality == cardinality),
                "{from}.{field} -> {to}"
            );
        }
        assert!(!report
            .warnings
            .iter()
            .any(|w| w.contains("unresolved target") || w.contains("ambiguous")));
        let output = render(&report, MARKER).unwrap();
        assert_eq!(
            output["runtime-dependencies.csv"].lines().count(),
            report.types.len() + 1
        );
        assert_eq!(output, render(&report, MARKER).unwrap());
    }
}
