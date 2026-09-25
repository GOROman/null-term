//! 外部制御: Unix ドメインソケット上の JSON Lines プロトコルと CLI クライアント
//!
//! 1 リクエスト = 1 行の JSON、1 レスポンス = 1 行の JSON。
//! 例: {"cmd":"send","ch":"A","data":"ATDT2\r"} → {"ok":true,"mark":123}

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::Subcommand;
use regex::Regex;
use serde_json::{json, Value};

use crate::channel::{self, encoding_label, Newline};
use crate::App;

pub fn default_socket() -> PathBuf {
    if let Ok(p) = std::env::var("NULL_TERM_SOCK") {
        return p.into();
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    std::env::temp_dir().join(format!("null-term-{user}.sock"))
}

pub struct CtlRequest {
    pub req: Value,
    pub reply: Sender<Value>,
}

/// 保留中の wait
pub struct PendingWait {
    ch: usize,
    re: Regex,
    from: usize,
    deadline: Instant,
    reply: Sender<Value>,
}

pub fn spawn_server(path: &Path, tx: Sender<CtlRequest>) -> Result<()> {
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            bail!("制御ソケットは使用中です (別の null-term が起動中?): {}", path.display());
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)
        .with_context(|| format!("制御ソケットを作成できません: {}", path.display()))?;
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let tx = tx.clone();
            thread::spawn(move || serve(stream, tx));
        }
    });
    Ok(())
}

fn serve(stream: UnixStream, tx: Sender<CtlRequest>) {
    let Ok(mut out) = stream.try_clone() else { return };
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Value>(&line) {
            Ok(req) => {
                let (rtx, rrx) = mpsc::channel();
                if tx.send(CtlRequest { req, reply: rtx }).is_err() {
                    break;
                }
                rrx.recv().unwrap_or_else(|_| err("内部エラー"))
            }
            Err(e) => err(format!("JSON が不正: {e}")),
        };
        if writeln!(out, "{resp}").is_err() {
            break;
        }
    }
}

fn err(msg: impl std::fmt::Display) -> Value {
    json!({ "ok": false, "error": msg.to_string() })
}

/// ch: "A"/"B" または 1/2。省略時はアクティブ画面
fn parse_ch(v: &Value, app: &App) -> Result<usize> {
    let s = match v.get("ch") {
        None | Some(Value::Null) => return Ok(app.active),
        Some(Value::String(s)) => s.to_ascii_uppercase(),
        Some(Value::Number(n)) => n.to_string(),
        Some(o) => bail!("ch が不正: {o}"),
    };
    match s.as_str() {
        "A" | "1" => Ok(0),
        "B" | "2" => Ok(1),
        _ => bail!("ch は A / B (または 1 / 2): {s}"),
    }
}

fn channel_status(app: &App, i: usize) -> Value {
    let ch = &app.channels[i];
    let (row, col) = ch.parser.screen().cursor_position();
    json!({
        "ch": ch.name().to_string(),
        "active": app.active == i,
        "path": ch.cfg.path,
        "open": ch.is_open(),
        "baud": ch.cfg.baud,
        "format": ch.cfg.format_label(),
        "encoding": encoding_label(ch.encoding),
        "newline": ch.newline.label(),
        "echo": ch.local_echo,
        "logging": ch.log_path,
        "rx_bytes": ch.rx_bytes,
        "tx_bytes": ch.tx_bytes,
        "rx_offset": ch.rx_offset(),
        "mark": ch.mark,
        "cursor": [row, col],
        "status": ch.status,
        "reconnecting": ch.is_reconnecting(),
        "modem": ch.modem,
    })
}

/// リクエストを処理する。wait のように保留するものは None を返し pending に積む
pub fn handle(app: &mut App, r: CtlRequest, pending: &mut Vec<PendingWait>) {
    match dispatch(app, &r.req) {
        Ok(Dispatch::Reply(v)) => {
            let _ = r.reply.send(v);
        }
        Ok(Dispatch::Wait { ch, re, from, timeout }) => {
            let w = PendingWait { ch, re, from, deadline: Instant::now() + timeout, reply: r.reply };
            pending.push(w);
            poll_waits(app, pending);
        }
        Err(e) => {
            let _ = r.reply.send(err(format!("{e:#}")));
        }
    }
}

enum Dispatch {
    Reply(Value),
    Wait { ch: usize, re: Regex, from: usize, timeout: Duration },
}

fn str_arg<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key).and_then(Value::as_str).ok_or_else(|| anyhow!("{key} が必要です"))
}

