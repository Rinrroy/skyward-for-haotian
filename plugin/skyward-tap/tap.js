(() => {
  const m = location.hash.match(/skywardtap=(\d+),(\d+)/);
  if (!m) return;
  const x = Math.max(0, Math.min(innerWidth - 1, innerWidth * Number(m[1]) / 1000));
  const y = Math.max(0, Math.min(innerHeight - 1, innerHeight * Number(m[2]) / 1000));
  setTimeout(() => {
    const el = document.elementFromPoint(x, y);
    if (!el) return;
    const base = { bubbles: true, cancelable: true, clientX: x, clientY: y, view: window };
    for (const type of ["pointerdown", "mousedown", "pointerup", "mouseup", "click"]) {
      try {
        const event = type.startsWith("pointer")
          ? new PointerEvent(type, { ...base, pointerId: 1, pointerType: "touch", isPrimary: true })
          : new MouseEvent(type, base);
        el.dispatchEvent(event);
      } catch (_) {}
    }
    try {
      if (typeof el.click === "function") el.click();
    } catch (_) {}
  }, 800);
})();
