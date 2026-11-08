//! What the phone's own secure services need before they will run inside a
//! recovery: the libraries their partitions were built against, and a
//! service directory that knows the phone describes them.

use anyhow::{ensure, Context, Result};
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Where a phone keeps the bundles of shared libraries its own partitions
/// were built against.
const BUNDLE_DIRS: [&str; 4] = ["/mnt/system_ext/apex", "/mnt/system/system/apex", "/mnt/product/apex", "/vendor/apex"];

/// The file inside a bundle that holds its filesystem. It is stored whole,
/// so the bundle can be opened where it lies.
const BUNDLE_IMAGE: &str = "apex_payload.img";

/// Where Android opens its bundles, and the directory of 64-bit libraries
/// inside one.
const BUNDLE_MOUNT: &str = "/apex";
const BUNDLE_LIBS: &str = "lib64";

/// Where the installed system keeps its own 64-bit libraries, and where a
/// library gathered for a phone that carries no bundle is put.
const INSTALLED_LIB64: &str = "/mnt/system/system/lib64";
const STAGED_DIR: &str = "/kmlibs/vendor";

/// Which build of the vendor interface the phone's own partitions were
/// made for.
const VENDOR_INTERFACE: &str = "ro.vndk.version";

/// Where the phone describes the services its own partitions carry, and
/// where a recovery looks for such descriptions.
const DESCRIPTIONS: [&str; 2] = ["/vendor/etc/vintf/manifest", "/odm/etc/vintf/manifest"];
const RECOVERY_DESCRIPTIONS: &str = "/system/etc/vintf/manifest";

/// The name init knows the service directory by, and the program it runs.
const DIRECTORY_SERVICE: &str = "servicemanager";
const DIRECTORY_PROGRAM: &str = "/system/bin/servicemanager";

/// Where init says which process each of its services runs as, one
/// property per service, the service's name following this.
const SERVICE_PIDS: &str = "init.svc_debug_pid.";

/// How long a service init starts again may take to register again.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(10);

/// How long init may take to see a service it stopped gone.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// The loader a program built to run before anything is mounted asks for,
/// and where the installed system keeps the one it was built for: in the
/// bundle that carries the rest of its own runtime.
const EARLY_LOADER: &str = "/system/bin/bootstrap/linker64";
const RUNTIME_BUNDLE: &str = "com.android.runtime";
const BUNDLE_LOADER: &str = "bin/linker64";

/// Which programs may register which hardware services, and where the
/// directory for those services looks for that.
const SERVICE_LABELS: &str = "/mnt/system/system/etc/selinux/plat_hwservice_contexts";
const RECOVERY_LABELS: &str = "/system/etc/selinux";

/// The vendor's own service labels: the folder they live in and the file
/// among them that says which interfaces may register with the hardware
/// directory.
const VENDOR_LABELS: &str = "/vendor/etc/selinux";
const VENDOR_LABEL_FILE: &str = "vendor_hwservice_contexts";

/// Every hardware service's interface chain ends here, and the directory
/// checks the whole chain before it lets one register.
const BASE_INTERFACE: &str = "android.hidl.base::IBase";

/// The driver the phone's hardware services talk over, as they expect to
/// find it and as a recovery has it.
const HARDWARE_DRIVER: &str = "/dev/hwbinder";
const HARDWARE_DRIVER_NODE: &str = "/dev/binderfs/hwbinder";

/// The directory for those services is itself one of them, and it turns
/// itself off when the running system does not describe it.
const HARDWARE_DIRECTORY: &str = "android.hidl.manager@1.2::IServiceManager/default";

/// What says the directory is off. It is read by the services, and in a
/// recovery there is nothing else to set it.
const DIRECTORY_OFF: &str = "hwservicemanager.disabled";

/// Where a recovery is given the descriptions of the services the running
/// system carries. A recovery has no framework of its own, so this is read
/// as the framework's description of what it offers.
const RECOVERY_MANIFEST: &str = "/system/etc/vintf/manifest.xml";

