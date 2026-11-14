//! The browser's pages: the start page, a page read as text and links, the
//! address field, and the files saved.

use crate::app::{Action, BrowserFull, BrowserSave, BrowserView, Dock, Frame, Hit, Rows, Screen};
use crate::canvas::Canvas;
use crate::color::{mix, Rgba};
use crate::fmt;
use crate::font::Fonts;
use crate::geom::Rect;
use crate::icons::{self, Icon};
use crate::lang::{Lang, Text};
use crate::pages::{Band, Ctx};
use crate::theme::Theme;
use crate::tr;
use crate::widgets::{self, Row, Trail};
use skyward_net::web::{self, Block as WebBlock, Page as WebPage};

const BROWSER: Text = ["浏览器", "瀏覽器", "Browser"];
const OPEN_ADDRESS: Text = ["打开网址", "開啟網址", "Open address"];
const HINT: Text = ["输入网址或搜索词", "輸入網址或搜尋詞", "Enter an address or search for something"];
const GO: Text = ["访问", "前往", "Go"];
const CLEAR: Text = ["清空", "清除", "Clear"];
const SEARCH: Text = ["搜索", "搜尋", "Search"];
const DOWNLOADS: Text = ["下载的文件", "下載的檔案", "Downloads"];
const BACK: Text = ["后退", "上一頁", "Back"];
const FORWARD: Text = ["前进", "下一頁", "Forward"];
const RELOAD: Text = ["刷新", "重新整理", "Reload"];
const DOWNLOAD: Text = ["下载", "下載", "Download"];
const RETRY: Text = ["重试", "重試", "Try again"];
const NOT_NOW: Text = ["输入其他网址", "輸入其他網址", "Open another address"];
const NO_DOWNLOADS: Text = ["还没有下载的文件", "還沒有下載的檔案", "Nothing has been downloaded yet"];

const BLUE: Rgba = Rgba::rgb(0x34, 0x82, 0xff);
const GREEN: Rgba = Rgba::rgb(0x1d, 0xcd, 0x3a);
const PURPLE: Rgba = Rgba::rgb(0x77, 0x67, 0xf9);
const ORANGE: Rgba = Rgba::rgb(0xff, 0x69, 0x00);
const RED: Rgba = Rgba::rgb(0xfa, 0x38, 0x2e);

/// The sites the start page offers, now kept in the browser's state.

fn sheet(cx: &Ctx, h: u32) -> Canvas {
    let w = cx.t.m.w;
    let Band { rows, cols } = cx.band.unwrap_or(Band { rows: (0, h), cols: (0, w) });
    Canvas::band(w, h, rows.0..rows.1, cols.0..cols.1, cx.t.pal.bg)
}

fn page(cx: &Ctx) -> Canvas {
    sheet(cx, cx.t.m.h)
}

fn long_page(cx: &Ctx) -> Canvas {
    sheet(cx, cx.t.m.h * 4)
}

fn fit(c: Canvas, t: &Theme, bottom: f32) -> Canvas {
    c.crop_height(((bottom + t.m.margin * 2.0).ceil() as u32).max(t.m.h))
}

fn hits(rects: &[Rect], actions: &[Action]) -> Vec<Hit> {
    rects.iter().zip(actions).map(|(&rect, &action)| Hit { rect, action }).collect()
}

/// The browser's page, in whatever state it stands.
pub fn screen(cx: &Ctx) -> Frame {
    match &cx.state.browser.view {
        BrowserView::Home => home(cx),
        BrowserView::Loading(url) => loading(cx, url),
        BrowserView::Full => full_page(cx),
        BrowserView::Failed { url, why } => failed(cx, url, why),
        BrowserView::Page(article) => read(cx, article),
        BrowserView::File(file) => {
            let mut c = page(cx);
            let top = cx.t.m.content_top;
            let mut out = Vec::new();
            let y = address_bar(&mut c, cx, top, &file.name);
            out.push(Hit { rect: rect_bar(cx, top), action: Action::BrowserAddress });
            let mut y = y + cx.t.m.card_gap;
            y = widgets::section(&mut c, cx.t, cx.f, y - cx.t.m.card_gap, cx.state.lang.pick(["文件", "檔案", "File"]));
            let size = file.size.map(fmt::file_size).unwrap_or_else(|| cx.state.lang.pick(["大小未知", "大小未知", "Size unknown"]).to_string());
            let kind = &file.kind;
            let rows = [
                Row::new(&file.name).icon(GREEN, Icon::Install).trail(Trail::None),
                Row::new(cx.state.lang.pick(["大小", "大小", "Size"])).trail(Trail::Text(&size)),
                Row::new(cx.state.lang.pick(["类型", "類型", "Type"])).trail(Trail::Text(kind)),
            ];
            let list = widgets::list(&mut c, cx.t, cx.f, y, &rows);
            y = list.bottom;
            let note = cx.state.lang.pick([
                "这是文件，不是网页。可下载后用文件管理打开，zip 和镜像可直接安装。",
                "這是檔案，不是網頁。可下載後用檔案管理開啟，zip 和映像檔可直接安裝。",
                "This is a file, not a page. Download it and open it in Files; a zip or an image installs from there.",
            ]);
            y = widgets::footnote(&mut c, cx.t, cx.f, y, note);
            let row = Row::new(cx.state.lang.pick(DOWNLOAD)).icon(BLUE, Icon::Install).trail(Trail::Arrow);
            let download = widgets::list(&mut c, cx.t, cx.f, y + cx.t.m.card_gap, &[row]);
            out.push(Hit { rect: download.rows[0], action: Action::BrowserSaveFile });
            y = download.bottom;
            let mut frame = Frame::new(cx.t, fit(c, cx.t, y), out, cx.state.lang.pick(BROWSER), top);
            frame.dock = Some(dock(cx));
            frame
        }
    }
}

fn rect_bar(cx: &Ctx, top: f32) -> Rect {
    let m = &cx.t.m;
    Rect::new(m.margin, top, m.w as f32 - m.margin * 2.0, m.dp(56.0))
}

/// An address bar at `top`, saying `text`, or the hint where it is empty.
fn address_bar(c: &mut Canvas, cx: &Ctx, top: f32, text: &str) -> f32 {
    let (t, f, l) = (cx.t, cx.f, cx.state.lang);
    let m = &t.m;
    let r = rect_bar(cx, top);
    c.fill_round_rect(r.x, r.y, r.w, r.h, m.dp(28.0), t.pal.card);
    let glyph = m.dp(23.0);
    icons::glyph(c, Icon::Globe, r.x + m.inner + glyph / 2.0, r.y + r.h / 2.0, glyph, t.pal.accent);
    let x = r.x + m.inner + glyph + m.dp(12.0);
    let max = r.w - (x - r.x) - m.inner - m.dp(10.0);
    let (shown, color) = if text.is_empty() { (l.pick(HINT), t.pal.placeholder) } else { (text, t.pal.text) };
    c.text_vc(&f.body, shown, x, r.y + r.h / 2.0, m.row_title_px, color, Some(max));
    r.y + r.h
}

