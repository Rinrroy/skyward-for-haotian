//! Magisk, KernelSU and APatch modules as they lie under `/data/adb`.

use anyhow::{bail, Context, Result};
use skyward_ui::app::{MagiskModule, MagiskRoot};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const MODULE_DIRS: [&str; 3] = ["/data/adb/modules", "/data/adb/ksu/modules", "/data/adb/ap/modules"];

/// The root solution installed, and the modules it runs.
pub fn read() -> (MagiskRoot, Result<Vec<MagiskModule>>) {
    if !Path::new("/data").exists() {
        return (MagiskRoot::Locked, Ok(Vec::new()));
    }
    let root = if Path::new("/data/adb/magisk/magisk").exists() {
        Some("Magisk")
    } else if Path::new("/data/adb/ksud").exists() {
        Some("KernelSU")
    } else if Path::new("/data/adb/apd").exists() {
        Some("APatch")
    } else {
        None
    };
    let mut modules = Vec::new();
    for dir in MODULE_DIRS {
        let dir = Path::new(dir);
        if !dir.is_dir() {
            continue;
        }
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => return (root.map_or(MagiskRoot::Absent, |r| MagiskRoot::Present(r.into())), Err(e.into())),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(module) = module_at(&path) else { continue };
            if modules.iter().any(|m: &MagiskModule| m.id == module.id) {
                continue;
            }
            modules.push(module);
        }
    }
    modules.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    let root = match root {
        Some(name) => MagiskRoot::Present(name.into()),
        None => MagiskRoot::Absent,
    };
    (root, Ok(modules))
}

fn module_at(path: &Path) -> Option<MagiskModule> {
    let prop = fs::read_to_string(path.join("module.prop")).ok()?;
    let mut id = None;
    let mut name = None;
    let mut version = String::new();
    let mut author = String::new();
    let mut description = String::new();
    for line in prop.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        let value = value.trim();
        match key.trim() {
            "id" => id = Some(value.to_string()),
            "name" => name = Some(value.to_string()),
            "version" => version = value.to_string(),
            "author" => author = value.to_string(),
            "description" => description = value.to_string(),
            _ => {}
        }
    }
    Some(MagiskModule {
        id: id?,
        name: name.unwrap_or_else(|| path.file_name().unwrap_or_default().to_string_lossy().into_owned()),
        version,
        author,
        description,
        enabled: !path.join("disable").exists(),
        removing: path.join("remove").exists(),
        path: path.to_string_lossy().into_owned(),
    })
}

pub fn version() -> Option<String> {
    let binary = Path::new("/data/adb/magisk/magisk");
    if !binary.exists() {
        return None;
    }
    let output = Command::new(binary).arg("-v").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

pub fn toggle(path: &str, enabled: bool) -> Result<()> {
    let path = Path::new(path);
    let disable = path.join("disable");
    if enabled {
        if disable.exists() {
            fs::remove_file(&disable).with_context(|| format!("remove {}", disable.display()))?;
        }
    } else if !disable.exists() {
        fs::write(&disable, b"").with_context(|| format!("write {}", disable.display()))?;
    }
    Ok(())
}

pub fn remove(path: &str) -> Result<()> {
    let path = Path::new(path);
    let remove = path.join("remove");
    fs::write(&remove, b"").with_context(|| format!("write {}", remove.display()))?;
    Ok(())
}

/// Unpack a module zip into `/data/adb/modules`, using its `module.prop` id.
/// The module's own install script is not run; overlay-only modules work,
/// modules that install files with a script need Magisk itself.
pub fn install(zip: &str) -> Result<()> {
    let zip = Path::new(zip);
    if !zip.is_file() {
        bail!("{} is not a file", zip.display());
    }
    let top = Path::new("/data/adb/modules");
    fs::create_dir_all(top).with_context(|| format!("make {}", top.display()))?;
    let tmp = top.join(".skyward-install");
    if tmp.exists() {
        fs::remove_dir_all(&tmp)?;
    }
    fs::create_dir_all(&tmp)?;
    let status = Command::new("/system/bin/unzip")
        .args(["-o", &zip.to_string_lossy(), "-d", &tmp.to_string_lossy()])
        .status()
        .context("run unzip")?;
    if !status.success() {
        bail!("unzip exited {status}");
    }
    let prop = find_prop(&tmp).context("the zip holds no module.prop")?;
    let text = fs::read_to_string(&prop)?;
    let id = text
        .lines()
        .find_map(|line| line.strip_prefix("id="))
        .map(str::trim)
        .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)))
        .context("module.prop holds no usable id")?
        .to_string();
    let folder = prop.parent().context("module.prop has no folder")?;
    let into = top.join(&id);
    if into.exists() {
        let old = top.join(format!(".{id}.old"));
        let _ = fs::remove_dir_all(&old);
        fs::rename(&into, &old)?;
    }
    fs::rename(folder, &into).or_else(|_| {
        fs::create_dir_all(&into)?;
        copy_dir(folder, &into)
    })?;
    let _ = fs::remove_dir_all(&tmp);
    Ok(())
}

fn find_prop(dir: &Path) -> Option<PathBuf> {
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_prop(&path) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|n| n == "module.prop") {
            return Some(path);
        }
    }
    None
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    for entry in fs::read_dir(from)?.flatten() {
        let path = entry.path();
        let target = to.join(entry.file_name());
        if path.is_dir() {
            fs::create_dir_all(&target)?;
            copy_dir(&path, &target)?;
        } else {
            fs::copy(&path, &target)?;
        }
    }
    Ok(())
}