/// What the directory for the phone's hardware services of the older kind
/// needs before it will run inside a recovery. The program is the installed
/// system's own: it is built to run before any partition is mounted, so it
/// asks for a loader under a name a recovery does not carry; it decides what
/// may register from labels a recovery does not carry either; and it reads
/// the driver under a name that a recovery, which has the driver's own
/// filesystem, does not have.
///
/// `wanted` names the hardware services that will be asked for.
pub fn prepare_hardware_directory(wanted: &[String]) -> Result<()> {
    // A loader and the lowest libraries are built together and agree
    // between them on how a program's own storage is laid out, so the
    // installed system's programs are loaded by the installed system's
    // loader rather than by the older one a recovery carries.
    let loader = Path::new(EARLY_LOADER);
    let from = bundle(RUNTIME_BUNDLE)?
        .map(|at| at.join(BUNDLE_LOADER))
        .filter(|p| p.is_file())
        .with_context(|| format!("the installed system carries no {RUNTIME_BUNDLE}, which holds the loader its own programs ask for"))?;
    let dir = loader.parent().expect("the loader is in a directory");
    fs::create_dir_all(dir).with_context(|| format!("make {}", dir.display()))?;
    // create_dir_all takes the process's umask, and the recovery service starts
    // with 077: the directory would be one only root may walk into, and the
    // programs that register with this one run as users, so the kernel would
    // refuse to load them their loader (EACCES on the exec, reported against
    // the program rather than against the loader).
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).with_context(|| format!("open {} to the services", dir.display()))?;
    if loader.exists() {
        fs::remove_file(loader).with_context(|| format!("take away {}", loader.display()))?;
    }
    std::os::unix::fs::symlink(&from, loader).with_context(|| format!("point {} at {}", loader.display(), from.display()))?;
    let driver = Path::new(HARDWARE_DRIVER);
    if !driver.exists() {
        std::os::unix::fs::symlink(HARDWARE_DRIVER_NODE, driver)
            .with_context(|| format!("point {HARDWARE_DRIVER} at {HARDWARE_DRIVER_NODE}"))?;
    }
    // The driver's own filesystem makes its nodes root-only, where the
    // installed system's ueventd rules open them to every program; the
    // services that talk over this one run as users.
    let node = Path::new(HARDWARE_DRIVER_NODE);
    if node.exists() {
        fs::set_permissions(node, fs::Permissions::from_mode(0o666)).with_context(|| format!("open {} to the services", node.display()))?;
    }
    let from = Path::new(SERVICE_LABELS);
    let into = Path::new(RECOVERY_LABELS).join(from.file_name().expect("a named file"));
    fs::create_dir_all(RECOVERY_LABELS).with_context(|| format!("make {RECOVERY_LABELS}"))?;
    fs::copy(from, &into).with_context(|| format!("copy {} to {}", from.display(), into.display()))?;
    declare_service_labels(wanted)?;
    fs::write(RECOVERY_MANIFEST, describe_hardware(wanted)?).with_context(|| format!("write {RECOVERY_MANIFEST}"))?;
    // The recovery service starts with umask 077, so the file is left one
    // only root may read. The programs that register with the directory run
    // as users, and each reads this before registering to learn whether the
    // phone allows it; finding it unreadable, they refuse to register and
    // the credential service never comes up. Leave the file open and the
    // folder above it walkable.
    let manifest = Path::new(RECOVERY_MANIFEST);
    if let Some(dir) = manifest.parent() {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("open {} to the services", dir.display()))?;
    }
    fs::set_permissions(manifest, fs::Permissions::from_mode(0o644))
        .with_context(|| format!("open {RECOVERY_MANIFEST} to the services"))?;
    open_descriptions(Path::new(RECOVERY_DESCRIPTIONS))?;
    // The directory turns itself off when it cannot find its own
    // description, and a run of it before this was written may have left
    // that behind.
    skyward_sys::set_prop(DIRECTORY_OFF, "false")?;
    Ok(())
}