/// The large address bar at the foot of the start page's hero, and the
/// hero itself.
fn home(cx: &Ctx) -> Frame {
    let (t, f, s, l) = (cx.t, cx.f, cx.state, cx.state.lang);
    let m = &t.m;
    let mut c = long_page(cx);
    let mut out = Vec::new();
    let top = m.content_top;
    let w = m.w as f32 - m.margin * 2.0;
    let hero_h = m.dp(132.0);
    c.fill_round_rect_graded(m.margin, top, w, hero_h, m.dp(28.0), &|fy| mix(BLUE, PURPLE, (fy - top) / hero_h));
    let disc = m.dp(44.0);
    let dcx = m.margin + w - m.dp(52.0);
    let dcy = top + hero_h / 2.0;
    c.fill_circle(dcx, dcy, disc, Rgba::rgb(255, 255, 255).with_alpha(38));
    icons::glyph(&mut c, Icon::Globe, dcx, dcy, m.dp(50.0), Rgba::rgb(255, 255, 255));
    let x = m.margin + m.dp(24.0);
    let white = Rgba::rgb(255, 255, 255);
    c.text_top(&f.heading, "Skyward", x, top + m.dp(30.0), m.dp(26.0), white, Some(w - m.dp(130.0)));
    let tagline = l.pick(["浏览器", "瀏覽器", "Browser"]);
    c.text_top(&f.body, tagline, x, top + m.dp(72.0), m.dp(16.0), white.with_alpha(220), Some(w - m.dp(130.0)));
    let hint = l.pick(["输入网址，下载刷机包", "輸入網址，下載刷機包", "Addresses, pages and files"]);
    c.text_top(&f.body, hint, x, top + m.dp(98.0), m.dp(13.0), white.with_alpha(185), Some(w - m.dp(130.0)));
    out.push(Hit { rect: Rect::new(m.margin, top, w, hero_h), action: Action::BrowserAddress });

    let bar = Rect::new(m.margin, top + hero_h + m.dp(14.0), w, m.dp(56.0));
    address_bar(&mut c, cx, bar.y, "");
    out.push(Hit { rect: bar, action: Action::BrowserAddress });
    let mut y = bar.y + bar.h + m.dp(14.0);

    y = widgets::section(&mut c, t, f, y, l.pick(["快捷访问", "快速存取", "Shortcuts"]));
    let gap = m.dp(10.0);
    let cols = 4;
    let tile_w = (w - gap * (cols - 1) as f32) / cols as f32;
    let tile_h = m.dp(106.0);
    for (k, q) in s.browser.quick.iter().enumerate() {
        let row = k / cols;
        let col = k % cols;
        let tx = m.margin + col as f32 * (tile_w + gap);
        let ty = y + row as f32 * (tile_h + gap);
        c.fill_round_rect(tx, ty, tile_w, tile_h, m.dp(20.0), t.pal.card);
        let ccx = tx + tile_w / 2.0;
        let ccy = ty + m.dp(36.0);
        c.fill_circle(ccx, ccy, m.dp(22.0), q.color);
        icons::glyph(&mut c, q.icon, ccx, ccy, m.dp(26.0), Rgba::rgb(255, 255, 255));
        c.text_center(&f.body, &q.title, ccx, ty + m.dp(82.0), m.dp(13.0), t.pal.text);
        out.push(Hit { rect: Rect::new(tx, ty, tile_w, tile_h), action: Action::BrowserQuickOpen(k) });
    }
    let rows = s.browser.quick.len().div_ceil(cols);
    y += rows.max(1) as f32 * (tile_h + gap);
    let edit = widgets::list(&mut c, t, f, y, &[Row::new(l.pick(["编辑快捷访问", "編輯快速存取", "Edit shortcuts"])).icon(BLUE, Icon::More).trail(Trail::Arrow)]);
    out.extend(hits(&edit.rows, &[Action::BrowserQuick]));
    y = edit.bottom;

    // The addresses read before, newest first and once each.
    let mut seen: Vec<&str> = Vec::new();
    let mut recent: Vec<(usize, &String)> = Vec::new();
    for (i, url) in s.browser.history.iter().enumerate().rev() {
        if seen.contains(&url.as_str()) {
            continue;
        }
        seen.push(url);
        recent.push((i, url));
        if recent.len() == 4 {
            break;
        }
    }
    if !recent.is_empty() {
        y = widgets::section(&mut c, t, f, y, l.pick(["最近访问", "最近造訪", "Recent"]));
        let subs: Vec<String> = recent.iter().map(|(_, u)| u.to_string()).collect();
        let titles: Vec<String> = recent.iter().map(|(_, u)| web::host_of(u)).collect();
        let rows: Vec<Row> =
            recent.iter().zip(&titles).zip(&subs).map(|((_, title), sub)| Row::new(title).subtitle(sub).icon(BLUE, Icon::Globe)).collect();
        let list = widgets::list(&mut c, t, f, y, &rows);
        let actions: Vec<Action> = recent.iter().map(|(i, _)| Action::BrowserHistory(*i)).collect();
        out.extend(hits(&list.rows, &actions));
        y = list.bottom;
    }

    y = widgets::section(&mut c, t, f, y, l.pick(DOWNLOADS));
    let count = s.browser.downloads.len();
    if let Some(i) = s.browser.saving {        if let Some(d) = s.browser.downloads.get(i) {
            if let BrowserSave::Downloading { done, total } = d.state {
                let (head, detail, share) = saving_line(l, &d.name, done, total);
                y = widgets::progress_card(&mut c, t, f, y, head, &detail, share);
            }
        }
    }
    let sub = if count == 0 {
        l.pick(["还没有下载", "還沒有下載", "Nothing downloaded"]).to_string()
    } else {
        tr!(l, "{count} 个文件", "{count} 個檔案", "{count} files")
    };
    let row = Row::new(l.pick(DOWNLOADS)).icon(GREEN, Icon::Install).subtitle(&sub);
    let list = widgets::list(&mut c, t, f, y, &[row]);
    out.extend(hits(&list.rows, &[Action::BrowserDownloads]));
    y = list.bottom;

    y = widgets::section(&mut c, t, f, y, l.pick(["更多", "更多", "More"]));
    let full = Row::new(l.pick(["完整模式（Linux Chromium）", "完整模式（Linux Chromium）", "Full mode (Linux Chromium)"]))
        .icon(PURPLE, Icon::Layers)
        .subtitle(l.pick(["外置插件，解密后可安装", "外置外掛，解密後可安裝", "An external plugin, installed after decryption"]));
    let list = widgets::list(&mut c, t, f, y, &[full]);
    out.extend(hits(&list.rows, &[Action::BrowserFull]));
    y = list.bottom;

    if !s.online() {
        y += m.card_gap * 2.0;
        y = widgets::section(&mut c, t, f, y, l.pick(["网络", "網路", "Network"]));
        let (title, sub) = match s.wlan.is_some() {
            true => (l.pick(["未连接 WLAN", "未連接 Wi-Fi", "Not on Wi-Fi"]).to_string(), l.pick(["点按连接网络", "點按連線網路", "Tap to join a network"]).to_string()),
            false => (l.pick(["本机未配置 WLAN", "本機未設定 Wi-Fi", "No Wi-Fi on this phone"]).to_string(), String::new()),
        };
        let row = Row::new(&title).icon(RED, Icon::Wlan).subtitle(&sub).trail(if s.wlan.is_some() { Trail::Arrow } else { Trail::None });
        let list = widgets::list(&mut c, t, f, y, &[row]);
        if s.wlan.is_some() {
            out.extend(hits(&list.rows, &[Action::Open(Screen::Wlan)]));
        }
        y = list.bottom;
    }

    let note = l.pick([
        "浏览器显示文字、图片和链接，支持基础 CSS、表单和 Cookie，不运行脚本。下载的文件保存在 /sdcard/Download/Skyward，未解密时保存在 /tmp。",
        "瀏覽器顯示文字、圖片和連結，支援基礎 CSS、表單和 Cookie，不執行指令碼。下載的檔案儲存在 /sdcard/Download/Skyward，未解密時儲存在 /tmp。",
        "The browser shows text, pictures and links, with basic CSS, forms and cookies. It doesn't run scripts. Downloads go to /sdcard/Download/Skyward, or /tmp before the data partition opens.",
    ]);
    y = widgets::footnote(&mut c, t, f, y + m.card_gap, note);
    let mut frame = Frame::new(t, fit(c, t, y), out, l.pick(BROWSER), top);
    frame.dock = Some(dock(cx));
    frame
}

