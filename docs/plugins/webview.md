# The browser, as a plugin

This is how the browser works now, and why it works this way.

## What changed, and why

In v0.3.4 the browser was compiled into the recovery. Two files made it up —
`crates/skyward/src/browser.rs`, which fetched and laid out a page on a worker
thread, and `crates/skyward-ui/src/browser.rs`, which drew the reader, the
address bar, the tabs and the form pages — and both were linked into
`system/bin/recovery`. Chromium was already a plugin; the reader was not.

That split was wrong in a way that took a release to see. The reader is several
thousand lines of page handling, and every one of them was a reason to rebuild
the recovery: a CSS fix, a broken site, a font, a download path. It also meant
the recovery had to know what a page *is*, which meant carrying an HTML parser,
an image decoder, a cookie jar and a form post into a program whose actual job
is to install system images.

So the browser is now **entirely outside the recovery**. The recovery knows
nothing about HTML, CSS, images, forms, cookies or Chromium. It knows one thing:
what it means to run a program that wants to paint the panel.

The version this was taken from — the source of the image that made the case —
is `Jamvii/skyward-for-aurora`, branch `browser-v034`. That is the browser as
it was written, and the reader, the worker and the full-mode session in the
plugin are its code, moved, not rewritten.

## The split

```
  skyward (recovery, bionic)                    the plugin (aarch64, bionic)
  ──────────────────────────                    ──────────────────────────
  plugin folder + plugin.json                   its own interface: address bar,
  payload image: download, digest, keep          tabs, downloads, forms, history
  mount the image read-only                      its own HTML, CSS, images
  run the program, keep it in a process group    its own cookies, its own cache
  hand it a frame buffer and an input pipe       Chromium, when the user asks for
  put its frames on the panel                    full mode
  forward touch, keys, typed text
  stop it, unmount, clean up
```

