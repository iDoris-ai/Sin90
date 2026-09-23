//! T5.1.1 (design DESIGN-LIFEOS.md §11.5) — **J7**: the structural gate that
//! keeps `src/ai/**` from ever writing `sin90.db` directly, a `syn`-based
//! whitelist walk over every file under `src/ai/**/*.rs`. Ported from the
//! frozen design's scratch crate `t501-check/src/boundary.rs` (§11.12) —
//! `check_source` and the 29 positive controls are copied near-verbatim;
//! the file-discovery glue (`ai_boundary` itself) and the H1/M2 fixes below
//! (2026-09-24 Opus review, reproduced against
//! `f50rev/src/tests/probe.rs`'s 46-case catalogue) are new.
//!
//! (J8, the "does the read/write path only touch the tables it's allowed
//! to" behavioural judgement, needs `Sin90Store::pool()` — `pub(crate)`, not
//! visible from an integration test — so it lives as a `#[cfg(test)]` unit
//! test inside `src/store/ai_port.rs` instead, alongside J9 and the
//! read-only-pool tests. This file is ONLY the syn checker.)
//!
//! `syn`/`proc-macro2` are dev-dependencies ONLY (`Cargo.toml`) — they are
//! not in `ai/`'s own allowed-external-crate list (`EXTERN_OK` below), so
//! this checker cannot live inside `src/ai/` without failing its own check;
//! it lives here, in `tests/`, instead.
//!
//! # H1 fixes (2026-09-24 review, 6 categories, 0C/1H/4M accepted)
//!
//! - **(a) bare-root aliasing**: `use std as s;` / `use std::{self as s};` /
//!   `use ::std as s;`, then `s::fs::write(...)` — the old `Locals` visitor
//!   trusted ANY renamed name unconditionally, so aliasing the WHOLE `std`
//!   root let `s::fs::…` slip through the "it's a local name" fallback even
//!   though `std::fs` itself is denied. Fixed: a bare `std`/`core`/`alloc`
//!   root `use` (optionally with a trailing `self`) is rejected outright,
//!   renamed or not (`is_bare_root_crate_use`).
//! - **(b) qualified `include!`**: `std::include!(...)` / `core::include!`
//!   / `::core::include!` — the old check only matched a BARE `include!`
//!   (`m.path.is_ident("include")`, which requires exactly one segment).
//!   Fixed: judge by the macro path's LAST segment.
//! - **(c) `cfg_attr` / attribute argument paths**:
//!   `#[cfg_attr(all(), path = "…")] mod x;` (a `cfg_attr`-produced `path`
//!   the old checker never looked for) and `#[derive(sqlx::FromRow)]` /
//!   `#[sqlx::test]` (a forbidden path living in an attribute's own path or
//!   argument tokens, which `visit_path` never walks — attribute contents
//!   are opaque `TokenStream`s to syn unless something explicitly scans
//!   them). Fixed: `cfg_attr` is banned outright in `ai/`; every
//!   attribute's own (multi-segment) path is checked, and every
//!   `Meta::List`'s argument tokens are run through the same
//!   `scan_tokens` macro-body scanner already used for macro invocations.
//! - **(d) unscanned subdirectories**: `ai_boundary`'s file discovery used
//!   a flat `read_dir`, so a file under `src/ai/<sub>/` would never be
//!   checked at all. Fixed: recursive walk, with module depth computed from
//!   the path (a `mod.rs` is the SAME depth as its parent directory name
//!   would be as a leaf file: `ai/mod.rs`=1, `ai/x.rs`=2, `ai/x/mod.rs`=2,
//!   `ai/x/y.rs`=3); a single-file `src/ai.rs` (if it ever replaced the
//!   `ai/` directory) is included too, at depth 1.
//! - **(e) `extern` blocks**: `extern "C" { fn sqlite3_exec(); }` was never
//!   rejected (no visitor for `ItemForeignMod`) — `ai/` has no legitimate
//!   reason to declare foreign functions. Fixed: rejected outright. Paired
//!   with `#![forbid(unsafe_code)]` on `ai/mod.rs` itself (compiler-enforced
//!   defense in depth — even if a future checker gap let an `extern`/`unsafe`
//!   item slip through, the crate would refuse to build it).
//! - **(M2) local-name scope leaks**: the `Locals` visitor is FLAT — a name
//!   bound anywhere in the file (inside an unrelated inner `mod`, a
//!   function's own generic parameter list, or `#[cfg(test)]` code) becomes
//!   a globally "trusted local" for the WHOLE file, unlike real Rust scoping.
//!   Two targeted defenses, not full scope-awareness (out of scope): (1)
//!   `Locals` now skips `#[cfg(test)]` items too (mirroring `Checker`), so a
//!   test-only alias cannot leak trust into non-test code; (2) any local
//!   name that collides with a REAL Cargo dependency's crate name (read
//!   from `Cargo.toml`'s `[dependencies]`/`[dev-dependencies]`/
//!   `[build-dependencies]`, e.g. `sqlx`, `tokio`, `axum`) is rejected
//!   outright, wherever it is bound — closes the specific "alias a local
//!   name to a real crate so `name::forbidden_thing` reads as trusted"
//!   trick regardless of which scope it's declared in.

