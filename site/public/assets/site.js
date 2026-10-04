// xssh.io: copy buttons (the label swaps to the "done" text with a check for a moment) and the desktop replica.
(() => {
  const CHECK = '<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M5 12.5l4.5 4.5L19 7.5"/></svg>';

  async function copy(text) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch {
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.setAttribute("readonly", "");
      ta.style.position = "fixed";
      ta.style.opacity = "0";
      document.body.appendChild(ta);
      ta.select();
      const ok = document.execCommand("copy");
      ta.remove();
      return ok;
    }
  }

  // The desktop replica: its sidebar switches between the pages it shows.
  document.addEventListener("click", (e) => {
    const item = e.target.closest(".app-nav [data-view]");
    if (!item) return;
    const app = item.closest(".app");
    for (const b of app.querySelectorAll(".app-nav [data-view]")) {
      if (b === item) b.setAttribute("aria-current", "true");
      else b.removeAttribute("aria-current");
    }
    for (const p of app.querySelectorAll("[data-pane]")) p.hidden = p.dataset.pane !== item.dataset.view;
  });

  document.addEventListener("click", async (e) => {
    const btn = e.target.closest("[data-copy]");
    if (!btn || !(await copy(btn.dataset.copy))) return;
    const icon = btn.querySelector(".i");
    const label = btn.querySelector(".t");
    if (!btn.dataset.idle) btn.dataset.idle = JSON.stringify([icon?.innerHTML ?? "", label?.textContent ?? ""]);
    const [idleIcon, idleText] = JSON.parse(btn.dataset.idle);
    if (icon) icon.innerHTML = CHECK;
    if (label) label.textContent = btn.dataset.done || "Copied";
    btn.classList.add("done");
    const live = document.querySelector("[data-live]");
    if (live) {
      live.textContent = "";
      setTimeout(() => (live.textContent = live.dataset.msg), 50);
    }
    clearTimeout(btn._t);
    btn._t = setTimeout(() => {
      if (icon) icon.innerHTML = idleIcon;
      if (label) label.textContent = idleText;
      btn.classList.remove("done");
    }, 1600);
  });
})();
