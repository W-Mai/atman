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

(function () {
  var platform = (navigator.platform || "").toLowerCase();
  var isMac = platform.indexOf("mac") >= 0;
  var isWin = platform.indexOf("win") >= 0;

  var row1 = document.getElementById("atman-install-row-1");
  var row2 = document.getElementById("atman-install-row-2");
  var cmd1 = document.getElementById("atman-install-cmd-1");
  var cmd2 = document.getElementById("atman-install-cmd-2");

  if (!row1 || !cmd1) return;

  if (isMac) {
    cmd1.textContent = "brew install W-Mai/cellar/atman-cli";
    if (cmd2) cmd2.textContent = "curl -fsSL https://atman.run/install.sh | sh";
    if (row2) row2.style.display = "flex";
  } else if (isWin) {
    cmd1.textContent = "cargo install atman-cli --locked";
  } else {
    cmd1.textContent = "curl -fsSL https://atman.run/install.sh | sh";
  }

  var pairs = [
    ["atman-copy-1", "atman-install-cmd-1"],
    ["atman-copy-2", "atman-install-cmd-2"],
  ];
  pairs.forEach(function (pair) {
    var btn = document.getElementById(pair[0]);
    var target = document.getElementById(pair[1]);
    if (!btn || !target) return;
    btn.addEventListener("click", function () {
      navigator.clipboard.writeText(target.textContent).then(function () {
        btn.classList.add("copied");
        setTimeout(function () { btn.classList.remove("copied"); }, 1500);
      });
    });
  });
})();
