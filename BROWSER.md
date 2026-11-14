# Skyward 浏览器（aurora v0.3.4）

基于 `guobaoji-122/skyward-for-aurora` 的 `aurora-v0.3.4` 源码加入的浏览器，
保留 v0.3.4 的 WLAN、解密、LocalSend 等改动。

## 功能

- **文字、标题、列表、引用、代码块、链接**：原有阅读器。
- **图片**：页面里的 `<img>` 按需抓取，最多 6 张、每张 512 KiB、总计 2 MiB；
  支持 PNG 和 JPEG，按屏幕宽度缩放后绘制。抓不到或格式不支持时显示替代文字。
- **基础 CSS**：`<style>` 中的标签、`.class`、`#id` 简单选择器，以及行内
  `style`；支持颜色、背景色、粗体、斜体、下划线、对齐和字号；不处理布局、
  浮动、Grid/Flex 和外部样式表。
- **表单**：`<form>`、文本/密码/隐藏输入、复选框、单选、提交按钮；GET 和
  POST 都支持，提交后重新读取结果页。
- **Cookie**：解析 `Set-Cookie`，按域名和路径回送；只保存在本次 recovery 运行中。
- **下载**：保存到 `/sdcard/Download/Skyward`，未解密时到 `/tmp/skyward-browser`；
  下载完成可直接安装 zip/img。
- **底栏图标**：后退、前进、刷新、主页、标签页、下载；不再用文字按钮。
- **标签页**：底栏的标签页按钮打开列表，可新建、切换、关闭当前标签。
- **搜索**：默认用 Bing 的 RSS 结果，标题是链接、摘要是段落，阅读页更整齐。
- **快捷访问**：首页的快捷入口可编辑，能删除、把当前页面加进去、恢复默认。
- **OTG**：文件管理器底栏有「浏览 OTG」，切换 USB host 后列出 U 盘中的文件。
- **文件搜索**：文件管理页面在路径下方是搜索框（不是底栏按钮），只搜索当前文件夹的名称，支持中文拼音全拼和首字母。
- **Magisk 模块**：高级 →「Magisk 模块」，可查看/启停/移除模块，打开模块仓库，或从本地 zip 解压安装。
- **完整模式插件入口**：浏览器首页 →「完整模式（Linux Chromium）」。

不运行 JavaScript，不处理 WebSocket、流媒体、Canvas 等。

## 完整模式插件

完整模式使用外置的 Linux Chromium，不进入 100 MiB recovery 镜像：

完整模式现在是**常驻 Chromium 会话**（CDP），不是每次点击重新启动截图：
- 点击、缩放、输入都在同一个页面完成；
- 点输入框会自动弹出恢复键盘，文字通过 CDP 写入焦点元素；
- 缩放按钮发送 Chromium 的真实 pinch gesture；
- 顶部只有返回基本模式按钮和常驻地址栏，没有「浏览器」大标题。

1. 把插件包下载到 `/sdcard/Skyward/plugins/chromium.zip`
   （或 `.tar.gz`，以及 `/sdcard/Download/Skyward/` 下的同名文件）。
2. 浏览器 →「完整模式」→「安装或更新」。Skyward 用 `unzip` 或 `tar -xzf` 解到
   `/data/media/0/Skyward/plugins/chromium`。
3. 先在内置浏览器打开一个地址并连接 WLAN；完整模式现在会先检查网络，未连接时不会启动 Chromium。
   - 启动前会把 recovery 的网关 DNS 写进插件，并用 `--disable-features=AsyncDns` 让 Chromium 走系统解析器，避免它自己连 8.8.8.8:53 被网络阻断；
   - 基本模式的 Bing RSS 地址会自动去掉 `format=rss`，完整模式 Chromium 加载普通 HTML 页面，不会把 RSS/XML 当成空白页；
   - 插件带有 CA 证书，HTTPS 页面不会因缺根证书而空白；
   - 页面加载上限 30 秒；外层用独立 session 的 60 秒硬超时，超时会显示失败，不会无限卡住，也不会连带结束 recovery。
4. 启动后不再停留在「完整模式」设置页，而是回到**普通浏览器界面**：
   - 顶部不显示「浏览器」大标题，只有左侧返回基本模式按钮和常驻地址栏；
   - 点页面在同一个完整模式界面里更新，不再重新打开一个页面；
   - 底部完整模式栏：后退、前进、刷新、键盘、缩小、放大、退出；
   - 点网页文本框后按「键盘」，用恢复键盘输入文字，再回车写入页面；
   - 基本模式底栏新增完整模式按钮，主页按它直接以完整模式打开 Bing。
5. 截图上的点按会以 6×12 的粗粒度网格转成点击，通过插件自带的
   `skyward-tap` 扩展注入 Chromium；这是实验性触控，复杂页面/滚动仍可能不准。
6. 插件根目录的 `run.sh` 被这样调用：

   ```sh
   chroot /data/media/0/Skyward/plugins/chromium /bin/sh /run.sh <url>
   ```

   `run.sh` 负责启动 Chromium，并把截图写到 rootfs 内的
   `/tmp/skyward-chromium.png`；Skyward 读取
   `/data/media/0/Skyward/plugins/chromium/tmp/skyward-chromium.png` 并显示。

当前完整模式是**一次性截图**，还没有把触摸/滚动/键盘通过 CDP 转发给 Chromium；
下一步才是交互式 CDP 桥。安装前请自行校验插件包的 SHA-256，并只使用可信来源。

## 日志

- 基本模式写到 `/skyward/run/browser.log`：打开的地址、表单提交、下载和错误。
- 完整模式写到插件的 `/data/media/0/Skyward/plugins/chromium/tmp/chromium.log`。
- 「高级」→「日志」里新增两项「浏览器日志（基本模式）」「浏览器日志（完整模式）」，
  勾选后导出为 `browser-basic.log` 和 `browser-full.log`。

## 构建

```sh
cargo build --release -p skyward-pack
# aarch64 Android 版：需要 NDK r29，脚本在仓库外的 work/build-v034.sh
work/build-v034.sh

skyward-pack put --partition-size 104857600 \
  --out skyward-v0.3.4-aurora-browser.img \
  skyward-aurora-v0.3.4-OS3.0.308.0.WNACNXM.img \
  system/bin/recovery \
  target/aarch64-linux-android/release/skyward
```

## 已知限制

- 图片抓取在页面加载时同步进行，页面会因此慢一点；大图只保留前 2 MiB。
- CSS 只支持简单选择器和少量属性。
- Cookie 不持久化，重启 recovery 后清空。
- 表单的 `<select>`/`<textarea>` 只做了基础字段，复杂表单可能不完整。
- 完整模式当前只显示 Chromium 截图，不能点击或滚动；插件包不随镜像分发。
