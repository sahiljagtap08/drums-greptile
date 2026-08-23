//! TTY-only live view for `drums watch`. Consumes the exact same
//! `EngineEvent` stream `render.rs` narrates line-by-line, but keeps one
//! place on screen per failure and updates it in place instead of appending.
//! Strictly additive: `main.rs` only reaches this module when stdout AND
//! stdin are both real terminals, `--plain` was not passed, and
//! `DRUMS_PLAIN` is not set — every other case keeps using `render.rs`,
//! byte-identical, via [`run_plain`] below.

mod model;
mod view;

pub use model::ViewModel;

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::engine::EngineEvent;
use crate::render::{self, RenderContext};
use model::StageStatus;

/// The static header banner content — everything fixed for the whole run,
/// built once in `main.rs` from the same `EngineConfig` fields the plain
/// `--repair auto` banner already echoes.
pub struct HeaderInfo {
    pub repo_name: String,
    pub agent_label: String,
    pub repair_label: String,
    pub ingest_port: u16,
}

const TICK: Duration = Duration::from_millis(100); // ~10fps redraw

fn now_ms_wall() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let secs_of_day = (ms / 1000) % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        secs_of_day / 3600,
        (secs_of_day / 60) % 60,
        secs_of_day % 60
    )
}

/// RAII guard that restores the terminal to a normal, cooked, primary-screen
/// state exactly once, no matter how the TUI exits: normal return, an early
/// `?` on any setup or per-frame error, or a panic unwinding through
/// [`run_with_terminal`]. ratatui's own `Drop for Terminal` restores only the
/// cursor — raw mode and the alternate screen are ours to give back. `restore`
/// is idempotent (an `AtomicBool` swap) so calling it explicitly on the
/// normal-exit path and then letting the guard drop afterward is safe and
/// does not double-emit the leave-alt-screen sequence.
pub struct TerminalGuard {
    restored: AtomicBool,
}

impl TerminalGuard {
    fn new() -> Self {
        Self {
            restored: AtomicBool::new(false),
        }
    }

    fn restore(&self) {
        if self.restored.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Installs a process-wide panic hook that restores the terminal (raw mode +
/// alternate screen) before handing off to whatever hook was previously
/// installed, so a panic anywhere in the TUI loop still prints its message
/// onto a normal, scrollable screen instead of vanishing into a raw-mode
/// alternate-screen buffer that clears on process exit. Only called once
/// terminal setup has actually put the terminal into that state (from inside
/// [`try_init_terminal`], after `enable_raw_mode` succeeds).
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        prev(info);
    }));
}

/// Puts the terminal into raw mode and the alternate screen. This is
/// deliberately the ENTIRE "terminal setup" phase and nothing more — it does
/// not touch `ev_rx` or print anything engine-related — so a caller can
/// attempt it, and on failure fall straight back to the plain path with the
/// event receiver never having been consumed.
///
/// Setup can fail even when `stdout` is a terminal: `enable_raw_mode` also
/// needs a working `stdin` (it reads/writes stdin's termios), and stdin can
/// be `/dev/null`, a closed pipe, or otherwise not a terminal (`drums watch <
/// /dev/null`, `nohup drums watch`, most supervisor/launchd wrappers) even
/// while stdout is attached to one. `main.rs`'s `use_tui` check already
/// guards against the common case by requiring `stdin().is_terminal()` too,
/// but this function is the actual belt-and-suspenders: ANY terminal-setup
/// error here — that one included — degrades to plain output instead of
/// aborting the command.
pub fn try_init_terminal() -> io::Result<(Terminal<CrosstermBackend<io::Stdout>>, TerminalGuard)> {
    enable_raw_mode()?;
    // From this point on, raw mode IS enabled, so any early return below
    // (via `?`) must restore it. `guard`'s `Drop` makes that automatic
    // regardless of which line returns — see `TerminalGuard` above.
    let guard = TerminalGuard::new();
    install_panic_hook();
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    Ok((terminal, guard))
}

