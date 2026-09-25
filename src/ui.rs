//! 画面描画

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::channel::{encoding_label, Channel, BAUD_RATES};
use crate::transfer::{Direction, Protocol, Transfer};
use crate::{App, Mode, Popup};

pub fn draw(f: &mut Frame, app: &mut App) {
    let [main, status] = Layout::vertical([Constraint::Min(2), Constraint::Length(1)]).areas(f.area());

    let panes: Vec<(usize, Rect)> = if app.zoom {
        vec![(app.active, main)]
    } else {
        let [top, bottom] =
            Layout::vertical([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).areas(main);
        vec![(0, top), (1, bottom)]
    };

    for (i, area) in panes {
        let active = i == app.active;
        let ch = &mut app.channels[i];
        let [title, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
        f.render_widget(Paragraph::new(title_line(ch, active)).style(title_style(i, active)), title);
        ch.resize(body.height, body.width);
        render_screen(ch.parser.screen(), body, f.buffer_mut(), PANE_BG[i]);
        if let Some(t) = &ch.transfer {
            let bar = Rect::new(body.x, body.y + body.height - 1, body.width, 1);
            draw_progress(f, t, bar);
        }
        let screen = ch.parser.screen();
        if active
            && app.popup.is_none()
            && ch.transfer.is_none()
            && screen.scrollback() == 0
            && !screen.hide_cursor()
        {
            let (r, c) = screen.cursor_position();
            f.set_cursor_position((body.x + c.min(body.width - 1), body.y + r.min(body.height - 1)));
        }
    }

    f.render_widget(Paragraph::new(status_line(app)).style(Style::new().bg(Color::DarkGray)), status);

    if let Some(popup) = &app.popup {
        draw_popup(f, popup, &app.channels[app.active]);
    }
}

fn title_style(i: usize, active: bool) -> Style {
    let (on, off) = TITLE_BG[i];
    if active {
        Style::new().bg(on).fg(Color::White).add_modifier(Modifier::BOLD)
    } else {
        Style::new().bg(off).fg(Color::Gray)
    }
}

fn title_line(ch: &Channel, active: bool) -> Line<'static> {
    let mark = if active { "▶" } else { " " };
    let state = if ch.is_open() {
        Span::styled(" ● ", Style::new().fg(Color::LightGreen))
    } else {
        Span::styled(" ○ ", Style::new().fg(Color::LightRed))
    };
    let path = ch.cfg.path.as_deref().unwrap_or("(未設定)");
    let modem = match &ch.modem {
        Some(m) => Span::styled(format!("[{m}] "), Style::new().fg(Color::LightYellow)),
        None => Span::raw(""),
    };
    let mut flags = vec![encoding_label(ch.encoding).to_string(), ch.newline.label().to_string()];
    if ch.backspace == 0x7f {
        flags.push("DEL".into());
    }
    if ch.local_echo {
        flags.push("ECHO".into());
    }
    if ch.is_logging() {
        flags.push("LOG".into());
    }
    Line::from(vec![
        Span::raw(format!("{mark}{} ", ch.name())),
        state,
        modem,
        Span::raw(format!(
            "{path}  {}bps {}  [{}]  RX:{} TX:{}  {}",
            ch.cfg.baud,
            ch.cfg.format_label(),
            flags.join(" "),
            ch.rx_bytes,
            ch.tx_bytes,
            ch.status
        )),
    ])
}

fn status_line(app: &App) -> Line<'static> {
    let key = Style::new().fg(Color::Black).bg(Color::Gray);
    match app.mode {
        Mode::Prefix => Line::from(vec![
            Span::styled(" Ctrl-A ", Style::new().fg(Color::Black).bg(Color::Yellow)),
            Span::raw(" Tab:切替 b:bps p:ポート i:モデム名 e:文字コード n:改行 l:エコー c:消去 r:再接続 x:切断 H:回線切断 u:送信 d:受信 L:ログ z:最大化 [:履歴 ?:ヘルプ q:終了"),
        ]),
        Mode::Scroll => Line::from(vec![
            Span::styled(" 履歴 ", Style::new().fg(Color::Black).bg(Color::Cyan)),
            Span::raw(format!(
                " {} 行上  ↑↓/PgUp/PgDn/g/G で移動  Esc/q で戻る",
                app.scroll
            )),
        ]),
        Mode::Normal => Line::from(vec![
            Span::styled(" Ctrl-A ", key),
            Span::raw(" コマンド  "),
            Span::styled(" Ctrl-A Tab ", key),
            Span::raw(" 画面切替  "),
            Span::styled(" Ctrl-A b ", key),
            Span::raw(" bps  "),
            Span::styled(" Ctrl-A u ", key),
            Span::raw(" 送信  "),
            Span::styled(" Ctrl-A d ", key),
            Span::raw(" 受信  "),
            Span::styled(" Ctrl-A ? ", key),
            Span::raw(" ヘルプ  "),
            Span::styled(" Ctrl-A q ", key),
            Span::raw(" 終了"),
        ]),
    }
}

