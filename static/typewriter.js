// Typewriter effect for hero title
(function () {
  const lines = [
    "repeatable engineering flows.",
    "inspectable agent workflows.",
    "auditable code pipelines.",
  ];
  const el = document.querySelector(".atman-tw-text");
  if (!el) return;

  let lineIdx = 0;
  let charIdx = 0;
  let deleting = false;

  function tick() {
    const current = lines[lineIdx];
    if (!deleting) {
      charIdx++;
      el.textContent = current.slice(0, charIdx);
      if (charIdx >= current.length) {
        deleting = true;
        setTimeout(tick, 1800);
        return;
      }
      setTimeout(tick, 55);
    } else {
      charIdx--;
      el.textContent = current.slice(0, charIdx);
      if (charIdx <= 0) {
        deleting = false;
        lineIdx = (lineIdx + 1) % lines.length;
        setTimeout(tick, 300);
        return;
      }
      setTimeout(tick, 30);
    }
  }

  tick();
})();
