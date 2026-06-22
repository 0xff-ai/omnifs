use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Parser, ValueEnum};
use ignore::WalkBuilder;
use proc_macro2::Span;
use quote::ToTokens;
use serde::Serialize;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{
    Attribute, Expr, ExprCall, ExprForLoop, ExprLoop, ExprMacro, ExprMatch, ExprMethodCall,
    ExprWhile, Fields, FnArg, ImplItem, Item, ItemEnum, ItemFn, ItemImpl, ItemMod, ItemStruct,
    ItemTrait, Pat, ReturnType, Signature, TraitItem, Type, Visibility,
};

#[derive(Debug, Parser)]
#[command(about = "Build a Rust structure map for agent cleanup work")]
struct Args {
    /// Rust source files or directories to scan.
    #[arg(required = true)]
    paths: Vec<PathBuf>,

    /// Output format.
    #[arg(long, value_enum, default_value_t = Format::Json)]
    format: Format,

    /// Omit cleanup signals.
    #[arg(long)]
    no_signals: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Format {
    Json,
    Tree,
}

#[derive(Debug, Serialize)]
struct StructureMap {
    files: Vec<FileMap>,
    signals: Vec<Signal>,
    recommendations: Vec<Recommendation>,
}

#[derive(Debug, Serialize)]
struct FileMap {
    path: Utf8PathBuf,
    module_hint: String,
    symbols: Vec<Symbol>,
}

#[derive(Clone, Debug, Serialize)]
struct Symbol {
    id: String,
    kind: SymbolKind,
    name: String,
    visibility: String,
    signature: Option<String>,
    range: SourceRange,
    children: Vec<Symbol>,
    tags: BTreeSet<Tag>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum SymbolKind {
    Const,
    Enum,
    Field,
    Function,
    Impl,
    Macro,
    Method,
    Module,
    Static,
    Struct,
    Trait,
    TraitMethod,
    TypeAlias,
    Variant,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
enum Tag {
    Env,
    Filesystem,
    Formatting,
    Network,
    Process,
    Serialization,
    Time,
}

#[derive(Clone, Debug, Serialize)]
struct SourceRange {
    start_line: usize,
    start_col: usize,
    end_line: usize,
    end_col: usize,
}

#[derive(Debug, Serialize)]
struct Signal {
    kind: SignalKind,
    confidence: Confidence,
    claim: String,
    evidence: Vec<String>,
    symbols: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Recommendation {
    kind: SignalKind,
    recommendation: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
enum SignalKind {
    CallSiteChoreography,
    ConversionImplDoesIo,
    DtoConversionFreeFunction,
    DtoDomainMix,
    EnumLadder,
    FanOutHotspot,
    FreeFunctionOwnershipCluster,
    IoPresentationMix,
    ParameterCluster,
    PresentationInEffectfulFunction,
    PrimitiveObsession,
    ReceiverCluster,
    RepeatedCalleeSequence,
    ReparseInLoop,
    NamespaceRedundantTypeName,
    PassiveEnumSwitchboard,
    ReceiverlessAssociatedNamespace,
    StaticAssociatedHelperCluster,
    ValidationBypass,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Confidence {
    Low,
    Medium,
}

#[derive(Debug)]
struct FunctionFact {
    symbol_id: String,
    name: String,
    owner: Option<String>,
    impl_trait: Option<String>,
    is_test: bool,
    has_receiver: bool,
    first_subject: Option<String>,
    params: Vec<String>,
    tags: BTreeSet<Tag>,
    calls: Vec<String>,
    loop_calls: Vec<String>,
}

#[derive(Debug)]
struct MatchFact {
    file: Utf8PathBuf,
    range: SourceRange,
    scrutinee: String,
    arms: Vec<String>,
}

#[derive(Debug)]
struct StructFact {
    symbol_id: String,
    name: String,
    fields: Vec<FieldFact>,
    derives: BTreeSet<String>,
}

#[derive(Debug)]
struct TypeFact {
    symbol_id: String,
    name: String,
    kind: TypeKind,
    visibility: String,
    module_hint: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TypeKind {
    Enum,
    Struct,
    Trait,
}

#[derive(Debug)]
struct ImplFact {
    symbol_id: String,
    owner: String,
    trait_name: Option<String>,
    method_count: usize,
    receiverless_count: usize,
    receiverless_methods: Vec<String>,
}

#[derive(Debug)]
struct FieldFact {
    name: String,
    ty: String,
}

#[derive(Default)]
struct ScanFacts {
    functions: Vec<FunctionFact>,
    impls: Vec<ImplFact>,
    matches: Vec<MatchFact>,
    structs: Vec<StructFact>,
    types: Vec<TypeFact>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let cwd = std::env::current_dir().context("read current directory")?;
    let files = discover_files(&args.paths)?;
    if files.is_empty() {
        bail!("no Rust files found");
    }

    let mut facts = ScanFacts::default();
    let mut file_maps = files
        .into_iter()
        .map(|path| scan_file(&cwd, &path, &mut facts))
        .collect::<Result<Vec<_>>>()?;
    file_maps.sort_by(|a, b| a.path.cmp(&b.path));

    let signals = if args.no_signals {
        Vec::new()
    } else {
        build_signals(&facts)
    };
    let recommendations = build_recommendations(&signals);
    let map = StructureMap {
        files: file_maps,
        signals,
        recommendations,
    };

    match args.format {
        Format::Json => println!("{}", serde_json::to_string_pretty(&map)?),
        Format::Tree => render_tree(&map),
    }

    Ok(())
}

fn discover_files(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files = BTreeSet::new();
    for path in paths {
        if path.is_file() {
            if path.extension().is_some_and(|ext| ext == "rs") {
                files.insert(path.clone());
            }
            continue;
        }
        if !path.is_dir() {
            bail!("path does not exist or is not readable: {}", path.display());
        }
        let mut builder = WalkBuilder::new(path);
        builder.hidden(false);
        for entry in builder
            .filter_entry(|entry| {
                let name = entry.file_name().to_string_lossy();
                name != "target" && name != ".git"
            })
            .build()
        {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
                files.insert(path.to_path_buf());
            }
        }
    }
    Ok(files.into_iter().collect())
}

fn scan_file(cwd: &Path, path: &Path, facts: &mut ScanFacts) -> Result<FileMap> {
    let source = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let syntax = syn::parse_file(&source).with_context(|| format!("parse {}", path.display()))?;
    let rel = relative_utf8_path(cwd, path)?;
    let mut symbols = Vec::new();
    for item in syntax.items {
        if let Some(symbol) = item_symbol(&rel, &item, facts) {
            symbols.push(symbol);
        }
    }
    Ok(FileMap {
        module_hint: module_hint(&rel),
        path: rel,
        symbols,
    })
}

fn item_symbol(file: &Utf8Path, item: &Item, facts: &mut ScanFacts) -> Option<Symbol> {
    match item {
        Item::Const(item) => Some(simple_symbol(
            file,
            SymbolKind::Const,
            item.ident.to_string(),
            &item.vis,
            Some(format!("const {}: {}", item.ident, type_text(&item.ty))),
            item.span(),
            Vec::new(),
        )),
        Item::Enum(item) => Some(enum_symbol(file, item, facts)),
        Item::Fn(item) => Some(function_symbol(file, item, facts)),
        Item::Impl(item) => Some(impl_symbol(file, item, facts)),
        Item::Macro(item) => item.mac.path.segments.last().map(|segment| {
            simple_symbol(
                file,
                SymbolKind::Macro,
                segment.ident.to_string(),
                &Visibility::Inherited,
                Some(tokens(item)),
                item.span(),
                Vec::new(),
            )
        }),
        Item::Mod(item) => Some(module_symbol(file, item, facts)),
        Item::Static(item) => Some(simple_symbol(
            file,
            SymbolKind::Static,
            item.ident.to_string(),
            &item.vis,
            Some(format!("static {}: {}", item.ident, type_text(&item.ty))),
            item.span(),
            Vec::new(),
        )),
        Item::Struct(item) => Some(struct_symbol(file, item, facts)),
        Item::Trait(item) => Some(trait_symbol(file, item, facts)),
        Item::Type(item) => Some(simple_symbol(
            file,
            SymbolKind::TypeAlias,
            item.ident.to_string(),
            &item.vis,
            Some(format!("type {} = {}", item.ident, type_text(&item.ty))),
            item.span(),
            Vec::new(),
        )),
        _ => None,
    }
}

fn enum_symbol(file: &Utf8Path, item: &ItemEnum, facts: &mut ScanFacts) -> Symbol {
    let children = item
        .variants
        .iter()
        .map(|variant| {
            simple_symbol(
                file,
                SymbolKind::Variant,
                variant.ident.to_string(),
                &Visibility::Inherited,
                Some(tokens(variant)),
                variant.span(),
                fields(file, &variant.fields),
            )
        })
        .collect();
    record_type_fact(
        facts,
        file,
        item.ident.to_string(),
        TypeKind::Enum,
        &item.vis,
        item.span(),
    );
    simple_symbol(
        file,
        SymbolKind::Enum,
        item.ident.to_string(),
        &item.vis,
        Some(tokens(&item.generics)),
        item.span(),
        children,
    )
}

fn function_symbol(file: &Utf8Path, item: &ItemFn, facts: &mut ScanFacts) -> Symbol {
    let sig = signature_text(&item.sig);
    let body = collect_body_facts(file, &item.block);
    let symbol = simple_symbol_with_tags(
        file,
        SymbolKind::Function,
        item.sig.ident.to_string(),
        &item.vis,
        Some(sig),
        item.span(),
        Vec::new(),
        body.tags.clone(),
    );
    collect_function_fact(
        facts,
        symbol.id.clone(),
        symbol.name.clone(),
        None,
        None,
        attrs_contain_test(&item.attrs),
        &item.sig,
        body.tags,
        body.calls,
        body.loop_calls,
    );
    facts.matches.extend(body.matches);
    symbol
}

fn impl_symbol(file: &Utf8Path, item: &ItemImpl, facts: &mut ScanFacts) -> Symbol {
    let owner = type_text(&item.self_ty);
    let trait_name = item.trait_.as_ref().map(|(_, path, _)| tokens(path));
    let impl_name = match &item.trait_ {
        Some((_, path, _)) => format!("impl {} for {}", tokens(path), owner),
        None => format!("impl {}", owner),
    };
    let mut children = Vec::new();
    let mut receiverless_methods = Vec::new();
    let mut method_count = 0;
    for impl_item in &item.items {
        if let ImplItem::Fn(method) = impl_item {
            method_count += 1;
            if !signature_has_receiver(&method.sig) {
                receiverless_methods.push(method.sig.ident.to_string());
            }
            let body = collect_body_facts(file, &method.block);
            let name = method.sig.ident.to_string();
            let symbol = simple_symbol_with_tags(
                file,
                SymbolKind::Method,
                name.clone(),
                &method.vis,
                Some(signature_text(&method.sig)),
                method.span(),
                Vec::new(),
                body.tags.clone(),
            );
            collect_function_fact(
                facts,
                symbol.id.clone(),
                name,
                Some(owner.clone()),
                trait_name.clone(),
                attrs_contain_test(&method.attrs),
                &method.sig,
                body.tags,
                body.calls,
                body.loop_calls,
            );
            facts.matches.extend(body.matches);
            children.push(symbol);
        }
    }
    let range = source_range(item.span());
    facts.impls.push(ImplFact {
        symbol_id: format!(
            "{}:{}:{}:{}",
            file, range.start_line, range.start_col, impl_name
        ),
        owner: owner.clone(),
        trait_name,
        method_count,
        receiverless_count: receiverless_methods.len(),
        receiverless_methods,
    });
    simple_symbol(
        file,
        SymbolKind::Impl,
        impl_name,
        &Visibility::Inherited,
        None,
        item.span(),
        children,
    )
}

fn module_symbol(file: &Utf8Path, item: &ItemMod, facts: &mut ScanFacts) -> Symbol {
    let mut children = Vec::new();
    if let Some((_, items)) = &item.content {
        for item in items {
            if let Some(symbol) = item_symbol(file, item, facts) {
                children.push(symbol);
            }
        }
    }
    simple_symbol(
        file,
        SymbolKind::Module,
        item.ident.to_string(),
        &item.vis,
        None,
        item.span(),
        children,
    )
}

fn struct_symbol(file: &Utf8Path, item: &ItemStruct, facts: &mut ScanFacts) -> Symbol {
    let children = fields(file, &item.fields);
    let range = source_range(item.span());
    let symbol_id = format!(
        "{}:{}:{}:{}",
        file, range.start_line, range.start_col, item.ident
    );
    facts.structs.push(StructFact {
        symbol_id,
        name: item.ident.to_string(),
        fields: field_facts(&item.fields),
        derives: derive_names(&item.attrs),
    });
    record_type_fact(
        facts,
        file,
        item.ident.to_string(),
        TypeKind::Struct,
        &item.vis,
        item.span(),
    );
    simple_symbol(
        file,
        SymbolKind::Struct,
        item.ident.to_string(),
        &item.vis,
        Some(tokens(&item.generics)),
        item.span(),
        children,
    )
}

fn trait_symbol(file: &Utf8Path, item: &ItemTrait, facts: &mut ScanFacts) -> Symbol {
    let mut children = Vec::new();
    record_type_fact(
        facts,
        file,
        item.ident.to_string(),
        TypeKind::Trait,
        &item.vis,
        item.span(),
    );
    for trait_item in &item.items {
        if let TraitItem::Fn(method) = trait_item {
            let symbol = simple_symbol(
                file,
                SymbolKind::TraitMethod,
                method.sig.ident.to_string(),
                &Visibility::Inherited,
                Some(signature_text(&method.sig)),
                method.span(),
                Vec::new(),
            );
            collect_function_fact(
                facts,
                symbol.id.clone(),
                symbol.name.clone(),
                Some(item.ident.to_string()),
                None,
                false,
                &method.sig,
                BTreeSet::new(),
                Vec::new(),
                Vec::new(),
            );
            children.push(symbol);
        }
    }
    simple_symbol(
        file,
        SymbolKind::Trait,
        item.ident.to_string(),
        &item.vis,
        Some(tokens(&item.generics)),
        item.span(),
        children,
    )
}

fn record_type_fact(
    facts: &mut ScanFacts,
    file: &Utf8Path,
    name: String,
    kind: TypeKind,
    visibility: &Visibility,
    span: Span,
) {
    let range = source_range(span);
    facts.types.push(TypeFact {
        symbol_id: format!("{}:{}:{}:{}", file, range.start_line, range.start_col, name),
        name,
        kind,
        visibility: visibility_text(visibility),
        module_hint: module_hint(file),
    });
}

fn fields(file: &Utf8Path, fields: &Fields) -> Vec<Symbol> {
    fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let name = field
                .ident
                .as_ref()
                .map_or_else(|| index.to_string(), ToString::to_string);
            simple_symbol(
                file,
                SymbolKind::Field,
                name,
                &field.vis,
                Some(type_text(&field.ty)),
                field.span(),
                Vec::new(),
            )
        })
        .collect()
}

fn field_facts(fields: &Fields) -> Vec<FieldFact> {
    fields
        .iter()
        .enumerate()
        .map(|(index, field)| FieldFact {
            name: field
                .ident
                .as_ref()
                .map_or_else(|| index.to_string(), ToString::to_string),
            ty: type_text(&field.ty),
        })
        .collect()
}

fn derive_names(attrs: &[Attribute]) -> BTreeSet<String> {
    let mut derives = BTreeSet::new();
    for attr in attrs {
        if !attr.path().is_ident("derive") {
            continue;
        }
        let _ = attr.parse_nested_meta(|meta| {
            if let Some(ident) = meta.path.get_ident() {
                derives.insert(ident.to_string());
            }
            Ok(())
        });
    }
    derives
}

fn attrs_contain_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| attr.path().is_ident("test"))
}

fn simple_symbol(
    file: &Utf8Path,
    kind: SymbolKind,
    name: String,
    visibility: &Visibility,
    signature: Option<String>,
    span: Span,
    children: Vec<Symbol>,
) -> Symbol {
    simple_symbol_with_tags(
        file,
        kind,
        name,
        visibility,
        signature,
        span,
        children,
        BTreeSet::new(),
    )
}

fn simple_symbol_with_tags(
    file: &Utf8Path,
    kind: SymbolKind,
    name: String,
    visibility: &Visibility,
    signature: Option<String>,
    span: Span,
    children: Vec<Symbol>,
    tags: BTreeSet<Tag>,
) -> Symbol {
    let range = source_range(span);
    Symbol {
        id: format!("{}:{}:{}:{}", file, range.start_line, range.start_col, name),
        kind,
        name,
        visibility: visibility_text(visibility),
        signature,
        range,
        children,
        tags,
    }
}

fn collect_function_fact(
    facts: &mut ScanFacts,
    symbol_id: String,
    name: String,
    owner: Option<String>,
    impl_trait: Option<String>,
    is_test: bool,
    sig: &Signature,
    tags: BTreeSet<Tag>,
    calls: Vec<String>,
    loop_calls: Vec<String>,
) {
    let params = sig
        .inputs
        .iter()
        .filter_map(fn_arg_type)
        .collect::<Vec<_>>();
    let has_receiver = sig
        .inputs
        .iter()
        .any(|input| matches!(input, FnArg::Receiver(_)));
    let first_subject = params.first().and_then(primary_subject);
    facts.functions.push(FunctionFact {
        symbol_id,
        name,
        owner,
        impl_trait,
        is_test,
        has_receiver,
        first_subject,
        params,
        tags,
        calls,
        loop_calls,
    });
}

fn collect_body_facts(file: &Utf8Path, block: &syn::Block) -> BodyFacts {
    let mut collector = BodyCollector {
        file: file.to_path_buf(),
        tags: BTreeSet::new(),
        calls: Vec::new(),
        loop_calls: Vec::new(),
        matches: Vec::new(),
    };
    collector.visit_block(block);
    BodyFacts {
        tags: collector.tags,
        calls: collector.calls,
        loop_calls: collector.loop_calls,
        matches: collector.matches,
    }
}

fn calls_in_block(block: &syn::Block) -> Vec<String> {
    let mut collector = LoopCallCollector { calls: Vec::new() };
    collector.visit_block(block);
    compact_call_sequence(&collector.calls)
}

struct LoopCallCollector {
    calls: Vec<String>,
}

impl<'ast> Visit<'ast> for LoopCallCollector {
    fn visit_expr_call(&mut self, node: &'ast ExprCall) {
        if let Some(call) = short_call_name(&tokens(&node.func)) {
            self.calls.push(call);
        }
        visit::visit_expr_call(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
        self.calls.push(node.method.to_string());
        visit::visit_expr_method_call(self, node);
    }
}

struct BodyFacts {
    tags: BTreeSet<Tag>,
    calls: Vec<String>,
    loop_calls: Vec<String>,
    matches: Vec<MatchFact>,
}

struct BodyCollector {
    file: Utf8PathBuf,
    tags: BTreeSet<Tag>,
    calls: Vec<String>,
    loop_calls: Vec<String>,
    matches: Vec<MatchFact>,
}

impl<'ast> Visit<'ast> for BodyCollector {
    fn visit_expr_call(&mut self, node: &'ast ExprCall) {
        let callee = tokens(&node.func);
        self.classify_call(&callee);
        if let Some(call) = short_call_name(&callee) {
            self.calls.push(call);
        }
        visit::visit_expr_call(self, node);
    }