use std::collections::HashSet;

use proc_macro2::{TokenStream, TokenTree};
use syn::visit::{self, Visit};

// =========================================================================
// J7 — syn whitelist checker (ported from t501-check/src/boundary.rs)
// =========================================================================

/// External crates `ai/` may name directly.
const EXTERN_OK: &[&str] = &[
    "std",
    "core",
    "alloc",
    "serde",
    "serde_json",
    "thiserror",
    "tracing",
    "sha2",
    "hex",
];
/// Prelude items and primitive types that may root a path (`String::new`, `u32::MAX`).
const PRELUDE: &[&str] = &[
    "String",
    "Vec",
    "Box",
    "Option",
    "Result",
    "Some",
    "None",
    "Ok",
    "Err",
    "Default",
    "Iterator",
    "IntoIterator",
    "ToString",
    "ToOwned",
    "Clone",
    "From",
    "Into",
    "AsRef",
    "PartialEq",
    "Eq",
    "PartialOrd",
    "Ord",
    "FromIterator",
    "TryFrom",
    "TryInto",
    "u8",
    "u16",
    "u32",
    "u64",
    "u128",
    "usize",
    "i8",
    "i16",
    "i32",
    "i64",
    "i128",
    "isize",
    "f32",
    "f64",
    "bool",
    "char",
    "str",
];
/// Modules of `std`/`core`/`alloc` the ai module may NOT touch (v2.1 M5).
const STD_DENY: &[&str] = &["fs", "process", "net", "os"];
/// Second segment allowed after `crate::` (or after enough `super::` to reach the crate root).
const CRATE_OK: &[&str] = &["core", "ai"];

/// H1(a): a `use` path that is EXACTLY a root crate (`std`/`core`/`alloc`),
/// optionally with a trailing `self` (`use std::{self as s};` expands to
/// `["std", "self"]`) — renamed or not. The only reason to write this is to
/// stash an alias that later reads as a "local name" to the checker, which
/// completely defeats `STD_DENY`.
fn is_bare_root_crate_use(segs: &[String]) -> bool {
    let is_root = |s: &str| matches!(s, "std" | "core" | "alloc");
    match segs {
        [a] => is_root(a),
        [a, b] if b == "self" => is_root(a),
        _ => false,
    }
}

/// M2: read `[dependencies]` / `[dev-dependencies]` / `[build-dependencies]`
/// crate names straight out of `Cargo.toml` (no toml crate needed — one
/// name per line in this manifest, `name = ...`). Hyphens are normalized to
/// underscores (Rust path segments can never contain `-`).
fn cargo_dependency_names() -> HashSet<String> {
    const CARGO_TOML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"));
    let mut names = HashSet::new();
    let mut in_deps = false;
    for raw in CARGO_TOML.lines() {
        let line = raw.trim();
        if let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            in_deps = matches!(
                section,
                "dependencies" | "dev-dependencies" | "build-dependencies"
            );
            continue;
        }
        if !in_deps || line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((name, _)) = line.split_once('=') {
            let name = name.trim().trim_matches('"');
            if !name.is_empty() {
                names.insert(name.replace('-', "_"));
            }
        }
    }
    names
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Violation(String);

struct Locals(HashSet<String>);

