![Skyward Recovery](assets/hero.jpg)

# Skyward for aurora

English | [简体中文](README.zh-CN.md)

Skyward is a recovery written in Rust, from scratch. This fork is dedicated to
one phone: the **Xiaomi 14 Ultra** (codename `aurora`). For the other devices the
upstream project supports, see [kirklin/skyward](https://github.com/kirklin/skyward).

<p align="center">
  <img src="docs/aurora-home.webp" width="30%" alt="Skyward home on the Xiaomi 14 Ultra">
  <img src="docs/aurora-files.webp" width="30%" alt="File manager">
  <img src="docs/aurora-about.webp" width="30%" alt="About">
</p>

## Features

- Decrypt the data partition
- Install zips, system update packages and partition images, from internal storage, a USB drive or adb sideload
- Online system updates
- Backup and restore
- Wipe data and factory reset
- Mount partitions, repair and format filesystems
- File manager and terminal
- WLAN
- MTP file transfer, adb and fastbootd
- Reboot to system, recovery, fastboot, bootloader or EDL, and switch slots
- Light and dark themes; Simplified Chinese, Traditional Chinese and English

## Supported device

| Device | Codename | Tested system version |
| --- | --- | --- |
| Xiaomi 14 Ultra | `aurora` | OS3.0.308.0.WNACNXM (Android 16) |

Decryption of the data partition works from **`aurora-v34`** on. Earlier tags
exist on this fork from when the key service chain was still being worked out;
they boot, but they do not open `/data`. **`aurora-v37`** merges upstream
v0.1.2, with the panel brightness fix included, on top of the same decryption
recipe, and is verified on the phone.

The recovery image is built on the stock recovery of OS3.0.308.0.WNACNXM, the
build `devices/aurora.toml` names.

## Installing

Skyward needs an unlocked bootloader. Download the zip from this repository's
[Releases](https://github.com/guobaoji-112/skyward-for-aurora/releases), which are
published from tags named `aurora-v*`. Each file is named
`skyward-aurora-<version>-<stock build>`. Check it against `SHA256SUMS.txt`,
unzip it, and flash the image inside to the recovery partition of the slot the
phone runs from, with the phone in fastboot mode:

```sh
unzip skyward-aurora-v37-OS3.0.308.0.WNACNXM.zip
fastboot flash recovery skyward-aurora-v37-OS3.0.308.0.WNACNXM.img
fastboot reboot recovery
```

`recovery` here is the partition of the slot the phone starts from. A phone on
the `_a` slot has `recovery_a` and `recovery_b`, and writing the one it does not
start from changes nothing that boots: the flash is reported as done, and the
phone still runs what was there before. So say which slot it is, and name the
partition where the two differ:

```sh
fastboot getvar current-slot                    # prints _a or _b
fastboot flash recovery_a <image>               # the letter it printed
fastboot reboot recovery
```

To see that the phone runs what was just flashed, ask the phone rather than the
computer:

```sh
adb shell "/system/bin/recovery info --config /skyward/device.toml" | grep recovery
```

The number it prints is the build's own, and the same number is on the About
page. A phone that still answers with the old one was flashed in the wrong slot.

Afterwards a newer image, unzipped, installs from Skyward's own Install page. With
**Flash Skyward again** on, a system update or zip that writes over the recovery
partitions gets Skyward back in them.

Without a system in super, Skyward still starts, with its screen and keys, adb,
`adb sideload` and fastbootd, enough to install one. Touch, WLAN and decryption run
from the installed system and come back with it.

adb in Skyward is a root shell that needs no authorization on the phone: anyone
with the phone and a cable can reach its storage while it is in recovery.

## Building

Skyward builds with the stable Rust that `rust-toolchain.toml` names, with its
`aarch64-linux-android` target, the Android NDK r29 and
[cargo-ndk](https://github.com/bbqsrc/cargo-ndk). The recovery service is built for
the phone, and the packer for the computer:

```sh
cargo ndk -t arm64-v8a --platform 30 build --release -p skyward
cargo build --release -p skyward-pack
```

The packer takes the stock recovery partition out of the build's full update
package on Xiaomi's server, reading only the part of the package the partition is
in, and checks its SHA-256 against the device file:

```sh
target/release/skyward-pack stock --config devices/aurora.toml --out target/stock/aurora/recovery.img
```

Then it builds the image on top of it:

```sh
target/release/skyward-pack recovery \
  --stock target/stock/aurora/recovery.img \
  --binary target/aarch64-linux-android/release/skyward \
  --config devices/aurora.toml \
  --font resources/fonts/MiSansVF.ttf \
  --mono resources/fonts/KirkMono-Regular.ttf \
  --mono-license resources/fonts/KirkMono-OFL.txt \
  --out target/skyward-aurora.img
```

Tests and lints run on the computer; what runs only on the phone is linted for it:

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo ndk -t arm64-v8a --platform 30 clippy -p skyward -p skyward-install -- -D warnings
```

`skyward-preview` draws the pages on the computer, as PNG files, without a phone:
each page whole, as a long screenshot shows it, in the states it can be in, and the
screen at moments of touches and motion. `--lang` picks the language by a locale
such as `en` or `zh-TW`:

```sh
cargo run --release -p skyward-preview -- target/preview --lang en
```

## License

[GPL-3.0-or-later](LICENSE)
