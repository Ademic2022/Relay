// @ts-check
// Relay app window. Talks to the Rust side (src-tauri/src/main.rs) through
// Tauri commands and events; the Rust side drives the `relay` engine.

/**
 * @typedef {{ event: string, [k: string]: any }} HostEvent
 * @typedef {{ target: string, last_used: number }} Recent
 * @typedef {{ key: string, value: string, default: boolean }} SettingRow
 * @typedef {{ state: "started" | "ended", target: string, ok?: boolean, message?: string }} ViewerEvent
 */

// withGlobalTauri exposes the API without a bundler.
const T = /** @type {any} */ (window).__TAURI__;
/** @type {(cmd: string, args?: object) => Promise<any>} */
const invoke = T.core.invoke;
/** @type {(name: string, cb: (e: { payload: any }) => void) => Promise<unknown>} */
const listen = T.event.listen;

/** @type {Record<string, string>} */
const S = /** @type {any} */ (window).RELAY_STRINGS;
const isMac = navigator.userAgent.includes("Mac");

/** Look up a string and fill `{name}` placeholders. */
function t(key, vars = {}) {
  let s = S[key] ?? key;
  for (const [k, v] of Object.entries(vars)) s = s.replaceAll(`{${k}}`, String(v));
  return s;
}

/** @template {HTMLElement} E @param {string} id @returns {E} */
function $(id) {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing #${id}`);
  return /** @type {E} */ (el);
}

/** Create an element with a class and text (text, never HTML). */
function el(tag, cls = "", text = "") {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text) e.textContent = text;
  return e;
}

/** An inline icon from the sprite in index.html. */
function icon(name) {
  const ns = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(ns, "svg");
  svg.setAttribute("class", "ic");
  const use = document.createElementNS(ns, "use");
  use.setAttribute("href", `#i-${name}`);
  svg.append(use);
  return svg;
}

function applyStrings() {
  document.querySelectorAll("[data-i18n]").forEach((e) => {
    const k = e.getAttribute("data-i18n");
    if (k && S[k]) e.textContent = S[k];
  });
  document.querySelectorAll("[data-i18n-ph]").forEach((e) => {
    const k = e.getAttribute("data-i18n-ph");
    if (k && S[k]) e.setAttribute("placeholder", S[k]);
  });
  document.querySelectorAll("[data-i18n-label]").forEach((e) => {
    const k = e.getAttribute("data-i18n-label");
    if (k && S[k]) {
      e.setAttribute("aria-label", S[k]);
      e.setAttribute("title", S[k]);
    }
  });
}

/** Group a 9-digit ID as "123 456 789". */
function groupId(id) {
  const d = String(id).replace(/\D/g, "");
  return d.length === 9 ? `${d.slice(0, 3)} ${d.slice(3, 6)} ${d.slice(6)}` : String(id);
}

function ago(unixSeconds) {
  const s = Math.max(0, Date.now() / 1000 - unixSeconds);
  if (s < 60) return t("time.now");
  if (s < 3600) return t("time.min", { n: Math.floor(s / 60) });
  if (s < 86400) return t("time.hour", { n: Math.floor(s / 3600) });
  return t("time.day", { n: Math.floor(s / 86400) });
}

/** @type {number | undefined} */
let toastTimer;
function toast(text) {
  const tEl = $("toast");
  tEl.textContent = text;
  tEl.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => (tEl.hidden = true), 1600);
}

function copyText(text) {
  navigator.clipboard
    ?.writeText(text.trim())
    .then(() => toast(t("copied")))
    .catch(() => {});
}

// ---------------------------------------------------------------- tabs

