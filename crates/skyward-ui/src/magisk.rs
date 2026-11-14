//! Magisk, KernelSU and APatch modules, as recovery sees them under
//! `/data/adb`.

use crate::app::{Action, Frame, Hit, MagiskRoot};
use crate::canvas::Canvas;
use crate::geom::Rect;
use crate::icons::Icon;
use crate::pages::{Band, Ctx};
use crate::theme::Theme;
use crate::widgets::{self, Row, Trail};

const MAGISK: &str = "https://github.com/topjohnwu/Magisk/releases/latest";
const KSU: &str = "https://github.com/tiann/KernelSU/releases/latest";
const APATCH: &str = "https://github.com/bmax121/APatch/releases/latest";

fn hits(rects: &[Rect], actions: &[Action]) -> Vec<Hit> {
    rects.iter().zip(actions).map(|(&rect, &action)| Hit { rect, action }).collect()
}

fn page(cx: &Ctx, h: u32) -> Canvas {
    let w = cx.t.m.w;
    Canvas::band(w, h, 0..h, 0..w, cx.t.pal.bg)
}

fn fit(c: Canvas, t: &Theme, bottom: f32) -> Canvas {
    c.crop_height(((bottom + t.m.margin * 2.0).ceil() as u32).max(t.m.h))
}