impl<'ast> Visit<'ast> for Locals {
    fn visit_item(&mut self, i: &'ast syn::Item) {
        // M2: a name bound only inside `#[cfg(test)]` must not become a
        // globally trusted local for the rest of the file — mirrors
        // `Checker::visit_item`'s own skip.
        if is_cfg_test(item_attrs(i)) {
            return;
        }
        let id = match i {
            syn::Item::Fn(x) => Some(&x.sig.ident),
            syn::Item::Struct(x) => Some(&x.ident),
            syn::Item::Enum(x) => Some(&x.ident),
            syn::Item::Mod(x) => Some(&x.ident),
            syn::Item::Trait(x) => Some(&x.ident),
            syn::Item::Type(x) => Some(&x.ident),
            syn::Item::Const(x) => Some(&x.ident),
            syn::Item::Static(x) => Some(&x.ident),
            syn::Item::Union(x) => Some(&x.ident),
            _ => None,
        };
        if let Some(id) = id {
            self.0.insert(id.to_string());
        }
        visit::visit_item(self, i);
    }
    fn visit_item_use(&mut self, u: &'ast syn::ItemUse) {
        bind_names(&u.tree, &mut self.0);
    }
    fn visit_generic_param(&mut self, g: &'ast syn::GenericParam) {
        if let syn::GenericParam::Type(t) = g {
            self.0.insert(t.ident.to_string());
        }
        visit::visit_generic_param(self, g);
    }
}

fn bind_names(t: &syn::UseTree, out: &mut HashSet<String>) {
    match t {
        syn::UseTree::Path(p) => bind_names(&p.tree, out),
        syn::UseTree::Name(n) => {
            out.insert(n.ident.to_string());
        }
        syn::UseTree::Rename(r) => {
            out.insert(r.rename.to_string());
        }
        syn::UseTree::Glob(_) => {}
        syn::UseTree::Group(g) => g.items.iter().for_each(|i| bind_names(i, out)),
    }
}

fn expand(t: &syn::UseTree, prefix: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
    match t {
        syn::UseTree::Path(p) => {
            prefix.push(p.ident.to_string());
            expand(&p.tree, prefix, out);
            prefix.pop();
        }
        syn::UseTree::Name(n) => {
            let mut v = prefix.clone();
            v.push(n.ident.to_string());
            out.push(v);
        }
        syn::UseTree::Rename(r) => {
            let mut v = prefix.clone();
            v.push(r.ident.to_string());
            out.push(v);
        }
        syn::UseTree::Glob(_) => out.push(prefix.clone()),
        syn::UseTree::Group(g) => g.items.iter().for_each(|i| expand(i, prefix, out)),
    }
}

struct Checker<'l> {
    depth: usize,
    locals: &'l HashSet<String>,
    errs: Vec<Violation>,
}

impl Checker<'_> {
    /// `in_use`: a `use` path's root may not be a local name.
    fn check(&mut self, segs_in: &[String], leading_colon: bool, in_use: bool, what: &str) {
        // v2.1 H2: `self::super::super::…` — drop leading `self` first, then
        // judge the `super` count against the depth.
        let lead_self = segs_in.iter().take_while(|s| *s == "self").count();
        let segs = if lead_self > 0 && segs_in.get(lead_self).map(String::as_str) == Some("super") {
            &segs_in[lead_self..]
        } else {
            segs_in
        };
        let Some(first) = segs.first() else { return };
        let bad = |c: &mut Self, why: &str| {
            c.errs
                .push(Violation(format!("{what} `{}`: {why}", segs.join("::"))))
        };
        // H1(a): bare `std`/`core`/`alloc` root `use`, renamed or not.
        if in_use && is_bare_root_crate_use(segs) {
            bad(
                self,
                "bare std/core/alloc root use not allowed (would alias around the fs/process/net/os deny)",
            );
            return;
        }
        if leading_colon {
            if !EXTERN_OK.contains(&first.as_str()) {
                bad(self, "external crate not allowed");
            } else if segs.get(1).is_some_and(|m| STD_DENY.contains(&m.as_str())) {
                bad(self, "std::fs/process/net/os not allowed in ai");
            }
            return;
        }
        match first.as_str() {
            "crate" | "sin90" => match segs.get(1) {
                Some(s) if CRATE_OK.contains(&s.as_str()) => {}
                _ => bad(self, "crate path must go through core or ai"),
            },
            "self" | "Self" => {}
            "super" => {
                let k = segs.iter().take_while(|s| *s == "super").count();
                if k > self.depth {
                    bad(self, "super above crate root");
                } else if k == self.depth {
                    match segs.get(k) {
                        Some(s) if CRATE_OK.contains(&s.as_str()) => {}
                        _ => bad(self, "super reaches crate root; next must be core or ai"),
                    }
                }
            }
            "std" | "core" | "alloc"
                if segs.get(1).is_some_and(|m| STD_DENY.contains(&m.as_str())) =>
            {
                bad(self, "std::fs/process/net/os not allowed in ai")
            }
            f if EXTERN_OK.contains(&f) => {}
            f if !in_use && PRELUDE.contains(&f) => {}
            f if !in_use && (segs.len() == 1 || self.locals.contains(f)) => {}
            _ => bad(self, "unknown path root"),
        }
    }

    fn scan_tokens(&mut self, ts: TokenStream) {
        let toks: Vec<TokenTree> = ts.into_iter().collect();
        let mut i = 0;
        while i < toks.len() {
            match &toks[i] {
                TokenTree::Group(g) => {
                    self.scan_tokens(g.stream());
                    i += 1;
                }
                TokenTree::Ident(id) => {
                    let mut segs = vec![id.to_string()];
                    let mut j = i + 1;
                    while j + 2 < toks.len() + 1 {
                        match (&toks.get(j), &toks.get(j + 1), &toks.get(j + 2)) {
                            (
                                Some(TokenTree::Punct(a)),
                                Some(TokenTree::Punct(b)),
                                Some(TokenTree::Ident(n)),
                            ) if a.as_char() == ':' && b.as_char() == ':' => {
                                segs.push(n.to_string());
                                j += 3;
                            }
                            _ => break,
                        }
                    }
                    if segs.len() > 1 {
                        self.check(&segs, false, false, "macro token path");
                    }
                    i = j;
                }
                _ => i += 1,
            }
        }
    }
}