/// A page the external Chromium drew, under the normal address bar and
/// dock: the plugin is the webview, the recovery still owns the controls.
fn full_page(cx: &Ctx) -> Frame {
    let (t, l) = (cx.t, cx.state.lang);
    let m = &t.m;
    let mut c = long_page(cx);
    let top = widgets::bar_bottom(t) + m.dp(8.0);
    let mut out = Vec::new();
    let back = Rect::new(m.margin, top, m.dp(44.0), m.dp(44.0));
    c.fill_round_rect(back.x, back.y, back.w, back.h, back.h / 2.0, t.pal.search);
    let (cx0, cy0) = back.centre();
    c.line(cx0 + m.dp(5.0), cy0 - m.dp(8.0), cx0 - m.dp(4.0), cy0, m.dp(2.6), t.pal.text);
    c.line(cx0 - m.dp(4.0), cy0, cx0 + m.dp(5.0), cy0 + m.dp(8.0), m.dp(2.6), t.pal.text);
    out.push(Hit { rect: back, action: Action::BrowserBasic });
    let bar = Rect::new(back.x + back.w + m.dp(10.0), top, m.w as f32 - back.x - back.w - m.margin - m.dp(10.0), m.dp(44.0));
    widgets::search_capsule(&mut c, t, cx.f, bar, &cx.state.browser.url);
    out.push(Hit { rect: bar, action: Action::BrowserAddress });
    let mut y = top + back.h + m.dp(8.0);
    let max_w = (m.w as f32 - m.margin * 2.0).max(m.dp(80.0)) as u32;
    if let Some(shot) = &cx.state.browser.full_shot {
        if let Some((w, h, rgba)) = decode_picture(shot, max_w) {
            let x = (m.w as f32 - w as f32) / 2.0;
            let picture = Canvas::from_rgba(w, h, rgba);
            c.lay(&picture, x.round() as i32, y.round() as i32, 1.0);
            let (cols, rows) = (12, 24);
            for gy in 0..rows {
                for gx in 0..cols {
                    let rect = Rect::new(
                        x + gx as f32 * w as f32 / cols as f32,
                        y + gy as f32 * h as f32 / rows as f32,
                        w as f32 / cols as f32,
                        h as f32 / rows as f32,
                    );
                    let px = ((gx * 1000 + 500) / cols as i32) as u16;
                    let py = ((gy * 1000 + 500) / rows as i32) as u16;
                    out.push(Hit { rect, action: Action::BrowserPluginTap(px, py) });
                }
            }
            if let Some(progress) = cx.state.browser.full_progress.filter(|p| *p < 1.0) {
                let progress = progress.clamp(0.0, 1.0);
                c.fill_round_rect(m.margin, y - m.dp(4.0), (m.w as f32 - m.margin * 2.0) * progress, m.dp(3.0), m.dp(1.5), t.pal.accent);
            }
            y += h as f32;
        } else {
            y = widgets::footnote(&mut c, t, cx.f, y, l.pick(["完整模式返回的图片无法解码", "完整模式回傳的圖片無法解碼", "Full mode returned a picture that can't be decoded"]));
        }
    } else {
        let progress = cx.state.browser.full_progress.unwrap_or(0.0);
        y = widgets::progress_card(
            &mut c,
            t,
            cx.f,
            y,
            l.pick(["正在渲染页面", "正在繪製頁面", "Drawing the page"]),
            &cx.state.browser.url,
            Some(progress),
        );
        out.push(Hit { rect: Rect::new(m.margin, top, m.w as f32 - m.margin * 2.0, m.dp(56.0)), action: Action::BrowserAddress });
    }
    let mut frame = Frame::new(t, fit(c, t, y), out, l.pick(BROWSER), top);
    frame.header = false;
    frame.back = false;
    frame.dock = Some(full_dock(cx));
    frame
}

fn loading(cx: &Ctx, url: &str) -> Frame {    let (t, f, l) = (cx.t, cx.f, cx.state.lang);
    let m = &t.m;
    let mut c = page(cx);
    let mut out = Vec::new();
    let top = m.content_top;
    let bar = address_bar(&mut c, cx, top, url);
    out.push(Hit { rect: rect_bar(cx, top), action: Action::BrowserAddress });
    let y = bar + m.card_gap;
    let head = l.pick(["正在打开", "正在開啟", "Opening"]);
    let y = widgets::progress_card(&mut c, t, f, y, head, url, None);
    let note = l.pick(["这可能需要几秒。按返回可离开。", "這可能需要幾秒。按返回可離開。", "This may take a few seconds. Go back to leave."]);
    let y = widgets::footnote(&mut c, t, f, y, note);
    let mut frame = Frame::new(t, fit(c, t, y), out, l.pick(BROWSER), top);
    frame.dock = Some(dock(cx));
    frame
}

fn failed(cx: &Ctx, url: &str, why: &str) -> Frame {
    let (t, f, l) = (cx.t, cx.f, cx.state.lang);
    let m = &t.m;
    let mut c = page(cx);
    let mut out = Vec::new();
    let top = m.content_top;
    let bar = address_bar(&mut c, cx, top, url);
    out.push(Hit { rect: rect_bar(cx, top), action: Action::BrowserAddress });
    let y = widgets::section(&mut c, t, f, bar + m.card_gap, l.pick(["无法打开", "無法開啟", "Couldn't open it"]));
    let url_text = url.to_string();
    let row = Row::new(&url_text).icon(RED, Icon::Globe).subtitle(why).trail(Trail::None);
    let list = widgets::list(&mut c, t, f, y, &[row]);
    let mut y = list.bottom;
    let rows = [Row::new(l.pick(RETRY)).icon(BLUE, Icon::Restore).trail(Trail::None), Row::new(l.pick(NOT_NOW)).icon(ORANGE, Icon::Globe)];
    let again = widgets::list(&mut c, t, f, y + m.card_gap, &rows);
    out.extend(hits(&again.rows, &[Action::BrowserReload, Action::BrowserAddress]));
    y = again.bottom;
    if !cx.state.online() {
        y = widgets::footnote(&mut c, t, f, y, l.pick(["未连接 WLAN。先去 WLAN 页面连接网络。", "未連接 Wi-Fi。請先到 Wi-Fi 頁面連線。", "Not on Wi-Fi. Join a network on the Wi-Fi page first."]));
    }
    let mut frame = Frame::new(t, fit(c, t, y), out, l.pick(BROWSER), top);
    frame.dock = Some(dock(cx));
    frame
}

fn saving_line(l: Lang, name: &str, done: u64, total: Option<u64>) -> (&'static str, String, Option<f32>) {
    let head = l.pick(["正在下载", "正在下載", "Downloading"]);
    let detail = match total {
        Some(total) if total > 0 => format!("{} / {}", fmt::file_size(done), fmt::file_size(total)),
        _ => fmt::file_size(done),
    };
    let share = total.filter(|&t| t > 0).map(|t| (done as f32 / t as f32).clamp(0.0, 1.0));
    let _ = name;
    (head, detail, share)
}

fn dock(cx: &Ctx) -> Dock {
    dock_with(cx, &[Action::BrowserBack, Action::BrowserForward, Action::BrowserReload, Action::BrowserHome, Action::BrowserTabs, Action::BrowserDownloads, Action::BrowserFullLoad])
}

fn full_dock(cx: &Ctx) -> Dock {
    dock_with(cx, &[Action::BrowserBack, Action::BrowserForward, Action::BrowserReload, Action::BrowserKeyboard, Action::BrowserZoomOut, Action::BrowserZoomIn, Action::BrowserHome])
}

