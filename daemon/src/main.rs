mod account;
mod auth;
mod codex_live;
mod config;
mod gemini_live;
mod jsonl;
mod paths;
mod pricing;
mod provision;
mod server;
mod session;
mod sessions;
mod state;
mod tunnel;
mod usage_store;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "cc-console",
    version,
    about = "cc-console daemon — bridges a tmux session to WebSocket clients (M1)"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Address to bind. Default 127.0.0.1 (local only). Use 0.0.0.0 to reach it
    /// from your phone/other devices — but there is NO auth, so do that only on a
    /// trusted network (ideally via Tailscale).
    #[arg(long, env = "CC_CONSOLE_HOST", default_value = "127.0.0.1")]
    host: String,

    #[arg(long, env = "CC_CONSOLE_PORT", default_value_t = 7878)]
    port: u16,

    #[arg(long, env = "CC_CONSOLE_TMUX_SOCKET", default_value = "cc-console")]
    tmux_socket: String,

    #[arg(long, env = "CC_CONSOLE_TMUX_SESSION", default_value = "main")]
    tmux_session: String,

    #[arg(long, env = "CC_CONSOLE_INITIAL_CMD", default_value = "claude")]
    initial_cmd: String,

    /// Session backend: `tmux` (unix, detach/reattach) or `pty` (cross-platform,
    /// incl. Windows; sessions live only while the daemon runs). Defaults to tmux
    /// on unix, pty on Windows.
    #[arg(long, env = "CC_CONSOLE_BACKEND")]
    backend: Option<String>,

    #[arg(long, env = "CC_CONSOLE_WEB_DIR", default_value = "web")]
    web_dir: String,

    /// Access token required to use the daemon. If omitted, one is auto-generated and persisted
    /// (`~/.claude/cc-console/token`) whenever bound to a non-loopback address; loopback-only
    /// stays open. Pass `--token none` to force-disable auth.
    #[arg(long, env = "CC_CONSOLE_TOKEN")]
    token: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Link this machine to your cc-console account so it comes up at your
    /// `<handle>.cchub.cloud` address. Paste a device token (`ccd_…`) generated at
    /// app.cchub.cloud/account; it's saved and the managed tunnel starts on the next
    /// `cc-console` boot. Re-run any time to replace the token.
    Link {
        /// The device token (`ccd_…`). If omitted, you'll be prompted to paste it
        /// (keeps it out of your shell history).
        token: Option<String>,

        /// Local port the daemon serves on — must match how you'll run `cc-console`
        /// (the tunnel's ingress points here).
        #[arg(long, env = "CC_CONSOLE_PORT", default_value_t = 7878)]
        port: u16,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,cc_console=debug")),
        )
        .init();

    let cli = Cli::parse();
    if let Some(Command::Link { token, port }) = cli.command {
        return run_link(token, port).await;
    }
    let backend = match cli.backend {
        Some(s) => s.parse().map_err(|e: String| anyhow::anyhow!(e))?,
        None => session::BackendKind::platform_default(),
    };
    let cfg = config::load();
    server::run(
        cli.host,
        cli.port,
        cli.tmux_socket,
        cli.tmux_session,
        cli.initial_cmd,
        backend,
        cli.web_dir,
        cli.token,
        cfg.tunnel,
        cfg.session.env,
    )
    .await
}

/// `cc-console link` — headless onboarding for a server with no GUI. Takes a device
/// token from app.cchub.cloud/account, persists it, and validates it against the
/// control plane so the user gets the bound `<handle>.cchub.cloud` URL (or an
/// actionable error) immediately, instead of discovering it only at boot.
async fn run_link(token: Option<String>, port: u16) -> Result<()> {
    let token = match token {
        Some(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => prompt_token()?,
    };
    if token.is_empty() {
        bail!("no device token provided");
    }
    if !token.starts_with("ccd_") {
        // Not fatal — the control plane is the source of truth — but almost always a
        // paste mistake, so warn loudly before we spend a round-trip on it.
        eprintln!("warning: device tokens normally start with `ccd_` — double-check the paste.");
    }

    let cfg = config::load();
    let control_plane = cfg.tunnel.control_plane.clone();

    // Persist first: the token is the durable artefact. Even if validation below
    // fails for a fixable reason (no handle yet, billing), the next `cc-console`
    // boot will retry with it — no need to re-paste.
    config::save_device_token(&token);

    println!("Validating device token with {control_plane} …");
    match provision::provision(&control_plane, &token, port).await {
        Ok(p) => {
            println!("\n  ✓ Linked. This server is bound to:\n");
            println!("        https://{}\n", p.hostname);
            println!("  Start it with `cc-console` — the managed tunnel comes up automatically");
            println!("  (auth is forced on; the boot logs print the URL with its ?token=…).");
            Ok(())
        }
        Err(e) => {
            eprintln!("\n  ✗ The token was saved, but provisioning failed:\n");
            eprintln!("        {e}\n");
            eprintln!("  Fix the issue above — most often it's \"no handle yet\" (claim one at");
            eprintln!("  app.cchub.cloud/account) or no active subscription — then run `cc-console`.");
            std::process::exit(1);
        }
    }
}

/// Prompt for a device token on stdin. Synchronous read is fine: this is a one-shot
/// command and nothing else runs on the async runtime yet.
fn prompt_token() -> Result<String> {
    use std::io::Write;
    print!("Paste your device token (from app.cchub.cloud/account): ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}