/// 画面ごとの既定背景色 (A: 紺, B: えんじ)
const PANE_BG: [Color; 2] = [Color::Rgb(0, 0, 80), Color::Rgb(64, 8, 16)];
/// タイトル行の背景色 (アクティブ, 非アクティブ) A: 青系, B: 緑系
const TITLE_BG: [(Color, Color); 2] = [
    (Color::Rgb(32, 80, 208), Color::Rgb(16, 32, 80)),
    (Color::Rgb(24, 144, 64), Color::Rgb(16, 56, 24)),
];
/// 既定の文字色 (背景色を固定するので端末のテーマに依存させない)
const PANE_FG: Color = Color::Rgb(224, 224, 224);

fn vt_color(c: vt100::Color, default: Color) -> Color {
    match c {
        vt100::Color::Default => default,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn render_screen(screen: &vt100::Screen, area: Rect, buf: &mut Buffer, bg: Color) {
    for row in 0..area.height {
        for col in 0..area.width {
            let Some(cell) = screen.cell(row, col) else { continue };
            if cell.is_wide_continuation() {
                continue;
            }
            let mut style = Style::new().fg(vt_color(cell.fgcolor(), PANE_FG)).bg(vt_color(cell.bgcolor(), bg));
            if cell.bold() {
                style = style.add_modifier(Modifier::BOLD);
            }
            if cell.italic() {
                style = style.add_modifier(Modifier::ITALIC);
            }
            if cell.underline() {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            if cell.inverse() {
                style = style.add_modifier(Modifier::REVERSED);
            }
            let contents = cell.contents();
            let sym = if contents.is_empty() { " " } else { contents.as_str() };
            let x = area.x + col;
            let y = area.y + row;
            if cell.is_wide() && col + 1 >= area.width {
                // 右端に全角が収まらない
                buf[(x, y)].set_symbol(" ").set_style(style);
            } else {
                buf.set_stringn(x, y, sym, (area.width - col) as usize, style);
            }
        }
    }
}

fn human(n: u64) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1}MB", n as f64 / 1048576.0)
    } else if n >= 1024 {
        format!("{:.1}KB", n as f64 / 1024.0)
    } else {
        format!("{n}B")
    }
}