fn dock_with(cx: &Ctx, actions: &[Action]) -> Dock {
    let (t, l) = (cx.t, cx.state.lang);
    let m = &t.m;
    let (h, pad, gap) = (m.dp(48.0), m.dp(12.0), m.dp(8.0));
    let mut c = Canvas::new(m.w, (pad + h + m.dp(26.0)).ceil() as u32);
    c.clear(t.pal.bg);
    let n = actions.len() as f32;
    let w = (m.w as f32 - m.margin * 2.0 - gap * (n - 1.0)) / n;
    let mut rects = Vec::new();
    for (k, action) in actions.iter().enumerate() {
        let x = m.margin + k as f32 * (w + gap);
        c.fill_round_rect(x, pad, w, h, h / 2.0, t.pal.search);
        let (cx0, cy0) = (x + w / 2.0, pad + h / 2.0);
        let ink = t.pal.text;
        match action {
            Action::BrowserBack => {
                c.line(cx0 + m.dp(4.0), cy0 - m.dp(8.0), cx0 - m.dp(4.0), cy0, m.dp(2.6), ink);
                c.line(cx0 - m.dp(4.0), cy0, cx0 + m.dp(4.0), cy0 + m.dp(8.0), m.dp(2.6), ink);
            }
            Action::BrowserForward => {
                c.line(cx0 - m.dp(4.0), cy0 - m.dp(8.0), cx0 + m.dp(4.0), cy0, m.dp(2.6), ink);
                c.line(cx0 + m.dp(4.0), cy0, cx0 - m.dp(4.0), cy0 + m.dp(8.0), m.dp(2.6), ink);
            }
            Action::BrowserReload => {
                c.stroke_arc(cx0, cy0, m.dp(8.0), -0.6, 4.8, m.dp(2.2), ink);
                c.line(cx0 + m.dp(6.0), cy0 - m.dp(7.0), cx0 + m.dp(9.0), cy0 - m.dp(2.0), m.dp(2.2), ink);
            }
            Action::BrowserHome => {
                c.line(cx0 - m.dp(8.0), cy0, cx0, cy0 - m.dp(8.0), m.dp(2.4), ink);
                c.line(cx0, cy0 - m.dp(8.0), cx0 + m.dp(8.0), cy0, m.dp(2.4), ink);
                c.stroke_round_rect(cx0 - m.dp(5.5), cy0 - m.dp(1.0), m.dp(11.0), m.dp(10.0), m.dp(1.5), m.dp(2.0), ink);
            }
            Action::BrowserTabs => {
                c.stroke_round_rect(cx0 - m.dp(8.0), cy0 - m.dp(6.0), m.dp(12.0), m.dp(12.0), m.dp(2.0), m.dp(2.0), ink);
                c.stroke_round_rect(cx0 - m.dp(1.0), cy0 - m.dp(9.0), m.dp(10.0), m.dp(10.0), m.dp(2.0), m.dp(2.0), ink);
            }
            Action::BrowserKeyboard => {
                c.stroke_round_rect(cx0 - m.dp(9.0), cy0 - m.dp(6.0), m.dp(18.0), m.dp(12.0), m.dp(2.0), m.dp(1.8), ink);
                for k in 0..3 {
                    let x = cx0 - m.dp(5.0) + k as f32 * m.dp(5.0);
                    c.line(x, cy0 - m.dp(2.0), x, cy0 + m.dp(1.0), m.dp(1.3), ink);
                }
                c.line(cx0 - m.dp(5.0), cy0 + m.dp(4.0), cx0 + m.dp(5.0), cy0 + m.dp(4.0), m.dp(1.5), ink);
            }
            Action::BrowserZoomIn | Action::BrowserZoomOut => {
                c.stroke_round_rect(cx0 - m.dp(9.0), cy0 - m.dp(9.0), m.dp(18.0), m.dp(18.0), m.dp(9.0), m.dp(2.0), ink);
                c.line(cx0 - m.dp(5.0), cy0, cx0 + m.dp(5.0), cy0, m.dp(2.0), ink);
                if action == &Action::BrowserZoomIn {
                    c.line(cx0, cy0 - m.dp(5.0), cx0, cy0 + m.dp(5.0), m.dp(2.0), ink);
                }
            }
            Action::BrowserFullLoad => {
                c.stroke_round_rect(cx0 - m.dp(8.0), cy0 - m.dp(8.0), m.dp(16.0), m.dp(16.0), m.dp(8.0), m.dp(1.8), ink);
                c.line(cx0 - m.dp(8.0), cy0, cx0 + m.dp(8.0), cy0, m.dp(1.5), ink);
                c.stroke_arc(cx0, cy0, m.dp(5.0), 0.0, 6.28, m.dp(1.4), ink);
            }
            _ => {
                c.line(cx0, cy0 - m.dp(9.0), cx0, cy0 + m.dp(3.0), m.dp(2.4), ink);
                c.line(cx0 - m.dp(4.0), cy0 - m.dp(1.0), cx0, cy0 + m.dp(3.0), m.dp(2.4), ink);
                c.line(cx0 + m.dp(4.0), cy0 - m.dp(1.0), cx0, cy0 + m.dp(3.0), m.dp(2.4), ink);
                c.line(cx0 - m.dp(8.0), cy0 + m.dp(7.0), cx0 + m.dp(8.0), cy0 + m.dp(7.0), m.dp(2.0), ink);
            }
        }
        rects.push(Rect::new(x, pad, w, h));
    }
    let _ = l;
    Dock { canvas: c, hits: hits(&rects, actions) }
}

/// A page read as text and links: its title and address in a card, then a
/// row for each part, drawn as it scrolls in.
fn read(cx: &Ctx, article: &WebPage) -> Frame {
    let (t, l) = (cx.t, cx.state.lang);
    let m = &t.m;
    let mut c = page(cx);
    let top = m.content_top;
    let mut out = Vec::new();
    let bar = address_bar(&mut c, cx, top, &article.url);
    out.push(Hit { rect: rect_bar(cx, top), action: Action::BrowserAddress });
    let header = article_head(&mut c, cx, article, bar + m.dp(12.0));
    let mut edges = vec![header.round()];
    for block in &article.blocks {
        let h = block_height(&c, cx, block);
        edges.push((*edges.last().unwrap() + h).round());
    }
    let mut rows = Vec::new();
    for (i, block) in article.blocks.iter().enumerate() {
        if let WebBlock::Link { .. } = block {
            rows.push(Hit { rect: Rect::new(0.0, edges[i], m.w as f32, edges[i + 1] - edges[i]), action: Action::BrowserLink(i) });
        }
        if let WebBlock::Form(form) = block {
            if let Some(fields) = article.forms.get(*form).map(|f| &f.fields) {
                let mut y = edges[i] + m.dp(38.0);
                for (j, field) in fields.iter().enumerate() {
                    if field.kind == web::FieldKind::Hidden {
                        continue;
                    }
                    let action = if field.kind == web::FieldKind::Submit {
                        Action::BrowserFormSubmit(*form)
                    } else {
                        Action::BrowserFormField(*form, j)
                    };
                    rows.push(Hit { rect: Rect::new(0.0, y, m.w as f32, m.dp(48.0)), action });
                    y += m.dp(48.0);
                }
            }
        }
    }
    out.extend(rows);
    let mut frame = Frame::new(t, c, out, l.pick(BROWSER), top);
    frame.rows = Some(Rows { edges, action: None });
    frame.dock = Some(dock(cx));
    frame
}

fn article_head(c: &mut Canvas, cx: &Ctx, page: &WebPage, top: f32) -> f32 {
    let (t, f, l) = (cx.t, cx.f, cx.state.lang);
    let m = &t.m;
    let x = m.margin;
    let w = m.w as f32 - m.margin * 2.0;
    let pad = m.inner + m.dp(4.0);
    let max = w - pad * 2.0;
    let mut title_lines = widgets::wrap(c, &f.heading, &page.title, m.dp(23.0), max);
    if title_lines.len() > 3 {
        title_lines.truncate(3);
        if let Some(last) = title_lines.last_mut() {
            last.push('…');
        }
    }
    let desc = page.description.as_deref().filter(|d| !d.trim().is_empty());
    let mut desc_lines = desc.map(|d| widgets::wrap(c, &f.body, d, m.sub_px, max)).unwrap_or_default();
    if desc_lines.len() > 2 {
        desc_lines.truncate(2);
    }
    let note = page.truncated.then(|| l.pick(["页面较长，只显示了前面一部分。", "頁面較長，只顯示了前面一部分。", "The page is long, so only its start shows."]));
    let h = pad * 2.0
        + m.dp(16.0)
        + m.dp(6.0)
        + title_lines.len() as f32 * m.dp(30.0)
        + if desc_lines.is_empty() { 0.0 } else { m.dp(8.0) + desc_lines.len() as f32 * m.dp(19.0) }
        + m.dp(8.0)
        + m.dp(16.0)
        + if note.is_some() { m.dp(20.0) } else { 0.0 };
    c.fill_round_rect(x, top, w, h, m.card_radius, t.pal.card);
    let mut y = top + pad;
    c.text_top(&f.label, &page.site, x + pad, y, m.dp(13.0), t.pal.accent, Some(max));
    y += m.dp(16.0) + m.dp(6.0);
    for line in &title_lines {
        c.text_top(&f.heading, line, x + pad, y, m.dp(23.0), t.pal.text, Some(max));
        y += m.dp(30.0);
    }
    if !desc_lines.is_empty() {
        y += m.dp(8.0);
        for line in &desc_lines {
            c.text_top(&f.body, line, x + pad, y, m.sub_px, t.pal.subtitle, Some(max));
            y += m.dp(19.0);
        }
    }
    y += m.dp(8.0);
    c.text_top(&f.value, &page.url, x + pad, y, m.dp(12.0), t.pal.value, Some(max));
    y += m.dp(16.0);
    if let Some(note) = note {
        c.text_top(&f.value, note, x + pad, y, m.dp(12.0), ORANGE, Some(max));
    }
    top + h
}

