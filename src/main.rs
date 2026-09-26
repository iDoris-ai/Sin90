//! Sin90 binary entry point.
//!
//! Default mode: spawned by Agent24 as an out-of-process module — reads the
//! `A24_*` env vars (design §5.2/§1.6) and runs the real handshake.
//!
//! `--standalone`: skips `adapter_agent24` entirely, binds its own port, and
//! uses a event sink that drops everything. Development/test tool only — not
//! the shipped form (design §5.2), not documented in the README's "how to
//! use" section.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use sin90::adapter_agent24::clients::SchedulerClient;
use sin90::adapter_agent24::reconciler;
use sin90::adapter_agent24::{
    listener_from_fd, wire_kernel_clients, FatalHook, KernelClients, SpawnEnv,
};
use sin90::http::{router, ActorKeys, NullEventSink, Sin90State};
use sin90::store::Sin90Store;

/// The manifest bytes sent to the kernel at `initialize` — `sin90::ai::
/// MANIFEST_YAML`'s bytes, NOT a second, independent `include_bytes!` of
/// this crate's own (2026-09-26 review H1). Before this fix, `main.rs` had
/// its own always-`domain-os.yml` `include_bytes!` here while `ai::
/// MANIFEST_YAML` switched with the `remote-allowed-manifest` feature —
/// two embeds of "the manifest" that could disagree: a mismatched
/// binary/yml pairing (package-B binary next to the official
/// `domain-os.yml`, or vice versa) would send bytes that describe ONE
/// manifest while `ai::MODEL_ACCESS` silently believed the OTHER, and
/// nothing would ever notice — J23b's whole point (catching that mismatch)
/// was defeated by construction. Now there is exactly one embed: a
/// mismatched pairing sends bytes that don't match what this binary's own
/// `include_str!` picked, but that mismatch can only happen between the
/// project's two YAML FILES on disk (a 2026-09-26 review round-2 regression
/// test guards that — `ai::ports::manifest_tests::manifest_yamls_agree_
/// outside_comments_and_model_access`, not a frozen-design § reference) —
/// the digest sent here and the text `ai::MODEL_ACCESS` parses are now
/// PROVABLY the same bytes (pinned by this file's own `tests::
/// manifest_sent_to_the_kernel_is_the_same_text_model_access_parses` below),
/// so a genuinely wrong package (built with the wrong feature for the yml
/// it ships next to) now fails the kernel's own `manifest_digest` check at
/// handshake (`manifest_mismatch`) instead of silently mounting.
const MANIFEST: &[u8] = sin90::ai::MANIFEST_YAML.as_bytes();

#[derive(Parser)]
#[command(name = "sin90")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Run as an out-of-process Agent24 module (the default if no subcommand
    /// is given — Agent24 spawns the bare binary, not `sin90 module`).
    Module,
    /// Development/test only: run standalone on `--port`, no Agent24, no
    /// persistence of events (they are dropped). NOT the shipped form.
    Serve {
        #[arg(long, default_value_t = 8099)]
        port: u16,
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// T5.1.2 (design §11.3.2, J23b): prints this binary's compiled-in
    /// `ai::MODEL_ACCESS` (`local_only` or `remote_allowed`) and exits — a
    /// packaged install's own manifest text can be parsed the same way and
    /// compared against this output to catch a binary/manifest mismatch
    /// (e.g. a test-package-B binary shipped next to the official
    /// `domain-os.yml`, or vice versa).
    PrintModelAccess,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    match cli.command {
        Some(Command::Serve { port, data_dir }) => run_standalone(port, data_dir).await,
        Some(Command::PrintModelAccess) => {
            println!("{}", sin90::ai::MODEL_ACCESS.as_str());
            Ok(())
        }
        Some(Command::Module) | None => run_as_agent24_module().await,
    }
}

