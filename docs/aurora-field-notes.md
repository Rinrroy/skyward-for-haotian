# 小米 14 Ultra（aurora）实测记录

这份不是移植教程（那是 `decryption-porting.md`），而是**在这台机器上实际踩过、并且推翻过预期的事**。每一条都注明了依据，接手的人可以照着复核。

机型：小米 14 Ultra，SM8650，HyperOS 3（Android 16），A/B 双槽，`recovery_a` / `recovery_b` 独立分区。

---

## 1. 关掉 AVB 之后，官方系统起不来（实测，与直觉相反）

**现象**：把 `vbmeta_b` 与 `vbmeta_system_b` 的 flags 置 `0x3`（关 dm-verity + 关校验），重启后系统启动失败，`init` 的 `fs_mgr_mount_all` 挂不上分区，随后把一条「进 recovery 并提示擦除数据」的指令写进 `misc`，设备停在 recovery。

**恢复**：把 flags 清回 `0x0`，清掉 `misc` 里的指令，重启即恢复。数据全程未动。

**为什么值得记**：直觉是「关掉校验只会更宽松」，但 `veritymode=disabled` 时 first-stage init 的挂载路径本身会失败。所以：

| flags | 系统 | 结果 |
|---|---|---|
| `0x0` | 官方 HyperOS | 正常启动 |
| `0x3` | 官方 HyperOS | **起不来**（实测） |
| `0x3` | 配套第三方系统 | 预期能起（未实测，TWRP 的前提即此） |
| `0x0` | 第三方系统 | 大概率被 verity 拦下 |

这也是 TWRP 那个按钮写着 "Disable AVB2.0 **after flashing ROM**" 的原因：它的适用场景就是「已经换成第三方了」。

**顺序不能反**：想回官方，先刷官方包，再打开校验。反了不会变砖（recovery 不受这个开关影响），但会多绕一轮。

---

## 2. `fastboot reboot recovery` 在这台机器上无效

命令返回 `OKAY`，设备却回到 fastboot 界面。同理 `fastboot oem reboot-recovery` 也不支持。

正确做法：

```bash
adb reboot recovery     # 系统内
# 或 音量上 + 电源
```

**误判风险**：命令返回 OKAY 会让人以为镜像坏了。有一次因此误判过一次镜像问题。

---

## 3. 换了 ramdisk 必须重算 vbmeta 里的 digest，否则 bootloader 拒绝启动

`recovery` 分区自身的结构：

```
0            boot v4 头
4096         ramdisk（LZ4 legacy）
...          补零到 4096 边界
vbmeta       512 字节，AVB0，algo=1（已签名）
末尾 64 字节  AVBf footer，指向 vbmeta
```

vbmeta 的 hash 描述符里存着 **`sha256(salt + image[0:image_size])`**，其中 `image_size` 等于 vbmeta 的偏移。

改过 ramdisk 却不更新这个 digest，结构完好、bootloader 照样拒绝——表现为**重启直接弹回 fastboot**，没有任何其他提示。两次踩到：

1. 把 ramdisk 之后的内容整段零填充，把 vbmeta 和 footer 一起抹掉了；
2. 保留了 AVB 结构但没重算 digest。

**正确顺序**：只替换 ramdisk 区段（并补零到 vbmeta 起点之前）→ 重算 digest → 写回 `vbmeta + 428` → 回读校验。

签名是 `algo=1`，改 flags 会破坏签名；但设备已解锁（`verifiedbootstate=orange`）时不校验签名，所以改完能启动，只是 `veritymode` 会变。

---

## 4. `misc` 里的一条便条决定「为什么开机总进 recovery」

bootloader 每次启动读 `misc` 前 2048 字节（bootloader message）：`command[32]`、`status[32]`、`recovery[768]`、`stage[32]`。

系统挂载失败时，`init` 会写一条 `boot-recovery` + `--prompt_and_wipe_data --reason=fs_mgr_mount_all`，于是**之后每次开机都进 recovery**，屏幕上还提示擦除数据——很容易被误认为「数据坏了」。

**看它**：

```bash
/system/bin/recovery bcb          # command / args / 解析出的请求
```

**清它**（只清指令区，不碰数据）：

```bash
/system/bin/recovery bcb --clear  # 或界面：高级 → 启动指令 → 滑动清除
```

注意：这台机器上没有叫 `skyward` 的命令，程序本体就是 `/system/bin/recovery`。而且**带参数调用才算命令**——不带参数时它直接进 recovery 界面（早期版本不看参数就进界面，会启动第二个实例抢走屏幕和 USB）。

`--prompt_and_wipe_data` 是**请求**，不是已经发生的事。清掉它不会丢数据。

---

## 5. 解密：A 槽的 fstab 可能没有加密字段