fn block_height(c: &Canvas, cx: &Ctx, block: &WebBlock) -> f32 {
    let (t, f) = (cx.t, cx.f);
    let m = &t.m;
    let x = m.title_x;
    let w = m.w as f32 - x * 2.0;
    match block {
        WebBlock::Heading { level, text, .. } => {
            let px = heading_px(*level, t);
            let lines = widgets::wrap(c, &f.heading, text, px, w);
            m.dp(20.0) + lines.len() as f32 * heading_pitch(*level, t) + m.dp(8.0)
        }
        WebBlock::Paragraph { text, .. } => {
            let lines = widgets::wrap(c, &f.body, text, m.dp(15.0), w);
            m.dp(10.0) + lines.len() as f32 * m.dp(23.0) + m.dp(10.0)
        }
        WebBlock::Bullet { text, .. } => {
            let lines = widgets::wrap(c, &f.body, text, m.dp(15.0), w - m.dp(22.0));
            m.dp(6.0) + lines.len() as f32 * m.dp(23.0) + m.dp(6.0)
        }
        WebBlock::Quote { text, .. } => {
            let lines = widgets::wrap(c, &f.body, text, m.dp(15.0), w - m.dp(20.0));
            m.dp(8.0) + lines.len() as f32 * m.dp(23.0) + m.dp(10.0)
        }
        WebBlock::Code { text, .. } => {
            let lines: usize = text.lines().map(|line| widgets::wrap(c, &f.body, line, m.dp(13.0), w - m.dp(24.0)).len()).sum();
            m.dp(10.0) + lines.max(1) as f32 * m.dp(20.0) + m.dp(16.0)
        }
        WebBlock::Link { label, url, download, .. } => link_height(c, t, f, cx.state.lang, label, url, *download),
        WebBlock::Image { alt, data, .. } => {
            let _ = alt;
            if data.is_some() { m.dp(190.0) } else { m.dp(78.0) }
        }
        WebBlock::Form(i) => form_height(c, t, f, cx, *i),
        WebBlock::Rule => m.dp(22.0),
    }
}

fn heading_px(level: u8, t: &Theme) -> f32 {
    t.m.dp(match level {
        1 => 26.0,
        2 => 21.0,
        _ => 17.5,
    })
}

fn heading_pitch(level: u8, t: &Theme) -> f32 {
    t.m.dp(match level {
        1 => 34.0,
        2 => 28.0,
        _ => 24.0,
    })
}

fn link_height(c: &Canvas, t: &Theme, f: &Fonts, l: Lang, _label: &str, url: &str, download: bool) -> f32 {
    let m = &t.m;
    let card_w = m.w as f32 - m.margin * 2.0;
    let text_x = m.inner + m.icon + m.icon_gap;
    let trail_w = match download {
        true => m.chevron_inset + m.chevron_w + m.value_gap * 2.0 + c.text_width(&f.value, l.pick(DOWNLOAD), m.sub_px),
        false => m.chevron_inset + m.chevron_w + m.value_gap,
    };
    let text_max = (card_w - text_x - trail_w).max(m.dp(40.0));
    let sub = web::host_of(url);
    let sub_lines = widgets::wrap(c, &f.body, &sub, m.sub_px, text_max);
    let (tt, tb) = f.heading.ink(m.row_title_px);
    let (st, sb) = f.body.ink(m.sub_px);
    let block = (tb - tt) + m.sub_gap + (sub_lines.len() as f32 - 1.0) * m.sub_pitch + (sb - st);
    let h = (block + m.row_pad_v * 2.0).max(m.row_h) + m.dp(10.0);
    h.ceil()
}

fn draw_block(c: &mut Canvas, cx: &Ctx, block: &WebBlock) {
    let (t, f, l) = (cx.t, cx.f, cx.state.lang);
    let m = &t.m;
    c.clear(t.pal.bg);
    let x = m.title_x;
    let w = m.w as f32 - x * 2.0;
    match block {
        WebBlock::Heading { level, text, style } => {
            let px = style.size.map(|s| m.dp(s as f32)).unwrap_or_else(|| heading_px(*level, t));
            let font = if style.bold { &f.strong } else { &f.heading };
            let color = style.color.map(rgba).unwrap_or(t.pal.text);
            let mut y = m.dp(20.0);
            for line in widgets::wrap(c, font, text, px, w) {
                let lx = aligned_x(c, font, &line, px, x, w, style.align);
                c.text_top(font, &line, lx, y, px, color, None);
                if style.underline {
                    c.fill_rect(lx, y + px + m.dp(2.0), c.text_width(font, &line, px), m.dp(1.0), color);
                }
                y += (px * 1.3).max(heading_pitch(*level, t));
            }
        }
        WebBlock::Paragraph { text, style } => {
            let px = style.size.map(|s| m.dp(s as f32).clamp(m.dp(12.0), m.dp(24.0))).unwrap_or(m.dp(15.0));
            let font = if style.bold { &f.strong } else { &f.body };
            let color = style.color.map(rgba).unwrap_or(t.pal.text);
            let mut y = m.dp(10.0);
            if let Some(bg) = style.background {
                c.fill_round_rect(m.margin, m.dp(4.0), m.w as f32 - m.margin * 2.0, block_height(c, cx, block) - m.dp(8.0), m.dp(8.0), rgba(bg));
            }
            for line in widgets::wrap(c, font, text, px, w) {
                let lx = aligned_x(c, font, &line, px, x, w, style.align);
                c.text_top(font, &line, lx, y, px, color, None);
                if style.underline {
                    c.fill_rect(lx, y + px + m.dp(2.0), c.text_width(font, &line, px), m.dp(1.0), color);
                }
                y += (px * 1.45).max(m.dp(20.0));
            }
        }
        WebBlock::Bullet { text, .. } => {
            let mut y = m.dp(6.0);
            c.fill_circle(x + m.dp(4.0), y + m.dp(10.0), m.dp(2.2), t.pal.accent);
            for line in widgets::wrap(c, &f.body, text, m.dp(15.0), w - m.dp(22.0)) {
                c.text_top(&f.body, &line, x + m.dp(22.0), y, m.dp(15.0), t.pal.text, None);
                y += m.dp(23.0);
            }
        }
        WebBlock::Quote { text, .. } => {
            let mut y = m.dp(8.0);
            c.fill_rect(x - m.dp(6.0), y, m.dp(3.0), (block_height(c, cx, block) - m.dp(18.0)).max(m.dp(10.0)), t.pal.accent.with_alpha(120));
            for line in widgets::wrap(c, &f.body, text, m.dp(15.0), w - m.dp(20.0)) {
                c.text_top(&f.body, &line, x + m.dp(12.0), y, m.dp(15.0), t.pal.subtitle, None);
                y += m.dp(23.0);
            }
        }
        WebBlock::Code { text, .. } => {
            c.fill_round_rect(m.margin, m.dp(6.0), m.w as f32 - m.margin * 2.0, c.h as f32 - m.dp(12.0), m.dp(12.0), t.pal.card);
            let mut y = m.dp(16.0);
            for line in text.lines().take(40) {
                for wrapped in widgets::wrap(c, &f.body, line, m.dp(13.0), w - m.dp(24.0)) {
                    c.text_top(&f.body, &wrapped, x + m.dp(12.0), y, m.dp(13.0), t.pal.info, None);
                    y += m.dp(20.0);
                }
            }
        }
        WebBlock::Link { label, url, download, .. } => {
            let trail_label = l.pick(DOWNLOAD);
            let trail = match download {
                true => Trail::Value(trail_label),
                false => Trail::Arrow,
            };
            let (color, icon) = match download {
                true => (ORANGE, Icon::Install),
                false => (BLUE, Icon::Globe),
            };
            let host = web::host_of(url);
            let row = Row::new(label).icon(color, icon).subtitle(&host).trail(trail);
            let _ = widgets::list(c, t, f, m.dp(5.0), &[row]);
        }
        WebBlock::Image { alt, data, .. } => {
            if data.is_some() {
                draw_picture(c, cx, block);
            } else {
                let row = Row::new(l.pick(["图片", "圖片", "Picture"])).icon(PURPLE, Icon::Layers).subtitle(alt).trail(Trail::None);
                let _ = widgets::list(c, t, f, m.dp(6.0), &[row]);
            }
        }
        WebBlock::Form(i) => draw_form(c, cx, *i),
        WebBlock::Rule => {
            c.fill_rect(m.margin, m.dp(10.0), m.w as f32 - m.margin * 2.0, m.dp(1.0), t.pal.track);
        }
    }
}

/// A row of a page read: the part at `i`, drawn on `c` alone.
pub fn row(cx: &Ctx, i: usize, c: &mut Canvas) {
    let page = cx.state.browser.page().expect("the page read draws its rows");
    if let Some(block) = page.blocks.get(i) {
        draw_block(c, cx, block);
    }
}