async fn run_standalone(
    port: u16,
    data_dir: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!(
        "sin90: --standalone mode — development/test only, this is NOT how a real \
         install runs (see README). No Agent24, events are dropped."
    );
    // Without --data-dir there is nowhere to persist keys, so they must come
    // from SIN90_HUMAN_KEY/SIN90_AUTOMATION_KEY (`ActorKeys::load` says so).
    let keys = ActorKeys::load(data_dir.as_deref())?;
    let store = match data_dir {
        Some(dir) => Sin90Store::open(&dir.join("sin90.db")).await?,
        None => Sin90Store::open_memory().await?,
    };
    let state = Sin90State::new(store, Arc::new(NullEventSink), keys);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!(port, "sin90: standalone listening");
    // `mounted = false`: standalone has no Agent24 proxy in front of it to
    // strip client-forged `X-A24-*` headers, so `POST /_a24/scheduler/fired`
    // must not exist here at all (router()'s doc, T3.2.2, architecture.md #4).
    axum::serve(listener, router(state, false)).await?;
    Ok(())
}

async fn run_as_agent24_module() -> Result<(), Box<dyn std::error::Error>> {
    let env = SpawnEnv::from_env()?;
    // Before the handshake: a bad key file must stop the module outright, not
    // leave a mounted module whose every write route answers 403.
    let keys = ActorKeys::load(Some(&env.data_dir))?;

    // T3.2.0/H3: the kernel serves exactly one callback connection per
    // generation and never offers a second one (user decision D1,
    // architecture.md §5) — so once `Transport` decides this connection is
    // dead, there is nothing left to do but end this generation and let the
    // supervisor start a fresh one. `on_fatal` is that: it runs exactly once,
    // from inside `adapter_agent24`, the moment that happens.
    //
    // L-2: `warn`, not `error` — a clean kernel shutdown of this generation
    // (the kernel closing its end on purpose, e.g. during its own graceful
    // stop) looks IDENTICAL from here to a genuine failure; this path is not
    // necessarily a bug being reported, just the one thing this process can
    // do about "the connection is gone" either way. (N-M5, softening `exit`
    // itself into something the supervisor can tell apart from a crash, is
    // deliberately NOT part of this change — tracked as Sin90 SFU-7.)
    let on_fatal: FatalHook = Arc::new(|| {
        tracing::warn!(
            "sin90: callback connection to the Agent24 kernel is gone (a clean kernel shutdown \
             of this generation looks the same as a real failure from here); this generation \
             cannot reconnect (design §5) — exiting so the supervisor starts a fresh one"
        );
        std::process::exit(70); // EX_SOFTWARE-ish: an unexpected runtime condition, not a CLI usage error.
    });
    let (clients, offer) = KernelClients::handshake(
        &env.callback_sock,
        "sin90",
        MANIFEST,
        &env.handshake_token,
        on_fatal,
    )
    .await?;
    tracing::info!(?offer, "sin90: handshake accepted");

    let store = Sin90Store::open(&env.data_dir.join("sin90.db")).await?;
    // `wire_kernel_clients` is the actual decision (decoupled from "is
    // `events` specifically granted" — design §5); tested directly in
    // `adapter_agent24`'s own test module with an injected `Offer`, so this
    // call site stays a one-liner with nothing left to get wrong.
    //
    // N-H1: `clients_handle` is ALWAYS bound (never conditionally dropped) —
    // the callback connection must outlive this whole function regardless of
    // what `wire_kernel_clients` decided about wiring a business client to
    // it. Dropping it early would close the kernel's only connection for
    // this generation, which the kernel treats as this generation crashing.
    let (sink, model, clients_handle) = wire_kernel_clients(&offer, Arc::new(clients));
    // T3.3.2: the reconciler only ever runs HERE, mounted mode — it needs a
    // real `SchedulerClient`, which only exists once the kernel's `Offer`
    // actually granted `_a24/scheduler/` (`SchedulerClient::new`'s own doc).
    // `run_standalone` below never constructs one at all, so there is no
    // reconciler task there — spec.md M3's "无内核（standalone）时 outbox 保持
    // pending 不报错" holds by construction, not by an extra check: with no
    // task pulling from `sin90_outbox`, pending rows simply sit there.
    if let Some(scheduler) = SchedulerClient::new(&clients_handle) {
        reconciler::spawn_pump_loop(store.clone(), scheduler);
    } else {
        tracing::warn!(
            "sin90: kernel did not grant the scheduler capability; Routine cron changes will sit \
             in sin90_outbox as pending until a future generation is granted it"
        );
    }
    let mut state = Sin90State::new(store, sink, keys);
    // T5.1.2: `Some` only when the kernel granted `_a24/model/` at
    // handshake — `Sin90State::model`'s own doc.
    state.model = model;

    let listener = listener_from_fd(env.listen_fd)?;
    tracing::info!("sin90: accepting on kernel-bound listener");
    // T3.2.3, `test-hooks` only: grab the `Arc<ActorKeys>` `state` already
    // holds BEFORE `router(state, ..)` below consumes `state` by value — the
    // debug router (`adapter_agent24::kernel_roundtrip`) needs its own copy
    // to run the same human-actor-key gate every other write route uses.
    #[cfg(feature = "test-hooks")]
    let debug_actor_keys = std::sync::Arc::clone(&state.actor_keys);
    // Agent24's proxy forwards the ORIGINAL request path, not a
    // namespace-stripped one (`agent24-os-proto::proxy::forward` builds the
    // upstream URI from `original.path_and_query()` verbatim) — so a request
    // for `_a24/memory/private/remember`-style Sin90 routes arrives here as
    // `/api/v1/sin90/today`, not `/today`. Agent24 also enforces
    // `route_namespace == "/api/v1/{name}"` at manifest validation
    // (`agent24-domain`), so hardcoding it here can never drift from what
    // `domain-os.yml` declares without the kernel refusing to mount at all.
    // `router(state, ..)` itself stays un-nested — `run_standalone` and every
    // existing test call it directly at bare paths, and both are legitimate:
    // Sin90 served on its own vs. Sin90 served behind Agent24's proxy.
    // `mounted = true`: this IS behind Agent24's kernel proxy, which is what
    // makes `POST /_a24/scheduler/fired` (T3.2.2) trustworthy here — see
    // router()'s doc and architecture.md #4.
    let inner_router = router(state, true);
    // T3.2.3, `test-hooks` only: merge the standalone debug router
    // (`adapter_agent24::kernel_roundtrip`) onto the same
    // `/api/v1/sin90` namespace — kept as its OWN router over its OWN tiny
    // state (never `Sin90State`) so `http` itself never has to know Agent24
    // exists; see that module's doc for the full reasoning. `clients_handle`
    // is `Arc::clone`d, not moved — the callback connection this generation
    // owns must still outlive this whole function regardless (N-H1, above).
    #[cfg(feature = "test-hooks")]
    let inner_router = inner_router.merge(sin90::adapter_agent24::kernel_roundtrip::router(
        std::sync::Arc::clone(&clients_handle),
        debug_actor_keys,
    ));
    let mounted_router = axum::Router::new().nest("/api/v1/sin90", inner_router);
    axum::serve(listener, mounted_router).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    /// H1 (2026-09-26 review): the exact bytes this binary sends to the
    /// kernel at `initialize` (`MANIFEST`) must be the SAME text
    /// `ai::MODEL_ACCESS` parses (`sin90::ai::MANIFEST_YAML`), not two
    /// independent embeds that could silently drift apart. `MANIFEST`'s own
    /// definition now IS `sin90::ai::MANIFEST_YAML.as_bytes()`, so this is a
    /// regression guard against a future edit reintroducing a second,
    /// independent `include_bytes!` here — that only matters under
    /// `--features remote-allowed-manifest` (§11.3.2): with the feature off
    /// both embeds would read the same literal `domain-os.yml` path anyway
    /// and this test would stay green by coincidence; only compiled and run
    /// WITH the feature does a hand-reverted `include_bytes!("../domain-os.
    /// yml")` diverge from `ai::MANIFEST_YAML` (which would then be reading
    /// `domain-os.remote-allowed.yml`) — verified by hand: reverting
    /// `MANIFEST`'s definition and running `cargo test --features
    /// remote-allowed-manifest` turns this red (see PR notes).
    #[test]
    fn manifest_sent_to_the_kernel_is_the_same_text_model_access_parses() {
        assert_eq!(super::MANIFEST, sin90::ai::MANIFEST_YAML.as_bytes());
    }
}
