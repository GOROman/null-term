//! pasotsu-term: パソコン通信用 2 画面 (上下分割) シリアルターミナル

mod channel;
mod ctl;
mod keys;
mod ui;

use std::io::stdout;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{bail, Result};
use clap::Parser;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::crossterm::execute;
use serialport::FlowControl;

use channel::{Channel, Newline, PortConfig, SerialEvent, BAUD_RATES, ENCODINGS};

#[derive(Parser, Debug)]
#[command(
    version,
    about = "パソコン通信用 2 画面シリアルターミナル (上下分割 / USB-UART 2ch)",
    args_conflicts_with_subcommands = true
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// 外部制御ソケットのパス (既定: $PASOTSU_SOCK または $TMPDIR/pasotsu-term-$USER.sock)
    #[arg(short, long, global = true)]
    socket: Option<PathBuf>,
    /// 画面を出さずに外部制御だけで動かす
    #[arg(long)]
    headless: bool,
    /// 上画面 (A) のポート  PATH[:BAUD[:FMT]]  例: /dev/cu.usbserial-XXXX:9600:8N1
    port_a: Option<String>,
    /// 下画面 (B) のポート  PATH[:BAUD[:FMT]]
    port_b: Option<String>,
    /// bps の既定値 (ポート指定で省略した場合)
    #[arg(short, long, default_value_t = 9600)]
    baud: u32,
    /// 文字コード: sjis / utf8 / eucjp / jis
    #[arg(short, long, default_value = "sjis")]
    encoding: String,
    /// Enter で送る改行: cr / crlf / lf
    #[arg(short, long, default_value = "cr")]
    newline: String,
    /// フロー制御: none / xon / rts
    #[arg(short, long, default_value = "none")]
    flow: String,
    /// BackSpace キーで DEL (0x7F) を送る (既定は BS 0x08)
    #[arg(long)]
    del: bool,
    /// ローカルエコーを有効にして起動
    #[arg(long)]
    echo: bool,
    /// 接続時に ATI3 でモデム名を問い合わせない
    #[arg(long)]
    no_probe: bool,
    /// 利用可能なシリアルポートを一覧表示して終了
    #[arg(short, long)]
    list: bool,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// 起動中の pasotsu-term を外部から操作する
    #[command(subcommand)]
    Ctl(ctl::CtlCmd),
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Mode {
    Normal,
    /// Ctrl-A を押した直後
    Prefix,
    /// スクロールバック閲覧中
    Scroll,
}

pub enum Popup {
    Baud { sel: usize, custom: String },
    Port { ports: Vec<String>, sel: usize },
    Help,
}

pub struct App {
    pub channels: [Channel; 2],
    pub active: usize,
    pub mode: Mode,
    pub popup: Option<Popup>,
    pub zoom: bool,
    pub scroll: usize,
    pub quit: bool,
    /// 次の描画前に端末を全消去して描き直す
    pub redraw: bool,
    tx: mpsc::Sender<SerialEvent>,
}

fn parse_flow(s: &str) -> Result<FlowControl> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "none" | "no" => FlowControl::None,
        "xon" | "soft" | "sw" => FlowControl::Software,
        "rts" | "hard" | "hw" => FlowControl::Hardware,
        _ => bail!("flow は none / xon / rts のいずれか: {s}"),
    })
}

pub fn list_ports() -> Vec<String> {
    let mut ports: Vec<String> = serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.port_name)
        // macOS では tty.* と cu.* が対で見える。発信側は cu.* を使う
        .filter(|n| !n.starts_with("/dev/tty."))
        .collect();
    ports.sort();
    ports
}