/// Runs the live TUI until the user quits (`q`/ctrl-c) or `stop_signal`
/// resolves — the SAME single stop-signal future `main.rs` already built
/// (`wait_for_stop_signal()`); this function registers no signal handler of
/// its own. Ctrl-C is instead read here as a plain keypress: enabling raw
/// mode (required for a live TUI at all) disables the terminal's own
/// SIGINT-on-Ctrl-C generation, so treating it as a key is the only way it's
/// ever seen while the alternate screen is up — SIGTERM sent externally
/// still arrives on `stop_signal` exactly as it always has.
///
/// Takes an already-initialized terminal + [`TerminalGuard`] (see
/// [`try_init_terminal`]) rather than doing setup itself: setup can fail for
/// reasons unrelated to anything in this loop (see that function's doc), and
/// the caller needs to be able to try setup, see it fail, and fall back to
/// [`run_plain`] with `ev_rx` still untouched — which isn't possible if this
/// function had already consumed `ev_rx` before setup's own fallible steps
/// ran.
///
/// Leaves the alternate screen and disables raw mode before returning (via
/// `guard.restore()`, idempotent with the guard's own `Drop`), then prints a
/// compact final summary to the NORMAL screen — the failure line, each
/// stage's outcome and chip, and the next command — so a screen recording
/// still shows the outcome after ratatui's alternate-screen buffer (which
/// clears on exit) is gone.
///
/// Returns whether the caller should still print its own "stopped — …"
/// follow-up line: true for every deliberate stop (quit key, ctrl-c key,
/// `stop_signal`); false only if the event channel itself closed first (the
/// engine side went away on its own) — matching the plain path's exact
/// condition for printing that line.
pub async fn run_with_terminal(
    mut terminal: Terminal<CrosstermBackend<io::Stdout>>,
    guard: TerminalGuard,
    header: HeaderInfo,
    mut ev_rx: UnboundedReceiver<EngineEvent>,
    render_ctx: RenderContext,
    record_path: PathBuf,
    stop_signal: impl std::future::Future<Output = ()>,
) -> io::Result<bool> {
    let mut vm = ViewModel::new(record_path);
    let use_color = std::env::var_os("NO_COLOR").is_none();

    // Blocking key reads live on their own OS thread (crossterm's async
    // event stream needs an extra feature + futures dependency this crate
    // otherwise has no use for); forwarded over a plain unbounded channel
    // into the tokio select loop below. Detached: it exits on its own once
    // `read`/`poll` starts erroring (e.g. the terminal going away), and the
    // process exiting reaps it either way — there is nothing left to
    // coordinate with it once this function returns.
    let (key_tx, mut key_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    std::thread::spawn(move || loop {
        match crossterm::event::poll(Duration::from_millis(50)) {
            Ok(true) => match crossterm::event::read() {
                Ok(ev) => {
                    if key_tx.send(ev).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            },
            Ok(false) => {}
            Err(_) => break,
        }
    });

    tokio::pin!(stop_signal);
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut channel_closed = false;
    let mut quit = false;
    loop {
        tokio::select! {
            ev = ev_rx.recv() => {
                match ev {
                    Some(ev) => vm.apply(&ev, Instant::now(), now_ms_wall, &render_ctx),
                    None => { channel_closed = true; quit = true; }
                }
            }
            key = key_rx.recv() => {
                if let Some(Event::Key(k)) = key {
                    if k.kind == KeyEventKind::Press || k.kind == KeyEventKind::Repeat {
                        match k.code {
                            KeyCode::Char('q') => quit = true,
                            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => quit = true,
                            KeyCode::Char('?') => vm.toggle_help(),
                            KeyCode::Up => vm.select_prev(),
                            KeyCode::Down => vm.select_next(),
                            _ => {}
                        }
                    }
                }
            }
            _ = tick.tick() => {}
            _ = &mut stop_signal => { quit = true; }
        }

        terminal.draw(|f| view::draw(f, &vm, &header, Instant::now(), use_color))?;

        if quit {
            break;
        }
    }

    guard.restore();
    drop(terminal);

    print_final_summary(&vm, use_color);

    Ok(!channel_closed)
}

/// Convenience wrapper: terminal setup (see [`try_init_terminal`]) followed
/// by [`run_with_terminal`] in one call. `main.rs` does not use this
/// directly — it needs to see setup fail BEFORE `ev_rx` is consumed so it
/// can fall back to [`run_plain`] with the receiver intact — but it keeps
/// this module's surface usable as a single call for any other caller that
/// doesn't need that fallback.
pub async fn run(
    header: HeaderInfo,
    ev_rx: UnboundedReceiver<EngineEvent>,
    render_ctx: RenderContext,
    record_path: PathBuf,
    stop_signal: impl std::future::Future<Output = ()>,
) -> io::Result<bool> {
    let (terminal, guard) = try_init_terminal()?;
    run_with_terminal(
        terminal,
        guard,
        header,
        ev_rx,
        render_ctx,
        record_path,
        stop_signal,
    )
    .await
}

/// The plain, line-based §7 narration loop. Used whenever the live TUI is
/// unavailable (not a terminal, or [`try_init_terminal`] failed) or
/// explicitly disabled (`--plain`, `DRUMS_PLAIN=1`). Prints straight to
/// stdout via `render::render`, one line per `EngineEvent`, flushing after
/// each — this is the exact code path `e2e.rs` and `demo/run-demo.sh` assert
/// byte-for-byte narration against.
///
/// Returns whether the caller should print its own "stopped — …" follow-up
/// line: true for `stop_signal` resolving, false if the event channel closed
/// first — the same condition [`run_with_terminal`] reports.
pub async fn run_plain(
    mut ev_rx: UnboundedReceiver<EngineEvent>,
    render_ctx: &RenderContext,
    stop_signal: impl std::future::Future<Output = ()>,
) -> bool {
    tokio::pin!(stop_signal);
    loop {
        tokio::select! {
            ev = ev_rx.recv() => {
                let Some(ev) = ev else { return false };
                print!("{}", render::render(&ev, render_ctx));
                let _ = io::stdout().flush();
            }
            _ = &mut stop_signal => { return true; }
        }
    }
}

const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

fn c(code: &str, use_color: bool) -> &str {
    if use_color {
        code
    } else {
        ""
    }
}

/// The compact plain-text recap printed to the normal screen on the way out
/// — see [`run_with_terminal`]'s doc comment for why this exists. Deliberately
/// independent of `render.rs`'s event-by-event narration (nothing here
/// re-renders raw `EngineEvent`s): it reads straight off the same
/// `ViewModel` the TUI was already showing, which is exactly the state a
/// viewer had on screen the instant before the alternate screen buffer
/// vanished.
fn print_final_summary(vm: &ViewModel, use_color: bool) {
    let now = Instant::now();
    let mut out = String::new();
    for card in &vm.cards {
        out.push_str(&format!(
            "{}▲{} {} failing\n",
            c(RED, use_color),
            c(RESET, use_color),
            card.service
        ));
        out.push_str(&format!(
            "  {} {} · {}\n",
            card.method, card.path, card.claim_text
        ));
        for stage in &card.stages {
            let (icon, color) = match stage.status {
                StageStatus::Done => ("✓", GREEN),
                StageStatus::Failed => ("✗", RED),
                StageStatus::Running => ("…", DIM),
            };
            let chip = stage
                .chip
                .map(|p| format!("[{}]", p.chip()))
                .unwrap_or_default();
            out.push_str(&format!(
                "  {}{}{} {:<11} {:<12} {} ({:.1}s)\n",
                c(color, use_color),
                icon,
                c(RESET, use_color),
                stage.label,
                chip,
                stage.detail,
                stage.elapsed_ms(now) as f64 / 1000.0
            ));
        }
        if let Some(ready) = &card.ready_line {
            out.push_str(&format!(
                "  {}✓{} {}{}{}\n",
                c(GREEN, use_color),
                c(RESET, use_color),
                c(BOLD, use_color),
                ready,
                c(RESET, use_color)
            ));
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "record: {}  ·  {} events\n",
        vm.record_path.display(),
        vm.event_count
    ));
    print!("{out}");
    let _ = io::stdout().flush();
}
