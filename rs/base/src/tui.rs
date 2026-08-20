use crate::client::Client;
use crate::home::Home;
use crate::ui::Ui;
use anyhow::Result;
use serde_json::json;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;
use tenon_ui::keys::{self, Key};
use tenon_ui::terminal::Frame;
use tokio::sync::mpsc;

const TICK: Duration = Duration::from_millis(400);
const SHOW_CURSOR: &str = "\x1b[?25h";
const HIDE_CURSOR: &str = "\x1b[?25l";

/// Raw mode for the duration of the UI and the terminal handed back exactly as
/// it was found, whatever ends the loop.
struct Raw {
    fd: i32,
    saved: Option<libc::termios>,
}

impl Raw {
    fn enter() -> Self {
        let fd = libc::STDIN_FILENO;
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        let ok = unsafe { libc::tcgetattr(fd, &mut saved) } == 0;
        if !ok {
            return Self { fd, saved: None };
        }
        let mut raw = saved;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
        raw.c_iflag &= !(libc::IXON | libc::ICRNL);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) };
        Self {
            fd,
            saved: Some(saved),
        }
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        if let Some(saved) = self.saved {
            unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &saved) };
        }
        let mut out = std::io::stdout();
        let _ = out.write_all(SHOW_CURSOR.as_bytes());
        let _ = out.flush();
    }
}

/// One reader thread on the real stdin: the wire client is async, a tty read
/// is not, and a blocking thread with a channel is the whole bridge.
fn stdin_keys() -> mpsc::UnboundedReceiver<u8> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut byte = [0u8; 1];
        while let Ok(read) = stdin.read(&mut byte) {
            if read == 0 || tx.send(byte[0]).is_err() {
                return;
            }
        }
    });
    rx
}

#[derive(Default)]
enum Mode {
    #[default]
    Keys,
    Prompt(String),
    Approve(i64),
    Rollback,
}

/// `tenon attach --ui`: the terminal carrier of RFC section 6b. The model is
/// rebuilt from base's front door on every event, key and resize; the renderer
/// itself stays the pure function `rs/ui` ships.
pub async fn attach(home: Option<PathBuf>, env: Option<String>) -> Result<i32> {
    let home = Home::resolve(home)?;
    let root = crate::config::Config::load(&home.config_file())
        .map(|config| config.root_env)
        .unwrap_or_else(|_| "root".to_string());
    let env = env.unwrap_or(root);
    let mut calls = Client::connect(&home.sock()).await?;
    let mut events = Client::connect(&home.sock()).await?;
    events
        .call(
            "bus.subscribe",
            json!({"topics": ["session/**", "base/**"], "coalesce_ms": 16}),
        )
        .await?;
    let mut ui = Ui::new(env);
    ui.backfill(&mut calls).await;
    let mut expanded: HashSet<usize> = HashSet::new();
    let mut mode = Mode::Keys;
    let mut size = Frame::size();
    let mut keys_rx = stdin_keys();
    let _raw = Raw::enter();
    print!("{HIDE_CURSOR}");
    let mut out = std::io::stdout();
    let _ = out.flush();
    let mut model = ui.model(&mut calls).await?;
    draw(&model, &expanded, &mode, size);
    loop {
        let mut refresh = false;
        tokio::select! {
            byte = keys_rx.recv() => match byte {
                None => return Ok(0),
                Some(byte) => match on_key(byte, &mut mode, &mut expanded, &model, &mut ui, &mut calls).await {
                    Action::Quit => return Ok(0),
                    Action::Refresh => refresh = true,
                    Action::Redraw => {}
                },
            },
            event = events.next_ev() => match event? {
                None => return Ok(0),
                Some(event) => {
                    ui.ingest(&event);
                    refresh = true;
                }
            },
            _ = tokio::time::sleep(TICK) => {
                let now = Frame::size();
                if now != size {
                    size = now;
                }
                refresh = true;
            }
        }
        if refresh {
            if let Ok(fresh) = ui.model(&mut calls).await {
                model = fresh;
            }
        }
        draw(&model, &expanded, &mode, size);
    }
}

enum Action {
    Quit,
    Refresh,
    Redraw,
}

async fn on_key(
    byte: u8,
    mode: &mut Mode,
    expanded: &mut HashSet<usize>,
    model: &tenon_ui::UiModel,
    ui: &mut Ui,
    calls: &mut Client,
) -> Action {
    match mode {
        Mode::Prompt(line) => {
            match byte {
                b'\r' | b'\n' => {
                    let text = std::mem::take(line);
                    *mode = Mode::Keys;
                    if text.trim().is_empty() {
                        return Action::Redraw;
                    }
                    let _ = ui.prompt(calls, &text).await;
                    return Action::Refresh;
                }
                0x7f | 0x08 => {
                    line.pop();
                }
                0x03 | 0x1b => *mode = Mode::Keys,
                other if other.is_ascii_graphic() || other == b' ' => line.push(other as char),
                _ => {}
            }
            Action::Redraw
        }
        Mode::Approve(id) => {
            let id = *id;
            let decided = match byte {
                b'y' | b'Y' => Some(true),
                b'n' | b'N' => Some(false),
                _ => None,
            };
            *mode = Mode::Keys;
            match decided {
                Some(approve) => {
                    let _ = ui.answer(calls, id, approve).await;
                    Action::Refresh
                }
                None => Action::Redraw,
            }
        }
        Mode::Rollback => {
            let go = matches!(byte, b'y' | b'Y');
            *mode = Mode::Keys;
            match go {
                true => {
                    let _ = ui.rollback(calls).await;
                    Action::Refresh
                }
                false => Action::Redraw,
            }
        }
        Mode::Keys => match keys::parse(byte) {
            Key::Quit => Action::Quit,
            Key::Prompt => {
                *mode = Mode::Prompt(String::new());
                Action::Redraw
            }
            Key::Approve => {
                if let Some(id) = model
                    .approvals
                    .first()
                    .and_then(|row| row.id.parse::<i64>().ok())
                {
                    *mode = Mode::Approve(id);
                }
                Action::Redraw
            }
            Key::Rollback => {
                *mode = Mode::Rollback;
                Action::Redraw
            }
            Key::Fold(index) => {
                if !expanded.remove(&index) {
                    expanded.insert(index);
                }
                Action::Redraw
            }
            Key::Other => Action::Redraw,
        },
    }
}

fn draw(model: &tenon_ui::UiModel, expanded: &HashSet<usize>, mode: &Mode, size: (usize, usize)) {
    let mut model = model.clone();
    model.expanded = expanded.clone();
    model.input_hint = hint(mode, &model);
    let (cols, rows) = size;
    let frame = Frame::draw_at(&model, cols, rows);
    let mut out = std::io::stdout();
    let _ = out.write_all(frame.as_bytes());
    let _ = out.flush();
}

fn hint(mode: &Mode, model: &tenon_ui::UiModel) -> String {
    match mode {
        Mode::Keys => format!(
            "p prompt  a approve ({})  r rollback  0-9 fold  q quit",
            model.approvals.len()
        ),
        Mode::Prompt(line) => format!("prompt> {line}"),
        Mode::Approve(id) => format!("approve {id}? y/n"),
        Mode::Rollback => "rollback this env to its last known good? y/n".to_string(),
    }
}