function setupTabs() {
  const tabs = /** @type {HTMLButtonElement[]} */ ([...document.querySelectorAll('[role="tab"]')]);
  /** @param {HTMLButtonElement} tab */
  const select = (tab) => {
    for (const x of tabs) {
      const on = x === tab;
      x.setAttribute("aria-selected", String(on));
      x.tabIndex = on ? 0 : -1;
      $(x.getAttribute("aria-controls") ?? "").hidden = !on;
    }
    tab.focus();
    if (tab.id === "tab-activity") loadAudit();
    if (tab.id === "tab-settings") loadSettings();
  };
  tabs.forEach((tab, i) => {
    tab.addEventListener("click", () => select(tab));
    tab.addEventListener("keydown", (e) => {
      const n = e.key === "ArrowDown" ? 1 : e.key === "ArrowUp" ? -1 : 0;
      if (n) {
        e.preventDefault();
        select(tabs[(i + n + tabs.length) % tabs.length]);
      }
    });
  });
}

// ---------------------------------------------------------------- host

const host = {
  /** @type {string | null} */ password: null,
  /** @type {string | null} */ id: null,
  /** @type {string | null} */ server: null,
  /** @type {string[]} */ direct: [],
  running: false,
  inSession: false,
  sessionStart: 0,
};

/** @param {"starting" | "ok" | "live" | "off"} kind */
function setStatus(kind, text) {
  const pill = $("status-pill");
  pill.className = `status-pill ${kind === "starting" ? "" : kind}`;
  $("status-text").textContent = text;
}

function notice(text, action) {
  const n = $("notice");
  $("notice-text").textContent = text;
  const a = $("notice-action");
  a.replaceChildren();
  if (action) {
    const b = el("button", "btn ghost", action.label);
    b.addEventListener("click", action.run);
    a.append(b);
  }
  n.hidden = !text;
}

function renderIdle() {
  $("creds").hidden = false;
  $("session").hidden = true;
  $("ready-badge").hidden = false;
  $("my-pw").textContent = host.password ?? "—";
  $("pw-block").hidden = host.password === null;

  const idEl = $("my-id");
  const others = [...host.direct];
  if (host.id) {
    $("id-label").textContent = t("allow.id");
    idEl.textContent = groupId(host.id);
    idEl.className = "display-num";
    $("id-hint").textContent = "";
    $("host-status").textContent = t("allow.ready");
    setStatus("ok", t("status.online"));
  } else {
    // No ID (no server yet): the first LAN address is the main identifier.
    $("id-label").textContent = t("allow.address");
    const first = others.shift() ?? "—";
    idEl.textContent = first;
    idEl.className = "display-num small mono";
    $("id-hint").textContent = host.server ? t("allow.serverWaiting") : t("allow.noServerHint");
    $("host-status").textContent = t("allow.readyDirect");
    setStatus("ok", t("status.lan"));
  }
  const list = $("direct-list");
  list.replaceChildren(
    ...others.map((a) => {
      const b = el("button", "chip mono", a);
      b.title = t("copy");
      b.addEventListener("click", () => copyText(a));
      return b;
    }),
  );
  $("direct-block").hidden = others.length === 0;
}

/** @type {number | undefined} */
let sessionTimer;