/// 転送中の進捗バー (画面の最下行)
fn draw_progress(f: &mut Frame, t: &Transfer, area: Rect) {
    let p = &t.progress;
    let file = if p.file.is_empty() { "開始待ち".to_string() } else { p.file.clone() };
    let size = match p.total {
        Some(total) => format!("{} / {}", human(p.bytes), human(total)),
        None => human(p.bytes),
    };
    let ratio = match p.total {
        Some(total) if total > 0 => (p.bytes as f64 / total as f64).clamp(0.0, 1.0),
        Some(_) => 1.0,
        None => 0.0,
    };
    let label = format!(
        "{} {}  {file}  {size}  再送:{}  Esc で中止",
        t.protocol.label(),
        t.direction.label(),
        p.errors
    );
    let gauge = Gauge::default()
        .gauge_style(Style::new().fg(Color::Rgb(40, 120, 200)).bg(Color::Rgb(20, 20, 40)))
        .ratio(ratio)
        .label(Span::styled(label, Style::new().fg(Color::White).add_modifier(Modifier::BOLD)));
    f.render_widget(gauge, area);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

fn draw_popup(f: &mut Frame, popup: &Popup, ch: &Channel) {
    let block = |t: String| {
        Block::default()
            .borders(Borders::ALL)
            .title(t)
            .style(Style::new().bg(Color::Black).fg(Color::White))
    };
    let hl = Style::new().bg(Color::Blue).add_modifier(Modifier::BOLD);
    match popup {
        Popup::Baud { sel, custom } => {
            let area = centered(f.area(), 32, BAUD_RATES.len() as u16 + 4);
            f.render_widget(Clear, area);
            let b = block(format!(" {} の bps ", ch.name()));
            let inner = b.inner(area);
            f.render_widget(b, area);
            let [list_area, input] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
            let items: Vec<ListItem> = BAUD_RATES
                .iter()
                .map(|b| {
                    let cur = if *b == ch.cfg.baud { " *" } else { "" };
                    ListItem::new(format!("{b:>8}{cur}"))
                })
                .collect();
            let mut st = ListState::default().with_selected(Some(*sel));
            f.render_stateful_widget(List::new(items).highlight_style(hl), list_area, &mut st);
            let text = if custom.is_empty() {
                "数字入力で任意値 / Enter".to_string()
            } else {
                format!("任意: {custom}_")
            };
            f.render_widget(Paragraph::new(text).style(Style::new().fg(Color::Yellow)), input);
        }
        Popup::Port { ports, sel } => {
            let area = centered(f.area(), 50, ports.len().max(1) as u16 + 2);
            f.render_widget(Clear, area);
            let b = block(format!(" {} のポート (Enter で接続) ", ch.name()));
            if ports.is_empty() {
                f.render_widget(Paragraph::new("シリアルポートが見つかりません").block(b), area);
            } else {
                let items: Vec<ListItem> = ports.iter().map(|p| ListItem::new(p.as_str())).collect();
                let mut st = ListState::default().with_selected(Some(*sel));
                f.render_stateful_widget(List::new(items).block(b).highlight_style(hl), area, &mut st);
            }
        }
        Popup::Transfer { dir, proto, path, error } => {
            let area = centered(f.area(), 64, 9);
            f.render_widget(Clear, area);
            let b = block(format!(" {} ファイル{} ", ch.name(), dir.label()));
            let inner = b.inner(area);
            f.render_widget(b, area);
            let mut protos = vec![Span::raw(" プロトコル: ")];
            for (i, p) in Protocol::ALL.iter().enumerate() {
                let style = if i == *proto { hl } else { Style::new().fg(Color::Gray) };
                protos.push(Span::styled(format!(" {} ", p.label()), style));
                protos.push(Span::raw(" "));
            }
            let what = match (dir, Protocol::ALL[*proto]) {
                (Direction::Send, Protocol::Ymodem) => "送るファイル (空白区切りで複数可)",
                (Direction::Send, _) => "送るファイル",
                (Direction::Recv, Protocol::Ymodem) => "保存先ディレクトリ",
                (Direction::Recv, _) => "保存するファイル名",
            };
            let lines = vec![
                Line::from(protos),
                Line::raw(""),
                Line::raw(format!(" {what}:")),
                Line::from(Span::styled(format!(" {path}_"), Style::new().fg(Color::Yellow))),
                Line::raw(""),
                match error {
                    Some(e) => Line::from(Span::styled(format!(" {e}"), Style::new().fg(Color::LightRed))),
                    None => Line::from(Span::styled(
                        " ←→ プロトコル切替  Enter 開始  Esc キャンセル",
                        Style::new().fg(Color::DarkGray),
                    )),
                },
            ];
            f.render_widget(Paragraph::new(lines), inner);
        }
        Popup::Help => {
            let lines = [
                "Ctrl-A をプレフィックスにして以下のキー",
                "",
                "  Tab / o / ↑↓  上下画面の切替",
                "  1 / 2         A(上) / B(下) を選択",
                "  b             bps 設定",
                "  p             ポート選択・接続",
                "  r / x         再接続 / 切断",
                "  i             モデム名を取得 (ATI3)",
                "  u / d         X/YMODEM 送信 / 受信",
                "  H             DTR OFF で回線切断",
                "  e             文字コード (SJIS/UTF-8/EUC/JIS)",
                "  n             改行コード (CR/CRLF/LF)",
                "  h             BackSpace を BS/DEL 切替",
                "  l             ローカルエコー",
                "  L             受信ログ保存 開始/停止",
                "  c             画面消去",
                "  Ctrl-L        表示の描き直し",
                "  z             アクティブ画面を最大化",
                "  [ / PgUp      スクロールバック",
                "  Ctrl-A        0x01 を送信",
                "  q             終了",
                "",
                "何かキーを押すと閉じます",
            ];
            let area = centered(f.area(), 48, lines.len() as u16 + 2);
            f.render_widget(Clear, area);
            f.render_widget(Paragraph::new(lines.join("\n")).block(block(" ヘルプ ".into())), area);
        }
    }
}