/// v2.1 M4 (decided): items carrying exactly `#[cfg(test)]` are NOT
/// checked — unit tests inside `src/ai/` may build fixtures with the real
/// store. `cfg(any(test, …))` and every other cfg ARE checked.
fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("cfg")
            && a.parse_args::<syn::Ident>()
                .map(|i| i == "test")
                .unwrap_or(false)
    })
}

fn item_attrs(i: &syn::Item) -> &[syn::Attribute] {
    match i {
        syn::Item::Const(x) => &x.attrs,
        syn::Item::Enum(x) => &x.attrs,
        syn::Item::ExternCrate(x) => &x.attrs,
        syn::Item::Fn(x) => &x.attrs,
        syn::Item::ForeignMod(x) => &x.attrs,
        syn::Item::Impl(x) => &x.attrs,
        syn::Item::Macro(x) => &x.attrs,
        syn::Item::Mod(x) => &x.attrs,
        syn::Item::Static(x) => &x.attrs,
        syn::Item::Struct(x) => &x.attrs,
        syn::Item::Trait(x) => &x.attrs,
        syn::Item::TraitAlias(x) => &x.attrs,
        syn::Item::Type(x) => &x.attrs,
        syn::Item::Union(x) => &x.attrs,
        syn::Item::Use(x) => &x.attrs,
        _ => &[],
    }
}

