// Abhoth website: theme toggle, mobile nav, install tabs with OS detection, copy buttons,
// the terminal demo and scroll reveals. No tracking, no cookies; localStorage keeps the theme only.
(() => {
  const root = document.documentElement;
  const reduced = matchMedia("(prefers-reduced-motion: reduce)").matches;

  // Theme
  for (const b of document.querySelectorAll(".theme-toggle")) {
    const label = () => b.setAttribute("aria-label", root.dataset.theme === "dark" ? "Switch to light theme" : "Switch to dark theme");
    label();
    b.addEventListener("click", () => {
      root.dataset.theme = root.dataset.theme === "dark" ? "light" : "dark";
      try { localStorage.setItem("theme", root.dataset.theme); } catch {}
      label();
    });
  }

  // Mobile nav
  const toggle = document.querySelector(".nav-toggle");
  const nav = document.getElementById("site-nav");
  if (toggle && nav) {
    toggle.addEventListener("click", () => {
      const open = nav.classList.toggle("open");
      toggle.setAttribute("aria-expanded", String(open));
    });
    nav.addEventListener("click", (e) => {
      if (e.target.closest("a")) { nav.classList.remove("open"); toggle.setAttribute("aria-expanded", "false"); }
    });
    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && nav.classList.contains("open")) { nav.classList.remove("open"); toggle.setAttribute("aria-expanded", "false"); toggle.focus(); }
    });
  }

  // Tabs (WAI-ARIA tabs pattern, automatic activation)
  function detectOs() {
    const p = (navigator.userAgentData && navigator.userAgentData.platform) || navigator.platform || "";
    const ua = navigator.userAgent || "";
    if (/win/i.test(p) || /Windows/.test(ua)) return "windows";
    if (/android|iphone|ipad/i.test(ua)) return null;
    if (/mac/i.test(p) || /Mac OS X/.test(ua)) return "macos";
    if (/linux|x11/i.test(p) || /Linux/.test(ua)) return "linux";
    return null;
  }
  for (const list of document.querySelectorAll('[role="tablist"]')) {
    const tabs = [...list.querySelectorAll('[role="tab"]')];
    const select = (tab, focus) => {
      for (const t of tabs) {
        const on = t === tab;
        t.setAttribute("aria-selected", String(on));
        t.tabIndex = on ? 0 : -1;
        document.getElementById(t.getAttribute("aria-controls")).hidden = !on;
      }
      if (focus) tab.focus();
    };
    tabs.forEach((t, i) => {
      t.addEventListener("click", () => select(t, false));
      t.addEventListener("keydown", (e) => {
        let j = null;
        if (e.key === "ArrowRight") j = (i + 1) % tabs.length;
        else if (e.key === "ArrowLeft") j = (i - 1 + tabs.length) % tabs.length;
        else if (e.key === "Home") j = 0;
        else if (e.key === "End") j = tabs.length - 1;
        if (j !== null) { e.preventDefault(); select(tabs[j], true); }
      });
    });
    if (list.dataset.detectOs !== undefined) {
      const os = detectOs();
      const t = os && tabs.find((x) => x.dataset.os === os);
      if (t) {
        const badge = document.createElement("span");
        badge.className = "detected";
        badge.textContent = "your OS";
        t.append(badge);
        select(t, false);
      }
    }
  }

  // Copy buttons on code blocks
  if (navigator.clipboard) {
    for (const pre of document.querySelectorAll("pre[data-copy]")) {
      const b = document.createElement("button");
      b.type = "button";
      b.className = "copy";
      b.textContent = "Copy";
      b.setAttribute("aria-label", "Copy code to clipboard");
      b.addEventListener("click", async () => {
        const text = pre.querySelector("code").innerText.split("\n").filter((l) => !/^\s*#/.test(l)).join("\n").trim();
        try { await navigator.clipboard.writeText(text); b.textContent = "Copied"; } catch { b.textContent = "Press Ctrl+C"; }
        setTimeout(() => (b.textContent = "Copy"), 1600);
      });
      pre.append(b);
    }
  }

  // Terminal demo
  const term = document.querySelector(".term[data-demo]");
  if (term && !reduced) {
    const steps = [...term.querySelectorAll("[data-step]")];
    const typed = term.querySelector("[data-type]");
    const fullText = typed ? typed.textContent : "";
    const caret = term.querySelector(".caret");
    let timers = [];
    const later = (fn, ms) => timers.push(setTimeout(fn, ms));
    const run = () => {
      timers.forEach(clearTimeout); timers = [];
      term.classList.add("animate"); term.classList.remove("done");
      steps.forEach((s) => s.classList.remove("on"));
      if (typed) typed.textContent = "";
      if (caret) caret.hidden = false;
      let t = 500;
      steps.filter((s) => s.dataset.step === "0").forEach((s) => s.classList.add("on"));
      if (typed) {
        for (let i = 1; i <= fullText.length; i++) later(() => (typed.textContent = fullText.slice(0, i)), t + i * 38);
        t += fullText.length * 38 + 450;
      }
      later(() => { if (caret) caret.hidden = true; }, t);
      for (const s of steps) {
        if (s.dataset.step === "0") continue;
        t += Number(s.dataset.delay || 420);
        later(() => s.classList.add("on"), t);
      }
      later(() => term.classList.add("done"), t + 300);
    };
    const replay = term.querySelector(".term-replay");
    if (replay) replay.addEventListener("click", run);
    const io = new IntersectionObserver((entries) => {
      if (entries.some((e) => e.isIntersecting)) { io.disconnect(); run(); }
    }, { threshold: 0.3 });
    io.observe(term);
  } else if (term) {
    term.classList.add("done");
    const caret = term.querySelector(".caret");
    if (caret) caret.hidden = true;
  }

  // Reveal on scroll
  const reveals = document.querySelectorAll(".reveal-up, .reveal");
  if (reduced || !("IntersectionObserver" in window)) {
    reveals.forEach((el) => el.classList.add("in"));
  } else {
    const io = new IntersectionObserver((entries) => {
      for (const e of entries) if (e.isIntersecting) { e.target.classList.add("in"); io.unobserve(e.target); }
    }, { rootMargin: "0px 0px -8% 0px", threshold: 0.12 });
    reveals.forEach((el) => io.observe(el));
  }
})();