/// The address field, the sites offered, and the keyboard.
pub fn address(cx: &Ctx) -> Frame {
    let (t, f, s, l) = (cx.t, cx.f, cx.state, cx.state.lang);
    let m = &t.m;
    let mut c = page(cx);
    let mut out = Vec::new();
    let top = m.content_top;
    let y = widgets::section_first(&mut c, t, f, top, l.pick(["网址", "網址", "Address"]));
    let (below, caret) = widgets::text_field(&mut c, t, f, y, &s.browser.address);
    if s.browser.address.trim().is_empty() {
        let hint = l.pick(HINT);
        c.text_top(&f.body, hint, m.margin + m.inner, y + m.dp(16.0), m.row_title_px, t.pal.placeholder, Some(m.w as f32 - m.margin * 2.0 - m.inner * 2.0));
    }
    out.push(Hit { rect: Rect::new(m.margin, y, m.w as f32 - m.margin * 2.0, m.dp(56.0)), action: Action::BrowserAddress });
    let clear = widgets::list(&mut c, t, f, below + m.card_gap, &[Row::new(l.pick(CLEAR)).trail(Trail::None)]);
    out.extend(hits(&clear.rows, &[Action::BrowserClearAddress]));
    let mut y = clear.bottom;
    y = widgets::section(&mut c, t, f, y, l.pick(["快捷访问", "快速存取", "Shortcuts"]));
    let rows: Vec<Row> = s
        .browser
        .quick
        .iter()
        .map(|q| Row::new(&q.title).icon(q.color, q.icon).subtitle(&q.url))
        .collect();
    let list = widgets::list(&mut c, t, f, y, &rows);
    let actions: Vec<Action> = (0..s.browser.quick.len()).map(Action::BrowserQuickOpen).collect();
    out.extend(hits(&list.rows, &actions));
    y = list.bottom;
    let note = l.pick([
        "可输入网址；输入关键词会用 Bing 搜索，搜索页按可读的文字和链接排版。仅支持 http 和 https。",
        "可輸入網址；輸入關鍵詞會用 Bing 搜尋，搜尋頁按可讀的文字和連結排版。僅支援 http 和 https。",
        "An address opens as it is; words search Bing, laid out as readable text and links. Only http and https work.",
    ]);
    let _ = widgets::footnote(&mut c, t, f, y, note);
    let enter = l.pick(GO);
    let kb_top = t.m.h as f32 - widgets::keyboard_height(t);
    let keys = widgets::keyboard(&mut c, t, f, l, kb_top, s.layer, s.key_held(), enter);
    out.extend(keys.into_iter().map(|(rect, cap)| Hit { rect, action: Action::Type(cap) }));
    Frame { caret: Some(caret), ..Frame::new(t, c, out, l.pick(OPEN_ADDRESS), top) }
}

/// The files saved and being saved.
pub fn downloads(cx: &Ctx) -> Frame {
    let (t, f, s, l) = (cx.t, cx.f, cx.state, cx.state.lang);
    let m = &t.m;
    let mut c = long_page(cx);
    let mut out = Vec::new();
    let top = m.content_top;
    let mut y = top;
    if let Some(i) = s.browser.saving {
        if let Some(d) = s.browser.downloads.get(i) {
            if let BrowserSave::Downloading { done, total } = d.state {
                let (head, detail, share) = saving_line(l, &d.name, done, total);
                y = widgets::progress_card(&mut c, t, f, y, head, &detail, share);
                let stop = widgets::list(&mut c, t, f, y + m.card_gap, &[Row::new(l.pick(["停止下载", "停止下載", "Stop downloading"])).icon(RED, Icon::Wipe).trail(Trail::None)]);
                out.extend(hits(&stop.rows, &[Action::BrowserStopDownload]));
                y = stop.bottom + m.card_gap;
            }
        }
    }
    if s.browser.downloads.is_empty() {
        let list = widgets::list(&mut c, t, f, y, &[Row::new(l.pick(NO_DOWNLOADS)).trail(Trail::None)]);
        y = list.bottom;
    } else {
        let names: Vec<String> = s.browser.downloads.iter().map(|d| d.name.clone()).collect();
        let subs: Vec<String> = s
            .browser
            .downloads
            .iter()
            .map(|d| {
                let state = match &d.state {
                    BrowserSave::Downloading { done, total } => match total {
                        Some(total) if *total > 0 => format!("{} / {}", fmt::file_size(*done), fmt::file_size(*total)),
                        _ => fmt::file_size(*done),
                    },
                    BrowserSave::Done => fmt::file_size(d.size.unwrap_or(d.done)),
                    BrowserSave::Stopped => l.pick(["已停止", "已停止", "Stopped"]).to_string(),
                    BrowserSave::Failed(why) => why.clone(),
                };
                format!("{state} · {}", d.path)
            })
            .collect();
        let rows: Vec<Row> = s
            .browser
            .downloads
            .iter()
            .zip(&names)
            .zip(&subs)
            .map(|((d, name), sub)| {
                let install = skyward_net::web::installable(name) && matches!(d.state, BrowserSave::Done);
                Row::new(name).icon(GREEN, Icon::Install).subtitle(sub).trail(if install { Trail::Value(l.pick(["安装", "安裝", "Install"])) } else { Trail::None })
            })
            .collect();
        let list = widgets::list(&mut c, t, f, y, &rows);
        let actions: Vec<Action> = (0..rows.len()).map(Action::BrowserOpenDownload).collect();
        out.extend(hits(&list.rows, &actions));
        y = list.bottom;
    }
    let note = l.pick([
        "文件保存在 /sdcard/Download/Skyward；data 未解密时保存在 /tmp/skyward-browser，重启后不再保留。",
        "檔案儲存在 /sdcard/Download/Skyward；data 未解密時儲存在 /tmp/skyward-browser，重新啟動後不會保留。",
        "Files go to /sdcard/Download/Skyward, or to /tmp/skyward-browser before the data partition is decrypted; those don't survive a restart.",
    ]);
    y = widgets::footnote(&mut c, t, f, y + m.card_gap, note);
    Frame::new(t, fit(c, t, y), out, l.pick(DOWNLOADS), top)
}

/// One field of a page's form, typed on the keyboard.
pub fn form(cx: &Ctx) -> Frame {
    let (t, f, s, l) = (cx.t, cx.f, cx.state, cx.state.lang);
    let m = &t.m;
    let mut c = page(cx);
    let top = m.content_top;
    let (form, field) = s.browser.field.expect("the form page has a field");
    let label = s
        .browser
        .page()
        .and_then(|p| p.forms.get(form))
        .and_then(|f| f.fields.get(field))
        .map(|f| if f.label.is_empty() { f.name.clone() } else { f.label.clone() })
        .unwrap_or_default();
    let value = s.browser.form.get(form).and_then(|v| v.get(field)).cloned().unwrap_or_default();
    let y = widgets::section_first(&mut c, t, f, top, &label);
    let (below, caret) = widgets::text_field(&mut c, t, f, y, &value);
    let mut out = vec![Hit {
        rect: Rect::new(m.margin, y, m.w as f32 - m.margin * 2.0, m.dp(56.0)),
        action: Action::BrowserFormField(form, field),
    }];
    let kb_top = t.m.h as f32 - widgets::keyboard_height(t);
    let keys = widgets::keyboard(&mut c, t, f, l, kb_top, s.layer, s.key_held(), l.pick(GO));
    out.extend(keys.into_iter().map(|(rect, cap)| Hit { rect, action: Action::Type(cap) }));
    let _ = below;
    Frame { caret: Some(caret), ..Frame::new(t, c, out, l.pick(["填写", "填寫", "Fill in"]), top) }
}