/** @param {HostEvent} ev */
function onHost(ev) {
  switch (ev.event) {
    case "ready":
      host.running = true;
      host.password = ev.password ?? null;
      host.server = ev.server ?? null;
      host.direct = ev.direct ?? [];
      notice("");
      renderIdle();
      break;
    case "id":
      host.id = ev.id;
      if (!host.inSession) renderIdle();
      break;
    case "password":
      host.password = ev.password;
      if (!host.inSession) renderIdle();
      break;
    case "consent":
      showConsent(ev.name, ev.timeout_s ?? 30);
      break;
    case "consent_done":
      hideConsent();
      break;
    case "session_start": {
      host.inSession = true;
      host.sessionStart = Date.now();
      $("creds").hidden = true;
      $("session").hidden = false;
      $("ready-badge").hidden = true;
      const name = String(ev.name ?? "");
      $("session-who").textContent = name;
      $("session-avatar").textContent = (name.trim()[0] ?? "?").toUpperCase();
      $("session-mode").textContent = t(ev.view_only ? "session.view" : "session.control");
      $("host-status").textContent = "";
      $("chat-log").replaceChildren();
      setStatus("live", t("status.session"));
      clearInterval(sessionTimer);
      const tick = () => {
        const s = Math.floor((Date.now() - host.sessionStart) / 1000);
        $("session-timer").textContent = `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
      };
      tick();
      sessionTimer = window.setInterval(tick, 1000);
      break;
    }
    case "session_end":
      host.inSession = false;
      clearInterval(sessionTimer);
      hideConsent();
      renderIdle();
      $("host-status").textContent = t("session.ended", { reason: ev.reason });
      break;
    case "chat":
      addChat(ev.from_me ? t("chat.you") : ev.name, ev.text, ev.from_me);
      break;
    case "failed_login":
      notice(t("session.failed", { name: ev.name }));
      break;
    case "busy":
      notice(t("session.busy", { name: ev.name }));
      break;
    case "error":
      if (ev.kind === "screen_recording") permissionNotice();
      break;
  }
}

function permissionNotice() {
  notice(t("perm.screen"), {
    label: t("perm.open"),
    run: () => invoke("open_privacy_settings", { pane: "screen" }),
  });
}

function addChat(who, text, mine) {
  const li = el("li", mine ? "mine" : "theirs");
  if (!mine) li.append(el("span", "from", who));
  li.append(document.createTextNode(text));
  $("chat-log").append(li);
  li.scrollIntoView({ block: "end" });
}

// ---------------------------------------------------------------- consent (FR-8)

/** @type {number | undefined} */
let consentTimer;

function showConsent(name, seconds) {
  const body = $("consent-body");
  const [pre, post] = t("consent.body", { name: "\u0000" }).split("\u0000");
  body.replaceChildren(document.createTextNode(pre ?? ""), el("strong", "", name), document.createTextNode(post ?? ""));
  const bar = $("consent-bar");
  const started = Date.now();
  const tick = () => {
    const left = Math.max(0, seconds - (Date.now() - started) / 1000);
    $("consent-timer").textContent = t("consent.timer", { s: Math.ceil(left) });
    bar.style.transform = `scaleX(${left / seconds})`;
    if (left <= 0) hideConsent();
  };
  tick();
  clearInterval(consentTimer);
  consentTimer = window.setInterval(tick, 200);
  $("consent").hidden = false;
  // Decline has focus: Enter must never accept by accident.
  $("consent-decline").focus();
}

function hideConsent() {
  clearInterval(consentTimer);
  $("consent").hidden = true;
}

// ---------------------------------------------------------------- connect

async function loadRecent() {
  /** @type {Recent[]} */
  const r = await invoke("recent");
  $("recent").replaceChildren(
    ...r.map((x) => {
      const isId = /^\d{9}$/.test(x.target);
      const li = el("li");
      const pick = el("button", "pick");
      const av = el("span", "avatar");
      av.append(icon(isId ? "monitor" : "lan"));
      const txt = el("span");
      txt.append(el("div", "t mono", isId ? groupId(x.target) : x.target));
      txt.append(el("div", "when", `${t(isId ? "control.byId" : "control.byAddress")} · ${ago(x.last_used)}`));
      pick.append(av, txt);
      pick.addEventListener("click", () => {
        /** @type {HTMLInputElement} */ ($("target")).value = isId ? groupId(x.target) : x.target;
        $("password").focus();
      });
      const rm = el("button", "icon-btn rm");
      rm.append(icon("x"));
      rm.setAttribute("aria-label", t("control.forget", { target: x.target }));
      rm.title = rm.getAttribute("aria-label") ?? "";
      rm.addEventListener("click", async () => {
        await invoke("forget_recent", { target: x.target });
        loadRecent();
      });
      li.append(pick, rm);
      return li;
    }),
  );
  $("recent-block").hidden = r.length === 0;
}

function setupConnect() {
  const pw = /** @type {HTMLInputElement} */ ($("password"));
  $("show-pw").addEventListener("click", () => {
    const show = pw.type === "password";
    pw.type = show ? "text" : "password";
    $("show-pw").setAttribute("aria-pressed", String(show));
  });
  $("connect-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const target = /** @type {HTMLInputElement} */ ($("target")).value.trim();
    const msg = $("connect-msg");
    const btn = /** @type {HTMLButtonElement} */ ($("connect"));
    msg.className = "msg";
    btn.disabled = true;
    try {
      await invoke("connect", { target, password: pw.value });
      pw.value = "";
      msg.textContent = t("control.connecting", { target });
      loadRecent();
    } catch (err) {
      msg.className = "msg error";
      msg.textContent = String(err);
    } finally {
      btn.disabled = false;
    }
  });
}

/** @param {ViewerEvent} ev */
function onViewer(ev) {
  const msg = $("connect-msg");
  if (ev.state === "ended") {
    msg.className = ev.ok ? "msg" : "msg error";
    msg.textContent = ev.ok ? t("control.closed", { target: ev.target }) : ev.message ?? "";
  }
}

// ---------------------------------------------------------------- activity

/** Split one audit line into time, kind and detail. */
function parseAudit(line) {
  const m = line.match(/^(\S+)\s+(.*)$/);
  const when = m ? new Date(m[1]) : null;
  const rest = m ? m[2] : line;
  /** @type {[RegExp, string, string][]} */
  const kinds = [
    [/^SESSION START /, "audit.start", "start"],
    [/^SESSION END /, "audit.end", "end"],
    [/^FAILED login from /, "audit.failed", "bad"],
    [/^REFUSED /, "audit.refused", "bad"],
    [/^DECLINED /, "audit.declined", "warn"],
    [/^UNATTENDED /i, "audit.unattended", "warn"],
  ];
  for (const [re, label, cls] of kinds) {
    if (re.test(rest)) return { when, label: t(label), cls, detail: rest.replace(re, "") };
  }
  return { when, label: t("audit.other"), cls: "", detail: rest };
}

async function loadAudit() {
  const list = $("audit");
  try {
    const text = /** @type {string} */ (await invoke("audit_log"));
    const lines = text.split("\n").filter((l) => l.trim() && !l.startsWith("("));
    if (lines.length === 0) {
      list.replaceChildren(el("li", "empty", t("activity.empty")));
      return;
    }
    list.replaceChildren(
      ...lines.reverse().map((l) => {
        const a = parseAudit(l);
        const li = el("li");
        const when =
          a.when && !isNaN(a.when.getTime())
            ? a.when.toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" })
            : "";
        li.append(el("span", "when", when), el("span", `tag ${a.cls}`, a.label), el("span", "what", a.detail));
        return li;
      }),
    );
  } catch (err) {
    list.replaceChildren(el("li", "empty", String(err)));
  }
}

// ---------------------------------------------------------------- settings

/** @param {string} key */
function control(key) {
  return document.querySelector(`[data-key="${key}"]`);
}

/** @param {string} key */
function readValue(key) {
  if (key === "quality") {
    const r = document.querySelector('input[name="quality"]:checked');
    return r instanceof HTMLInputElement ? r.value : "speed";
  }
  const c = control(key);
  if (c instanceof HTMLInputElement) return c.type === "checkbox" ? String(c.checked) : c.value.trim();
  return "";
}

async function loadSettings() {
  /** @type {SettingRow[]} */
  const rows = await invoke("settings");
  for (const r of rows) {
    if (r.key === "quality") {
      const q = document.getElementById(`q-${r.value || "speed"}`);
      if (q instanceof HTMLInputElement) q.checked = true;
      continue;
    }
    const c = control(r.key);
    if (c instanceof HTMLInputElement) {
      if (c.type === "checkbox") c.checked = r.value === "true";
      else c.value = r.value;
    }
  }
  $("settings-msg").textContent = "";
}

function setupSettings() {
  $("settings-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const msg = $("settings-msg");
    msg.className = "msg";
    /** @type {SettingRow[]} */
    const before = await invoke("settings");
    const old = Object.fromEntries(before.map((r) => [r.key, r.value]));
    let changed = 0;
    let serverChanged = false;
    try {
      // Key before address, so the host never sees a new server without its key.
      for (const key of ["server_key", "server", "quality", "clipboard", "cmd_ctrl_swap", "start_with_os"]) {
        const value = readValue(key);
        if (value === (old[key] ?? "")) continue;
        await invoke("set_setting", { key, value });
        changed++;
        if (key === "server" || key === "server_key") serverChanged = true;
      }
      if (serverChanged && !host.inSession) {
        await invoke("stop_host");
        host.id = null;
        await startHost();
        msg.textContent = t("settings.restartHost");
      } else {
        msg.textContent = changed ? t("settings.saved") : t("settings.nothing");
      }
      msg.className = "msg ok";
    } catch (err) {
      msg.className = "msg error";
      msg.textContent = String(err);
    }
  });
  $("perm-screen").addEventListener("click", () => invoke("open_privacy_settings", { pane: "screen" }));
  $("perm-ax").addEventListener("click", () => invoke("open_privacy_settings", { pane: "accessibility" }));
  if (!isMac) $("mac-perms").hidden = true;
}

// ---------------------------------------------------------------- start

async function startHost() {
  setStatus("starting", t("status.starting"));
  $("host-status").textContent = t("allow.starting");
  try {
    await invoke("start_host");
  } catch (err) {
    notice(String(err));
    setStatus("off", t("status.off"));
  }
}

async function main() {
  applyStrings();
  $("hotkey").textContent = isMac ? "⌃⌥⇧X" : "Ctrl+Alt+Shift+X";
  setupTabs();
  setupConnect();
  setupSettings();

  await listen("host", (e) => onHost(e.payload));
  await listen("viewer", (e) => onViewer(e.payload));
  await listen("host_exit", (e) => {
    host.running = false;
    host.inSession = false;
    clearInterval(sessionTimer);
    hideConsent();
    $("creds").hidden = true;
    $("session").hidden = true;
    $("ready-badge").hidden = true;
    setStatus("off", t("status.off"));
    const p = e.payload ?? {};
    $("host-status").textContent = t("allow.stopped");
    if (/screen recording/i.test(p.raw ?? "")) permissionNotice();
    else notice(p.message ?? "", { label: t("allow.restart"), run: startHost });
  });

  $("copy-id").addEventListener("click", () => copyText($("my-id").textContent ?? ""));
  $("copy-pw").addEventListener("click", () => copyText($("my-pw").textContent ?? ""));
  $("new-pw").addEventListener("click", () => invoke("host_cmd", { cmd: { cmd: "new_password" } }));
  $("end-session").addEventListener("click", () => invoke("host_cmd", { cmd: { cmd: "end" } }));
  $("refresh-audit").addEventListener("click", loadAudit);
  $("consent-accept").addEventListener("click", () => {
    hideConsent();
    invoke("host_cmd", { cmd: { cmd: "accept" } });
  });
  $("consent-decline").addEventListener("click", () => {
    hideConsent();
    invoke("host_cmd", { cmd: { cmd: "decline" } });
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !$("consent").hidden) $("consent-decline").click();
  });
  $("chat-form").addEventListener("submit", (e) => {
    e.preventDefault();
    const input = /** @type {HTMLInputElement} */ ($("chat-input"));
    const text = input.value.trim();
    if (!text) return;
    invoke("host_cmd", { cmd: { cmd: "chat", text } });
    input.value = "";
  });

  $("version").textContent = `Version ${await invoke("version")}`;
  loadRecent();
  startHost();
}

main();
