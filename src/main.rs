mod app;
mod config;
mod procs;
mod tunnel;
mod ui;

use anyhow::Result;
use app::App;
use crossterm::event::{self, Event, KeyEventKind};
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_secs(1);

fn main() -> Result<()> {
    let arg = std::env::args().nth(1);
    if matches!(arg.as_deref(), Some("-V") | Some("--version")) {
        println!("socatui {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if matches!(arg.as_deref(), Some("-h") | Some("--help")) {
        println!("usage: socatui [CONFIG.json]\n\nManage and monitor socat relays in a TUI.\nConfig defaults to $SOCATUI_CONFIG or ~/.config/socatui/tunnels.json");
        return Ok(());
    }
    let config_path = config::resolve_path(arg);
    let mut app = App::new(config_path)?;
    app.autostart();

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app);
    ratatui::restore();
    app.shutdown();
    result
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    let mut last_tick = Instant::now();
    loop {
        app.drain_events();
        terminal.draw(|f| ui::draw(f, app))?;

        let until_tick = TICK.saturating_sub(last_tick.elapsed());
        if event::poll(until_tick.min(Duration::from_millis(100)))? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => app.on_key(k),
                _ => {}
            }
        }
        if last_tick.elapsed() >= TICK {
            app.on_tick();
            last_tick = Instant::now();
        }
        if app.should_quit {
            return Ok(());
        }
    }
}
