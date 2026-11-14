//! A small Chrome DevTools Protocol client for the persistent full-mode
//! Chromium. It speaks just enough WebSocket and CDP to keep one page alive
//! and send it real touch, pinch and text input.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

const PORT: u16 = 9222;
const VIEW_W: u32 = 412;
const VIEW_H: u32 = 915;

pub struct Cdp {
    stream: TcpStream,
    next: u64,
}

impl Cdp {
    pub fn connect() -> Result<Cdp> {
        let deadline = Instant::now() + Duration::from_secs(15);
        let list = loop {
            match http_get("/json/list") {
                Ok(text) => break text,
                Err(e) if Instant::now() < deadline => {
                    let _ = e;
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(e) => return Err(e).context("wait for Chromium's debugging port"),
            }
        };
        let pages: Value = serde_json::from_str(&list).context("read Chromium's page list")?;
        let ws = pages
            .as_array()
            .and_then(|pages| pages.iter().find(|page| page["type"] == "page"))
            .and_then(|page| page["webSocketDebuggerUrl"].as_str())
            .context("Chromium lists no page to attach to")?;
        let path = ws.strip_prefix("ws://127.0.0.1:9222").or_else(|| ws.strip_prefix("ws://localhost:9222")).context("unexpected debugging socket")?;
        let mut stream = TcpStream::connect(("127.0.0.1", PORT)).context("connect to Chromium")?;
        stream.set_read_timeout(Some(Duration::from_secs(20)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        let key = "U2t5d2FyZC1icm93c2VyIQ==";
        write!(stream, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n")?;
        let headers = read_headers(&mut stream)?;
        if !headers.starts_with("HTTP/1.1 101") && !headers.starts_with("HTTP/1.0 101") {
            bail!("Chromium refused the debugging socket: {}", headers.lines().next().unwrap_or(""));
        }
        let mut cdp = Cdp { stream, next: 1 };
        cdp.call("Page.enable", json!({}))?;
        cdp.call(
            "Emulation.setDeviceMetricsOverride",
            json!({"width": VIEW_W, "height": VIEW_H, "deviceScaleFactor": 1, "mobile": true}),
        )?;
        cdp.call("Emulation.setTouchEmulationEnabled", json!({"enabled": true, "maxTouchPoints": 5}))?;
        Ok(cdp)
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next;
        self.next += 1;
        let message = json!({"id": id, "method": method, "params": params});
        write_frame(&mut self.stream, message.to_string().as_bytes())?;
        loop {
            let payload = read_frame(&mut self.stream)?;
            let answer: Value = match serde_json::from_slice(&payload) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if answer["id"].as_u64() != Some(id) {
                continue;
            }
            if let Some(error) = answer.get("error") {
                bail!("{method}: {error}");
            }
            return Ok(answer.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    pub fn navigate(&mut self, url: &str) -> Result<()> {
        self.call("Page.navigate", json!({"url": url}))?;
        std::thread::sleep(Duration::from_millis(1200));
        Ok(())
    }

    pub fn reload(&mut self) -> Result<()> {
        self.call("Page.reload", json!({}))?;
        std::thread::sleep(Duration::from_millis(900));
        Ok(())
    }

    pub fn touch(&mut self, x: u32, y: u32) -> Result<()> {
        let point = json!([{"x": x, "y": y, "id": 1, "radiusX": 2, "radiusY": 2, "force": 1.0}]);
        self.call("Input.dispatchTouchEvent", json!({"type": "touchStart", "touchPoints": point}))?;
        std::thread::sleep(Duration::from_millis(60));
        self.call("Input.dispatchTouchEvent", json!({"type": "touchEnd", "touchPoints": []}))?;
        std::thread::sleep(Duration::from_millis(500));
        Ok(())
    }

    pub fn pinch(&mut self, x: u32, y: u32, scale: f64) -> Result<()> {
        self.call(
            "Input.synthesizePinchGesture",
            json!({"x": x, "y": y, "scaleFactor": scale, "relativeSpeed": 800}),
        )?;
        std::thread::sleep(Duration::from_millis(600));
        Ok(())
    }

    pub fn insert_text(&mut self, text: &str) -> Result<()> {
        self.call("Input.insertText", json!({"text": text}))?;
        std::thread::sleep(Duration::from_millis(300));
        Ok(())
    }

    pub fn focus_is_text(&mut self) -> bool {
        let answer = self.call(
            "Runtime.evaluate",
            json!({"expression": "(()=>{const e=document.activeElement;return e?`${e.tagName}:${e.type||''}:${e.isContentEditable}`:''})()", "returnByValue": true}),
        );
        let Ok(answer) = answer else { return false };
        let value = answer["result"]["value"].as_str().unwrap_or("");
        value.starts_with("INPUT") || value.starts_with("TEXTAREA") || value.ends_with(":true")
    }

    pub fn screenshot(&mut self) -> Result<Vec<u8>> {
        let answer = self.call("Page.captureScreenshot", json!({"format": "png", "fromSurface": true}))?;
        let data = answer["data"].as_str().context("screenshot holds no data")?;
        base64::engine::general_purpose::STANDARD.decode(data).context("decode screenshot")
    }
}

fn http_get(path: &str) -> Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", PORT)).context("connect to Chromium")?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    write!(stream, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\nConnection: close\r\n\r\n")?;
    let mut bytes = Vec::new();
    let _ = stream.read_to_end(&mut bytes);
    let text = String::from_utf8_lossy(&bytes);
    let (_, body) = text.split_once("\r\n\r\n").context("Chromium answered without a body")?;
    Ok(body.to_string())
}

fn read_headers(stream: &mut TcpStream) -> Result<String> {
    let mut bytes = Vec::new();
    let mut one = [0u8; 1];
    while bytes.len() < 16384 {
        let n = stream.read(&mut one)?;
        if n == 0 {
            break;
        }
        bytes.push(one[0]);
        if bytes.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn write_frame(stream: &mut TcpStream, payload: &[u8]) -> Result<()> {
    let mut header = Vec::with_capacity(14);
    header.push(0x81);
    let mask = [0x53, 0x6b, 0x79, 0x77];
    if payload.len() < 126 {
        header.push(0x80 | payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        header.push(0x80 | 126);
        header.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        header.push(0x80 | 127);
        header.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    header.extend_from_slice(&mask);
    stream.write_all(&header)?;
    let mut masked = payload.to_vec();
    for (i, byte) in masked.iter_mut().enumerate() {
        *byte ^= mask[i % 4];
    }
    stream.write_all(&masked)?;
    stream.flush()?;
    Ok(())
}

fn read_frame(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut two = [0u8; 2];
    stream.read_exact(&mut two)?;
    let opcode = two[0] & 0x0f;
    let masked = two[1] & 0x80 != 0;
    let mut len = (two[1] & 0x7f) as u64;
    if len == 126 {
        let mut bytes = [0u8; 2];
        stream.read_exact(&mut bytes)?;
        len = u16::from_be_bytes(bytes) as u64;
    } else if len == 127 {
        let mut bytes = [0u8; 8];
        stream.read_exact(&mut bytes)?;
        len = u64::from_be_bytes(bytes);
    }
    if len > 16 * 1024 * 1024 {
        bail!("Chromium sent an oversized frame");
    }
    let mask = if masked {
        let mut mask = [0u8; 4];
        stream.read_exact(&mut mask)?;
        Some(mask)
    } else {
        None
    };
    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload)?;
    if let Some(mask) = mask {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[i % 4];
        }
    }
    if opcode == 0x8 {
        return Err(anyhow!("Chromium closed the debugging socket"));
    }
    if opcode == 0x9 {
        return read_frame(stream);
    }
    Ok(payload)
}

pub const WIDTH: u32 = VIEW_W;
pub const HEIGHT: u32 = VIEW_H;

#[allow(dead_code)]
fn accept_for(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}