impl<'ast> Visit<'ast> for Checker<'_> {
    fn visit_item(&mut self, i: &'ast syn::Item) {
        if is_cfg_test(item_attrs(i)) {
            return;
        }
        visit::visit_item(self, i);
    }
    fn visit_item_use(&mut self, u: &'ast syn::ItemUse) {
        let mut out = Vec::new();
        expand(&u.tree, &mut Vec::new(), &mut out);
        for p in out {
            self.check(&p, u.leading_colon.is_some(), true, "use");
        }
    }
    fn visit_item_extern_crate(&mut self, e: &'ast syn::ItemExternCrate) {
        self.errs
            .push(Violation(format!("extern crate `{}` not allowed", e.ident)));
    }
    /// H1(e): `extern "C" { ... }` — no legitimate use in `ai/`, and it is
    /// the one place `unsafe` FFI declarations could hide. Paired with
    /// `#![forbid(unsafe_code)]` on `ai/mod.rs` itself.
    fn visit_item_foreign_mod(&mut self, f: &'ast syn::ItemForeignMod) {
        let _ = f;
        self.errs.push(Violation("extern block not allowed".into()));
    }
    fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
        if m.attrs.iter().any(|a| a.path().is_ident("path")) {
            self.errs.push(Violation(format!(
                "#[path] on mod `{}` not allowed",
                m.ident
            )));
        }
        self.depth += 1;
        visit::visit_item_mod(self, m);
        self.depth -= 1;
    }
    fn visit_path(&mut self, p: &'ast syn::Path) {
        let segs: Vec<String> = p.segments.iter().map(|s| s.ident.to_string()).collect();
        self.check(&segs, p.leading_colon.is_some(), false, "path");
        visit::visit_path(self, p);
    }
    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        // H1(b): judge by the macro path's LAST segment, not
        // `is_ident("include")` (which only matches a BARE `include!`,
        // missing `std::include!`/`core::include!`/`::core::include!`).
        if m.path.segments.last().is_some_and(|s| s.ident == "include") {
            self.errs.push(Violation("include! not allowed".into()));
        }
        self.scan_tokens(m.tokens.clone());
        visit::visit_macro(self, m);
    }
    /// H1(c): `cfg_attr` is banned outright in `ai/` (it can produce a
    /// `#[path = "…"]` — or anything else — that the checker would never
    /// see literally); every attribute's own (multi-segment) path is
    /// checked like any other path (`#[sqlx::test]`); and every
    /// `Meta::List`'s argument tokens (`#[derive(sqlx::FromRow)]`,
    /// `#[serde(...)]`, …) are scanned the same way macro-invocation bodies
    /// already are.
    fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
        let path = attr.path();
        if path.is_ident("cfg_attr") {
            self.errs
                .push(Violation("cfg_attr not allowed in ai".into()));
        }
        let segs: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        if segs.len() > 1 {
            self.check(&segs, path.leading_colon.is_some(), false, "attribute path");
        }
        // Lint-control attributes (`#[allow(clippy::foo)]`, `#[warn(dead_code)]`,
        // …) hold LINT names, not Rust value/type paths — `clippy::foo`,
        // `rustdoc::broken_intra_doc_links` etc. can never resolve to
        // `crate::store` and are not something `scan_tokens` should judge.
        // Every other `Meta::List` (`#[derive(sqlx::FromRow)]`,
        // `#[serde(deserialize_with = ...)]`, …) IS scanned — H1(c).
        const LINT_CONTROL: &[&str] = &["allow", "warn", "deny", "forbid"];
        let is_lint_control = segs.len() == 1 && LINT_CONTROL.contains(&segs[0].as_str());
        if !is_lint_control {
            if let syn::Meta::List(list) = &attr.meta {
                self.scan_tokens(list.tokens.clone());
            }
        }
        visit::visit_attribute(self, attr);
    }
}

/// `module_depth`: 1 for `src/ai/mod.rs`, 2 for `src/ai/ports.rs`, …
fn check_source(src: &str, module_depth: usize) -> Result<(), Vec<Violation>> {
    let file = syn::parse_file(src).map_err(|e| vec![Violation(format!("parse: {e}"))])?;
    let mut locals = Locals(HashSet::new());
    locals.visit_file(&file);

    // M2: a local name that collides with a REAL Cargo dependency name is
    // rejected outright, wherever in the file it was bound — closes the
    // "alias a name to a real crate so `name::forbidden` reads as trusted"
    // trick regardless of which (unscoped, flat) part of the file it came
    // from.
    let dep_names = cargo_dependency_names();
    let mut errs: Vec<Violation> = locals
        .0
        .iter()
        .filter(|name| dep_names.contains(*name))
        .map(|name| {
            Violation(format!(
                "local name `{name}` shadows a real Cargo dependency name — not allowed"
            ))
        })
        .collect();

    let mut c = Checker {
        depth: module_depth,
        locals: &locals.0,
        errs: std::mem::take(&mut errs),
    };
    c.visit_file(&file);
    if c.errs.is_empty() {
        Ok(())
    } else {
        Err(c.errs)
    }
}

/// H1(d): `mod.rs` sits at the SAME module depth as its parent directory
/// would be as a leaf file (`ai/mod.rs` = 1, `ai/x/mod.rs` = 2, matching
/// `ai/x.rs` = 2); anything else is one deeper than its path-component
/// count would otherwise suggest is needed (`ai/x/y.rs` = 3). `rel` is a
/// path relative to `src/` (first component is always `ai`).
fn module_depth_for(rel: &std::path::Path) -> usize {
    let stem = rel.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    let n = rel.components().count();
    if stem == "mod" {
        n - 1
    } else {
        n
    }
}

/// H1(d): recursively walk `src/ai/`, plus `src/ai.rs` if that single-file
/// form is ever used instead of (or alongside) the `ai/` directory — the
/// old flat `read_dir` never descended into subdirectories at all.
fn discover_ai_files(src_root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let ai_dir = src_root.join("ai");
    if ai_dir.is_dir() {
        walk_dir(&ai_dir, &mut out);
    }
    let ai_single_file = src_root.join("ai.rs");
    if ai_single_file.is_file() {
        out.push(ai_single_file);
    }
    out
}

