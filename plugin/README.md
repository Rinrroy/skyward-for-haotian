# 完整模式插件包

插件包可以是 `chromium.zip` 或 `chromium.tar.gz`，根目录是 Linux arm64 rootfs 的内容，外加 `run.sh`：

```
chromium.tar.gz
  run.sh
  bin/
  lib/
  usr/
  etc/
  tmp/
```

`run.sh` 在 chroot 里执行，参数是当前地址；它需要把截图写到
`/tmp/skyward-chromium.png`。

示例 `run.sh`：

```sh
#!/bin/sh
mkdir -p /tmp /dev/shm
mount -t proc proc /proc 2>/dev/null || true
mount -t sysfs sysfs /sys 2>/dev/null || true
mount -t tmpfs tmpfs /dev/shm 2>/dev/null || true
[ -e /dev/null ] || mknod -m 666 /dev/null c 1 3
[ -e /dev/urandom ] || mknod -m 666 /dev/urandom c 1 9
exec /usr/bin/chromium \
  --headless=new --no-sandbox --disable-gpu --disable-dev-shm-usage \
  --hide-scrollbars --window-size=1440,3200 \
  --screenshot=/tmp/skyward-chromium.png "$1"
```

rootfs 需要 glibc、Chromium 的共享库、字体和证书。常见大小 300–600 MB。
把 `chromium.zip`（或 `.tar.gz`）放到手机的 `/sdcard/Skyward/plugins/`，
然后在 Skyward 浏览器 →「完整模式」里安装。

注意：当前 Skyward 只读取截图，不把触摸事件转发给 Chromium；交互式 CDP
桥是下一步。安装前请校验 tar.gz 的 SHA-256，只使用可信来源。
