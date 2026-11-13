#!/bin/sh
# Skyward full-mode plugin. The recovery binds /dev, /proc and /sys into
# this root before running it; the fallbacks are for a bare chroot.
set -u
mkdir -p /tmp /dev /dev/shm
[ -e /proc/self ] || /bin/busybox mount -t proc proc /proc 2>/dev/null || true
[ -e /sys/class ] || /bin/busybox mount -t sysfs sysfs /sys 2>/dev/null || true
[ -e /dev/null ] || /bin/busybox mknod -m 666 /dev/null c 1 3
[ -e /dev/urandom ] || /bin/busybox mknod -m 666 /dev/urandom c 1 9
[ -e /dev/random ] || /bin/busybox mknod -m 666 /dev/random c 1 8
printf 'nameserver 223.5.5.5\nnameserver 1.1.1.1\n' > /etc/resolv.conf 2>/dev/null || true
export HOME=/root
export PATH=/bin:/usr/bin:/usr/sbin
export LD_LIBRARY_PATH=/usr/lib/chromium:/usr/lib/aarch64-linux-gnu
: > /tmp/chromium.log
for flags in \
  "--headless=new --no-sandbox --disable-gpu --disable-dev-shm-usage --disable-extensions-except=/opt/skyward-tap --load-extension=/opt/skyward-tap --virtual-time-budget=6000" \
  "--headless=new --no-sandbox --disable-gpu --disable-dev-shm-usage --single-process --no-zygote --virtual-time-budget=6000" \
  "--headless=old --no-sandbox --disable-gpu --disable-dev-shm-usage --virtual-time-budget=6000" \
  "--headless=new --no-sandbox --disable-gpu --disable-dev-shm-usage --disable-features=VizDisplayCompositor --virtual-time-budget=6000"
do
  echo "try: $flags" >> /tmp/chromium.log
  /usr/lib/chromium/chromium $flags \
    --disable-crash-reporter --disable-breakpad \
    --hide-scrollbars --window-size=1440,3200 \
    --screenshot=/tmp/skyward-chromium.png "$1" >> /tmp/chromium.log 2>&1 || true
  if [ -s /tmp/skyward-chromium.png ]; then
    exit 0
  fi
done
exit 1
