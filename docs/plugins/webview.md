# The browser, as a plugin

This is the design for the built-in browser: a plugin that Skyward downloads
after `/data` is decrypted and the phone is online, reads on every recovery
start, and hands the phone to so that it can draw a page and play a video.

It is written down before the code because the two or three decisions here
decide what the code can be. Everything below marked **unverified** has not
been run on a phone yet, and is listed at the end so it can be probed rather
than believed.

## What is being built

- A **plugin**, in the sense the plugins page already uses: a folder with a
  `plugin.json` in it, put there by the user or by Skyward on the user's
  request, read at every start.
- The plugin carries a **payload**: one image file holding a whole Linux
  userspace, Chromium among it.
- Skyward's job is to **install** that image (download, check, keep), and to
  **hand over** the phone: mounts, network, display, input, and the way back.

## Why not Android's `WebView`

The obvious answer — instantiate `android.webkit.WebView` — is not available
here, and it is worth saying why rather than trying again later.

`WebView` is not a library. It is a facade over the Android framework: the
provider (Trichrome/WebView APK) is chosen by `WebViewUpdateService` inside
`system_server`, the renderer runs in a process forked from `webview_zygote`,
drawing goes through `SurfaceFlinger`, and all of it talks over binder to
services that only `system_server` and the installed system's `init` bring up.

The recovery ramdisk has none of that. It has `init`, `adbd`, a handful of
binaries and the libraries those need; it has no zygote, no `system_server`, no
`SurfaceFlinger`, and no property service loaded with the installed system's
properties. Starting that framework from the recovery means running the
installed system's `init` as a second init — which is a container runtime
(Waydroid-shaped: binderfs, private property area, its own mount and PID
namespaces, its own `SurfaceFlinger` competing for the display) and not a
feature of a recovery.

So the browser is **not** an Android app. It is a Linux program, and the
"WebView" the user sees is Chromium in kiosk mode.

## The architecture

```
  Skyward (recovery, bionic)
      │  install: download → sha256 → /data/skyward/plugins/webview/
      │
      └── hand over ──────────────────────────────────────────────┐
            mount the payload image read-only                    │
            bind /dev /proc /sys, tmpfs /tmp, /data for the cache │
            write /etc/resolv.conf from the DHCP answer           │
            write the search-engine policy                        │
            setenforce 0            (restored on the way back)    │
            release the DRM card                                  │
            chroot + exec  /usr/bin/chromium --kiosk …            │
            wait                                                  │
            take the DRM card back, restore SELinux, unmount      │
      ◄───────────────────────────────────────────────────────────┘
```

### Why a chroot of a glibc userspace works at all

Three restrictions are named in the request, and one design answers all three.

- **The linker.** Android's linker enforces namespaces: a binary may only load
  the libraries its namespace allows, and `LD_LIBRARY_PATH` is ignored for
  anything but the namespace's own paths. That is exactly why a prebuilt
  Chromium cannot simply be dropped next to the recovery's own libraries. Inside
  the payload there is a **second dynamic linker** — the payload's own
  `/lib/ld-linux-aarch64.so.1` — and the kernel picks it up from the binary's
  `PT_INTERP`. Bionic's namespaces never enter into it. This is the whole
  reason the payload is a full rootfs rather than a directory of `.so` files.
- **DNS.** Android resolves through `netd` over binder; there is no `netd` in
  the recovery and no resolver library a glibc program could use anyway. Inside
  the payload, glibc reads `/etc/resolv.conf`, so Skyward writes that file from
  the DNS servers the DHCP answer carried, and Chromium's own resolver reads the
  same file. Nothing needs `netd`.