fn walk_dir(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk_dir(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Walk `src/ai/**/*.rs` (recursively, H1(d)) from the crate root and
/// assert every file passes.
#[test]
fn ai_boundary() {
    let src_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = discover_ai_files(&src_root);
    files.sort();
    assert!(!files.is_empty(), "no files found under {src_root:?}/ai");

    let mut failures = Vec::new();
    for path in &files {
        let src = std::fs::read_to_string(path).unwrap();
        let rel = path.strip_prefix(&src_root).unwrap();
        let depth = if rel == std::path::Path::new("ai.rs") {
            1
        } else {
            module_depth_for(rel)
        };
        if let Err(violations) = check_source(&src, depth) {
            failures.push(format!("{}: {violations:?}", path.display()));
        }
    }
    assert!(
        failures.is_empty(),
        "src/ai/**/*.rs violated the boundary whitelist:\n{}",
        failures.join("\n")
    );
}

/// H1(d) positive control: a synthetic nested tree under a temp dir proves
/// (1) the walker actually recurses (finds all 3 files, not just the top
/// one) and (2) `module_depth_for` computes the right depth for a doubly
/// nested file — a violation embedded in `ai/sub/leaf.rs` is only caught if
/// BOTH hold (wrong depth would make the `super::super` arithmetic land on
/// a different, wrongly-permissive branch).
#[test]
fn ai_boundary_h1d_walker_recurses_into_subdirectories() {
    let tmp = std::env::temp_dir().join(format!(
        "ai_boundary_walk_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("ai/sub")).unwrap();
    std::fs::write(tmp.join("ai/mod.rs"), "pub mod sub;\n").unwrap();
    std::fs::write(tmp.join("ai/sub/mod.rs"), "pub mod leaf;\n").unwrap();
    std::fs::write(
        tmp.join("ai/sub/leaf.rs"),
        "pub fn go() { let _ = super::super::super::store::X; }\n",
    )
    .unwrap();

    let files = discover_ai_files(&tmp);
    assert_eq!(files.len(), 3, "must find mod.rs, sub/mod.rs, sub/leaf.rs");

    let mut saw_leaf = false;
    for path in &files {
        let src = std::fs::read_to_string(path).unwrap();
        let rel = path.strip_prefix(&tmp).unwrap();
        let depth = module_depth_for(rel);
        if rel == std::path::Path::new("ai/sub/leaf.rs") {
            assert_eq!(depth, 3, "ai/sub/leaf.rs must be module depth 3");
            assert!(
                check_source(&src, depth).is_err(),
                "the embedded super::super::store violation must trip at the correct depth"
            );
            saw_leaf = true;
        } else {
            assert_eq!(check_source(&src, depth), Ok(()));
        }
    }
    assert!(saw_leaf, "the walker must have visited ai/sub/leaf.rs");

    // Positive control: same tree, but leaf.rs's `super::super` chain
    // reaches `core` (allowed) instead of `store` — must pass.
    std::fs::write(
        tmp.join("ai/sub/leaf.rs"),
        "pub fn go() { let _ = super::super::super::core::X; }\n",
    )
    .unwrap();
    let src = std::fs::read_to_string(tmp.join("ai/sub/leaf.rs")).unwrap();
    assert_eq!(check_source(&src, 3), Ok(()));

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn ai_boundary_checker_clean_source_passes() {
    const CLEAN: &str = r#"
        use crate::core::{Sin90Op, validate};
        use super::ports::{AiSink, ModelPort};
        use super::super::ai::ladder;
        use std::collections::HashMap;
        // crate::store::Sin90Store in a comment is fine
        pub fn f<T: AiSink>(t: &T) -> String {
            let _ = Sin90Op::CreateArea { title: String::new() };
            let _ = HashMap::<u8, u8>::new();
            let _ = T::default_hint;
            let _ = ladder::plan;
            let u = "http://x crate::store::Sin90Store";
            format!("{u} {:?}", Sin90Op::CreateArea { title: String::new() })
        }
    "#;
    assert_eq!(check_source(CLEAN, 2), Ok(()));
    assert_eq!(check_source("use self::super::ports::AiSink;", 2), Ok(()));
    assert_eq!(
        check_source(
            "pub fn f() { tracing::warn!(\"x\"); let _ = std::time::Duration::from_secs(1); }",
            2
        ),
        Ok(())
    );
}

#[test]
fn ai_boundary_checker_skips_exactly_cfg_test() {
    let src =
        "#[cfg(test)] mod tests { use crate::store::Sin90Store; use tokio::runtime::Runtime; }";
    assert_eq!(check_source(src, 2), Ok(()));
}

/// H1(a): `use std as s;` / `use std::{self as s};` / `use ::std as s;`,
/// later `s::fs::write(...)`. Mutation target: delete the
/// `is_bare_root_crate_use` guard in `Checker::check` and all three go from
/// `is_err()` to passing.
#[test]
fn ai_boundary_h1a_bare_root_alias_rejected() {
    for src in [
        "use std as s; pub fn go(){ let _ = s::fs::write(\"x\", b\"\"); }",
        "use std::{self as s}; pub fn go(){ let _ = s::fs::write(\"x\", b\"\"); }",
        "use ::std as s; pub fn go(){ let _ = s::fs::write(\"x\", b\"\"); }",
    ] {
        assert!(check_source(src, 2).is_err(), "must reject: {src}");
    }
    // Positive control: a normal, non-aliasing `use std::...` still passes.
    assert_eq!(
        check_source(
            "use std::collections::HashMap; pub fn go() { let _ = HashMap::<u8,u8>::new(); }",
            2
        ),
        Ok(())
    );
}

/// H1(b): `std::include!`/`core::include!`/`::core::include!` — a
/// qualified macro path used to slip past the bare-`include!`-only check.
/// Mutation target: revert `visit_macro` to `m.path.is_ident("include")`
/// and all three go from `is_err()` to passing.
#[test]
fn ai_boundary_h1b_qualified_include_rejected() {
    for src in [
        "pub fn go() { std::include!(\"../store/repo.rs\"); }",
        "pub fn go() { core::include!(\"../store/repo.rs\"); }",
        "::core::include!(\"../store/ai_port.rs\");",
    ] {
        assert!(check_source(src, 2).is_err(), "must reject: {src}");
    }
    // Positive control: bare `include!` was already rejected before this fix.
    assert!(check_source("pub fn go() { include!(\"../store/repo.rs\"); }", 2).is_err());
}

/// H1(c): `cfg_attr` and forbidden paths hiding in an attribute's own path
/// or its `Meta::List` argument tokens. Mutation target: remove
/// `Checker::visit_attribute` entirely and all three go from `is_err()` to
/// passing.
#[test]
fn ai_boundary_h1c_cfg_attr_and_attribute_paths_rejected() {
    assert!(check_source(r#"#[cfg_attr(all(), path = "../store/repo.rs")] mod r;"#, 2).is_err());
    assert!(check_source("#[derive(sqlx::FromRow)] pub struct A { x: i64 }", 2).is_err());
    assert!(check_source("#[sqlx::test] async fn t() {}", 2).is_err());
    // Positive control: an ordinary, allowed derive still passes.
    assert_eq!(
        check_source(
            "#[derive(Debug, Clone, serde::Serialize)] pub struct A { x: i64 }",
            2
        ),
        Ok(())
    );
}

/// H1(e): `extern "C" { ... }` blocks. Mutation target: remove
/// `Checker::visit_item_foreign_mod` and this goes from `is_err()` to
/// passing.
#[test]
fn ai_boundary_h1e_extern_block_rejected() {
    assert!(check_source(r#"extern "C" { fn sqlite3_exec(); }"#, 2).is_err());
}

/// M2 (defense 1 of 2): a local name that collides with a REAL Cargo
/// dependency name (`sqlx`) is rejected wherever it is bound — an unrelated
/// inner `mod`, or a function's own generic parameter — even though real
/// Rust scoping would never let the outer code see either. Mutation target:
/// remove the `cargo_dependency_names` pre-check in `check_source` and both
/// go from `is_err()` to passing.
#[test]
fn ai_boundary_m2_local_name_shadowing_cargo_dependency_rejected() {
    for src in [
        "mod inner { use crate::core as sqlx; }\npub async fn go() { let _ = sqlx::SqlitePool::connect(\"sqlite://sin90.db\"); }",
        "fn a<sqlx>() {}\npub async fn go() { let _ = sqlx::SqlitePool::connect(\"x\"); }",
    ] {
        assert!(check_source(src, 2).is_err(), "must reject: {src}");
    }
    // Positive control: an ordinary generic param name (not a crate name) is fine.
    assert_eq!(
        check_source("fn a<T>() -> T { unimplemented!() }", 2),
        Ok(())
    );
}

/// M2 (defense 2 of 2): a name bound ONLY inside `#[cfg(test)]` must not
/// leak into a trusted local for code OUTSIDE that block. Mutation target:
/// revert `Locals::visit_item`'s `is_cfg_test` skip and this goes from
/// `is_err()` to passing.
#[test]
fn ai_boundary_m2_cfg_test_locals_do_not_leak_outside_test() {
    let leaks = "#[cfg(test)] mod tests { use crate::store::Sin90Store as helper; }\npub fn go() { let _ = helper::open_memory; }";
    assert!(check_source(leaks, 2).is_err());
    // Positive control: the SAME alias, used INSIDE the cfg(test) block
    // itself, is fine — cfg(test) content is exempt from the check entirely.
    let inside = "#[cfg(test)] mod tests { use crate::store::Sin90Store as helper; fn f() { let _ = helper::open_memory; } }";
    assert_eq!(check_source(inside, 2), Ok(()));
}

/// The 29 positive controls from the frozen design's scratch checker (§11.5,
/// H2's `self`/`super` fix, M4's `cfg(test)`/`cfg(any(test,…))` split, M5's
/// `std::{fs,process,net,os}` deny) — each MUST trip the checker. Mutation
/// target: comment out any one arm of `Checker::check` (e.g. the `STD_DENY`
/// branch) and one of these goes from `is_err()` to passing, which this test
/// catches.
#[test]
fn ai_boundary_checker_positive_controls_each_trip() {
    let wrap = |body: &str| format!("pub fn go() {{ {body} }}");
    let cases: Vec<(String, usize)> = vec![
        ("use crate::{core::Sin90Op, http::Sin90State};".into(), 2),
        ("use crate::{core::Sin90Op, store::StoreError};".into(), 2),
        ("use crate::{store};".into(), 2),
        ("use super::super::{store as s};".into(), 2),
        ("use super::store;".into(), 1),
        ("use super::super::super::core;".into(), 2),
        (wrap(r#"let u = "http://x"; use crate::store::Sin90Store;"#), 2),
        (
            "use crate::{core::Sin90Op, http::Sin90State};\npub async fn go(s: &Sin90State) { let _ = s.store.update_review_body(\"r\", \"b\").await; }".into(),
            2,
        ),
        ("pub fn go(s: &crate::http::Sin90State) {}".into(), 2),
        (wrap(r#"let _ = sqlx::query("UPDATE x");"#), 2),
        (wrap(r#"let _ = ::sqlx::query("UPDATE x");"#), 2),
        (
            wrap(r#"let _ = format!("{:?}", crate::store::StoreError::Internal(String::new()));"#),
            2,
        ),
        ("#[path = \"../store/repo.rs\"] mod r;".into(), 2),
        (wrap(r#"include!("../store/repo.rs");"#), 2),
        ("extern crate sqlx;".into(), 2),
        ("use sin90::store::Sin90Store;".into(), 2),
        ("use axum::Router;".into(), 2),
        ("mod inner { use super::super::store; }".into(), 1),
        ("use self::super::super::store::Sin90Store;".into(), 2),
        (
            wrap("let _ = self::super::super::store::Sin90Store::open_memory;"),
            2,
        ),
        (wrap(r#"let _ = std::fs::write("sin90.db", b"");"#), 2),
        (
            wrap(r#"let _ = std::process::Command::new("sqlite3");"#),
            2,
        ),
        ("use std::net::TcpStream;".into(), 2),
        ("use std::os::unix::net::UnixStream;".into(), 2),
        (wrap(r#"let _ = ::std::fs::remove_file("x");"#), 2),
        (
            "macro_rules! p { () => { $crate::store::Sin90Store::open_memory }; }".into(),
            2,
        ),
        ("use crate::r#store::Sin90Store;".into(), 2),
        (
            wrap("let _ = <crate::store::Sin90Store as Clone>::clone;"),
            2,
        ),
        (
            "#[cfg(any(test, feature = \"x\"))] mod t { use crate::store::Sin90Store; }".into(),
            2,
        ),
    ];
    assert_eq!(cases.len(), 29, "expected exactly 29 positive controls");
    for (src, depth) in cases {
        assert!(
            check_source(&src, depth).is_err(),
            "should trip at depth {depth}: {src}"
        );
    }
}
