# 文件搜索与 Magisk 模块

## 文件管理器搜索

文件管理底栏有「搜索文件」：

- 只搜索**当前文件夹**，不递归子文件夹。
- 只匹配文件和文件夹的**名称**，不搜索内容。
- 中文名称支持拼音全拼和拼音首字母：`微信` 可以用 `weixin` 或 `wx` 找到。
- 英文和数字按原名匹配，大小写不敏感。

搜索结果里点文件夹进入，点文本文件打开编辑器，点其他文件打开文件页。

## Magisk / KernelSU / APatch

「高级」→「Magisk 模块」读取：

- `/data/adb/modules`
- `/data/adb/ksu/modules`
- `/data/adb/ap/modules`

会识别 root 方案（`/data/adb/magisk/magisk`、`/data/adb/ksud`、`/data/adb/apd`），
列出模块的 `module.prop` 名称、版本、作者和说明。

- 点模块行切换 `disable` 文件，停用/启用。
- 点「移除」写入 `remove` 标记，由 Magisk 在重启后删除。
- 「模块仓库」会直接打开基本浏览器并加载 GitHub 主题；返回键回到 Magisk 管理器。
- 使用官方 Magisk 时显示已安装版本，可点「检查 GitHub Releases」查看最新版本；
  有新版本时显示「下载并安装更新」，从官方 Releases 下载 APK 并交给安装器。
- 「从本地 zip 安装」会打开文件管理器选择 zip，不再要求固定放到某个地址。
- 需要先解密 data；未解密时页面会直接显示「需先解密 data 分区」，而不是「未安装」。
- 没有安装 root 方案时，页面提供 Magisk、KernelSU、APatch 官方 Releases 链接，
  可在内置浏览器下载安装包；安装包下载后可在「安装」中选择（Magisk APK 通常
  需要按官方说明处理/改名）。
- 「模块仓库」打开 GitHub 的 `magisk-module` 主题，可浏览并下载模块 zip。
- 「从本地 zip 安装」读取 `/sdcard/Download/Skyward/module.zip`，解压到
  `/data/adb/modules/<id>`，重启后由 Magisk 挂载。它**不会执行模块自己的
  `customize.sh`/安装脚本**；只适合 overlay 型模块，带脚本的模块请用 Magisk
  官方安装器。

## 限制

- 模块仓库目前是 GitHub 主题浏览 + 手动下载安装，还没有做版本比较和自动更新。
- root 方案安装包只提供官方下载入口，不在 recovery 内自动刷 boot。
- 模块安装不执行安装脚本，复杂模块可能不完整。