pub fn screen(cx: &Ctx) -> Frame {
    let (t, f, s, l) = (cx.t, cx.f, cx.state, cx.state.lang);
    let m = &t.m;
    let count = s.magisk.modules.as_ref().and_then(|m| m.as_ref().ok()).map_or(0, |modules| modules.len());
    let need = (count as f32 * 2.0 + 10.0) * m.dp(72.0) + m.h as f32;
    let mut c = page(cx, need.ceil() as u32);
    let mut out = Vec::new();
    let top = m.content_top;
    let mut y = widgets::section_first(&mut c, t, f, top, "Magisk / KernelSU / APatch");
    y = match &s.magisk.root {
        MagiskRoot::Unknown => widgets::footnote(
            &mut c,
            t,
            f,
            y,
            l.pick(["正在读取 root 方案", "正在讀取 root 方案", "Reading the root solution"]),
        ),
        MagiskRoot::Locked => widgets::footnote(
            &mut c,
            t,
            f,
            y,
            l.pick(["需先解密 data 分区", "需先解密 data 分割區", "Decrypt the data partition first"]),
        ),
        MagiskRoot::Absent => {
            let y = widgets::footnote(
                &mut c,
                t,
                f,
                y,
                l.pick([
                    "未检测到 Magisk、KernelSU 或 APatch。可先下载官方安装包，再在「安装」中选择它。",
                    "未偵測到 Magisk、KernelSU 或 APatch。可先下載官方安裝包，再在「安裝」中選擇它。",
                    "No Magisk, KernelSU or APatch was found. Download an official package, then choose it from Install.",
                ]),
            );
            let rows = [
                Row::new("Magisk").icon(crate::color::Rgba::rgb(0x34, 0x82, 0xff), Icon::Shield).subtitle(MAGISK),
                Row::new("KernelSU").icon(crate::color::Rgba::rgb(0x1d, 0xcd, 0x3a), Icon::Shield).subtitle(KSU),
                Row::new("APatch").icon(crate::color::Rgba::rgb(0x77, 0x67, 0xf9), Icon::Shield).subtitle(APATCH),
            ];
            let list = widgets::list(&mut c, t, f, y, &rows);
            out.extend(hits(&list.rows, &[Action::BrowserTo(MAGISK), Action::BrowserTo(KSU), Action::BrowserTo(APATCH)]));
            list.bottom
        }
        MagiskRoot::Present(name) => {
            let list = widgets::list(
                &mut c,
                t,
                f,
                y,
                &[Row::new(name).icon(crate::color::Rgba::rgb(0x1d, 0xcd, 0x3a), Icon::Shield).trail(Trail::None)],
            );
            out.extend(hits(&list.rows, &[Action::ReadMagisk]));
            list.bottom
        }
    };
    if matches!(&s.magisk.root, MagiskRoot::Present(name) if name == "Magisk") {
        let version = s.magisk.version.clone().unwrap_or_else(|| "未知版本".into());
        let latest = s.magisk.latest.clone();
        let sub = match latest {
            Some(latest) if latest != version => format!("已安装 {version} · 最新 {latest}"),
            Some(_) => format!("已安装 {version} · 已是最新"),
            None => format!("已安装 {version} · 检查 GitHub Releases"),
        };
        let row = Row::new(l.pick(["Magisk 版本", "Magisk 版本", "Magisk version"]))
            .subtitle(&sub)
            .icon(crate::color::Rgba::rgb(0x34, 0x82, 0xff), Icon::Shield)
            .trail(Trail::None);
        let list = widgets::list(&mut c, t, f, y, &[row]);
        y = list.bottom;
        out.extend(hits(&list.rows, &[Action::CheckMagiskUpdate]));
        if let Some(latest) = &s.magisk.latest {
            if Some(latest) != s.magisk.version.as_ref() {
                let row = Row::new(l.pick(["下载并安装更新", "下載並安裝更新", "Download and install"]))
                    .subtitle(latest)
                    .icon(crate::color::Rgba::rgb(0x1d, 0xcd, 0x3a), Icon::Install)
                    .trail(Trail::None);
                let list = widgets::list(&mut c, t, f, y, &[row]);
                y = list.bottom;
                out.extend(hits(&list.rows, &[Action::MagiskDownload]));
            }
        }
    }
    y = widgets::section(&mut c, t, f, y, l.pick(["模块", "模組", "Modules"]));
    match &s.magisk.modules {
        None => {
            y = widgets::footnote(&mut c, t, f, y, l.pick(["正在读取模块", "正在讀取模組", "Reading the modules"]));
        }
        Some(Err(why)) => {
            y = widgets::footnote(&mut c, t, f, y, why);
        }
        Some(Ok(modules)) if modules.is_empty() => {
            y = widgets::footnote(&mut c, t, f, y, l.pick(["没有已安装的模块", "沒有已安裝的模組", "No modules are installed"]));
        }
        Some(Ok(modules)) => {
            for (i, module) in modules.iter().enumerate() {
                let sub = if module.description.is_empty() {
                    format!("{} · {}", module.version, module.author)
                } else {
                    format!("{} · {}", module.version, module.description)
                };
                let row = Row::new(&module.name).subtitle(&sub).trail(Trail::Switch(module.enabled));
                let list = widgets::list(&mut c, t, f, y, &[row]);
                out.extend(hits(&list.rows, &[Action::ToggleMagiskModule(i)]));
                y = list.bottom + m.dp(6.0);
                let remove = Row::new(l.pick(["移除", "移除", "Remove"]))
                    .icon(crate::color::Rgba::rgb(0xfa, 0x38, 0x2e), Icon::Wipe)
                    .trail(Trail::None);
                let list = widgets::list(&mut c, t, f, y, &[remove]);
                out.extend(hits(&list.rows, &[Action::RemoveMagiskModule(i)]));
                y = list.bottom + m.card_gap;
            }
        }
    }
    y = widgets::section(&mut c, t, f, y, l.pick(["仓库与安装", "倉庫與安裝", "Repository and install"]));
    let rows = [
        Row::new(l.pick(["模块仓库", "模組倉庫", "Module repository"]))
            .icon(crate::color::Rgba::rgb(0x34, 0x82, 0xff), Icon::Globe)
            .trail(Trail::Arrow),
        Row::new(l.pick(["从本地 zip 安装", "從本機 zip 安裝", "Install a local zip"]))
            .icon(crate::color::Rgba::rgb(0x1d, 0xcd, 0x3a), Icon::Install)
            .trail(Trail::None),
    ];
    let list = widgets::list(&mut c, t, f, y, &rows);
    out.extend(hits(&list.rows, &[Action::MagiskRepo, Action::MagiskPickZip]));
    y = widgets::footnote(
        &mut c,
        t,
        f,
        list.bottom,
        l.pick([
            "本地安装读取 /sdcard/Download/Skyward/module.zip，解压到 /data/adb/modules；需要重启后由 Magisk 完成挂载。",
            "本機安裝讀取 /sdcard/Download/Skyward/module.zip，解壓到 /data/adb/modules；需要重新啟動後由 Magisk 完成掛載。",
            "Local install reads /sdcard/Download/Skyward/module.zip and unpacks it into /data/adb/modules; Magisk applies it after a reboot.",
        ]),
    );
    Frame::new(t, fit(c, t, y), out, l.pick(["模块", "模組", "Modules"]), top)
}