从 A 槽启动的 recovery 读 A 槽 vendor 的 `fstab.qcom`。实测这台机器的 A 槽那份**没有** `metadata_encryption=`，而系统跑在 B 槽、data 实际是加密的（`ro.crypto.state=encrypted`，`/dev/block/dm-54 on /data`）。

原版 `Encryption::of()` 强制要求这个字段，于是直接放弃，报：

```
the data partition stays closed: the phone says its data partition is not encrypted whole
```

修复见 `crates/skyward-crypto/src/data.rs`：字段缺失时取 Android 的默认值（`aes-256-xts` + `wrappedkey_v0`）与 vold 的默认 key 目录；裸设备能读出文件系统时按明文直接挂载；映射后读不出文件系统则明确报「密钥不对」。

**验证过**：官方系统与官改包两种环境，`/data` 都能解开、`/sdcard` 可读。

---

## 6. 备份：哪些分区该备，哪些不该

**要备**（丢了会残废）：

| 分区 | 大小 | 丢了会怎样 |
|---|---|---|
| `persist` | 32 MB | 指纹失效、传感器失灵、MAC 丢失 |
| `modemst1` / `modemst2` | 8 MB ×2 | 基带 NV 丢失，无信号 |
| `fsg` / `fsc` | 8 MB / 1 MB | modem 文件系统异常，同样无信号 |
| `boot` / `init_boot` / `vendor_boot` / `dtbo` / `vbmeta*` / `recovery` | — | 起不来，可由 fastboot 救 |
| `super` 内各分区 | 数 GB | 系统坏了，刷包可救 |
| `userdata` | — | 数据丢失 |

**不要备**：

- `keystore`：实测整分区全零，备了是一坨零；
- `devinfo`：解锁状态凭证，恢复旧副本会改变锁定状态；
- `secdata` / `storsec`：设备唯一绑定，恢复会破坏信任链；
- `frp`：恢复旧副本可能把机器锁成「要 Google 账号」；
- `blackbox` / `rawdump` / `oops`：日志转储，无恢复价值；
- bootloader 链（`xbl` / `abl` / `tz` / `hyp` / `uefi` …）：真砖时 recovery 也起不来，手机里的备份拿不到；权威副本在官方线刷包的 `rawprogram*.xml` 里。

**`metadata` 有意不备**：它存的是 vold 的密钥元数据，把旧副本恢复到已变化的系统上大概率让 data 永久解不开，比不恢复更糟。真正的保险是 data 的文件级备份。

---

## 7. 本地交叉编译的坑（Windows）

```bash
export PATH="<ndk>/toolchains/llvm/prebuilt/windows-x86_64/bin:<w64devkit>/bin:$PATH"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="<ndk>/.../aarch64-linux-android30-clang.cmd"
export CC_aarch64_linux_android="aarch64-linux-android30-clang.cmd"
export AR_aarch64_linux_android="llvm-ar.exe"
cargo build --release --target aarch64-linux-android -p skyward
```

踩过的：

1. **PATH 必须同时含 NDK 的 bin 和 w64devkit 的 bin**。只加 NDK 会覆盖 w64devkit，报 `dlltool.exe not found`（`dlltool` 来自 MinGW）。
2. `cargo check` 走宿主 target 会失败：`skyward-block` 用了 `FileExt::read_exact_at`，Windows 上没有；`libc::_SC_PAGESIZE` 也只认 android。所以**校验只能针对 android target 做**，`cargo test` 在本机跑不了。
3. w64devkit 缺 `libgcc_eh.a`，需要 `ar rcs` 造一个空归档补上链接项。
4. 仓库是浅克隆（`--depth 1`）时，`git bundle create --all` 产出的包**无法恢复**（缺父提交）。备份要么先 `fetch --unshallow`，要么直接压缩整个工作副本。

---

## 8. 起不来时的救援顺序

| 现象 | 处理 |
|---|---|
| 卡在 fastboot，系统起不来 | `fastboot --set-active=b` 然后 `fastboot continue` |
| 每次开机都往 recovery 跑 | `fastboot erase misc`，或进 recovery 清启动指令 |
| 进 recovery 弹回 fastboot | 重刷 `recovery_<slot>.img`（确认 vbmeta digest 已重算） |
| 要回官方 recovery | 刷官方包里的 `recovery.img`，或 `fastboot flash recovery_<slot>` |

`erase misc` + `reboot` 有时不够——实测有效的是 `set-active` 再 `continue`，它会重置槽位状态并让 bootloader 继续。

---

## 9. 日志在哪，以及为什么在那里

三处，按可靠性排序：

| 位置 | 何时写 | 重启后 |
|---|---|---|
| `/mnt/logfs/skyward/session-<开机秒数>.log` | **总能写** | **在** |
| `/tmp/recovery.log` | 总能写 | 没了（内存盘） |
| `/sdcard/Skyward/logs/` | 仅当 logfs 挂不上、且 data 已挂载 | 在 |