Neither side can see into the other. The recovery writes a dozen environment
variables and two files; the plugin writes pixels and a forty-byte header. That
is the whole contract, and it is written down in
[`skyward-plugins/webview/PROTOCOL.md`](https://github.com/Rinrroy/skyward-plugins)
because two repositories implement it.

### What the recovery keeps

| job | where |
|---|---|
| read a plugin folder, hold its manifest to a shape it can trust | `crates/skyward-ui/src/plugin.rs` |
| fetch the index, download the payload, check the digest, resume a stopped download | `crates/skyward/src/pluginjob.rs` |
| loop-mount the image read-only, make the run directory, exec the program | `crates/skyward/src/pluginhost.rs` |
| copy the frames that changed onto the panel, hand touch and keys to the pipe | same file |
| stop it, unmount it, remove the run directory | same file |
| the page that lists them and says what a running plugin is doing | `crates/skyward-ui/src/plugin.rs` |

Everything else in this document is about the other side.

## The frame

The plugin does not open `/dev/dri`. It never asks who owns the display,
because the answer is *the recovery does*, and that is not negotiable: the
recovery owns the panel, the touch screen, the back gesture and the way out of
whatever is on screen. Two processes cannot both be DRM master, and a plugin
that could take the panel could also fail to give it back.

So the plugin draws into **a file**, and the recovery puts that file on the
panel. The file is created by the recovery, `ftruncate`d to
`4096 + height * width * 4`, and both sides `mmap` it `MAP_SHARED`. The first
4096 bytes are a header:

| offset | size | field |
|---|---|---|
| 0 | 4 | magic `SWFR` |
| 4 | 4 | version = 1 |
| 8 | 4 | width |
| 12 | 4 | height |
| 16 | 4 | stride = width × 4 |
| 20 | 4 | `seq` — the plugin bumps it once per finished frame |
| 24 | 4 | `flags` — bit 0 ready, bit 1 quit, bit 2 wants the keyboard |
| 28 | 4 | `dirty_count`, 0…16 |
| 32 | 256 | sixteen `{x, y, w, h}` rectangles |
| 288 | 3808 | reserved |
| 4096 | w×h×4 | pixels, RGBA8888 straight alpha, row major |

The plugin writes pixels first, then the dirty rectangles, then `dirty_count`,
and only then `seq`, bumping it to odd while it is mid-frame and to even when
the frame is complete. The recovery presents a frame only when `seq` is even
and larger than the last one it saw, so a frame that is still being drawn is
never half-shown, and no lock is needed on either side.

The dirty rectangles are what makes this affordable. The panel is 1440×3200 —
18 MB a frame. Copying all of it thirty times a second would be 550 MB/s of
`memcpy` for a page that is standing still. A reader scrolls a line at a time
and blinks a cursor; that is a few hundred kilobytes, and the cost of a still
page is the cost of noticing that nothing changed.

What this arrangement gives up is the display's own refresh: the panel updates
at the rate a frame is copied and committed, not at 120 Hz. For a reader that
is invisible. For video it means the frames the plugin decodes are the frames
the panel shows, at the rate the plugin can get them there.

## The input pipe

Input goes the other way, one line per event, into a FIFO the recovery creates
and the plugin reads `O_RDONLY|O_NONBLOCK`:

```
down  <x> <y>              move  <x> <y>              up    <x> <y>
key   <Name>               stop
```

That is everything this build sends, because it is everything the recovery has:
four hardware keys and a finger. `key` carries device-independent names — `Back`
and `Power` are the two that do anything. `stop` is the recovery asking the
plugin to finish: it writes it, waits two seconds, and then kills the process
group, so a plugin that hangs on the way out cannot hang the recovery.

The pipe also knows `swipe x0 y0 x1 y1 ms`, `pinch cx cy scale`,
`text <escaped>`, `rotate 0|90|180|270`, `reload` and `start`. A plugin must
ignore them, and this build never writes them: the recovery has no soft keyboard
to type with, so **typing is the plugin's own work** — a plugin that wants text
draws its own keyboard and turns the `down`/`move`/`up` it receives into
keystrokes. Flings and pinches are the same story: the plugin sees raw touch and
decides for itself what a gesture was.

The recovery never blocks writing to the pipe. If the plugin is not reading, the
line is dropped and the drop is logged; a stuck plugin degrades the input, not
the recovery.

## The plugin program

One binary, `skyward-browser`, built from a new crate that lives in
`skyward-plugins` and is compiled by that repository's CI against a pinned
revision of this one. It links `skyward-ui` and `skyward-net` — the same crates
the recovery uses — which is why the reader looks the same and why a CSS fix
lands in the plugin without touching the recovery image.

It is an ordinary bionic program: no chroot, no second linker, no rootfs of its
own. The recovery mounts the payload image at
`/skyward/run/plugin/<name>/img` and executes
`/opt/skyward/bin/skyward-browser` inside it. The image therefore holds a
binary, two fonts and a `run.sh`, and nothing else — the fonts because the
reader must render the same face as the recovery does (`MiSansVF.ttf`,
`KirkMono-Regular.ttf`), and the script because it is useful to be able to run
the thing by hand.

What it does with a page is v0.3.4's behaviour, unchanged and moved:

- **a reader** — text, links, headings, lists, quotes, tables, preformatted
  blocks, and images fetched on demand: at most six per page, at most 512 KiB
  each, 2 MiB a page, PNG and JPEG, scaled to the panel width, with alt text
  where an image did not arrive.
- **basic CSS** — a `<style>` block's rules for tags, `.class` and `#id`, and
  inline `style`, covering colour, background, bold, italic, underline,
  alignment and size. No layout: no float, no grid, no flex, no external
  stylesheet. This is a reader, and it says so.
- **forms** — `text`, `password`, `hidden`, `checkbox`, `radio`, `submit`, sent
  `GET` or `POST` with the encoding the form asked for.
- **cookies** — `Set-Cookie` kept per domain and path and sent back on matching
  requests. They live as long as the recovery does; a captive portal is
  answerable, a login that spans a reboot is not.
- **downloads** — into `$SKYWARD_DATA` (`/data/media/0/Skyward`) when `/data` is
  open, into `$SKYWARD_HOME` before that.
- **tabs**, a home page, editable quick links, and Bing as the search engine.
- **no scripts.** No JavaScript, no WebSocket, no canvas, no video element. A
  page that needs any of them needs full mode, and the address bar says so.

### Full mode, inside the plugin

Full mode is Chromium, and it stays a chroot of a glibc userspace, because that
is the only way a prebuilt browser can run here at all — the payload's own
`/lib/ld-linux-aarch64.so.1` is what makes it loadable, and bionic's namespace
rules are never consulted. But it is now the *plugin's* business:

- the Chromium rootfs is installed as a separate payload, under
  `$SKYWARD_STORE/webview-chromium`, by the plugin's own page;
- the plugin mounts `/dev`, `/proc`, `/sys` and a `/dev/shm` inside it, writes
  `etc/resolv.conf` from what it was told (`$SKYWARD_DNS`) or from the gateway it
  can see in `/proc/net/route`, and runs `chroot … /bin/sh /run.sh <url> serve`;
- it talks to it over CDP — navigate, screenshot, focus test, touch, pinch,
  insert text, reload — exactly as `crates/skyward/src/cdp.rs` did;
- the screenshot becomes the frame. The recovery does not know Chromium exists.

The display decision from the previous design is *not* carried over, and it is
worth recording why, because it cost a release to learn:

- **Wayland is impossible on this kernel.** `CONFIG_VT` is not set, libseat
  marks a session active by opening `tty0`, so no session is ever active and
  weston and every wlroots compositor die before making a socket:
  `Could not open tty0 to update VT`, `no drm device found`.
- **Xorg's modesetting driver segfaults** in `RRChangeOutputProperty` while
  `xf86CrtcScreenInit` describes this card's four connectors, whatever the
  configuration.
- The old answer was Xvfb writing `fb/Xvfb_screen0` and the recovery copying it.
  The new answer is that the reader needs no compositor at all: it draws its own
  pixels, which is what `skyward-ui` has always done. Full mode keeps Xvfb,
  because Chromium wants a display, but the display is the plugin's own problem
  now and the copy it writes to is the frame — one `mmap` away instead of a
  screen-dump parser in the recovery.

## The plugin package

A plugin is a folder with a manifest and an image, as it always was. Protocol 1
is one field more:

```json
{
  "title": "浏览器",
  "version": "2.0",
  "author": "Rinrroy",
  "about": "在恢复里浏览网页、播放视频。默认用必应搜索。",
  "needs": ["data", "network"],
  "protocol": 1,
  "payload": {
    "url": "https://github.com/Rinrroy/skyward-plugins/releases/download/webview-2.0/webview-2.0-arm64.erofs",
    "sha256": "…64 hex…",
    "size": 20971520,
    "format": "erofs",
    "program": "/opt/skyward/bin/skyward-browser"
  }
}
```

- `protocol` names the frame protocol. A manifest without it is a plugin the
  recovery lists and does not run — which is what every plugin was before this
  branch, so nothing that used to work stops being listed.
- `program` is the path **inside** the image. It must be an aarch64 Android
  binary or a script with a `#!/system/bin/sh` line, because the recovery runs
  it with its own libc and does not chroot.
- `format` is one of `squashfs`, `erofs`, `ext4`, `img`. What we ship is erofs:
  the recovery already mounts the phone's own partitions as erofs, so the
  kernel in the ramdisk is certain to read it, where squashfs depends on
  `CONFIG_SQUASHFS` being on.
- Installed plugins live in `/data/skyward/plugins/<name>/`, which means the
  page can only see them after `/data` is decrypted — and that a plugin
  installed there survives a reboot.

The index is `https://github.com/Rinrroy/skyward-plugins/releases/latest/download/plugins.json`,
read the same way the recovery's own update is; the same `manifest()` on the
phone judges each entry, plus the rule that its `name` is one folder name.

### Putting one in by hand

Unchanged: a single image in `Skyward/plugins`, a folder holding one image and
no manifest, or a folder with a manifest and the image it names. A folder with
two images and no manifest is an error the page shows, not a guess.

## What is not known yet

Marked so it can be probed, not believed:

1. **Does `recovery` execute a binary from an erofs mount on the ramdisk?** The
   previous design needed `setenforce 0` because it executed from `/data`. The
   mount point is now `img/` under `/skyward/run`, which is the ramdisk's own
   tree, but the label of the mounted files comes from the image, and an image
   built without labels takes the mount's `context=` or the policy default. If
   the first run is denied, the fallback is the one already used: permissive for
   the duration of the plugin and the mode restored on the way back, said on the
   page.
2. **What a full-frame copy costs.** 18 MB at 30 Hz is theoretical; the reader
   wants dirty rows. The measurement that matters is a scroll of a long page on
   the phone, which is the first thing to try.
3. **How much RAM a reader plus full-mode Chromium leaves.** Chromium wants
   ~400 MB idle and Xvfb another slice; the phone has 16 GB but the recovery has
   no zram and no low-memory killer.
4. **Audio.** Still expected to be silent: `/dev/snd` exists, but no vendor HAL
   does, so there is no mixer setup for this codec.
5. **Hardware video decode.** Not reachable from Chromium's V4L2 path: Venus is
   a stateless decoder that wants `v4l2_codec2` in front of it. Software decode
   only, and the page says so.

## The staged plan

| stage | what it lands | how it is known to work |
|---|---|---|
| S1 | the host: mount, run directory, frame, input, timeouts, cleanup, `protocol: 1` in the manifest | unit tests around the frame file; `cargo ndk check` and clippy clean |
| S2 | `skyward-browser`: the reader, drawn into a frame file, with a test page | on the phone: a page loads and scrolls |
| S3 | the payload built in CI, published as a release asset, installable from the index | on the phone: install, run, uninstall |
| S4 | full mode inside the plugin; downloads; the download page; captive portals | on the phone |

S1 and S2 are why the protocol exists: with the frame and the pipe fixed, the
plugin can be built, rebuilt and shipped without touching the recovery image
once — which is the entire point of taking the browser out of it.