fn main() -> Result<()> {
    let args = Args::parse();
    let socket = args.socket.clone().unwrap_or_else(ctl::default_socket);
    if let Some(Command::Ctl(cmd)) = args.command {
        let code = ctl::client(&socket, cmd)?;
        std::process::exit(code);
    }
    if args.list {
        for p in list_ports() {
            println!("{p}");
        }
        return Ok(());
    }
    let encoding = channel::parse_encoding(&args.encoding)?;
    let newline = Newline::parse(&args.newline)?;
    let flow = parse_flow(&args.flow)?;
    let bs = if args.del { 0x7f } else { 0x08 };
    let cfg = |spec: &Option<String>| -> Result<PortConfig> {
        match spec {
            Some(s) => PortConfig::parse(s, args.baud, flow),
            None => Ok(PortConfig::empty(args.baud, flow)),
        }
    };
    let (cfg_a, cfg_b) = (cfg(&args.port_a)?, cfg(&args.port_b)?);

    let (tx, rx) = mpsc::channel();
    let mut app = App {
        channels: [
            Channel::new(0, cfg_a, encoding, newline, bs),
            Channel::new(1, cfg_b, encoding, newline, bs),
        ],
        active: 0,
        mode: Mode::Normal,
        popup: None,
        zoom: false,
        scroll: 0,
        quit: false,
        redraw: false,
        tx,
    };
    for ch in app.channels.iter_mut() {
        ch.local_echo = args.echo;
        ch.auto_probe = !args.no_probe;
        ch.open(&app.tx);
    }

    let (ctl_tx, ctl_rx) = mpsc::channel();
    ctl::spawn_server(&socket, ctl_tx)?;

    let result = if args.headless {
        eprintln!("pasotsu-term: headless 起動 (制御ソケット {})", socket.display());
        for ch in &app.channels {
            eprintln!("  {}: {} {}", ch.name(), ch.cfg.path.as_deref().unwrap_or("-"), ch.status);
        }
        run(None, &mut app, &rx, &ctl_rx)
    } else {
        let mut terminal = ratatui::init();
        execute!(stdout(), EnableBracketedPaste)?;
        let result = run(Some(&mut terminal), &mut app, &rx, &ctl_rx);
        let _ = execute!(stdout(), DisableBracketedPaste);
        ratatui::restore();
        result
    };
    let _ = std::fs::remove_file(&socket);
    result
}

fn run(
    mut terminal: Option<&mut ratatui::DefaultTerminal>,
    app: &mut App,
    rx: &mpsc::Receiver<SerialEvent>,
    ctl_rx: &mpsc::Receiver<ctl::CtlRequest>,
) -> Result<()> {
    let mut dirty = true;
    let mut pending = Vec::new();
    while !app.quit {
        while let Ok(ev) = rx.try_recv() {
            let ch = match &ev {
                SerialEvent::Data { ch, .. } | SerialEvent::Error { ch, .. } => *ch,
            };
            app.channels[ch].handle_event(ev);
            dirty = true;
        }
        while let Ok(req) = ctl_rx.try_recv() {
            ctl::handle(app, req, &mut pending);
            dirty = true;
        }
        for ch in app.channels.iter_mut() {
            dirty |= ch.poll_probe();
            dirty |= ch.poll_reconnect(&app.tx);
        }
        if !pending.is_empty() {
            ctl::poll_waits(app, &mut pending);
        }
        let Some(terminal) = terminal.as_deref_mut() else {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        };
        if app.redraw {
            terminal.clear()?;
            app.redraw = false;
            dirty = true;
        }
        if dirty {
            terminal.draw(|f| ui::draw(f, app))?;
            dirty = false;
        }
        if event::poll(Duration::from_millis(10))? {
            match event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => handle_key(app, k),
                Event::Paste(s) => {
                    if app.mode == Mode::Normal && app.popup.is_none() {
                        app.channels[app.active].send_text(&s);
                    }
                }
                _ => {}
            }
            dirty = true;
        }
    }
    Ok(())
}

fn is_ctrl_a(k: &KeyEvent) -> bool {
    k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('a' | 'A'))
}

fn handle_key(app: &mut App, k: KeyEvent) {
    if app.popup.is_some() {
        handle_popup_key(app, k);
        return;
    }
    match app.mode {
        Mode::Normal => {
            if is_ctrl_a(&k) {
                app.mode = Mode::Prefix;
            } else {
                let ch = &mut app.channels[app.active];
                if let Some(bytes) = keys::key_to_bytes(&k, ch) {
                    ch.write_raw(&bytes);
                }
            }
        }
        Mode::Prefix => {
            app.mode = Mode::Normal;
            handle_command(app, k);
        }
        Mode::Scroll => handle_scroll_key(app, k),
    }
}

