//! T5.1.1 (design DESIGN-LIFEOS.md §11.5) — **J7**: the structural gate that
//! keeps `src/ai/**` from ever writing `sin90.db` directly, a `syn`-based
//! whitelist walk over every file under `src/ai/**/*.rs`. Ported from the
//! frozen design's scratch crate `t501-check/src/boundary.rs` (§11.12) —
//! `check_source` and the 29 positive controls are copied near-verbatim;
//! only the file-discovery glue (`ai_boundary` itself) is new.
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct Violation(String);

struct Locals(HashSet<String>);

impl<'ast> Visit<'ast> for Locals {
    fn visit_item_use(&mut self, u: &'ast syn::ItemUse) {
        bind_names(&u.tree, &mut self.0);
    }
    fn visit_item(&mut self, i: &'ast syn::Item) {
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
        if m.path.is_ident("include") {
            self.errs.push(Violation("include! not allowed".into()));
        }
        self.scan_tokens(m.tokens.clone());
        visit::visit_macro(self, m);
    }
}

/// `module_depth`: 1 for `src/ai/mod.rs`, 2 for `src/ai/ports.rs`, …
fn check_source(src: &str, module_depth: usize) -> Result<(), Vec<Violation>> {
    let file = syn::parse_file(src).map_err(|e| vec![Violation(format!("parse: {e}"))])?;
    let mut locals = Locals(HashSet::new());
    locals.visit_file(&file);
    let mut c = Checker {
        depth: module_depth,
        locals: &locals.0,
        errs: Vec::new(),
    };
    c.visit_file(&file);
    if c.errs.is_empty() {
        Ok(())
    } else {
        Err(c.errs)
    }
}

/// Walk `src/ai/**/*.rs` from the crate root and assert every file passes.
/// `src/ai/mod.rs` is depth 1; every other file directly under `src/ai/` is
/// depth 2 (none of this branch's `ai/` files declare an inline `mod`, so no
/// file needs depth 3+ yet — the checker itself still tracks nested `mod`
/// via `visit_item_mod`, see `Checker::visit_item_mod`).
#[test]
fn ai_boundary() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ai");
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no files found under {root:?}");

    let mut failures = Vec::new();
    for path in &files {
        let src = std::fs::read_to_string(path).unwrap();
        let depth = if path.file_name().unwrap() == "mod.rs" {
            1
        } else {
            2
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
