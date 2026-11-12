//! The slot's vbmeta image kept as a copy that outlives a start, and put
//! back from one.
//!
//! `/tmp` is memory in a recovery, so the backup `avbctl` writes there
//! before it changes a flag is a convenience for the session and nothing
//! more: a phone restarted after a bad write has nothing left to be put back
//! with. This is the lasting copy. It goes in the recovery's own folder in
//! the owner's storage, beside the backups, so that what the recovery keeps
//! on a phone is all found in one place.
//!
//! **The write back is the half that can brick a phone**, and the check that
//! guards it is `avbctl`'s rather than this module's: an image is only
//! written if the rollback index it carries is not below the one the
//! partition holds now, because writing an older image back is exactly what
//! makes a bootloader refuse to start. That check is made against the
//! partition as it is at that moment, not against what a page was showing.

use skyward_ui::app::{Place, VbmetaCopy};
use skyward_ui::lang::Lang;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
/// Where the copies are kept, from the top of a place.
pub const FOLDER: &str = "Skyward/vbmeta";

/// The folder of `place` the copies go in.
pub fn folder(place: Place) -> PathBuf {
    let top = match place {
        Place::Storage => Path::new(skyward_crypto::data::SDCARD),
        Place::UsbDrive => Path::new(skyward_ui::app::USB_DRIVE),
    };
    top.join(FOLDER)
}

/// Keep the slot's vbmeta image as it is now, and say how it went in words
/// for the page.
pub fn keep(place: Place, lang: Lang) -> String {
    let dir = folder(place);
    match skyward_install::commands::avbctl::keep(&dir) {
        Ok(path) => {
            let _ = skyward_sys::kmsg(&format!("the slot's vbmeta is kept at {}", path.display()));
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            format!("{} {name}", lang.pick([
                "已备份当前 vbmeta：",
                "已備份目前 vbmeta：",
                "This slot's vbmeta is kept:",
            ]))
        }
        Err(e) => {
            let _ = skyward_sys::kmsg(&format!("the slot's vbmeta is not kept: {e:#}"));
            format!("{}: {e}", lang.pick(["备份失败", "備份失敗", "The copy was not kept"]))
        }
    }
}

/// Write the copy `name` in `place` back into the slot's vbmeta, and say how
/// it went in words for the page: the rollback index refusing it is one of
/// the answers, not a failure of the page.
pub fn write_back(place: Place, name: &str, lang: Lang) -> String {
    let path = folder(place).join(name);
    match skyward_install::commands::avbctl::write_back(&path) {
        Ok(skyward_install::commands::avbctl::Back::Written) => {
            let _ = skyward_sys::kmsg(&format!("vbmeta written back from {}", path.display()));
            lang.pick([
                "已写回并校验，重启后生效。",
                "已寫回並驗證，重新啟動後生效。",
                "Written back and verified; effective after the next start.",
            ])
            .to_string()
        }
        Ok(skyward_install::commands::avbctl::Back::Refused { why }) => {
            let _ = skyward_sys::kmsg(&format!("vbmeta is not written back from {}: {why}", path.display()));
            refusal(lang, &why)
        }
        Err(e) => {
            let _ = skyward_sys::kmsg(&format!("vbmeta is not written back from {}: {e:#}", path.display()));
            format!("{}: {e}", lang.pick(["写回失败", "寫回失敗", "It was not written back"]))
        }
    }
}

/// What a refusal reads as on the page: the rollback index's own refusal
/// names both numbers, and anything else says what could not be read.
///
/// The numbers travel in the refusal as `ROLLBACK <copy> <phone>`, which is
/// `avbctl`'s to build and this to say: the words a person reads belong to
/// the page, not to the crate that writes partitions.
fn refusal(lang: Lang, why: &str) -> String {
    let numbers = why.strip_prefix("ROLLBACK ").and_then(|rest| {
        let mut parts = rest.split_whitespace();
        Some((parts.next()?.parse::<u64>().ok()?, parts.next()?.parse::<u64>().ok()?))
    });
    match numbers {
        Some((copy, phone)) => skyward_ui::tr!(
            lang,
            "已拒绝写回：备份的回滚索引 {copy} 低于手机当前的 {phone}。写回它会让手机无法开机，请改用更新的备份。",
            "已拒絕寫回：備份的回滾索引 {copy} 低於手機目前的 {phone}。寫回它會讓手機無法開機，請改用較新的備份。",
            "Not written back: the copy's rollback index {copy} is below the phone's {phone}. Writing it would leave the phone unable to start; use a newer copy."
        ),
        None => skyward_ui::tr!(
            lang,
            "已拒绝写回：无法确认回滚索引。{why}",
            "已拒絕寫回：無法確認回滾索引。{why}",
            "Not written back: the rollback index can't be established. {why}"
        ),
    }
}

/// The copies kept where the places that can be read have them, most recent
/// first, or why they could not be read.
pub fn find(storage: bool, usb: bool) -> Result<Vec<VbmetaCopy>, String> {
    let mut found = Vec::new();
    for place in [Place::Storage, Place::UsbDrive] {
        let open = match place {
            Place::Storage => storage,
            Place::UsbDrive => usb,
        };
        if !open {
            continue;
        }
        found.extend(in_place(place, &folder(place))?);
    }
    // The newest first, which is the one a person is most likely to want;
    // two taken in the same second keep a steady order rather than whichever
    // the folder happened to return first.
    found.sort_by(|a, b| b.made.cmp(&a.made).then_with(|| a.name.cmp(&b.name)));
    Ok(found)
}

/// The copies in `dir`, as their own headers describe them.
fn in_place(place: Place, dir: &Path) -> Result<Vec<VbmetaCopy>, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // A folder a phone has never made a copy into is empty, not broken.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{e}")),
    };
    let mut copies = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "img") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        // The slot and when it was taken are in the name this recovery writes
        // them under; the index is in the image itself. A copy whose header
        // will not read is still shown, with the reason: a person looking for
        // one should see that it is there, and the write refuses on it anyway.
        let (suffix, made) = skyward_vab::vbmeta::Copy::slot_of(&name)
            .unwrap_or_else(|| (String::new(), meta.mtime()));
        let rollback = match std::fs::read(&path) {
            Ok(image) => skyward_vab::vbmeta::rollback(&image),
            Err(e) => Err(format!("{e}")),
        };
        if let Err(why) = &rollback {
            let _ = skyward_sys::kmsg(&format!("the copy at {} does not read: {why}", path.display()));
        }
        copies.push(VbmetaCopy { place, name, suffix, size: meta.len(), rollback, made });
    }
    Ok(copies)
}
