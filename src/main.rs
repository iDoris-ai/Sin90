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
use sin90::adapter_agent24::{
    listener_from_fd, wire_kernel_clients, FatalHook, KernelClients, SpawnEnv,
};
use sin90::http::{router, ActorKeys, NullEventSink, Sin90State};
use sin90::store::Sin90Store;

/// Compiled-in manifest, so the digest this binary computes for `initialize`
/// is always the digest of the manifest it actually shipped with — the same
/// "identity cannot drift from the code" reasoning Agent24's own in-process
/// modules use for their embedded `domain-os.yml`.
const MANIFEST: &[u8] = include_bytes!("../domain-os.yml");

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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    match cli.command {
        Some(Command::Serve { port, data_dir }) => run_standalone(port, data_dir).await,
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
    // N-H1: `_clients` is ALWAYS bound (never conditionally dropped) — the
    // callback connection must outlive this whole function regardless of
    // what `wire_kernel_clients` decided about wiring a business client to
    // it. Dropping it early would close the kernel's only connection for
    // this generation, which the kernel treats as this generation crashing.
    let (sink, _clients) = wire_kernel_clients(&offer, Arc::new(clients));
    let state = Sin90State::new(store, sink, keys);

    let listener = listener_from_fd(env.listen_fd)?;
    tracing::info!("sin90: accepting on kernel-bound listener");
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
    let mounted_router = axum::Router::new().nest("/api/v1/sin90", router(state, true));
    axum::serve(listener, mounted_router).await?;
    Ok(())
}
