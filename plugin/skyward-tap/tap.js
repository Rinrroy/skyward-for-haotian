(() => {
  const zoom = location.hash.match(/skywardzoom=([0-9.]+)/);
  if (zoom) {
    document.documentElement.style.zoom = String(Math.max(0.5, Math.min(3, Number(zoom[1]))));
  }
  const tm = location.hash.match(/skywardtext=(\d+),(\d+),([^&]+)/);
  if (tm) {
    const x = Math.max(0, Math.min(innerWidth - 1, innerWidth * Number(tm[1]) / 1000));
    const y = Math.max(0, Math.min(innerHeight - 1, innerHeight * Number(tm[2]) / 1000));
    const text = decodeURIComponent(tm[3]);
    let el = document.activeElement;
    if (!el || !("value" in el)) el = document.elementFromPoint(x, y);
    if (el && "value" in el) {
      try {
        el.focus();
        el.value = text;
        el.dispatchEvent(new Event("input", { bubbles: true }));
        el.dispatchEvent(new Event("change", { bubbles: true }));
      } catch (_) {}
    }
  }
  const m = location.hash.match(/skywardtap=(\d+),(\d+)/);
  if (!m) return;
  const x = Math.max(0, Math.min(innerWidth - 1, innerWidth * Number(m[1]) / 1000));
  const y = Math.max(0, Math.min(innerHeight - 1, innerHeight * Number(m[2]) / 1000));
  const fire = () => {
    const el = document.elementFromPoint(x, y);
    if (!el) return;
    const base = { bubbles: true, cancelable: true, clientX: x, clientY: y, view: window };
    for (const type of ["pointerdown", "touchstart", "mousedown", "pointerup", "touchend", "mouseup", "click"]) {
      try {
        if (type.startsWith("pointer")) {
          el.dispatchEvent(new PointerEvent(type, { ...base, pointerId: 1, pointerType: "touch", isPrimary: true }));
        } else if (type.startsWith("touch")) {
          const touch = new Touch({ identifier: 1, target: el, clientX: x, clientY: y });
          el.dispatchEvent(new TouchEvent(type, { ...base, touches: type === "touchend" ? [] : [touch], targetTouches: type === "touchend" ? [] : [touch], changedTouches: [touch] }));
        } else {
          el.dispatchEvent(new MouseEvent(type, base));
        }
      } catch (_) {}
    }
    try {
      if (typeof el.click === "function") el.click();
    } catch (_) {}
  };
  fire();
  setTimeout(fire, 60);
})();