fn handle_command(app: &mut App, k: KeyEvent) {
    if is_ctrl_a(&k) {
        // Ctrl-A Ctrl-A で 0x01 そのものを送る
        app.channels[app.active].write_raw(&[0x01]);
        return;
    }
    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('l') {
        app.redraw = true;
        return;
    }
    let tx = app.tx.clone();
    let ch = &mut app.channels[app.active];
    match k.code {
        KeyCode::Tab | KeyCode::Char('o') | KeyCode::Up | KeyCode::Down => app.active ^= 1,
        KeyCode::Char('1') => app.active = 0,
        KeyCode::Char('2') => app.active = 1,
        KeyCode::Char('b') => {
            let sel = BAUD_RATES.iter().position(|&b| b == ch.cfg.baud).unwrap_or(4);
            app.popup = Some(Popup::Baud { sel, custom: String::new() });
        }
        KeyCode::Char('p') => {
            let ports = list_ports();
            let sel = ch
                .cfg
                .path
                .as_ref()
                .and_then(|p| ports.iter().position(|x| x == p))
                .unwrap_or(0);
            app.popup = Some(Popup::Port { ports, sel });
        }
        KeyCode::Char('e') => {
            let i = ENCODINGS.iter().position(|&e| e == ch.encoding).unwrap_or(0);
            ch.set_encoding(ENCODINGS[(i + 1) % ENCODINGS.len()]);
        }
        KeyCode::Char('n') => ch.newline = ch.newline.next(),
        KeyCode::Char('l') => ch.local_echo = !ch.local_echo,
        KeyCode::Char('h') => ch.backspace = if ch.backspace == 0x08 { 0x7f } else { 0x08 },
        KeyCode::Char('c') => ch.clear(),
        KeyCode::Char('r') => ch.open(&tx),
        KeyCode::Char('i') => ch.probe_modem(),
        KeyCode::Char('H') => ch.hangup(),
        KeyCode::Char('x') => ch.close(),
        KeyCode::Char('L') => ch.toggle_log(),
        KeyCode::Char('z') => app.zoom = !app.zoom,
        KeyCode::Char('[') | KeyCode::PageUp => {
            app.mode = Mode::Scroll;
            app.scroll = 0;
            if k.code == KeyCode::PageUp {
                handle_scroll_key(app, k);
            }
        }
        KeyCode::Char('?') => app.popup = Some(Popup::Help),
        KeyCode::Char('q') => app.quit = true,
        _ => {}
    }
}

fn handle_scroll_key(app: &mut App, k: KeyEvent) {
    let ch = &mut app.channels[app.active];
    let page = ch.parser.screen().size().0 as usize;
    match k.code {
        KeyCode::Up | KeyCode::Char('k') => app.scroll += 1,
        KeyCode::Down | KeyCode::Char('j') => app.scroll = app.scroll.saturating_sub(1),
        KeyCode::PageUp | KeyCode::Char('b') => app.scroll += page,
        KeyCode::PageDown | KeyCode::Char(' ') => app.scroll = app.scroll.saturating_sub(page),
        KeyCode::Home | KeyCode::Char('g') => app.scroll = usize::MAX,
        KeyCode::End | KeyCode::Char('G') => app.scroll = 0,
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => app.scroll = 0,
        _ => {}
    }
    ch.parser.set_scrollback(app.scroll);
    // 実際にスクロールできた量に丸める
    app.scroll = ch.parser.screen().scrollback();
    if matches!(k.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter) {
        app.mode = Mode::Normal;
    }
}

fn handle_popup_key(app: &mut App, k: KeyEvent) {
    let tx = app.tx.clone();
    let ch = &mut app.channels[app.active];
    let Some(popup) = app.popup.as_mut() else { return };
    let mut close = matches!(k.code, KeyCode::Esc);
    match popup {
        Popup::Help => close = true,
        Popup::Baud { sel, custom } => match k.code {
            KeyCode::Up => *sel = sel.saturating_sub(1),
            KeyCode::Down => *sel = (*sel + 1).min(BAUD_RATES.len() - 1),
            KeyCode::Char(c) if c.is_ascii_digit() && custom.len() < 8 => custom.push(c),
            KeyCode::Backspace => {
                custom.pop();
            }
            KeyCode::Enter => {
                let baud = custom.parse::<u32>().ok().filter(|&b| b > 0).unwrap_or(BAUD_RATES[*sel]);
                ch.set_baud(baud);
                close = true;
            }
            _ => {}
        },
        Popup::Port { ports, sel } => match k.code {
            KeyCode::Up => *sel = sel.saturating_sub(1),
            KeyCode::Down => *sel = (*sel + 1).min(ports.len().saturating_sub(1)),
            KeyCode::Enter => {
                if let Some(p) = ports.get(*sel) {
                    ch.cfg.path = Some(p.clone());
                    ch.open(&tx);
                }
                close = true;
            }
            _ => {}
        },
    }
    if close {
        app.popup = None;
    }
}