/// The description a recovery offers of the hardware services that will be
/// asked for, and of the directory they are asked of. Each is named as the
/// package it is in and the rest of its name, which is how a description
/// holds the two.
fn describe_hardware(wanted: &[String]) -> Result<String> {
    let mut out = String::from("<manifest version=\"1.0\" type=\"framework\">\n");
    for name in std::iter::once(HARDWARE_DIRECTORY).chain(wanted.iter().map(String::as_str)) {
        let (package, rest) = name
            .split_once('@')
            .with_context(|| format!("{name} does not read as a hardware service's name"))?;
        // A description holds the version and the interface as elements of
        // their own; the one-line name alone is not read as either, and a
        // service it names is then refused as undescribed.
        let (version, rest) = rest
            .split_once("::")
            .with_context(|| format!("{name} does not name a version"))?;
        let (interface, instance) = rest
            .split_once('/')
            .with_context(|| format!("{name} does not name an instance"))?;
        out.push_str("    <hal format=\"hidl\">\n");
        out.push_str(&format!("        <name>{package}</name>\n"));
        out.push_str("        <transport>hwbinder</transport>\n");
        out.push_str(&format!("        <version>{version}</version>\n"));
        out.push_str("        <interface>\n");
        out.push_str(&format!("            <name>{interface}</name>\n"));
        out.push_str(&format!("            <instance>{instance}</instance>\n"));
        out.push_str("        </interface>\n");
        out.push_str(&format!("        <fqname>@{version}::{interface}/{instance}</fqname>\n"));
        out.push_str("    </hal>\n");
    }
    out.push_str("</manifest>\n");
    Ok(out)
}

/// Let the services read the descriptions. They run as users, and the
/// recovery service writes under umask 077, so every description it wrote
/// is one only root may read. That is not a local failure: a single
/// unreadable fragment fails the whole framework manifest, and the services
/// are then told the phone describes nothing at all.
fn open_descriptions(dir: &Path) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("open {} to the services", dir.display()))?;
    for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o644))
                .with_context(|| format!("open {} to the services", entry.path().display()))?;
        }
    }
    Ok(())
}

/// Give the vendor's service labels the lines for the services this phone
/// still carries. hwservicemanager decides whether one may register by
/// looking its interface up in this file; a lookup that finds nothing is a
/// refusal of its own, not a policy decision, so a phone in permissive mode
/// refuses just the same. Android 17's file no longer names the older
/// services, so a copy with the lines added is shown in its place.
fn declare_service_labels(wanted: &[String]) -> Result<()> {
    let dir = Path::new(VENDOR_LABELS);
    let labels = dir.join(VENDOR_LABEL_FILE);
    if !labels.is_file() {
        return Ok(());
    }
    let have = fs::read_to_string(&labels).with_context(|| format!("read {}", labels.display()))?;
    let mut missing = Vec::new();
    if !have.contains(BASE_INTERFACE) {
        missing.push(BASE_INTERFACE.to_string());
    }
    for name in wanted {
        if let Some(interface) = interface_of(name) {
            if !have.contains(&interface) {
                missing.push(interface);
            }
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    // The installed system's own file still names them, so the lines are
    // taken from there rather than written out here: each carries the label
    // the system's policy gives that service.
    let plat = fs::read_to_string(SERVICE_LABELS).with_context(|| format!("read {SERVICE_LABELS}"))?;
    let mut add = String::new();
    for interface in missing {
        if let Some(line) = plat.lines().find(|line| line.starts_with(&interface)) {
            add.push_str(line);
            add.push('\n');
        }
    }
    if add.is_empty() {
        return Ok(());
    }
    let staged = Path::new("/tmp/selinux");
    if staged.exists() {
        fs::remove_dir_all(staged).with_context(|| format!("clear {}", staged.display()))?;
    }
    copy_labels(dir, staged)?;
    let staged_labels = staged.join(VENDOR_LABEL_FILE);
    let mut text = fs::read_to_string(&staged_labels).with_context(|| format!("read {}", staged_labels.display()))?;
    text.push_str(&add);
    fs::write(&staged_labels, text).with_context(|| format!("add the services to {}", staged_labels.display()))?;
    fs::set_permissions(&staged_labels, fs::Permissions::from_mode(0o644))?;
    skyward_block::mount::bind(staged, dir).with_context(|| format!("show {} at {}", staged.display(), dir.display()))?;
    Ok(())
}

/// The interface a service's name belongs to, without the version or the
/// instance: `pkg@1.2::IFace/default` becomes `pkg::IFace`.
fn interface_of(name: &str) -> Option<String> {
    let (package, rest) = name.split_once('@')?;
    let (_, rest) = rest.split_once("::")?;
    let (interface, _) = rest.split_once('/')?;
    Some(format!("{package}::{interface}"))
}

/// Copy the labels folder, keeping it readable: the directory and every
/// file in it are read by services that run as users, and the recovery
/// service writes under umask 077.
fn copy_labels(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).with_context(|| format!("make {}", to.display()))?;
    fs::set_permissions(to, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("open {} to the services", to.display()))?;
    for entry in fs::read_dir(from).with_context(|| format!("read {}", from.display()))? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_labels(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &target)
                .with_context(|| format!("copy {} to {}", entry.path().display(), target.display()))?;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o644))
                .with_context(|| format!("open {} to the services", target.display()))?;
        }
    }
    Ok(())
}

