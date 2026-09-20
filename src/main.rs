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
use sin90::adapter_agent24::{listener_from_fd, CallbackChannel, KernelEventSink, SpawnEnv};
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
    let store = match data_dir {
        Some(dir) => Sin90Store::open(&dir.join("sin90.db")).await?,
        None => Sin90Store::open_memory().await?,
    };
    let state = Sin90State::new(
        store,
        Arc::new(NullEventSink),
        ActorKeys::from_env_or_generate(),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!(port, "sin90: standalone listening");
    axum::serve(listener, router(state)).await?;
    Ok(())
}

async fn run_as_agent24_module() -> Result<(), Box<dyn std::error::Error>> {
    let env = SpawnEnv::from_env()?;
    let (channel, offer) =
        CallbackChannel::handshake(&env.callback_sock, "sin90", MANIFEST, &env.handshake_token)
            .await?;
    tracing::info!(?offer, "sin90: handshake accepted");

    let store = Sin90Store::open(&env.data_dir.join("sin90.db")).await?;
    let sink: Arc<dyn sin90::http::EventSink> = if offer
        .iter()
        .any(|p| "_a24/events/emit".starts_with(p.as_str()) || p.starts_with("_a24/events/"))
    {
        Arc::new(KernelEventSink(Arc::new(channel)))
    } else {
        // Not granted events — degrade, don't fail (design §5.3).
        tracing::warn!("sin90: events not offered by kernel; running with events dropped");
        Arc::new(NullEventSink)
    };
    let state = Sin90State::new(store, sink, ActorKeys::from_env_or_generate());

    let listener = listener_from_fd(env.listen_fd)?;
    tracing::info!("sin90: accepting on kernel-bound listener");
    axum::serve(listener, router(state)).await?;
    Ok(())
}