fn dispatch(app: &mut App, v: &Value) -> Result<Dispatch> {
    let cmd = str_arg(v, "cmd")?;
    let i = parse_ch(v, app)?;
    let tx = app.tx.clone();
    let ok = |extra: Value| {
        let mut o = json!({ "ok": true });
        if let (Some(o), Value::Object(e)) = (o.as_object_mut(), extra) {
            o.extend(e);
        }
        Ok(Dispatch::Reply(o))
    };
    let ch = &mut app.channels[i];
    match cmd {
        "status" => {
            let chans: Vec<Value> = (0..2).map(|i| channel_status(app, i)).collect();
            ok(json!({ "channels": chans }))
        }
        "send" | "sendhex" => {
            let bytes = if cmd == "send" {
                let (b, _, _) = ch.encoding.encode(str_arg(v, "data")?);
                b.into_owned()
            } else {
                hex_decode(str_arg(v, "data")?)?
            };
            if !ch.is_open() {
                bail!("{} は未接続です", ch.name());
            }
            // 送信直前の位置を mark にして、応答を取りこぼさないようにする
            ch.mark = ch.rx_offset();
            ch.write_raw(&bytes);
            if !ch.is_open() {
                bail!("{}", ch.status);
            }
            ok(json!({ "mark": ch.mark, "bytes": bytes.len() }))
        }
        "wait" => {
            let re = Regex::new(str_arg(v, "pattern")?)?;
            let timeout = v.get("timeout").and_then(Value::as_f64).unwrap_or(10.0);
            let from = match v.get("from") {
                Some(Value::String(s)) if s == "now" => ch.rx_offset(),
                Some(Value::Number(n)) => n.as_u64().unwrap_or(0) as usize,
                _ => ch.mark,
            };
            Ok(Dispatch::Wait { ch: i, re, from, timeout: Duration::from_secs_f64(timeout.max(0.0)) })
        }
        "read" => {
            // mark 以降の受信テキストを返し、mark を末尾に進める
            let from = v.get("from").and_then(Value::as_u64).map(|n| n as usize).unwrap_or(ch.mark);
            let (start, text) = ch.rx_text_since(from);
            let text = text.to_string();
            ch.mark = ch.rx_offset();
            ok(json!({ "from": start, "text": text, "mark": ch.mark }))
        }
        "screen" => {
            let screen = ch.parser.screen();
            let (row, col) = screen.cursor_position();
            ok(json!({ "text": screen.contents(), "cursor": [row, col] }))
        }
        "baud" => {
            let baud = v.get("baud").and_then(Value::as_u64).ok_or_else(|| anyhow!("baud が必要です"))?;
            ch.set_baud(baud as u32);
            ok(channel_status(app, i))
        }
        "format" => {
            ch.cfg.set_format(str_arg(v, "format")?)?;
            if ch.is_open() {
                ch.open(&tx);
            }
            ok(channel_status(app, i))
        }
        "open" => {
            if let Some(p) = v.get("path").and_then(Value::as_str) {
                ch.cfg.path = Some(p.to_string());
            }
            if let Some(b) = v.get("baud").and_then(Value::as_u64) {
                ch.cfg.baud = b as u32;
            }
            ch.open(&tx);
            if !ch.is_open() {
                bail!("{}", ch.status);
            }
            ok(channel_status(app, i))
        }
        "hangup" => {
            ch.hangup();
            ok(channel_status(app, i))
        }
        "identify" => {
            ch.probe_modem();
            ok(json!({}))
        }
        "close" => {
            ch.close();
            ok(channel_status(app, i))
        }
        "encoding" => {
            ch.set_encoding(channel::parse_encoding(str_arg(v, "encoding")?)?);
            ok(channel_status(app, i))
        }
        "newline" => {
            ch.newline = Newline::parse(str_arg(v, "newline")?)?;
            ok(channel_status(app, i))
        }
        "echo" => {
            ch.local_echo = v.get("on").and_then(Value::as_bool).unwrap_or(!ch.local_echo);
            ok(channel_status(app, i))
        }
        "clear" => {
            ch.clear();
            ok(json!({}))
        }
        "log" => {
            let want = v.get("on").and_then(Value::as_bool).unwrap_or(!ch.is_logging());
            if want != ch.is_logging() {
                ch.toggle_log();
            }
            ok(channel_status(app, i))
        }
        "redraw" => {
            app.redraw = true;
            ok(json!({}))
        }
        "reset" => {
            for ch in app.channels.iter_mut() {
                ch.clear();
            }
            app.redraw = true;
            ok(json!({}))
        }
        "focus" => {
            app.active = i;
            ok(json!({}))
        }
        "ports" => ok(json!({ "ports": crate::list_ports() })),
        "quit" => {
            app.quit = true;
            ok(json!({}))
        }
        _ => bail!("不明なコマンド: {cmd}"),
    }
}