/// Open the bundle of libraries the phone's own partitions were built
/// against and give the directory they are in. A phone whose partitions
/// need no bundle has none to open.
///
/// The bundle is named for the build of the vendor interface the phone's
/// partitions were made for, but not every phone names it that way: one
/// build of Android 14 carries it as `com.android.vndk.current`, the one
/// after that as `com.android.vndk.v34`. The name is therefore tried in
/// the order the phone is likeliest to carry it, and the one that is
/// found is the one that is opened.
///
/// A phone that carries no such bundle has the libraries its own
/// partitions were built against somewhere else, and those are gathered
/// where a program that lacks one can find it.
pub fn libraries() -> Result<Option<PathBuf>> {
    let version = skyward_sys::prop(VENDOR_INTERFACE);
    let mut names = Vec::new();
    if let Some(version) = &version {
        names.push(format!("com.android.vndk.v{version}"));
    }
    // The build that carries the interface under a name of its own rather
    // than under the number of the build it is.
    names.push("com.android.vndk.current".to_string());
    for name in &names {
        if let Some(at) = bundle(name)? {
            return Ok(Some(at.join(BUNDLE_LIBS)));
        }
    }
    Ok(staged())
}

/// Where the libraries of the installed system are put when the phone
/// carries no bundle of its own, and the directory they are in.
///
/// The libraries a program of the vendor needs are those the vendor was
/// built against, and the vendor carries none of them: it is the bundle
/// of the vendor interface that holds them. A phone without that bundle
/// leaves the programs of the vendor without the libraries they were
/// built for, and the one they are asked for most is the heap the
/// secure world hands out its buffers through.
///
/// Only that one library is taken, and only from the installed system's
/// own library folder, which the phone keeps for the programs of the
/// system and not for those of the vendor: a library of the system's
/// build brought in among the vendor's own is of another build, and the
/// two do not agree on the symbols they share.
fn staged() -> Option<PathBuf> {
    const LIBRARY: &str = "libdmabufheap.so";
    let dir = Path::new(STAGED_DIR);
    let from = Path::new(INSTALLED_LIB64).join(LIBRARY);
    if !from.is_file() {
        return None;
    }
    if fs::create_dir_all(dir).is_err() {
        return None;
    }
    let into = dir.join(LIBRARY);
    if !into.is_file() && fs::copy(&from, &into).is_err() {
        return None;
    }
    Some(dir.to_path_buf())
}

/// Open the bundle of this name and give where it is. A bundle the phone
/// does not carry gives nothing; one already open is left as it is.
///
/// The file is copied into the recovery's own memory first and the loop
/// device points at the copy. A loop device backed by a file inside one of
/// the phone's partitions keeps that whole partition from being unmounted,
/// and installing a package unmounts them all before it writes — a bundle
/// opened where it lies makes every install stop at "Device or resource
/// busy". Where the copy cannot be made the bundle is opened where it lies
/// and the log says so: that costs the install, not the libraries.
fn bundle(name: &str) -> Result<Option<PathBuf>> {
    let at = Path::new(BUNDLE_MOUNT).join(name);
    if skyward_block::mount::is_mount_point(&at)? {
        return Ok(Some(at));
    }
    let Some(file) = BUNDLE_DIRS.iter().map(|d| Path::new(d).join(format!("{name}.apex"))).find(|p| p.is_file()) else {
        return Ok(None);
    };
    let (offset, length) = image(&file)?;
    let opened = held(&file).unwrap_or_else(|e| {
        let _ = skyward_sys::kmsg(&format!("the bundle at {} is opened where it lies: {e:#}", file.display()));
        file.clone()
    });
    let device = skyward_block::loopdev::attach(&opened, offset, length)?;
    fs::create_dir_all(&at).with_context(|| format!("make {}", at.display()))?;
    skyward_block::mount::mount_ro(&device, &at)?;
    Ok(Some(at))
}

