#!/bin/sh
# Skyward full-mode plugin. The recovery binds /dev, /proc and /sys into
# this root before running it; the fallbacks are for a bare chroot.
mkdir -p /tmp /dev /dev/shm
[ -e /proc/self ] || /bin/busybox mount -t proc proc /proc 2>/dev/null || true
[ -e /sys/class ] || /bin/busybox mount -t sysfs sysfs /sys 2>/dev/null || true
[ -e /dev/null ] || /bin/busybox mknod -m 666 /dev/null c 1 3
[ -e /dev/urandom ] || /bin/busybox mknod -m 666 /dev/urandom c 1 9
[ -e /dev/random ] || /bin/busybox mknod -m 666 /dev/random c 1 8
[ -s /etc/resolv.conf ] || printf 'nameserver 223.5.5.5\nnameserver 1.1.1.1\n' > /etc/resolv.conf 2>/dev/null || true
export HOME=/root
export PATH=/bin:/usr/bin:/usr/sbin
export LD_LIBRARY_PATH=/usr/lib/chromium:/usr/lib/aarch64-linux-gnu
export SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
export SSL_CERT_DIR=/etc/ssl/certs
: > /tmp/chromium.log
if [ "${2:-}" = "serve" ]; then
  rm -rf /tmp/chromium-profile
  UA="Mozilla/5.0 (Linux; Android 14; 23127PN0CC) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/156.0.0.0 Mobile Safari/537.36"
  exec /usr/lib/chromium/chromium \
    --headless=new --no-sandbox --disable-gpu --disable-dev-shm-usage \
    --no-first-run --no-default-browser-check --disable-background-networking \
    --ignore-certificate-errors --disable-quic --disable-dns-prefetch \
    --disable-features=AsyncDns --hide-scrollbars \
    --user-agent="$UA" --window-size=412,915 --force-device-scale-factor=1 \
    --remote-debugging-address=127.0.0.1 --remote-debugging-port=9222 --remote-allow-origins=* \
    --user-data-dir=/tmp/chromium-profile \
    --disable-crash-reporter --disable-breakpad \
    "$1" >> /tmp/chromium.log 2>&1
fi
EXTRA=
case "$1" in
  *skywardtap*|*skywardzoom*) EXTRA="--virtual-time-budget=2500" ;;
esac
UA="Mozilla/5.0 (Linux; Android 14; 23127PN0CC) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/156.0.0.0 Mobile Safari/537.36"
for flags in \
  "--headless=new --no-sandbox --disable-gpu --disable-dev-shm-usage --no-first-run --no-default-browser-check --disable-background-networking --ignore-certificate-errors --disable-quic --disable-dns-prefetch --disable-features=AsyncDns --disable-extensions-except=/opt/skyward-tap --load-extension=/opt/skyward-tap --timeout=30000" \
  "--headless=new --no-sandbox --disable-gpu --disable-dev-shm-usage --no-first-run --disable-background-networking --ignore-certificate-errors --disable-quic --disable-dns-prefetch --disable-features=AsyncDns --single-process --no-zygote --timeout=30000" \
  "--headless=old --no-sandbox --disable-gpu --disable-dev-shm-usage --no-first-run --ignore-certificate-errors --disable-quic --disable-dns-prefetch --disable-features=AsyncDns --timeout=30000" \
  "--headless=new --no-sandbox --disable-gpu --disable-dev-shm-usage --no-first-run --ignore-certificate-errors --disable-quic --disable-dns-prefetch --disable-features=VizDisplayCompositor,AsyncDns --timeout=30000"
do
  : > /tmp/chromium.log
  echo "try: $flags" >> /tmp/chromium.log
  echo "resolv: $(cat /etc/resolv.conf 2>/dev/null)" >> /tmp/chromium.log
  echo "hosts: $(cat /etc/hosts 2>/dev/null)" >> /tmp/chromium.log
  rm -f /tmp/skyward-chromium.png
  /usr/lib/chromium/chromium $flags $EXTRA \
    --user-agent="$UA" --window-size=412,915 --force-device-scale-factor=2.6 \
    --disable-crash-reporter --disable-breakpad \
    --hide-scrollbars \
    --screenshot=/tmp/skyward-chromium.png "$1" >> /tmp/chromium.log 2>&1 || true
  if [ -s /tmp/skyward-chromium.png ] && ! grep -q "Page load timed out" /tmp/chromium.log 2>/dev/null; then
    exit 0
  fi
done
exit 1
