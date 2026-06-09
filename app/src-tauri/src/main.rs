// cc-console desktop shell.
//
// This binary is a thin Tauri wrapper around the existing `cc-console` daemon.
// It does NOT reimplement any console logic: at startup it spawns the daemon as
// a bundled sidecar bound to a free loopback port, waits until that port accepts
// connections, then points the webview at `http://127.0.0.1:<port>/` — the exact
// same UI you'd get in a browser. The daemon owns tmux, the WebSocket bridge, the
// tunnel button, everything. We just give it a native window + installer.
#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use tauri::{Manager, RunEvent};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;

/// Holds the running daemon process so we can kill it when the app exits.
struct Daemon(Mutex<Option<CommandChild>>);

/// GUI apps launched from Finder/launchd inherit a stripped PATH (no
/// `/opt/homebrew/bin`, etc.), so the daemon can't find `tmux`/`claude` and
/// exits immediately — leaving the splash spinning forever. Recover the user's
/// real PATH from their login shell, then union in the usual install dirs.
///
/// This spawns an interactive login shell (~0.5–1.5s), so it's never on the
/// startup critical path — `resolve_path_fast()` serves a cached value and
/// refreshes this in the background.
#[cfg(unix)]
fn resolve_path() -> String {
    let mut dirs: Vec<String> = Vec::new();

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    if let Ok(out) = std::process::Command::new(&shell)
        .args(["-ilc", "printf %s \"$PATH\""])
        .output()
    {
        let p = String::from_utf8_lossy(&out.stdout);
        dirs.extend(p.trim().split(':').filter(|s| !s.is_empty()).map(String::from));
    }

    for d in [
        "/opt/homebrew/bin", "/opt/homebrew/sbin",
        "/usr/local/bin", "/usr/local/sbin",
        "/usr/bin", "/bin", "/usr/sbin", "/sbin",
    ] {
        dirs.push(d.to_string());
    }
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(format!("{home}/.local/bin"));
        dirs.push(format!("{home}/.cargo/bin"));
    }
    if let Ok(existing) = std::env::var("PATH") {
        dirs.extend(existing.split(':').filter(|s| !s.is_empty()).map(String::from));
    }

    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| seen.insert(d.clone()));
    dirs.join(":")
}

#[cfg(not(unix))]
fn resolve_path() -> String {
    std::env::var("PATH").unwrap_or_default()
}

/// Where the resolved PATH is cached between launches.
fn path_cache_file() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude/cc-console/path-cache"))
}

/// PATH for the daemon, optimised for startup latency.
///
/// `resolve_path()` is expensive (login shell). To keep launches snappy we cache
/// its result: a cache hit returns instantly and kicks off a background refresh
/// for next time; a cache miss computes once and persists. The standard install
/// dirs are unioned in by `resolve_path()` regardless, so a slightly stale cache
/// still finds Homebrew-installed `tmux`/`claude`.
fn resolve_path_fast() -> String {
    let cache = path_cache_file();

    if let Some(ref f) = cache {
        if let Ok(s) = std::fs::read_to_string(f) {
            let cached = s.trim().to_string();
            if !cached.is_empty() {
                let f = f.clone();
                std::thread::spawn(move || {
                    let _ = std::fs::write(&f, resolve_path());
                });
                return cached;
            }
        }
    }

    // Cache miss: compute now and persist for subsequent launches.
    let p = resolve_path();
    if let Some(f) = cache {
        if let Some(parent) = f.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&f, &p);
    }
    p
}

/// Ask the OS for an unused loopback port by binding to :0 and reading it back.
/// There's a tiny race between drop and the daemon re-binding, but on loopback
/// it's effectively never contended for a desktop app.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(7878)
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(Daemon(Mutex::new(None)))
        .setup(|app| {
            // Reveal the splash window *immediately* so the user gets instant
            // feedback. Everything below (PATH resolution + daemon boot) now
            // happens behind the spinner instead of behind a blank screen.
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.set_focus();
                // Stamp the splash with the build version. Retries until the DOM is
                // ready (the webview may still be loading splash HTML at this point).
                let _ = win.eval(concat!(
                    "(function s(){var e=document.getElementById('ver');",
                    "if(e){e.textContent='v", env!("CARGO_PKG_VERSION"), "';}",
                    "else{setTimeout(s,40);}})();"
                ));
            }

            let port = free_port();
            // A per-launch token. The daemon turns auth ON whenever it's reachable
            // from outside — and a managed tunnel auto-enables when a device token
            // is saved (GUI "Connect" / `cc-console link`), at which point the bare
            // http://127.0.0.1/ the webview loads would 401. Passing the token to
            // the daemon AND in the first navigate URL keeps the local webview
            // authorised in every case (and secures the tunnel). The `?token=` is
            // consumed once; the daemon replies with a cookie for later requests.
            let token = uuid::Uuid::new_v4().simple().to_string();
            let url = format!("http://127.0.0.1:{port}/?token={token}");
            let handle = app.handle().clone();

            // Resolve PATH, spawn the daemon, wait for its port, then navigate —
            // all off the main thread, so the window above is already on screen.
            std::thread::spawn(move || {
                let cmd = match handle.shell().sidecar("cc-console") {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("[shell] sidecar lookup failed: {e}");
                        return;
                    }
                };
                let (mut rx, child) = match cmd
                    .args(["--host", "127.0.0.1", "--port", &port.to_string(), "--token", &token])
                    .env("PATH", resolve_path_fast())
                    .spawn()
                {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("[shell] failed to spawn daemon: {e}");
                        return;
                    }
                };
                handle.state::<Daemon>().0.lock().unwrap().replace(child);

                // Surface daemon logs on the shell's stdout/stderr for debugging.
                tauri::async_runtime::spawn(async move {
                    while let Some(event) = rx.recv().await {
                        match event {
                            CommandEvent::Stdout(b) => print!("{}", String::from_utf8_lossy(&b)),
                            CommandEvent::Stderr(b) => eprint!("{}", String::from_utf8_lossy(&b)),
                            CommandEvent::Error(e) => eprintln!("[daemon error] {e}"),
                            CommandEvent::Terminated(p) => eprintln!("[daemon exited] {p:?}"),
                            _ => {}
                        }
                    }
                });

                // Wait for the daemon to start listening, then point the window at
                // the local UI. Poll at 50ms for a snappy handoff (~30s cap).
                let ready = (0..600).any(|_| {
                    if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                        true
                    } else {
                        std::thread::sleep(Duration::from_millis(50));
                        false
                    }
                });
                if !ready {
                    eprintln!("[shell] daemon did not come up on port {port} within ~30s");
                }

                let h = handle.clone();
                let _ = handle.run_on_main_thread(move || {
                    if let Some(win) = h.get_webview_window("main") {
                        if let Ok(u) = tauri::Url::parse(&url) {
                            let _ = win.navigate(u);
                        }
                    }
                });
            });

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building cc-console shell")
        .run(|app, event| {
            // Don't leave an orphaned daemon (and its tmux client) behind.
            if let RunEvent::Exit = event {
                if let Some(child) = app.state::<Daemon>().0.lock().unwrap().take() {
                    let _ = child.kill();
                }
            }
        });
}