- **SELinux.** The recovery runs as `recovery`, and `recovery` is not allowed to
  execute a file under `/data` as a new domain. There are two honest options:
  put the payload where the policy already allows execution (the recovery
  ramdisk's own `/system`, which is a small tmpfs and cannot hold a gigabyte),
  or run permissive for the duration of the browser and restore the mode on the
  way back. This design does the second, and says so on the page: the phone is
  permissive only while the browser is in front, and only because the user asked
  for the browser.

### Display and input

Three arrangements have been tried on this phone. Only the third works, and the
first two failed for reasons worth keeping written down, because each one looked
reasonable from the outside.

**First: a Wayland compositor.** Impossible here. The kernel is built with
`# CONFIG_VT is not set`; libseat marks a session active by opening `tty0`, so
no session ever becomes active and every libseat-based compositor — weston,
cage, anything wlroots-shaped — dies before it creates its socket. Weston's own
log says it: `Could not open tty0 to update VT`, `Could not open device:
Operation not permitted`, `no drm device found`, `fatal: failed to create
compositor backend`. Chromium then reports `Failed to connect to Wayland
display`, which is the symptom, not the fault.

**Second: Xorg with the modesetting driver.** It needs no seat — it opens
`/dev/dri` itself and `-sharevts` keeps it out of VT handling — and it got far
enough to prove that. Then it segfaulted during screen initialisation, and gdb
said exactly where:

```
#0 modesetting_drv.so
#1 RRChangeOutputProperty ()
#3 xf86RandR12Init ()
#4 xf86CrtcScreenInit ()
#6 AddScreen ()
x0 = 0x0        the null that is dereferenced
```

It died while describing the card's outputs to RandR. The card advertises four
connectors — the panel on `DSI-1`, plus `DP-1` and two vendor `Virtual-*` ones
for screen recording — and none of the obvious measures helped: ignoring three of
the four in `xorg.conf.d` (which keeps them out of the screen but not out of
RandR), `ShadowFB`, `AccelMethod none`, software cursor, `-nocursor`, glamor, and
`ZaphodHeads` on the panel each crashed at the same address. So the fault is in
the driver's own description of this card, not in the configuration.

**Third, what ships: the payload draws into its own memory, and Skyward puts
that memory on the panel.** The payload runs `Xvfb :0 -screen 0 1440x3200x24
-fbdir /data/skyward/browser/fb`, which never opens the card at all; Xvfb writes
its framebuffer to `fb/Xvfb_screen0` as a plain file. Skyward keeps DRM master,
reads that file thirty times a second, copies the rows that changed into its own
canvas (swizzling the server's byte order into the canvas's), and presents. Over
a page being read this costs almost nothing, because nothing changed.

Input then runs the other way round: Skyward owns the touch screen, so it writes
one line per event into a named pipe — `down x y`, `move x y`, `up x y`,
`key Name` — and a loop in the payload turns each line into an `xdotool`
command. The phone's back button is `Escape`; its power button ends the browser.

What this arrangement costs: the panel updates at the rate the copy and the
commit allow rather than at the display's own rate, and video will look like a
sequence of frames rather than a moving picture. What it buys is a browser that
starts at all on a phone whose display driver refuses every compositor that asks
it directly.

Typing still needs a keyboard on the glass: `xvkbd` is started inside the payload
(in its own Xvfb display), which is what makes a captive portal answerable.

Two other things the phone taught, both now in the payload's script: the binary
to run is `/usr/lib/chromium/chromium`, because `/usr/bin/chromium` is a shell
wrapper that fails under dash at its ninth line; and everything the payload
prints has to go to a file on the data partition, because the recovery's only
record otherwise is an exit code.

### Video and audio — what is honest

- **Video** is decoded by Chromium's own ffmpeg. H.264 at 1080p is comfortable on
  this class of CPU; VP9 and AV1 are heavier and will drop frames on large
  streams. Hardware decoding is **not** reachable this way: Qualcomm's Venus
  decoder is a *stateless* V4L2 device that needs a userspace decoder
  (`v4l2_codec2`, an Android component) in front of it, and Chromium's V4L2
  path does not speak that dialect. A later stage can add a shim, but the first
  version decodes in software and says so.
- **Audio** needs `/dev/snd` plus a mixer setup for this codec, which Android
  normally gets from its audio HAL. The first version will most likely be
  **silent**; that is a known gap, not an oversight.

### The default search engine

Chromium reads managed policy from `/etc/chromium/policies/managed/*.json`.
Skyward writes one file there naming Bing China:

```json
{
  "DefaultSearchProviderEnabled": true,
  "DefaultSearchProviderName": "Bing",
  "DefaultSearchProviderSearchURL": "https://cn.bing.com/search?q={searchTerms}",
  "DefaultSearchProviderSuggestURL": "https://cn.bing.com/osjson.aspx?query={searchTerms}",
  "HomepageLocation": "https://cn.bing.com",
  "HomepageIsNewTabPage": false
}
```

A captive portal is then just a page: the browser can open the portal address
Skyward learned from the network, and the user logs in the way they would on any
phone.

## The plugin format

A plugin is a folder. Today it is read and listed; from this release it may also
carry a payload, and the manifest says where to get it:

```json
{
  "title": "浏览器",
  "version": "1.0",
  "about": "在恢复里浏览网页",
  "payload": {
    "url": "https://…/webview-1.1-arm64.erofs",
    "sha256": "…64 hex…",
    "size": 717701120,
    "format": "erofs",
    "program": "/opt/skyward/start.sh"
  }
}
```

- `url` must be `https://`, `sha256` must be 64 hex characters, `size` must be
  more than zero, `format` is one of `squashfs`, `erofs`, `ext4`, `img`, and
  `program` is the absolute path inside the payload to run. Anything else is a
  manifest error the page shows, not a plugin it offers to install.
- What we ship is **erofs**, not squashfs, and for one reason: the recovery
  mounts the phone's own system partitions as erofs, so the kernel in the
  ramdisk is certain to read it. squashfs depends on `CONFIG_SQUASHFS`, which
  Android does not always turn on, and a payload that cannot be mounted is a
  browser that cannot start.
- Installed plugins live in **`/data/skyward/plugins/<name>/`** — `/data`
  because that is where a gigabyte belongs, and because it survives a reboot so
  the plugin is there at the next start. It also means the plugins page can only
  see them after `/data` is decrypted, which the page says.
- The image is kept as `payload.<format>` beside the manifest, written as
  `payload.<format>.part` while it downloads and renamed only after the digest
  matches, exactly like a system package.

### Putting one in by hand

Fetching it in the system and copying it in is the shortest way to get a plugin
on a phone, so it needs no manifest written by hand:

- A **single image** dropped into `Skyward/plugins` on internal storage —
  `webview-1.1-arm64.erofs` or any name — is a plugin. Its name is the file's,
  and it is started with `/opt/skyward/start.sh`, the program every payload of
  this project is built with.
- A **folder holding one image and no `plugin.json`** is a plugin too, named
  after the folder.
- With a `plugin.json` beside it, the image may keep the name it was downloaded
  with: the file named by the manifest is looked for first, then any one file
  beside it whose extension is the format it names.
- A folder with two images and no manifest is listed with an error rather than
  guessed at: which of the two runs is not something to decide for someone.
- Any other file is not a plugin and is not listed.

This is the same list the page draws either way; what a hand-copied plugin
cannot do is be deleted from the page under `/data` — it lives where it was put.
Deleting an installed one is asked for exactly like a downloaded system package:
the row says what goes, a footnote says what deleting costs, and a swipe is the
only way to say yes.

## Where the image comes from

The browser is not shipped in the recovery image: it is a plugin, fetched from
its own repository's release assets when the phone is asked to. The page reads
one small index and installs what it names.

- The index is
  `https://github.com/Rinrroy/skyward-plugins/releases/latest/download/plugins.json`.
  It is read the same way the recovery's own update is: the direct address
  first, and the mainland mirror only when GitHub does not answer.
- `crates/skyward-ui/src/plugin.rs`'s `index()` parses it through the same
  `manifest()` a folder on the phone goes through, so an entry is held to the
  same rules — an `https://` url, a 64-hex digest, a size in range, a format it
  can mount, an absolute program path — plus one more: the entry's `name` must
  be a single folder name, because it becomes the folder it is installed in.
- `crates/skyward/src/pluginjob.rs` does the work on its own thread: it reads
  the index, takes the first entry, downloads into
  `/data/skyward/plugins/<name>/payload.<format>` with the digest checked, and
  writes `plugin.json` **last** — so a download that was stopped half way is
  not a plugin the page lists.
- A stopped download keeps its `.part`, so asking again goes on from where it
  stopped rather than from nothing.
- Deleting an installed plugin removes its folder, image and manifest together.
  An image that was copied straight into a plugins folder has no folder of its
  own, and only that file is removed — taking its parent for a plugin folder
  would take every plugin in it.

## The staged plan

| stage | what it lands | how it is known to work |
|---|---|---|
| S1 | manifest `payload`, install layout, download + digest, listing after decryption | unit tests in CI; the downloader is the one already used for system packages |
| S2 | the handover: mounts, resolv.conf, udev, SELinux, DRM release, chroot, exec, way back — with a tiny test payload that draws and exits | on the phone: the test payload paints the panel and returns to the page |
| S3 | the real payload, built in CI on an arm64 runner (debootstrap + Chromium + Mesa + udev), published as a plugin asset | on the phone: a page loads |
| S4 | video, audio, captive portal, the Via-like chrome (tabs, address bar) | on the phone |

S2 is the one that decides the project: if the chroot, the DRM release and the
udev/libinput path do not work on this device, the payload has to be something
else, and it is better to know that with a 2 MB test payload than with a 700 MB
one.

## What is not known yet — to be probed on the phone

Answered so far, all from the phone with `adb`:

1. ~~Is `/data` mounted `noexec`?~~ No: `/data` and `/sdcard` are one `dm-8`
   f2fs mounted `rw,nosuid,nodev`, and the payload runs from a loop-mounted
   read-only image, so this never mattered.
2. ~~Does `/dev/dri/card0` come back cleanly?~~ Yes — the card is dropped before
   the payload starts and reopened after it exits, and the page redraws.
3. ~~Are `/dev/input/event*` readable?~~ Yes, `event0`…`event7` are there and
   readable as root.
5. ~~Does the kernel mount the image, and do loop devices work?~~ Yes: erofs
   mounts with `losetup -r`, and the image's own superblock checks out
   (`blocks × 4096` equals the file size exactly).
6. Is `/dev/snd` present and does a plain ALSA open work? Still open — and with
   no vendor HAL in the chroot, sound is expected to be silent.

Still open:

4. How much RAM is free while Skyward is up? (Chromium wants ~400 MB idle, and
   X plus Chromium together more.)
7. Does Xorg's `modesetting` driver take this card under `-sharevts`, and does
   Chromium draw a page through it? This is what payload 1.3 exists to answer.

## Risks

- **Size.** The payload is 700 MB–1.2 GB. Over a mainland connection, from
  GitHub, that is a long download; it is the reason the plugin index and the
  payload should be served from a mirror (the same one the recovery's own update
  uses), and the reason the download must be resumable — which it is.
- **The first frame.** Chromium's startup on a phone's GPU with Mesa's freedreno
  is the least proven part. SwiftShader (software GL) is the fallback, at a
  visibly lower frame rate.
- **Nothing here is tested on a phone yet.** S2 exists to change that cheaply.