`logfs` 是 bootloader 写自己日志用的 8 MB FAT 分区（`/dev/block/by-name/logfs`），不加密、独立于系统。**data 锁着的时候它是唯一能持久写的地方**——而「会话结束时设备重启了」恰恰是 data 锁着的时候。只保留最近 20 个会话。

Skyward 只在里面加一个 `skyward/` 子目录，不动 `UefiLog0..4.txt`。

```bash
adb pull /mnt/logfs/skyward/          # 取全部
adb shell cat /mnt/logfs/skyward/session-*.log   # 或直接看
```

**一条踩过的坑**：`mount(2)` 的 `data` 参数是给文件系统驱动自己的，不是 `mount(8)` 读的选项。`"rw,noatime"` 交给 vfat 会得到 `EINVAL`——`rw` 靠不设 `MS_RDONLY` 表达，`noatime` 是 `MS_NOATIME` 标志位。照抄命令行写法会挂不上。

## 10. 安装队列与固件导出

**安装队列**：安装页有「安装队列」一节。点「添加安装包」从内部存储选一个包，它不进安装而是排队；可以继续加，队列按顺序列出并显示大小，点某一行移除；滑一下开始，第一个装完自动接下一个。

**失败即停**：任一个包没装成，队列停在那一个上，页面写明是哪个包停的，后面还在队列里。这样不会出现「后面的包被当成已装」。

**`recovery firmware`**：把 bootloader 链所在的 23 个分区（两个槽共 46 个）导出成文件。

```bash
/system/bin/recovery firmware --out /sdcard/Skyward/firmware
# 46 files, 230,162,432 bytes, 约 0.2 秒
```

只读不写。原因写在 `README.txt` 里：把这些写回去需要 fastboot 或 EDL 工具，而恢复模式写错是它自己无法撤销的；官方线刷包里有同样这些分区，还带校验和写入地址，真要救砖该用它。

实测：导出后 `xbl_a` / `abl_b` / `tz_a` / `uefi_a` 四个分区的 SHA-256 与块设备逐一相同。

## 11. 两个页面行为值得知道

- **「无法启动系统」页**（系统请求擦除数据时出现）：页面上的擦除是**请求**，滑动才会执行。先选「重试」或「进入恢复模式」，两者都不动数据。
- **「启动指令」页**（高级 → 启动指令）：显示 `misc` 里的便条及其解析结果。系统正常启动时这里是空的——一次性指令（如 `bootonce-bootloader`）在启动读完就被清掉了，只有持续性的请求（擦除、装更新、sideload）才会一直留着。

## 12. 一条命令失败，整台机器解不开 data（A14 HyperOS 的报告）

一位网友在 **Android 14 的 HyperOS** 上报告「不支持解密 data」。日志给出的是另一回事：

```
[15.163544] devices not brought up: /system/bin/sh -c mkdir -p /kmlibs && cp … && mount --bind … ended: exit status: 1
[36.308048] the data partition stays closed: android.hardware.security.keymint.IKeyMintDevice/default is not running
```

第一行是**原因**，第二行是**结果**。`services::start` 逐条启动服务，one-shot 用 `ensure!(status.success(), …)`，第一条失败就返回，于是排在后面的 `qseecomd`、`ssgtzd`、keymint、gatekeeper、boot control **一条都没启动**——日志里 `ran /system/bin/sh …` 出现 0 次就是证据。

而失败的那条命令，最后一段是给 **WLAN** 用的：

```
V=$(ls -d /system_dlkm/lib/modules/*/kernel/net/rfkill | head -1)
mount --bind $V/rfkill.ko /system_dlkm/flatten/lib/modules/rfkill.ko
```

它把运行内核里那份已签名的 `rfkill.ko` 盖到扁平副本上（`[wlan] modules` 里有 rfkill）。两个路径都随构建版本变化：源不在、或 `flatten` 里没有对应文件，`$V` 为空、`mount` 报 ENOENT、退出码 1——**WLAN 的准备步骤于是决定了 data 能不能解开**。

三处改动：

1. **rfkill 叠加拆成独立 one-shot**，并且自己判空：找不到源、找不到目标、`mount` 失败，都打印一句原因后 `exit 0`。它失败只失去 WLAN。
2. **one-shot 的输出进日志**。原来 `services.rs` 把三个标准流全接 `/dev/null`，`cp: cannot stat …` 这类话一个字都不留，只剩一个退出码——这是那次排查里最大的障碍。现在 stdout/stderr 收下来，非空行写进内核日志（最多 40 行）。
3. **版本号 0.1.4**，让日志能分清是哪一版。

**教训**：给 A 功能准备的前置步骤，不能坐在 B 功能的必经链上，尤其当 A 是锦上添花（WLAN）、B 是核心能力（解密）。`&&` 链把「可选」和「必需」焊在一起，而 `services::start` 又把「一条失败」升级成「全部不启动」。