/// A copy of `file` in the recovery's own memory, for the loop device to
/// read instead of the file where it lies.
fn held(file: &Path) -> Result<PathBuf> {
    const HELD: &str = "/tmp/bundles";
    let name = file.file_name().with_context(|| format!("{} has no name", file.display()))?;
    let at = Path::new(HELD).join(name);
    fs::create_dir_all(HELD).with_context(|| format!("make {HELD}"))?;
    fs::copy(file, &at).with_context(|| format!("copy {} to {}", file.display(), at.display()))?;
    Ok(at)
}

/// Where the filesystem lies inside a bundle.
fn image(file: &Path) -> Result<(u64, u64)> {
    let f = fs::File::open(file).with_context(|| format!("open {}", file.display()))?;
    let mut bundle = zip::ZipArchive::new(f).with_context(|| format!("read what is in {}", file.display()))?;
    let entry = bundle.by_name(BUNDLE_IMAGE).with_context(|| format!("{} holds no {BUNDLE_IMAGE}", file.display()))?;
    if entry.compression() != zip::CompressionMethod::Stored {
        anyhow::bail!("{} keeps its {BUNDLE_IMAGE} packed, which cannot be opened where it lies", file.display());
    }
    let start = entry.data_start().with_context(|| format!("{BUNDLE_IMAGE} in {} says where it is not", file.display()))?;
    Ok((start, entry.size()))
}

/// The one shape both ways a description names an instance come to:
/// `I/instance`. A fragment writes either `<fqname>I/instance</fqname>`
/// or an `<interface>` with `<name>` and `<instance>`; an fqname may carry
/// a version or the package it belongs to as well.
fn instance_name(fq: &str) -> String {
    let fq = fq.trim();
    let fq = fq.rsplit("::").next().unwrap_or(fq);
    fq.split('@').next().unwrap_or(fq).trim().to_string()
}

fn tag(block: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let from = block.find(open.as_str())? + open.len();
    let to = from + block[from..].find(close.as_str())?;
    Some(block[from..to].trim().to_string())
}

/// Every instance a description declares, in either of the two spellings.
fn named_instances(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut rest = text;
    while let Some(begin) = rest.find("<fqname>") {
        let from = begin + "<fqname>".len();
        let Some(len) = rest[from..].find("</fqname>") else { break };
        out.insert(instance_name(&rest[from..from + len]));
        rest = &rest[from + len + "</fqname>".len()..];
    }
    let mut rest = text;
    while let Some(begin) = rest.find("<interface>") {
        let Some(len) = rest[begin..].find("</interface>") else { break };
        let block = &rest[begin..begin + len];
        if let (Some(interface), Some(instance)) = (tag(block, "name"), tag(block, "instance")) {
            out.insert(format!("{interface}/{instance}"));
        }
        rest = &rest[begin + len + "</interface>".len()..];
    }
    out
}