/// 保留中の wait を検査し、成立 / タイムアウトしたものに応答する
pub fn poll_waits(app: &mut App, pending: &mut Vec<PendingWait>) {
    let now = Instant::now();
    pending.retain(|w| {
        let ch = &mut app.channels[w.ch];
        let (start, text) = ch.rx_text_since(w.from);
        if let Some(m) = w.re.find(text) {
            let resp = json!({
                "ok": true,
                "match": m.as_str(),
                "text": &text[..m.end()],
                "from": start,
            });
            // 次の wait は一致した直後から
            ch.mark = start + m.end();
            let _ = w.reply.send(resp);
            return false;
        }
        // 自動再接続中は待ち続ける
        if now >= w.deadline || (!ch.is_open() && !ch.is_reconnecting()) {
            let reason = if ch.is_open() { "タイムアウト".to_string() } else { format!("切断: {}", ch.status) };
            let _ = w.reply.send(json!({ "ok": false, "error": reason, "text": text, "from": start }));
            return false;
        }
        true
    });
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if s.len() % 2 != 0 {
        bail!("hex の桁数が奇数です");
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| anyhow!("hex が不正: {e}")))
        .collect()
}

/// `\r` `\n` `\t` `\e` `\xNN` `\\` を展開する (それ以外の `\X` はそのまま)
pub fn unescape(s: &str) -> Result<String> {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('e') => out.push('\x1b'),
            Some('0') => out.push('\0'),
            Some('\\') => out.push('\\'),
            Some('x') => {
                let h: String = it.by_ref().take(2).collect();
                let b = u8::from_str_radix(&h, 16).map_err(|_| anyhow!("\\x の後は 16 進 2 桁: {h}"))?;
                if b >= 0x80 {
                    bail!("\\x80 以上は sendhex を使ってください");
                }
                out.push(b as char);
            }
            // AT\N0 などのため、知らないエスケープはそのまま通す
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    Ok(out)
}

#[derive(Subcommand, Debug)]
pub enum CtlCmd {
    /// 両チャンネルの状態を表示 (JSON)
    Status,
    /// テキストを送信 (\r \n \t \e \xNN を展開)。--expect で応答待ちも行う
    Send {
        ch: String,
        data: String,
        /// 送信後、この正規表現に一致する受信を待つ
        #[arg(short = 'x', long)]
        expect: Option<String>,
        #[arg(short, long, default_value_t = 10.0)]
        timeout: f64,
    },
    /// 16 進でバイト列を送信 (例: 1b5b41)
    Sendhex { ch: String, hex: String },
    /// 受信テキストが正規表現に一致するまで待つ (直前の send / wait 以降を検索)
    Wait {
        ch: String,
        pattern: String,
        #[arg(short, long, default_value_t = 10.0)]
        timeout: f64,
        /// 過去の受信を無視し、今から後だけを検索
        #[arg(long)]
        now: bool,
    },
    /// 前回の read / send / wait 以降の受信テキストを取得
    Read { ch: String },
    /// 画面の内容をテキストで取得
    Screen { ch: String },
    /// bps を変更
    Baud { ch: String, baud: u64 },
    /// データ形式を変更 (8N1 など)
    Format { ch: String, format: String },
    /// ポートを開く (パス省略で再接続)
    Open {
        ch: String,
        path: Option<String>,
        #[arg(short, long)]
        baud: Option<u64>,
    },
    /// ポートを閉じる
    Close { ch: String },
    /// DTR を一旦落としてモデムに回線を切らせる
    Hangup { ch: String },
    /// ATI3 でモデム名を再取得 (結果は status の modem)
    Identify { ch: String },
    /// 文字コード (sjis / utf8 / eucjp / jis)
    Encoding { ch: String, encoding: String },
    /// 改行コード (cr / crlf / lf)
    Newline { ch: String, newline: String },
    /// ローカルエコー on / off
    Echo { ch: String, on: String },
    /// 受信ログ on / off
    Log { ch: String, on: String },
    /// 画面消去
    Clear { ch: String },
    /// 端末の表示を全消去して描き直す
    Redraw,
    /// 両画面の内容を消去して描き直す
    Reset,
    /// アクティブ画面を切替
    Focus { ch: String },
    /// 利用可能なポート一覧
    Ports,
    /// null-term を終了
    Quit,
    /// JSON リクエストをそのまま送る
    Raw { json: String },
}

fn on_off(s: &str) -> Result<bool> {
    match s.to_ascii_lowercase().as_str() {
        "on" | "1" | "true" | "yes" => Ok(true),
        "off" | "0" | "false" | "no" => Ok(false),
        _ => bail!("on / off で指定: {s}"),
    }
}

