//! キー入力 → 送信バイト列の変換

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::channel::Channel;

pub fn key_to_bytes(k: &KeyEvent, ch: &Channel) -> Option<Vec<u8>> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let seq: &[u8] = match k.code {
        KeyCode::Char(c) => {
            let mut out = Vec::new();
            if alt {
                out.push(0x1b);
            }
            if ctrl {
                out.push(match c.to_ascii_lowercase() {
                    c @ 'a'..='z' => c as u8 & 0x1f,
                    '@' | ' ' | '2' => 0x00,
                    '[' | '3' => 0x1b,
                    '\\' | '4' => 0x1c,
                    ']' | '5' => 0x1d,
                    '^' | '6' => 0x1e,
                    '_' | '-' | '7' => 0x1f,
                    '?' | '8' => 0x7f,
                    _ => return None,
                });
            } else {
                let mut buf = [0u8; 4];
                let (bytes, _, _) = ch.encoding.encode(c.encode_utf8(&mut buf));
                out.extend_from_slice(&bytes);
            }
            return Some(out);
        }
        KeyCode::Enter => ch.newline.bytes(),
        KeyCode::Backspace => return Some(vec![ch.backspace]),
        KeyCode::Tab => b"\t",
        KeyCode::BackTab => b"\x1b[Z",
        KeyCode::Esc => b"\x1b",
        KeyCode::Up => b"\x1b[A",
        KeyCode::Down => b"\x1b[B",
        KeyCode::Right => b"\x1b[C",
        KeyCode::Left => b"\x1b[D",
        KeyCode::Home => b"\x1b[H",
        KeyCode::End => b"\x1b[F",
        KeyCode::Insert => b"\x1b[2~",
        KeyCode::Delete => b"\x1b[3~",
        KeyCode::PageUp => b"\x1b[5~",
        KeyCode::PageDown => b"\x1b[6~",
        KeyCode::F(n) => match n {
            1 => b"\x1bOP",
            2 => b"\x1bOQ",
            3 => b"\x1bOR",
            4 => b"\x1bOS",
            5 => b"\x1b[15~",
            6 => b"\x1b[17~",
            7 => b"\x1b[18~",
            8 => b"\x1b[19~",
            9 => b"\x1b[20~",
            10 => b"\x1b[21~",
            11 => b"\x1b[23~",
            12 => b"\x1b[24~",
            _ => return None,
        },
        _ => return None,
    };
    Some(seq.to_vec())
}