/// The optional full browser plugin.
pub fn full(cx: &Ctx) -> Frame {
    let (t, f, s, l) = (cx.t, cx.f, cx.state, cx.state.lang);
    let m = &t.m;
    let mut c = page(cx);
    let top = m.content_top;
    let mut out = Vec::new();
    let mut y = widgets::section_first(&mut c, t, f, top, l.pick(["完整模式", "完整模式", "Full mode"]));
    let status = match &s.browser.full {
        BrowserFull::Unknown => l.pick(["尚未检查插件", "尚未檢查外掛", "The plugin hasn't been checked"]),
        BrowserFull::Absent(why) => why.as_str(),
        BrowserFull::Ready(_) => l.pick(["插件已安装", "外掛已安裝", "The plugin is installed"]),
        BrowserFull::Installing { .. } => l.pick(["正在安装插件", "正在安裝外掛", "Installing the plugin"]),
        BrowserFull::Running(_) => l.pick(["完整模式正在运行", "完整模式正在執行", "Full mode is running"]),
        BrowserFull::Failed(why) => why.as_str(),
    };
    y = widgets::paragraph(&mut c, t, f, y, l.pick([
        "完整模式使用外置的 Linux Chromium，解密 data 后才能安装和使用。它下载到内部存储，不进入 recovery 镜像。",
        "完整模式使用外置的 Linux Chromium，解密 data 後才能安裝和使用。它下載到內部儲存空間，不進入 recovery 鏡像。",
        "Full mode uses an external Linux Chromium. It installs after the data partition is decrypted, and is kept in internal storage rather than in the recovery image.",
    ]));
    y = widgets::footnote(&mut c, t, f, y + m.dp(4.0), status);
    y = widgets::footnote(
        &mut c,
        t,
        f,
        y + m.dp(4.0),
        l.pick([
            "把插件包（chromium.zip 或 chromium.tar.gz）放到 /sdcard/Skyward/plugins/，再点安装；插件里的 run.sh 负责用 Chromium 画出页面。",
            "把外掛包（chromium.zip 或 chromium.tar.gz）放到 /sdcard/Skyward/plugins/，再點安裝；外掛裡的 run.sh 負責用 Chromium 畫出頁面。",
            "Put chromium.zip or chromium.tar.gz in /sdcard/Skyward/plugins/, then install. Its run.sh draws the page with Chromium.",
        ]),
    );
    let mut rows = Vec::new();
    let mut actions = Vec::new();
    match &s.browser.full {
        BrowserFull::Ready(_) => {
            rows.push(Row::new(l.pick(["启动完整模式", "啟動完整模式", "Start full mode"])).icon(GREEN, Icon::Install).trail(Trail::Arrow));
            actions.push(Action::BrowserPluginStart);
            rows.push(Row::new(l.pick(["移除插件", "移除外掛", "Remove the plugin"])).icon(ORANGE, Icon::Wipe).trail(Trail::None));
            actions.push(Action::BrowserPluginRemove);
        }
        BrowserFull::Running(_) => {
            rows.push(Row::new(l.pick(["停止完整模式", "停止完整模式", "Stop full mode"])).icon(ORANGE, Icon::Power).trail(Trail::None));
            actions.push(Action::BrowserPluginStop);
        }
        BrowserFull::Installing { .. } => {}
        _ => {
            rows.push(Row::new(l.pick(["安装或更新", "安裝或更新", "Install or update"])).icon(GREEN, Icon::Install).trail(Trail::None));
            actions.push(Action::BrowserPluginInstall);
        }
    }
    let list = widgets::list(&mut c, t, f, y + m.card_gap, &rows);
    out.extend(hits(&list.rows, &actions));
    y = list.bottom;
    if s.browser.full_shot.is_none() {
        if let Some(progress) = s.browser.full_progress {
            y = widgets::progress_card(
                &mut c,
                t,
                f,
                y + m.card_gap,
                l.pick(["正在启动完整模式", "正在啟動完整模式", "Starting full mode"]),
                l.pick(["Chromium 正在渲染页面，请稍候", "Chromium 正在繪製頁面，請稍候", "Chromium is drawing the page. Wait a moment"]),
                Some(progress),
            );
        }
    }
    if let Some(shot) = &s.browser.full_shot {
        let max_w = m.w as f32 - m.margin * 2.0;
        if let Some((w, h, rgba)) = decode_picture(shot, max_w as u32) {
            let x = (m.w as f32 - w as f32) / 2.0;
            y += m.card_gap;
            let picture = Canvas::from_rgba(w, h, rgba);
            c.lay(&picture, x.round() as i32, y.round() as i32, 1.0);
            let (cols, rows) = (6, 12);
            for gy in 0..rows {
                for gx in 0..cols {
                    let rect = Rect::new(
                        x + gx as f32 * w as f32 / cols as f32,
                        y + gy as f32 * h as f32 / rows as f32,
                        w as f32 / cols as f32,
                        h as f32 / rows as f32,
                    );
                    let px = ((gx * 1000 + 500) / cols as i32) as u16;
                    let py = ((gy * 1000 + 500) / rows as i32) as u16;
                    out.push(Hit { rect, action: Action::BrowserPluginTap(px, py) });
                }
            }
            y += h as f32;
        }
    }
    Frame::new(t, fit(c, t, y), out, l.pick(BROWSER), top)
}

/// The open pages, one row each, and a way to add or close one.
pub fn tabs(cx: &Ctx) -> Frame {
    let (t, f, s, l) = (cx.t, cx.f, cx.state, cx.state.lang);
    let m = &t.m;
    let mut c = page(cx);
    let top = m.content_top;
    let y = widgets::section_first(&mut c, t, f, top, l.pick(["标签页", "分頁", "Tabs"]));
    let titles: Vec<String> = s
        .browser
        .tabs
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            let active = if i == s.browser.active { " ·" } else { "" };
            format!("{}{active}", tab.title(l))
        })
        .collect();
    let rows: Vec<Row> = s
        .browser
        .tabs
        .iter()
        .zip(&titles)
        .enumerate()
        .map(|(i, (tab, title))| {
            Row::new(title)
                .subtitle(if tab.url.is_empty() { l.pick(["新标签页", "新分頁", "New tab"]) } else { tab.url.as_str() })
                .icon(if i == s.browser.active { BLUE } else { t.pal.subtitle }, Icon::Layers)
        })
        .collect();
    let list = widgets::list(&mut c, t, f, y, &rows);
    let actions: Vec<Action> = (0..s.browser.tabs.len()).map(Action::BrowserPickTab).collect();
    let mut out = hits(&list.rows, &actions);
    let close = widgets::list(
        &mut c,
        t,
        f,
        list.bottom + m.card_gap,
        &[Row::new(l.pick(["关闭当前标签", "關閉目前分頁", "Close this tab"])).icon(RED, Icon::Wipe).trail(Trail::None)],
    );
    out.extend(hits(&close.rows, &[Action::BrowserCloseTab(s.browser.active)]));
    let new = widgets::list(
        &mut c,
        t,
        f,
        close.bottom + m.card_gap,
        &[Row::new(l.pick(["新建标签页", "新增分頁", "New tab"])).icon(GREEN, Icon::Install).trail(Trail::None)],
    );
    out.extend(hits(&new.rows, &[Action::BrowserNewTab]));
    Frame::new(t, fit(c, t, new.bottom), out, l.pick(["标签页", "分頁", "Tabs"]), top)
}

/// The start page's shortcuts, to remove one, add the page shown, or put
/// them back as they started.
pub fn quick(cx: &Ctx) -> Frame {
    let (t, f, s, l) = (cx.t, cx.f, cx.state, cx.state.lang);
    let m = &t.m;
    let mut c = page(cx);
    let top = m.content_top;
    let y = widgets::section_first(&mut c, t, f, top, l.pick(["快捷访问", "快速存取", "Shortcuts"]));
    let rows: Vec<Row> = s
        .browser
        .quick
        .iter()
        .map(|q| Row::new(&q.title).icon(q.color, q.icon).subtitle(&q.url).trail(Trail::Value(l.pick(["删除", "刪除", "Remove"]))).to_owned())
        .collect();
    let list = widgets::list(&mut c, t, f, y, &rows);
    let actions: Vec<Action> = (0..s.browser.quick.len()).map(Action::BrowserQuickRemove).collect();
    let mut out = hits(&list.rows, &actions);
    let add = widgets::list(
        &mut c,
        t,
        f,
        list.bottom + m.card_gap,
        &[Row::new(l.pick(["添加当前页面", "加入目前頁面", "Add the page shown"])).icon(GREEN, Icon::Install).trail(Trail::None)],
    );
    out.extend(hits(&add.rows, &[Action::BrowserQuickAdd]));
    let reset = widgets::list(
        &mut c,
        t,
        f,
        add.bottom + m.card_gap,
        &[Row::new(l.pick(["恢复默认", "回復預設", "Restore the defaults"])).icon(BLUE, Icon::Restore).trail(Trail::None)],
    );
    out.extend(hits(&reset.rows, &[Action::BrowserQuickReset]));
    Frame::new(t, fit(c, t, reset.bottom), out, l.pick(["快捷访问", "快速存取", "Shortcuts"]), top)
}