/// Take out of a description the instances another fragment already
/// declares. Two fragments naming one instance make the directory throw
/// the whole of its VINTF information away: it then finds no service
/// described at all, and every hardware service is turned away on its
/// way in. A `<hal>` left naming no instance is taken away as well.
fn without_declared(text: &str, declared: &BTreeSet<String>) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(begin) = rest.find("<fqname>") {
        let from = begin + "<fqname>".len();
        let Some(len) = rest[from..].find("</fqname>") else { break };
        let end = from + len + "</fqname>".len();
        kept.push_str(&rest[..begin]);
        if !declared.contains(&instance_name(&rest[from..from + len])) {
            kept.push_str(&rest[begin..end]);
        }
        rest = &rest[end..];
    }
    kept.push_str(rest);

    let mut out = String::with_capacity(kept.len());
    let mut rest = kept.as_str();
    while let Some(begin) = rest.find("<hal") {
        let Some(len) = rest[begin..].find("</hal>") else { break };
        let end = begin + len + "</hal>".len();
        out.push_str(&rest[..begin]);
        let block = &rest[begin..end];
        if block.contains("<fqname>") || block.contains("<interface>") {
            out.push_str(block);
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Give the phone's service directory the descriptions of the services in
/// `wanted`, and the description files in `files`, which the phone keeps
/// apart from the rest. The directory turns away a hardware service the
/// running system does not describe, and a recovery starts up describing
/// only the few services of its own. Only the descriptions that are needed
/// are taken: the directory reads the whole of its description directory
/// at once, and one file it cannot make sense of leaves it describing
/// nothing.
pub fn publish_descriptions(wanted: &[String], files: &[String], texts: &[String]) -> Result<usize> {
    let mut declared = BTreeSet::new();
    let into = Path::new(RECOVERY_DESCRIPTIONS);
    fs::create_dir_all(into).with_context(|| format!("make {}", into.display()))?;
    let mut copied = 0;
    let mut take = |from: &Path, text: &str| -> Result<()> {
        let name = from.file_name().with_context(|| format!("{} names no file", from.display()))?;
        fs::write(into.join(name), as_framework(text)).with_context(|| format!("copy {}", from.display()))?;
        copied += 1;
        Ok(())
    };
    for dir in DESCRIPTIONS {
        let listing = match fs::read_dir(dir) {
            Ok(listing) => listing,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("read {dir}")),
        };
        for entry in listing {
            let from = entry.with_context(|| format!("read {dir}"))?.path();
            if from.extension().is_none_or(|e| e != "xml") {
                continue;
            }
            let text = fs::read_to_string(&from).with_context(|| format!("read {}", from.display()))?;
            if wanted.iter().any(|w| describes(&text, w)) {
                take(&from, &text)?;
                declared.extend(named_instances(&text));
            }
        }
    }
    for file in files {
        let from = Path::new(file);
        let text = fs::read_to_string(from).with_context(|| format!("read {file}"))?;
        take(from, &text)?;
        declared.extend(named_instances(&text));
    }
    for (n, text) in texts.iter().enumerate() {
        // The phone's own fragments are in place already; an instance one
        // of them names must not be named again by this recovery's text.
        let text = without_declared(text, &declared);
        declared.extend(named_instances(&text));
        let at = into.join(format!("recovery-{n}.xml"));
        fs::write(&at, as_framework(&text)).with_context(|| format!("write {}", at.display()))?;
        copied += 1;
    }
    let rewritten = say_framework(into)?;
    open_descriptions(into)?;
    if copied > 0 || rewritten > 0 {
        // The directory reads the descriptions once and keeps to them, so
        // it has to start again to see these. It comes back knowing nothing
        // of what had registered with it, and nothing registers again by
        // itself, so init starts each of those services again, as the
        // installed system's init does whenever its directory restarts.
        let registered = registered()?;
        let was = crate::services::running(DIRECTORY_PROGRAM);
        skyward_sys::set_prop("ctl.restart", DIRECTORY_SERVICE).context("have the service directory start again")?;
        answering_again(was)?;
        // The descriptions are in place and the directory is answering
        // with them now. What register_again() waits for is the ROM's own
        // services coming back after that restart; on a build that names
        // one differently, or starts it slowly, the wait runs out.
        // Taking that as fatal kept every one of this recovery's own HALs
        // from starting: no keymint registered, and the data partition
        // stayed closed. Say what happened and carry on -- the services
        // started next register with a directory that already knows their
        // descriptions.
        if let Err(e) = register_again(&registered) {
            let _ = skyward_sys::kmsg(&format!("not all of the phone's services registered again: {e:#}"));
        }
    }
    Ok(copied)
}

/// What has registered with the service directory, each with the init
/// service whose process serves it; the directory's own entry is left out.
fn registered() -> Result<Vec<(String, String)>> {
    let Some(directory) = crate::services::running(DIRECTORY_PROGRAM) else { return Ok(Vec::new()) };
    let binder = skyward_binder::driver::Binder::open()?;
    let started = skyward_sys::props(SERVICE_PIDS);
    let mut out = Vec::new();
    for name in skyward_binder::service::names(&binder)? {
        let Some(service) = skyward_binder::service::find(&binder, &name)? else { continue };
        let pid = skyward_binder::service::pid(&service).with_context(|| format!("ask which process serves {name}"))?;
        if pid == directory {
            continue;
        }
        let (prop, _) = started
            .iter()
            .find(|(_, value)| value.parse() == Ok(pid))
            .with_context(|| format!("{name} is served by process {pid}, which init does not run, so nothing would register it again"))?;
        out.push((name, prop[SERVICE_PIDS.len()..].to_string()));
    }
    Ok(out)
}

/// Have init start again the services behind what had registered, and wait
/// until each name is registered once more. A restart through init waits
/// out the pause it keeps between two starts of a service, seconds after
/// the service last started; stopping it and starting it does not.
fn register_again(registered: &[(String, String)]) -> Result<()> {
    let services: BTreeSet<&str> = registered.iter().map(|(_, service)| service.as_str()).collect();
    for service in &services {
        skyward_sys::set_prop("ctl.stop", service).with_context(|| format!("have init stop {service}"))?;
    }
    for service in &services {
        skyward_sys::wait_prop(&format!("init.svc.{service}"), "stopped", STOP_TIMEOUT)?;
        skyward_sys::set_prop("ctl.start", service).with_context(|| format!("have init start {service} again"))?;
    }
    let binder = skyward_binder::driver::Binder::open()?;
    let until = Instant::now() + REGISTER_TIMEOUT;
    for (name, service) in registered {
        while skyward_binder::service::find(&binder, name)?.is_none() {
            ensure!(Instant::now() < until, "{name} has not registered again since init started {service} again");
            std::thread::sleep(Duration::from_millis(50));
        }
        skyward_sys::kmsg(&format!("{name} registered again from {service}"))?;
    }
    Ok(())
}

/// A description of what the running system offers, rather than of what
/// the phone's own partitions carry. A recovery publishes a description
/// into the place both readers take as the framework's, and the newer of
/// the two readers drops the whole set over one description that says it
/// is the phone's.
fn as_framework(text: &str) -> String {
    text.replace("type=\"device\"", "type=\"framework\"").replace("type='device'", "type='framework'")
}

/// Say the same of the descriptions already there, which a recovery ships
/// with, and give how many had to be rewritten.
fn say_framework(dir: &Path) -> Result<usize> {
    let mut rewritten = 0;
    for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let path = entry.with_context(|| format!("read {}", dir.display()))?.path();
        if path.extension().is_none_or(|e| e != "xml") {
            continue;
        }
        let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let said = as_framework(&text);
        if said != text {
            fs::write(&path, said).with_context(|| format!("rewrite {}", path.display()))?;
            rewritten += 1;
        }
    }
    Ok(rewritten)
}

/// Whether a description is of the service registered under `name`, which
/// reads as the interface and then the instance: the description names the
/// two separately.
fn describes(text: &str, name: &str) -> bool {
    let Some((interface, instance)) = name.rsplit_once('/') else { return false };
    let Some((package, interface)) = interface.rsplit_once('.') else { return false };
    text.contains(package) && text.contains(&format!("{interface}/{instance}"))
}

/// Wait until the service directory is a new process and takes calls
/// again. init is only asked to restart it, so for a moment the one that
/// has not read the new descriptions is still the one answering.
fn answering_again(was: Option<u32>) -> Result<()> {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let now = crate::services::running(DIRECTORY_PROGRAM);
        // Looking the directory up in itself answers nothing, and that it
        // answers at all is the whole question.
        if now.is_some() && now != was && crate::services::reachable() {
            return Ok(());
        }
        if std::time::Instant::now() >= until {
            anyhow::bail!("the service directory did not come back");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
