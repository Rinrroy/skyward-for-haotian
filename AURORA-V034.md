# 小米 14 Ultra（aurora）Skyward v0.3.4 + 浏览器

基于工作区的
`skyward-aurora-v0.3.4-OS3.0.308.0.WNACNXM.img`，替换
`system/bin/recovery` 为 `aurora-v0.3.4` 源码加浏览器后的二进制。

## 镜像

- `skyward-v0.3.4-aurora-browser.img`
- 104,857,600 字节
- ramdisk 约 49.1 MB（压缩后）
- SHA-256：`179c9d9ad94e8b4466ea1713ce57daf2c41a9f1af06ef9f802af4bb119af0096`

刷入：

```sh
fastboot flash recovery skyward-v0.3.4-aurora-browser.img
fastboot reboot recovery
```

## 包含

- v0.3.4 原有的 WLAN、解密、LocalSend、系统安装等改动。
- 浏览器：图片（PNG/JPEG）、基础 CSS、表单、Cookie、下载。
- 「完整模式」插件入口：从内部存储安装外置 Linux Chromium 插件。

## 首次验证

1. 启动后进「高级」→「浏览器」。
2. 连接 WLAN，打开 `https://example.com`，确认文字、图片、链接。
3. 打开一个带表单的页面，填写并提交。
4. 下载一个 zip，确认保存到 `/sdcard/Download/Skyward`。
5. 如果 WLAN/解密仍失败，发回 `/skyward/run/recovery.log` 和 `dmesg`。

## 完整模式

把 `chromium.zip` 放到 `/sdcard/Skyward/plugins/`，在「完整模式」里安装；
插件包的 `run.sh` 负责 Chromium 截图。当前只是截图查看，触摸/CDP 交互
是下一步。详见 `BROWSER.md` 和 `plugin/README.md`。