    fn visit_expr_macro(&mut self, node: &'ast ExprMacro) {
        let callee = format!("{}!", tokens(&node.mac.path));
        self.classify_call(&callee);
        self.calls.push(callee);
        visit::visit_expr_macro(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
        let callee = node.method.to_string();
        self.classify_call(&callee);
        self.calls.push(callee);
        visit::visit_expr_method_call(self, node);
    }

    fn visit_expr_for_loop(&mut self, node: &'ast ExprForLoop) {
        self.loop_calls.extend(calls_in_block(&node.body));
        visit::visit_expr_for_loop(self, node);
    }

    fn visit_expr_loop(&mut self, node: &'ast ExprLoop) {
        self.loop_calls.extend(calls_in_block(&node.body));
        visit::visit_expr_loop(self, node);
    }

    fn visit_expr_while(&mut self, node: &'ast ExprWhile) {
        self.loop_calls.extend(calls_in_block(&node.body));
        visit::visit_expr_while(self, node);
    }

    fn visit_expr_match(&mut self, node: &'ast ExprMatch) {
        self.matches.push(MatchFact {
            file: self.file.clone(),
            range: source_range(node.match_token.span),
            scrutinee: expr_subject(&node.expr),
            arms: node.arms.iter().filter_map(match_arm_subject).collect(),
        });
        visit::visit_expr_match(self, node);
    }
}

impl BodyCollector {
    fn classify_call(&mut self, callee: &str) {
        let compact = callee.replace(' ', "");
        if compact.contains("std::fs")
            || compact.contains("tokio::fs")
            || compact.contains("File::")
            || compact.contains("read_to_string")
            || compact.contains("write")
        {
            self.tags.insert(Tag::Filesystem);
        }
        if compact.contains("std::env") || compact.contains("var_os") || compact == "var" {
            self.tags.insert(Tag::Env);
        }
        if compact.contains("Command::") || compact.contains("std::process") {
            self.tags.insert(Tag::Process);
        }
        if compact.contains("reqwest") || compact.contains("hyper") || compact.contains("ureq") {
            self.tags.insert(Tag::Network);
        }
        if compact.contains("serde_json") || compact.contains("to_string_pretty") {
            self.tags.insert(Tag::Serialization);
        }
        if compact.contains("format!") || compact == "format" || compact.contains("println!") {
            self.tags.insert(Tag::Formatting);
        }
        if compact.contains("SystemTime")
            || compact.contains("Instant::")
            || compact.contains("OffsetDateTime")
        {
            self.tags.insert(Tag::Time);
        }
    }
}

fn build_signals(facts: &ScanFacts) -> Vec<Signal> {
    let mut signals = Vec::new();
    signals.extend(receiver_cluster_signals(facts));
    signals.extend(parameter_cluster_signals(facts));
    signals.extend(static_associated_helper_cluster_signals(facts));
    signals.extend(receiverless_associated_namespace_signals(facts));
    signals.extend(free_function_ownership_cluster_signals(facts));
    signals.extend(dto_conversion_free_function_signals(facts));
    signals.extend(passive_enum_switchboard_signals(facts));
    signals.extend(namespace_redundant_type_name_signals(facts));
    signals.extend(enum_ladder_signals(facts));
    signals.extend(repeated_callee_sequence_signals(facts));
    signals.extend(reparse_in_loop_signals(facts));
    signals.extend(call_site_choreography_signals(facts));
    signals.extend(io_presentation_mix_signals(facts));
    signals.extend(conversion_impl_does_io_signals(facts));
    signals.extend(presentation_in_effectful_function_signals(facts));
    signals.extend(dto_domain_mix_signals(facts));
    signals.extend(primitive_obsession_signals(facts));
    signals.extend(validation_bypass_signals(facts));
    signals.extend(fan_out_hotspot_signals(facts));
    signals
}

fn build_recommendations(signals: &[Signal]) -> Vec<Recommendation> {
    signals
        .iter()
        .map(|signal| signal.kind)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|kind| Recommendation {
            kind,
            recommendation: recommendation_for(kind),
        })
        .collect()
}

fn recommendation_for(kind: SignalKind) -> &'static str {
    match kind {
        SignalKind::CallSiteChoreography => {
            "Inspect the caller first. If it is assembling lookup order, fallback, conversion, or invalidation policy before delegating, move that policy behind one domain method or adapter."
        },
        SignalKind::ConversionImplDoesIo => {
            "Conversion, Display, and serde-style impls should stay pure. Move filesystem, env, process, network, and clock access to an explicit collection step before conversion."
        },
        SignalKind::DtoConversionFreeFunction => {
            "For one-to-one conversions, prefer From/TryFrom, Display, or a named DTO/view type over foo_to_json, render_foo, or encode_foo free functions."
        },
        SignalKind::DtoDomainMix => {
            "Check whether transport, JSON, config, status, or view types are carrying domain decisions. Keep schema/presentation DTOs at the boundary and convert into domain types early."
        },
        SignalKind::EnumLadder => {
            "Look for repeated external matches over the same enum. If callers project labels, severity, display rows, or next actions, move that behavior onto the enum or a view type in the enum module."
        },
        SignalKind::FanOutHotspot => {
            "Treat this as a risk marker, not a refactor command. Read the call site from the intended model and decide whether it is legitimate orchestration or hiding a smaller domain operation."
        },
        SignalKind::FreeFunctionOwnershipCluster => {
            "Group the functions by subject and ask which type owns the behavior and invariants. Prefer methods or a focused view type when the subject owns the data; use an adapter only when shared context is real state."
        },
        SignalKind::IoPresentationMix => {
            "Separate collection/effects from rendering. Let effectful code return structured facts and keep terminal, JSON, or prose formatting in a presentation layer."
        },
        SignalKind::ParameterCluster => {
            "Repeated leading parameters usually mean missing context. If the same handles or policy travel together, introduce an adapter that owns that state and exposes methods taking only the per-operation input."
        },
        SignalKind::PresentationInEffectfulFunction => {
            "Keep probe/load/fetch/resolve paths focused on effects and domain results. Move formatting, pretty JSON, terminal labels, and status rows to the caller-facing presentation layer."
        },
        SignalKind::PrimitiveObsession => {
            "Check whether raw String, PathBuf, Vec<u8>, bool, or numeric fields represent ids, paths, modes, versions, or states. Newtypes or enums may remove invalid states."
        },
        SignalKind::ReceiverCluster => {
            "Several functions share the same primary subject. Verify whether the behavior belongs on that type, a view over it, or a policy-owning domain type; do not add a wrapper unless it owns real state or invariants."
        },
        SignalKind::RepeatedCalleeSequence => {
            "Compare the repeated call sequence with the intended model. If the sequence is policy rather than plumbing, name the operation once and route callers through it."
        },
        SignalKind::ReparseInLoop => {
            "A parse/build/load/index operation inside a loop may be rebuilding stable context. Check whether an index or parsed value should be built once before iteration."
        },
        SignalKind::NamespaceRedundantTypeName => {
            "Review the fully qualified path. If the module already names the concept, shorten the public type name unless the extra noun distinguishes a real sibling concept."
        },
        SignalKind::PassiveEnumSwitchboard => {
            "If helper functions repeatedly project one enum into labels, JSON, status, or severity, move that projection onto the enum or a view type in the enum module."
        },
        SignalKind::ReceiverlessAssociatedNamespace => {
            "An impl full of receiverless methods may be a namespace, not a type. Give the type state and instance methods, move behavior to the real owner, or use free functions if no owner exists."
        },
        SignalKind::StaticAssociatedHelperCluster => {
            "Static helpers inside an impl that repeatedly take the same external context often indicate underused state. Prefer instance methods on the owning type, or a smaller adapter if the external context is the real owner."
        },
        SignalKind::ValidationBypass => {
            "A deserializable type with validation-looking constructors can bypass invariants. Check whether serde should go through a checked DTO or custom deserialization path."
        },
    }
}

fn receiver_cluster_signals(facts: &ScanFacts) -> Vec<Signal> {
    let mut by_subject: BTreeMap<&str, Vec<&FunctionFact>> = BTreeMap::new();
    for function in &facts.functions {
        if function.is_test || is_constructor_like(&function.name) {
            continue;
        }
        if let Some(subject) = &function.first_subject {
            if is_low_value_receiver_subject(subject) || is_constructor_like(&function.name) {
                continue;
            }
            by_subject.entry(subject).or_default().push(function);
        }
    }
    by_subject
        .into_iter()
        .filter(|(_, functions)| functions.len() >= 3)
        .map(|(subject, functions)| Signal {
            kind: SignalKind::ReceiverCluster,
            confidence: Confidence::Medium,
            claim: format!("multiple functions share {subject} as their primary subject"),
            evidence: functions
                .iter()
                .take(8)
                .map(|function| {
                    format!(
                        "{} takes {} first",
                        function.name,
                        function.params.first().map_or("_", String::as_str)
                    )
                })
                .collect(),
            symbols: functions
                .iter()
                .map(|function| function.symbol_id.clone())
                .collect(),
        })
        .collect()
}

fn is_low_value_receiver_subject(subject: &str) -> bool {
    matches!(
        subject,
        "Path" | "PathBuf" | "str" | "String" | "OsStr" | "OsString"
    )
}

fn is_constructor_like(name: &str) -> bool {
    name == "new" || name == "open" || name == "default" || name.starts_with("from_")
}

fn name_based_subject(name: &str) -> Option<String> {
    let parts = name.split('_').collect::<Vec<_>>();
    if parts.len() < 2 {
        return None;
    }
    let ownership_verbs = [
        "apply",
        "build",
        "decode",
        "encode",
        "format",
        "load",
        "make",
        "parse",
        "project",
        "render",
        "resolve",
        "serialize",
        "store",
        "validate",
    ];
    if ownership_verbs.contains(&parts[0]) {
        return parts.get(1).map(|part| (*part).to_string());
    }
    if parts.len() >= 3 && parts[1] == "to" {
        return Some(parts[0].to_string());
    }
    None
}

fn compact_call_sequence(calls: &[String]) -> Vec<String> {
    let mut sequence = Vec::new();
    for call in calls {
        if ignored_call_name(call) {
            continue;
        }
        if sequence.last() != Some(call) {
            sequence.push(call.clone());
        }
        if sequence.len() == 12 {
            break;
        }
    }
    sequence
}

fn ignored_call_name(call: &str) -> bool {
    matches!(
        call,
        "and_then"
            | "as_bytes"
            | "as_deref"
            | "as_ref"
            | "as_slice"
            | "as_str"
            | "borrow"
            | "builder"
            | "clone"
            | "collect"
            | "copied"
            | "default"
            | "expect"
            | "extend_from_slice"
            | "filter"
            | "filter_map"
            | "flatten"
            | "format!"
            | "from"
            | "into"
            | "into_iter"
            | "is_empty"
            | "is_some_and"
            | "iter"
            | "join"
            | "len"
            | "map"
            | "map_err"
            | "map_or"
            | "new"
            | "ok"
            | "or_default"
            | "path"
            | "push"
            | "range"
            | "println!"
            | "remove"
            | "then"
            | "tempdir"
            | "to_string"
            | "to_vec"
            | "try_into"
            | "unwrap"
            | "unwrap_or"
            | "value"
            | "vec!"
            | "Some"
            | "Ok"
            | "Err"
            | "None"
    )
}

fn policy_call_name(call: &str) -> bool {
    [
        "lookup", "resolve", "select", "fallback", "validate", "parse", "convert", "try_from",
    ]
    .iter()
    .any(|needle| call.contains(needle))
}

fn effectful_name(name: &str) -> bool {
    [
        "fetch", "load", "open", "probe", "read", "resolve", "scan", "write",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
}

fn presentation_name(name: &str) -> bool {
    [
        "display", "format", "render", "status", "summary", "terminal", "to_json",
    ]
    .iter()
    .any(|needle| name.contains(needle))
}

fn boundary_terms(name: &str) -> Vec<&'static str> {
    let lower = name.to_ascii_lowercase();
    [
        "args", "config", "display", "dto", "json", "request", "response", "schema", "status",
        "view", "wire",
    ]
    .into_iter()
    .filter(|term| lower.contains(term))
    .collect()
}

fn boundary_type(ty: &str) -> bool {
    !boundary_terms(ty).is_empty()
}

fn domain_type(ty: &str) -> bool {
    let compact = ty.trim_start_matches('&');
    compact
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_uppercase())
        && !boundary_type(compact)
        && !matches!(
            compact,
            "String" | "Path" | "PathBuf" | "Vec" | "Option" | "Result"
        )
}

fn domain_field_name(name: &str) -> bool {
    [
        "account",
        "auth",
        "generation",
        "handle",
        "id",
        "key",
        "kind",
        "mode",
        "mount",
        "path",
        "provider",
        "scheme",
        "scope",
        "state",
        "token",
        "version",
    ]
    .iter()
    .any(|needle| name.contains(needle))
}

fn primitive_domain_field(field: &FieldFact) -> bool {
    domain_field_name(&field.name)
        && (field.ty == "String"
            || field.ty == "PathBuf"
            || field.ty == "Vec<u8>"
            || field.ty == "Option<String>"
            || field.ty == "bool"
            || field.ty == "u64"
            || field.ty == "usize")
}

fn validation_constructor_name(name: &str) -> bool {
    name == "new"
        || name == "parse"
        || name == "validate"
        || name == "try_from"
        || name.starts_with("from_")
        || name.starts_with("try_")
}

fn dto_conversion_function_name(name: &str) -> bool {
    name.contains("_to_json")
        || name.contains("_to_dto")
        || name.contains("_to_wire")
        || name.starts_with("render_")
        || name.starts_with("format_")
        || name.starts_with("encode_")
        || name.starts_with("serialize_")
}

fn projection_function_name(name: &str) -> bool {
    name.contains("display")
        || name.contains("format")
        || name.contains("json")
        || name.contains("label")
        || name.contains("render")
        || name.contains("severity")
        || name.contains("status")
        || name.contains("summary")
        || name.contains("terminal")
        || name.ends_with("_row")
        || name.ends_with("_prefix")
}

fn function_name_mentions_type(function_name: &str, type_name: &str) -> bool {
    function_name.contains(&snake_case_type_name(type_name))
}

fn snake_case_type_name(type_name: &str) -> String {
    let mut out = String::new();
    for (index, ch) in type_name.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

fn redundant_namespace_component(type_name: &str, module_hint: &str) -> Option<String> {
    let type_tokens = snake_case_type_name(type_name);
    if !type_tokens.contains('_') {
        return None;
    }
    let first_type_token = type_tokens.split('_').next()?;
    module_hint
        .split("::")
        .flat_map(|component| component.split('_'))
        .filter(|component| component.len() >= 4)
        .find(|component| first_type_token == component.to_ascii_lowercase())
        .map(str::to_string)
}

fn loop_reparse_call(call: &str) -> bool {
    [
        "build",
        "decode",
        "from_str",
        "index",
        "load",
        "parse",
        "read_to_string",
        "resolve",
        "to_allocvec",
    ]
    .iter()
    .any(|needle| call.contains(needle))
}

fn conversion_trait_name(name: &str) -> bool {
    name.ends_with("Display")
        || name.ends_with("FromStr")
        || name.ends_with("From")
        || name.ends_with("TryFrom")
        || name.ends_with("Serialize")
        || name.ends_with("Deserialize")
        || name.contains("::Display")
        || name.contains("::FromStr")
        || name.contains("::TryFrom")
}

fn conversion_method_name(name: &str) -> bool {
    matches!(name, "fmt" | "serialize" | "deserialize")
        || name == "to_json"
        || name == "from_str"
        || name == "try_from"
        || name == "from"
}

fn parameter_cluster_signals(facts: &ScanFacts) -> Vec<Signal> {
    let mut by_params: BTreeMap<Vec<String>, Vec<&FunctionFact>> = BTreeMap::new();
    for function in &facts.functions {
        if function.is_test {
            continue;
        }
        if function.params.len() >= 3 {
            by_params
                .entry(function.params.iter().take(4).cloned().collect())
                .or_default()
                .push(function);
        }
    }
    by_params
        .into_iter()
        .filter(|(_, functions)| functions.len() >= 2)
        .map(|(params, functions)| Signal {
            kind: SignalKind::ParameterCluster,
            confidence: Confidence::Medium,
            claim: "multiple functions accept the same leading parameter group".to_string(),
            evidence: vec![format!("shared leading params: ({})", params.join(", "))],
            symbols: functions
                .iter()
                .map(|function| function.symbol_id.clone())
                .collect(),
        })
        .collect()
}

fn static_associated_helper_cluster_signals(facts: &ScanFacts) -> Vec<Signal> {
    let mut by_owner_and_subject: BTreeMap<(String, String), Vec<&FunctionFact>> = BTreeMap::new();
    for function in &facts.functions {
        if function.is_test {
            continue;
        }
        let Some(owner) = &function.owner else {
            continue;
        };
        if function.has_receiver || is_constructor_like(&function.name) {
            continue;
        }
        let Some(subject) = function.params.first().and_then(primary_subject) else {
            continue;
        };
        if subject == *owner || is_low_value_receiver_subject(&subject) {
            continue;
        }
        by_owner_and_subject
            .entry((owner.clone(), subject))
            .or_default()
            .push(function);
    }
    by_owner_and_subject
        .into_iter()
        .filter(|(_, functions)| functions.len() >= 2)
        .map(|((owner, subject), functions)| Signal {
            kind: SignalKind::StaticAssociatedHelperCluster,
            confidence: Confidence::Medium,
            claim: format!("{owner} has several static helpers centered on {subject}"),
            evidence: functions
                .iter()
                .take(8)
                .map(|function| {
                    format!(
                        "{}({})",
                        function.name,
                        function.params.first().map_or("_", String::as_str)
                    )
                })
                .collect(),
            symbols: functions
                .iter()
                .map(|function| function.symbol_id.clone())
                .collect(),
        })
        .collect()
}

fn receiverless_associated_namespace_signals(facts: &ScanFacts) -> Vec<Signal> {
    facts
        .impls
        .iter()
        .filter(|item| item.trait_name.is_none())
        .filter(|item| item.method_count >= 3 && item.receiverless_count >= 3)
        .filter(|item| item.receiverless_count * 2 >= item.method_count)
        .map(|item| Signal {
            kind: SignalKind::ReceiverlessAssociatedNamespace,
            confidence: Confidence::Low,
            claim: format!(
                "impl {} is mostly receiverless associated functions",
                item.owner
            ),
            evidence: vec![format!(
                "receiverless methods: {}",
                item.receiverless_methods
                    .iter()
                    .take(8)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )],
            symbols: vec![item.symbol_id.clone()],
        })
        .collect()
}

fn free_function_ownership_cluster_signals(facts: &ScanFacts) -> Vec<Signal> {
    let mut by_subject: BTreeMap<String, Vec<&FunctionFact>> = BTreeMap::new();
    for function in &facts.functions {
        if function.is_test {
            continue;
        }
        if function.owner.is_some() {
            continue;
        }
        if let Some(subject) = name_based_subject(&function.name) {
            by_subject.entry(subject).or_default().push(function);
        }
    }
    by_subject
        .into_iter()
        .filter(|(_, functions)| functions.len() >= 2)
        .map(|(subject, functions)| Signal {
            kind: SignalKind::FreeFunctionOwnershipCluster,
            confidence: Confidence::Low,
            claim: format!("free functions repeat `{subject}` as a named subject"),
            evidence: functions
                .iter()
                .take(8)
                .map(|function| function.name.clone())
                .collect(),
            symbols: functions
                .iter()
                .map(|function| function.symbol_id.clone())
                .collect(),
        })
        .collect()
}

fn dto_conversion_free_function_signals(facts: &ScanFacts) -> Vec<Signal> {
    facts
        .functions
        .iter()
        .filter(|function| !function.is_test && function.owner.is_none())
        .filter(|function| dto_conversion_function_name(&function.name))
        .map(|function| Signal {
            kind: SignalKind::DtoConversionFreeFunction,
            confidence: Confidence::Low,
            claim: format!("{} looks like a boundary conversion helper", function.name),
            evidence: vec![format!("params: {}", function.params.join(", "))],
            symbols: vec![function.symbol_id.clone()],
        })
        .collect()
}

fn passive_enum_switchboard_signals(facts: &ScanFacts) -> Vec<Signal> {
    let enum_names = facts
        .types
        .iter()
        .filter(|item| item.kind == TypeKind::Enum)
        .map(|item| item.name.as_str())
        .collect::<BTreeSet<_>>();
    let mut by_enum: BTreeMap<&str, Vec<&FunctionFact>> = BTreeMap::new();
    for function in &facts.functions {
        if function.is_test {
            continue;
        }
        let matching_enum = function
            .first_subject
            .as_deref()
            .filter(|subject| enum_names.contains(subject))
            .or_else(|| {
                enum_names
                    .iter()
                    .copied()
                    .find(|name| function_name_mentions_type(&function.name, name))
            });
        if let Some(enum_name) = matching_enum
            && projection_function_name(&function.name)
        {
            by_enum.entry(enum_name).or_default().push(function);
        }
    }
    by_enum
        .into_iter()
        .filter(|(_, functions)| functions.len() >= 2)
        .map(|(enum_name, functions)| Signal {
            kind: SignalKind::PassiveEnumSwitchboard,
            confidence: Confidence::Low,
            claim: format!("{enum_name} has several projection helpers outside enum behavior"),
            evidence: functions
                .iter()
                .take(8)
                .map(|function| function.name.clone())
                .collect(),
            symbols: functions
                .iter()
                .map(|function| function.symbol_id.clone())
                .collect(),
        })
        .collect()
}

fn namespace_redundant_type_name_signals(facts: &ScanFacts) -> Vec<Signal> {
    facts
        .types
        .iter()
        .filter(|item| item.visibility == "pub")
        .filter_map(|item| {
            redundant_namespace_component(&item.name, &item.module_hint).map(|component| Signal {
                kind: SignalKind::NamespaceRedundantTypeName,
                confidence: Confidence::Low,
                claim: format!("{} repeats `{component}` from its module path", item.name),
                evidence: vec![format!("qualified context: {}", item.module_hint)],
                symbols: vec![item.symbol_id.clone()],
            })
        })
        .collect()
}

fn enum_ladder_signals(facts: &ScanFacts) -> Vec<Signal> {
    let mut signals = Vec::new();
    let mut by_scrutinee: BTreeMap<&str, Vec<&MatchFact>> = BTreeMap::new();
    for match_fact in &facts.matches {
        if match_fact.scrutinee != "_" {
            by_scrutinee
                .entry(&match_fact.scrutinee)
                .or_default()
                .push(match_fact);
        }
    }
    signals.extend(
        by_scrutinee
            .into_iter()
            .filter(|(_, matches)| matches.len() >= 3)
            .map(|(scrutinee, matches)| Signal {
                kind: SignalKind::EnumLadder,
                confidence: Confidence::Low,
                claim: format!("several match ladders inspect `{scrutinee}`"),
                evidence: matches
                    .iter()
                    .take(8)
                    .map(|match_fact| {
                        format!(
                            "{}:{}:{}",
                            match_fact.file,
                            match_fact.range.start_line,
                            match_fact.range.start_col
                        )
                    })
                    .collect(),
                symbols: Vec::new(),
            }),
    );

    let mut by_arm_owner: BTreeMap<&str, Vec<&MatchFact>> = BTreeMap::new();
    for match_fact in &facts.matches {
        for arm in &match_fact.arms {
            if arm.chars().next().is_some_and(char::is_uppercase) && !standard_variant_owner(arm) {
                by_arm_owner.entry(arm).or_default().push(match_fact);
            }
        }
    }
    signals.extend(
        by_arm_owner
            .into_iter()
            .filter(|(_, matches)| matches.len() >= 2)
            .map(|(owner, matches)| Signal {
                kind: SignalKind::EnumLadder,
                confidence: Confidence::Medium,
                claim: format!("several match ladders inspect variants under `{owner}`"),
                evidence: matches
                    .iter()
                    .map(|match_fact| {
                        format!(
                            "{}:{}:{}",
                            match_fact.file,
                            match_fact.range.start_line,
                            match_fact.range.start_col
                        )
                    })
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .take(8)
                    .collect(),
                symbols: Vec::new(),
            }),
    );
    signals
}

fn standard_variant_owner(owner: &str) -> bool {
    matches!(
        owner,
        "Ok" | "Err" | "Some" | "None" | "Poll" | "ControlFlow"
    )
}

fn repeated_callee_sequence_signals(facts: &ScanFacts) -> Vec<Signal> {
    let mut by_sequence: BTreeMap<Vec<String>, Vec<&FunctionFact>> = BTreeMap::new();
    for function in &facts.functions {
        if function.is_test {
            continue;
        }
        let sequence = compact_call_sequence(&function.calls);
        if sequence.len() >= 3 {
            by_sequence.entry(sequence).or_default().push(function);
        }
    }
    by_sequence
        .into_iter()
        .filter(|(_, functions)| functions.len() >= 2)
        .map(|(sequence, functions)| Signal {
            kind: SignalKind::RepeatedCalleeSequence,
            confidence: Confidence::Low,
            claim: "multiple functions share the same leading callee sequence".to_string(),
            evidence: vec![format!("calls: {}", sequence.join(" -> "))],
            symbols: functions
                .iter()
                .map(|function| function.symbol_id.clone())
                .collect(),
        })
        .collect()
}

fn reparse_in_loop_signals(facts: &ScanFacts) -> Vec<Signal> {
    facts
        .functions
        .iter()
        .filter(|function| !function.is_test)
        .filter_map(|function| {
            let suspicious = function
                .loop_calls
                .iter()
                .filter(|call| loop_reparse_call(call))
                .cloned()
                .collect::<BTreeSet<_>>();
            if suspicious.is_empty() {
                return None;
            }
            Some(Signal {
                kind: SignalKind::ReparseInLoop,
                confidence: Confidence::Low,
                claim: format!(
                    "{} performs parse/build/load work inside a loop",
                    function.name
                ),
                evidence: vec![format!(
                    "loop calls: {}",
                    suspicious.into_iter().collect::<Vec<_>>().join(", ")
                )],
                symbols: vec![function.symbol_id.clone()],
            })
        })
        .collect()
}

fn call_site_choreography_signals(facts: &ScanFacts) -> Vec<Signal> {
    facts
        .functions
        .iter()
        .filter(|function| !function.is_test)
        .filter(|function| !is_constructor_like(&function.name))
        .filter(|function| {
            let sequence = compact_call_sequence(&function.calls);
            sequence.len() >= 5
                && sequence.iter().any(|call| policy_call_name(call))
                && function.tags.is_empty()
        })
        .map(|function| Signal {
            kind: SignalKind::CallSiteChoreography,
            confidence: Confidence::Low,
            claim: format!("{} coordinates a long local call sequence", function.name),
            evidence: vec![format!(
                "leading calls: {}",
                compact_call_sequence(&function.calls)
                    .into_iter()
                    .take(8)
                    .collect::<Vec<_>>()
                    .join(" -> ")
            )],
            symbols: vec![function.symbol_id.clone()],
        })
        .collect()
}

fn io_presentation_mix_signals(facts: &ScanFacts) -> Vec<Signal> {
    facts
        .functions
        .iter()
        .filter(|function| !function.is_test)
        .filter(|function| {
            (presentation_name(&function.name) || function.tags.contains(&Tag::Formatting))
                && (function.tags.contains(&Tag::Filesystem)
                    || function.tags.contains(&Tag::Env)
                    || function.tags.contains(&Tag::Network)
                    || function.tags.contains(&Tag::Process))
        })
        .map(|function| Signal {
            kind: SignalKind::IoPresentationMix,
            confidence: Confidence::Low,
            claim: format!(
                "{} mixes presentation formatting with external effects",
                function.name
            ),
            evidence: vec![format!("tags: {:?}", function.tags)],
            symbols: vec![function.symbol_id.clone()],
        })
        .collect()
}

fn conversion_impl_does_io_signals(facts: &ScanFacts) -> Vec<Signal> {
    facts
        .functions
        .iter()
        .filter(|function| !function.is_test)
        .filter(|function| {
            function
                .impl_trait
                .as_deref()
                .is_some_and(conversion_trait_name)
                || conversion_method_name(&function.name)
        })
        .filter(|function| {
            function.tags.contains(&Tag::Filesystem)
                || function.tags.contains(&Tag::Env)
                || function.tags.contains(&Tag::Network)
                || function.tags.contains(&Tag::Process)
                || function.tags.contains(&Tag::Time)
        })
        .map(|function| Signal {
            kind: SignalKind::ConversionImplDoesIo,
            confidence: Confidence::Low,
            claim: format!(
                "{} mixes conversion-style code with external effects",
                function.name
            ),
            evidence: vec![format!("tags: {:?}", function.tags)],
            symbols: vec![function.symbol_id.clone()],
        })
        .collect()
}

fn presentation_in_effectful_function_signals(facts: &ScanFacts) -> Vec<Signal> {
    facts
        .functions
        .iter()
        .filter(|function| !function.is_test)
        .filter(|function| {
            effectful_name(&function.name)
                && (function.tags.contains(&Tag::Formatting)
                    || function.tags.contains(&Tag::Serialization))
        })
        .map(|function| Signal {
            kind: SignalKind::PresentationInEffectfulFunction,
            confidence: Confidence::Low,
            claim: format!(
                "{} has an effectful name but does presentation work",
                function.name
            ),
            evidence: vec![format!("tags: {:?}", function.tags)],
            symbols: vec![function.symbol_id.clone()],
        })
        .collect()
}

fn dto_domain_mix_signals(facts: &ScanFacts) -> Vec<Signal> {
    let mut signals = Vec::new();
    signals.extend(facts.structs.iter().filter_map(|item| {
        let name_terms = boundary_terms(&item.name);
        if name_terms.is_empty() {
            return None;
        }
        let field_terms = item
            .fields
            .iter()
            .filter(|field| domain_field_name(&field.name) || boundary_type(&field.ty))
            .map(|field| format!("{}: {}", field.name, field.ty))
            .collect::<Vec<_>>();
        if field_terms.len() < 2 {
            return None;
        }
        Some(Signal {
            kind: SignalKind::DtoDomainMix,
            confidence: Confidence::Low,
            claim: format!(
                "{} mixes boundary naming with domain-looking fields",
                item.name
            ),
            evidence: field_terms.into_iter().take(8).collect(),
            symbols: vec![item.symbol_id.clone()],
        })
    }));
    signals.extend(facts.functions.iter().filter_map(|function| {
        if function.is_test {
            return None;
        }
        if !boundary_terms(&function.name).is_empty()
            && function.params.iter().any(|param| domain_type(param))
        {
            Some(Signal {
                kind: SignalKind::DtoDomainMix,
                confidence: Confidence::Low,
                claim: format!(
                    "{} mixes boundary naming with domain parameters",
                    function.name
                ),
                evidence: vec![format!("params: {}", function.params.join(", "))],
                symbols: vec![function.symbol_id.clone()],
            })
        } else {
            None
        }
    }));
    signals
}

fn primitive_obsession_signals(facts: &ScanFacts) -> Vec<Signal> {
    facts
        .structs
        .iter()
        .filter_map(|item| {
            let primitive_fields = item
                .fields
                .iter()
                .filter(|field| primitive_domain_field(field))
                .map(|field| format!("{}: {}", field.name, field.ty))
                .collect::<Vec<_>>();
            if primitive_fields.len() >= 3 {
                Some(Signal {
                    kind: SignalKind::PrimitiveObsession,
                    confidence: Confidence::Low,
                    claim: format!(
                        "{} stores several domain-looking values as primitives",
                        item.name
                    ),
                    evidence: primitive_fields.into_iter().take(8).collect(),
                    symbols: vec![item.symbol_id.clone()],
                })
            } else {
                None
            }
        })
        .collect()
}

fn validation_bypass_signals(facts: &ScanFacts) -> Vec<Signal> {
    let functions_by_owner = facts
        .functions
        .iter()
        .filter_map(|function| function.owner.as_ref().map(|owner| (owner, function)))
        .fold(
            BTreeMap::<&str, Vec<&FunctionFact>>::new(),
            |mut by_owner, (owner, function)| {
                by_owner.entry(owner).or_default().push(function);
                by_owner
            },
        );
    facts
        .structs
        .iter()
        .filter(|item| item.derives.contains("Deserialize"))
        .filter_map(|item| {
            let constructors = functions_by_owner
                .get(item.name.as_str())
                .into_iter()
                .flatten()
                .filter(|function| validation_constructor_name(&function.name))
                .map(|function| function.name.clone())
                .collect::<Vec<_>>();
            if constructors.is_empty() {
                return None;
            }
            Some(Signal {
                kind: SignalKind::ValidationBypass,
                confidence: Confidence::Low,
                claim: format!(
                    "{} derives Deserialize and also has validation-looking constructors",
                    item.name
                ),
                evidence: vec![format!("constructors: {}", constructors.join(", "))],
                symbols: vec![item.symbol_id.clone()],
            })
        })
        .collect()
}

fn fan_out_hotspot_signals(facts: &ScanFacts) -> Vec<Signal> {
    facts
        .functions
        .iter()
        .filter(|function| !function.is_test)
        .filter(|function| !is_constructor_like(&function.name))
        .filter_map(|function| {
            let calls = compact_call_sequence(&function.calls);
            if calls.len() >= 10 {
                Some(Signal {
                    kind: SignalKind::FanOutHotspot,
                    confidence: Confidence::Low,
                    claim: format!("{} calls many local operations", function.name),
                    evidence: vec![format!(
                        "{} calls: {}",
                        calls.len(),
                        calls.into_iter().take(12).collect::<Vec<_>>().join(", ")
                    )],
                    symbols: vec![function.symbol_id.clone()],
                })
            } else {
                None
            }
        })
        .collect()
}

fn fn_arg_type(arg: &FnArg) -> Option<String> {
    match arg {
        FnArg::Receiver(receiver) => {
            if receiver.reference.is_some() {
                Some("&self".to_string())
            } else {
                Some("self".to_string())
            }
        },
        FnArg::Typed(pat_type) => Some(type_text(&pat_type.ty)),
    }
}

fn signature_has_receiver(sig: &Signature) -> bool {
    sig.inputs
        .iter()
        .any(|input| matches!(input, FnArg::Receiver(_)))
}

fn primary_subject(param: &String) -> Option<String> {
    if param == "self" || param == "&self" {
        return None;
    }
    let trimmed = param
        .trim_start_matches('&')
        .trim_start_matches("mut ")
        .trim();
    let subject = trimmed
        .split(['<', '[', '(', ' '])
        .next()
        .unwrap_or(trimmed)
        .trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != ':');
    if subject.is_empty() || subject.chars().next().is_some_and(char::is_lowercase) {
        None
    } else {
        Some(subject.to_string())
    }
}

fn expr_subject(expr: &Expr) -> String {
    match expr {
        Expr::Path(path) => path
            .path
            .segments
            .last()
            .map_or_else(|| "_".to_string(), |segment| segment.ident.to_string()),
        Expr::Reference(reference) => expr_subject(&reference.expr),
        Expr::Field(field) => tokens(&field.member),
        Expr::MethodCall(call) => call.method.to_string(),
        _ => "_".to_string(),
    }
}

fn match_arm_subject(arm: &syn::Arm) -> Option<String> {
    let pat = tokens(&arm.pat);
    if pat == "_" {
        return None;
    }
    if let Some((head, _)) = pat.rsplit_once("::") {
        return Some(head.to_string());
    }
    pat.split_whitespace().next().map(str::to_string)
}

fn short_call_name(callee: &str) -> Option<String> {
    if callee.contains('{') || callee.contains(';') || callee.contains('|') {
        return None;
    }
    let name = callee
        .rsplit("::")
        .next()
        .unwrap_or(callee)
        .trim_matches(|ch: char| !ch.is_alphanumeric() && ch != '_' && ch != '!')
        .to_string();
    (!name.is_empty()).then_some(name)
}

fn signature_text(sig: &Signature) -> String {
    let output = match &sig.output {
        ReturnType::Default => String::new(),
        ReturnType::Type(_, ty) => format!(" -> {}", type_text(ty)),
    };
    let params = sig
        .inputs
        .iter()
        .map(|input| match input {
            FnArg::Receiver(receiver) => tokens(receiver),
            FnArg::Typed(pat_type) => {
                format!("{}: {}", pat_text(&pat_type.pat), type_text(&pat_type.ty))
            },
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("fn {}({}){}", sig.ident, params, output)
}

fn pat_text(pat: &Pat) -> String {
    match pat {
        Pat::Ident(ident) => ident.ident.to_string(),
        _ => tokens(pat),
    }
}

fn type_text(ty: &Type) -> String {
    tokens(ty)
}

fn visibility_text(vis: &Visibility) -> String {
    match vis {
        Visibility::Public(_) => "pub".to_string(),
        Visibility::Restricted(_) => tokens(vis),
        Visibility::Inherited => "private".to_string(),
    }
}

fn tokens<T: ToTokens>(value: T) -> String {
    rust_display(
        value
            .into_token_stream()
            .to_string()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn rust_display(text: String) -> String {
    text.replace(" :: ", "::")
        .replace(" < ", "<")
        .replace("< ", "<")
        .replace(" >", ">")
        .replace(" , ", ", ")
        .replace(" : ", ": ")
        .replace("& '", "&'")
        .replace("& [", "&[")
        .replace("& mut ", "&mut ")
        .replace("& ", "&")
        .replace("* const ", "*const ")
        .replace("* mut ", "*mut ")
        .replace(" !", "!")
}

fn source_range(span: Span) -> SourceRange {
    let start = span.start();
    let end = span.end();
    SourceRange {
        start_line: start.line,
        start_col: start.column,
        end_line: end.line,
        end_col: end.column,
    }
}

fn relative_utf8_path(cwd: &Path, path: &Path) -> Result<Utf8PathBuf> {
    let rel = path.strip_prefix(cwd).unwrap_or(path);
    Utf8PathBuf::from_path_buf(rel.to_path_buf())
        .map_err(|path| anyhow::anyhow!("path is not UTF-8: {}", path.display()))
}

fn module_hint(path: &Utf8Path) -> String {
    let stem = path.with_extension("");
    let components = stem
        .components()
        .map(|component| component.as_str())
        .collect::<Vec<_>>();
    let Some(src_index) = components.iter().position(|component| *component == "src") else {
        return components
            .into_iter()
            .filter(|component| !is_module_root_file(component))
            .collect::<Vec<_>>()
            .join("::");
    };
    let crate_name = src_index
        .checked_sub(1)
        .and_then(|index| components.get(index))
        .map_or_else(|| "crate".to_string(), |name| name.replace('-', "_"));
    let mut module = vec![crate_name];
    module.extend(
        components[src_index + 1..]
            .iter()
            .filter(|component| !is_module_root_file(component))
            .map(|component| component.to_string()),
    );
    module.join("::")
}

fn is_module_root_file(component: &str) -> bool {
    matches!(component, "lib" | "main" | "mod")
}

fn render_tree(map: &StructureMap) {
    use owo_colors::OwoColorize as _;

    for file in &map.files {
        anstream::println!("{}", file.path.as_str().cyan().bold());
        for symbol in &file.symbols {
            render_symbol(symbol, 1);
        }
    }
    if !map.signals.is_empty() {
        anstream::println!();
        anstream::println!("{}", "signals".yellow().bold());
        for signal in &map.signals {
            anstream::println!(
                "  {}: {}",
                format!("{:?}", signal.kind).yellow().bold(),
                signal.claim
            );
            for evidence in &signal.evidence {
                anstream::println!("    {} {evidence}", "-".bright_black());
            }
        }
    }
    if !map.recommendations.is_empty() {
        anstream::println!();
        anstream::println!("{}", "recommendations".green().bold());
        for recommendation in &map.recommendations {
            anstream::println!(
                "  {}: {}",
                format!("{:?}", recommendation.kind).green().bold(),
                recommendation.recommendation
            );
        }
    }
}

fn render_symbol(symbol: &Symbol, depth: usize) {
    use owo_colors::OwoColorize as _;

    let indent = "  ".repeat(depth);
    let kind = format!("{:?}", symbol.kind);
    let name = symbol.name.as_str();
    match &symbol.signature {
        Some(signature) if !signature.is_empty() => {
            anstream::println!(
                "{indent}{} {} {}",
                kind.bright_black(),
                name.bold(),
                signature.bright_black()
            );
        },
        _ => anstream::println!("{indent}{} {}", kind.bright_black(), name.bold()),
    }
    for child in &symbol.children {
        render_symbol(child, depth + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_rust_shaped_module_hint_for_crate_src_paths() {
        assert_eq!(
            module_hint(Utf8Path::new("crates/omnifs-cache/src/view.rs")),
            "omnifs_cache::view"
        );
        assert_eq!(
            module_hint(Utf8Path::new("crates/omnifs-cache/src/lib.rs")),
            "omnifs_cache"
        );
    }

    #[test]
    fn ignores_low_value_receiver_subjects() {
        assert!(is_low_value_receiver_subject("Path"));
        assert!(is_low_value_receiver_subject("String"));
        assert!(!is_low_value_receiver_subject("Database"));
    }

    #[test]
    fn extracts_primary_subject_from_reference_type() {
        assert_eq!(
            primary_subject(&"& Database".to_string()),
            Some("Database".to_string())
        );
        assert_eq!(primary_subject(&"& str".to_string()), None);
    }

    #[test]
    fn normalizes_rust_token_spacing_for_display() {
        assert_eq!(
            rust_display("Result < Option < Record > , Error >".to_string()),
            "Result<Option<Record>, Error>"
        );
        assert_eq!(
            rust_display("self : & Arc < Self >".to_string()),
            "self: &Arc<Self>"
        );
    }

    #[test]
    fn namespace_redundancy_requires_compound_type_name() {
        assert_eq!(
            redundant_namespace_component("PathPattern", "omnifs_path"),
            Some("path".to_string())
        );
        assert_eq!(
            redundant_namespace_component("Cache", "omnifs_cache::view"),
            None
        );
    }
}