fn request(stream: &mut UnixStream, reader: &mut BufReader<UnixStream>, req: &Value) -> Result<Value> {
    writeln!(stream, "{req}")?;
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.is_empty() {
        bail!("null-term から応答がありません");
    }
    Ok(serde_json::from_str(&line)?)
}

/// CLI クライアント。戻り値はプロセスの終了コード
pub fn client(socket: &Path, cmd: CtlCmd) -> Result<i32> {
    let mut stream = UnixStream::connect(socket).with_context(|| {
        format!("null-term に接続できません ({}). 起動していますか?", socket.display())
    })?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut plain_text = false;
    let mut reqs = vec![];
    match cmd {
        CtlCmd::Status => reqs.push(json!({"cmd": "status"})),
        CtlCmd::Send { ch, data, expect, timeout } => {
            reqs.push(json!({"cmd": "send", "ch": ch, "data": unescape(&data)?}));
            if let Some(p) = expect {
                reqs.push(json!({"cmd": "wait", "ch": ch, "pattern": p, "timeout": timeout}));
                plain_text = true;
            }
        }
        CtlCmd::Sendhex { ch, hex } => reqs.push(json!({"cmd": "sendhex", "ch": ch, "data": hex})),
        CtlCmd::Wait { ch, pattern, timeout, now } => {
            let mut r = json!({"cmd": "wait", "ch": ch, "pattern": pattern, "timeout": timeout});
            if now {
                r["from"] = json!("now");
            }
            reqs.push(r);
            plain_text = true;
        }
        CtlCmd::Read { ch } => {
            reqs.push(json!({"cmd": "read", "ch": ch}));
            plain_text = true;
        }
        CtlCmd::Screen { ch } => {
            reqs.push(json!({"cmd": "screen", "ch": ch}));
            plain_text = true;
        }
        CtlCmd::Baud { ch, baud } => reqs.push(json!({"cmd": "baud", "ch": ch, "baud": baud})),
        CtlCmd::Format { ch, format } => reqs.push(json!({"cmd": "format", "ch": ch, "format": format})),
        CtlCmd::Open { ch, path, baud } => {
            reqs.push(json!({"cmd": "open", "ch": ch, "path": path, "baud": baud}))
        }
        CtlCmd::Close { ch } => reqs.push(json!({"cmd": "close", "ch": ch})),
        CtlCmd::Hangup { ch } => reqs.push(json!({"cmd": "hangup", "ch": ch})),
        CtlCmd::Identify { ch } => reqs.push(json!({"cmd": "identify", "ch": ch})),
        CtlCmd::Encoding { ch, encoding } => {
            reqs.push(json!({"cmd": "encoding", "ch": ch, "encoding": encoding}))
        }
        CtlCmd::Newline { ch, newline } => {
            reqs.push(json!({"cmd": "newline", "ch": ch, "newline": newline}))
        }
        CtlCmd::Echo { ch, on } => reqs.push(json!({"cmd": "echo", "ch": ch, "on": on_off(&on)?})),
        CtlCmd::Log { ch, on } => reqs.push(json!({"cmd": "log", "ch": ch, "on": on_off(&on)?})),
        CtlCmd::Clear { ch } => reqs.push(json!({"cmd": "clear", "ch": ch})),
        CtlCmd::Focus { ch } => reqs.push(json!({"cmd": "focus", "ch": ch})),
        CtlCmd::Redraw => reqs.push(json!({"cmd": "redraw"})),
        CtlCmd::Reset => reqs.push(json!({"cmd": "reset"})),
        CtlCmd::Ports => reqs.push(json!({"cmd": "ports"})),
        CtlCmd::Quit => reqs.push(json!({"cmd": "quit"})),
        CtlCmd::Raw { json } => reqs.push(serde_json::from_str(&json)?),
    }
    let mut last = Value::Null;
    for req in &reqs {
        last = request(&mut stream, &mut reader, req)?;
        if last["ok"] != json!(true) {
            break;
        }
    }
    let ok = last["ok"] == json!(true);
    if plain_text {
        // wait / read / screen はテキストをそのまま出力 (失敗時も途中までの受信を出す)
        if let Some(t) = last["text"].as_str() {
            print!("{t}");
            if !t.ends_with('\n') {
                println!();
            }
        }
        if !ok {
            eprintln!("error: {}", last["error"].as_str().unwrap_or("?"));
        }
    } else if ok {
        println!("{}", serde_json::to_string_pretty(&last)?);
    } else {
        eprintln!("error: {}", last["error"].as_str().unwrap_or("?"));
    }
    Ok(if ok { 0 } else { 1 })
}