fn rgba(hex: u32) -> Rgba {
    Rgba::rgb(((hex >> 16) & 0xff) as u8, ((hex >> 8) & 0xff) as u8, (hex & 0xff) as u8)
}

fn aligned_x(c: &Canvas, font: &crate::font::Face, line: &str, px: f32, x: f32, w: f32, align: web::Align) -> f32 {
    let width = c.text_width(font, line, px);
    match align {
        web::Align::Center => x + (w - width).max(0.0) / 2.0,
        web::Align::Right => x + (w - width).max(0.0),
        web::Align::Left => x,
    }
}

fn form_height(c: &Canvas, t: &Theme, f: &Fonts, cx: &Ctx, i: usize) -> f32 {
    let m = &t.m;
    let Some(form) = cx.state.browser.page().and_then(|p| p.forms.get(i)) else { return m.dp(40.0) };
    let mut h = m.dp(30.0);
    for field in &form.fields {
        let _ = (c, f);
        h += match field.kind {
            web::FieldKind::Hidden => 0.0,
            _ => m.dp(48.0),
        };
    }
    h + m.dp(10.0)
}

fn draw_form(c: &mut Canvas, cx: &Ctx, i: usize) {
    let (t, f, l) = (cx.t, cx.f, cx.state.lang);
    let m = &t.m;
    let Some(form) = cx.state.browser.page().and_then(|p| p.forms.get(i)) else { return };
    let mut y = m.dp(6.0);
    c.fill_round_rect(m.margin, y, m.w as f32 - m.margin * 2.0, form_height(c, t, f, cx, i), m.card_radius, t.pal.card);
    y += m.dp(12.0);
    c.text_top(&f.label, l.pick(["表单", "表單", "Form"]), m.title_x, y, m.sub_px, t.pal.accent, None);
    y += m.dp(26.0);
    for (j, field) in form.fields.iter().enumerate() {
        if field.kind == web::FieldKind::Hidden {
            continue;
        }
        let value = cx.state.browser.form.get(i).and_then(|v| v.get(j)).cloned().unwrap_or_else(|| field.value.clone());
        let label = if field.label.is_empty() { &field.name } else { &field.label };
        let (title, trail_value): (String, Option<String>) = match field.kind {
            web::FieldKind::Submit => (
                if label.is_empty() { l.pick(["提交", "提交", "Submit"]).to_string() } else { label.clone() },
                None,
            ),
            web::FieldKind::Checkbox | web::FieldKind::Radio => {
                (label.clone(), Some(if value.is_empty() { "[ ]".to_string() } else { "[x]".to_string() }))
            }
            _ => (
                label.clone(),
                Some(if value.is_empty() { l.pick(["点按填写", "點按填寫", "Tap to fill"]).to_string() } else { value }),
            ),
        };
        let trail = match &trail_value {
            Some(value) => Trail::Value(value.as_str()),
            None => Trail::Arrow,
        };
        let row = Row::new(&title).icon(BLUE, Icon::Log).trail(trail);
        let list = widgets::list(c, t, f, y, &[row]);
        y = list.bottom + m.dp(4.0);
    }
}

fn draw_picture(c: &mut Canvas, cx: &Ctx, block: &WebBlock) {
    let (t, l) = (cx.t, cx.state.lang);
    let m = &t.m;
    let WebBlock::Image { alt, data: Some(bytes), .. } = block else { return };
    let max_w = (m.w as f32 - m.margin * 2.0).max(m.dp(80.0)) as u32;
    match decode_picture(bytes, max_w) {
        Some((w, h, rgba)) => {
            let x = (m.w as f32 - w as f32) / 2.0;
            let y = m.dp(8.0);
            let picture = Canvas::from_rgba(w, h, rgba);
            c.lay(&picture, x.round() as i32, y.round() as i32, 1.0);
        }
        None => {
            let row = Row::new(l.pick(["图片", "圖片", "Picture"]))
                .icon(PURPLE, Icon::Layers)
                .subtitle(if alt.is_empty() { l.pick(["无法显示这张图片", "無法顯示這張圖片", "This picture can't be shown"]) } else { alt })
                .trail(Trail::None);
            let _ = widgets::list(c, t, cx.f, m.dp(6.0), &[row]);
        }
    }
}

/// Decode a picture's bytes into RGBA, no wider than `max_w`.
fn decode_picture(bytes: &[u8], max_w: u32) -> Option<(u32, u32, Vec<u8>)> {
    if let Some(picture) = decode_png(bytes) {
        return scale_picture(picture, max_w);
    }
    decode_jpeg(bytes).and_then(|picture| scale_picture(picture, max_w))
}

fn decode_png(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    if w == 0 || h == 0 || w as u64 * h as u64 > 16_000_000 {
        return None;
    }
    let bpp = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Grayscale => 1,
        png::ColorType::Indexed => return None,
    };
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for pixel in buf.chunks_exact(bpp) {
        match bpp {
            4 => rgba.extend_from_slice(pixel),
            3 => {
                rgba.extend_from_slice(pixel);
                rgba.push(255);
            }
            2 => {
                let g = pixel[0];
                rgba.extend_from_slice(&[g, g, g, pixel[1]]);
            }
            _ => {
                let g = pixel[0];
                rgba.extend_from_slice(&[g, g, g, 255]);
            }
        }
    }
    Some((w, h, rgba))
}

fn decode_jpeg(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(bytes));
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    let (w, h) = (u32::from(info.width), u32::from(info.height));
    if w == 0 || h == 0 || w as u64 * h as u64 > 16_000_000 {
        return None;
    }
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    match info.pixel_format {
        jpeg_decoder::PixelFormat::L8 => {
            for g in pixels {
                rgba.extend_from_slice(&[g, g, g, 255]);
            }
        }
        jpeg_decoder::PixelFormat::RGB24 => {
            for p in pixels.chunks_exact(3) {
                rgba.extend_from_slice(&[p[0], p[1], p[2], 255]);
            }
        }
        jpeg_decoder::PixelFormat::CMYK32 => {
            for p in pixels.chunks_exact(4) {
                let k = u16::from(p[3]);
                let c = (u16::from(p[0]) * k) / 255;
                let m = (u16::from(p[1]) * k) / 255;
                let y = (u16::from(p[2]) * k) / 255;
                rgba.extend_from_slice(&[c as u8, m as u8, y as u8, 255]);
            }
        }
        _ => return None,
    }
    Some((w, h, rgba))
}

fn scale_picture((w, h, rgba): (u32, u32, Vec<u8>), max_w: u32) -> Option<(u32, u32, Vec<u8>)> {
    if w <= max_w {
        return Some((w, h, rgba));
    }
    let nw = max_w.max(1);
    let nh = ((h as u64 * nw as u64) / w as u64).max(1) as u32;
    let mut scaled = Vec::with_capacity(nw as usize * nh as usize * 4);
    for y in 0..nh {
        let sy = (y as u64 * h as u64 / nh as u64) as u32;
        for x in 0..nw {
            let sx = (x as u64 * w as u64 / nw as u64) as u32;
            let at = ((sy as usize * w as usize) + sx as usize) * 4;
            scaled.extend_from_slice(&rgba[at..at + 4]);
        }
    }
    Some((nw, nh, scaled))
}

mod tests {
    use super::*;

    #[test]
    fn a_paragraph_is_wrapped_to_the_content_width() {
        let t = Theme::light(1200, 3200, 3.0);
        let f = crate::font::bundled();
        let c = Canvas::new(1200, 3200);
        let w = t.m.w as f32 - t.m.title_x * 2.0;
        let text = "完整镜像，解压后按下面的命令刷入 recovery 分区。";
        let lines = widgets::wrap(&c, &f.body, text, t.m.dp(15.0), w);
        for line in &lines {
            assert!(
                c.text_width(&f.body, line, t.m.dp(15.0)) <= w + t.m.dp(15.0),
                "{line:?} is wider than the page and a hanging punctuation mark: {lines:?}"
            );
        }
        assert!(!lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn a_link_row_is_as_tall_as_the_recovery_draws_it() {
        let t = Theme::light(1440, 3200, 3.5);
        let f = crate::font::bundled();
        let c = Canvas::new(1440, 3200);
        let h = link_height(&c, &t, &f, Lang::Hans, "下载", "https://example.com/a/very/long/address/for/a/file.zip", true);
        assert!(h > t.m.row_h, "a link with its host under it is taller than a row: {h}");
    }
}
