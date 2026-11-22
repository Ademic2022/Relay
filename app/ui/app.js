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
// eslint-disable-next-line @typescript-eslint/no-explicit-any -- Tauri global has no bundled types here
const T = /** @type {any} */ (window).__TAURI__;
/** @type {(cmd: string, args?: object) => Promise<any>} */
const invoke = T.core.invoke;
/** @type {(name: string, cb: (e: { payload: any }) => void) => Promise<unknown>} */
const listen = T.event.listen;

/** @type {Record<string, string>} */
const S = /** @type {any} */ (window).RELAY_STRINGS;

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

function applyStrings() {
  document.querySelectorAll("[data-i18n]").forEach((el) => {
    const k = el.getAttribute("data-i18n");
    if (k && S[k]) el.textContent = S[k];
  });
  document.querySelectorAll("[data-i18n-ph]").forEach((el) => {
    const k = el.getAttribute("data-i18n-ph");
    if (k && S[k]) el.setAttribute("placeholder", S[k]);
  });
  document.querySelectorAll("[data-i18n-label]").forEach((el) => {
    const k = el.getAttribute("data-i18n-label");
    if (k && S[k]) el.setAttribute("aria-label", S[k]), el.setAttribute("title", S[k]);
  });
}

/** Group a 9-digit ID as "123 456 789". */
function groupId(id) {
  const d = String(id).replace(/\D/g, "");
  return d.length === 9 ? `${d.slice(0, 3)} ${d.slice(3, 6)} ${d.slice(6)}` : String(id);
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
      const panel = $(x.getAttribute("aria-controls") ?? "");
      panel.hidden = !on;
    }
    tab.focus();
    if (tab.id === "tab-activity") loadAudit();
    if (tab.id === "tab-settings") loadSettings();
  };
  tabs.forEach((tab, i) => {
    tab.addEventListener("click", () => select(tab));
    tab.addEventListener("keydown", (e) => {
      const n = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
      if (n) select(tabs[(i + n + tabs.length) % tabs.length]);
    });
  });
}

// ---------------------------------------------------------------- host

const host = {
  /** @type {string | null} */ password: null,
  /** @type {string | null} */ id: null,
  /** @type {string | null} */ server: null,
  inSession: false,
};

function notice(text, action) {
  const n = $("notice");
  n.textContent = text;
  if (action) {
    const b = document.createElement("button");
    b.textContent = action.label;
    b.addEventListener("click", action.run);
    n.append(" ", b);
  }
  n.hidden = !text;
}

function renderIdle() {
  $("creds").hidden = false;
  $("session").hidden = true;
  $("my-pw").textContent = host.password ?? "—";
  $("pw-block").hidden = host.password === null;
  if (host.id) {
    $("my-id").textContent = groupId(host.id);
    $("id-hint").textContent = "";
    $("host-status").textContent = t("allow.ready");
  } else if (host.server) {
    $("my-id").textContent = "…";
    $("id-hint").textContent = t("allow.serverWaiting");
    $("host-status").textContent = t("allow.readyDirect");
  } else {
    $("my-id").textContent = "—";
    $("id-hint").textContent = t("allow.noServer");
    $("host-status").textContent = t("allow.readyDirect");
  }
}

/** @param {HostEvent} ev */
function onHost(ev) {
  switch (ev.event) {
    case "ready": {
      host.password = ev.password ?? null;
      host.server = ev.server ?? null;
      const list = $("direct-list");
      list.replaceChildren(
        ...(ev.direct ?? []).map((/** @type {string} */ a) => {
          const li = document.createElement("li");
          li.textContent = a;
          return li;
        }),
      );
      const details = document.querySelector("details.direct");
      if (!host.server && details instanceof HTMLDetailsElement) details.open = true;
      notice("");
      renderIdle();
      break;
    }
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
    case "session_start":
      host.inSession = true;
      $("creds").hidden = true;
      $("session").hidden = false;
      $("session-who").textContent = t(ev.view_only ? "session.viewOnly" : "session.with", { name: ev.name });
      $("host-status").textContent = "";
      $("chat-log").replaceChildren();
      break;
    case "session_end":
      host.inSession = false;
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
      if (ev.kind === "screen_recording") {
        notice(t("perm.screen"), { label: t("perm.open"), run: () => invoke("open_privacy_settings", { pane: "screen" }) });
      }
      break;
  }
}

function addChat(who, text, mine) {
  const li = document.createElement("li");
  li.className = mine ? "mine" : "theirs";
  const b = document.createElement("strong");
  b.textContent = `${who}: `;
  li.append(b, document.createTextNode(text)); // text, never HTML
  $("chat-log").append(li);
  li.scrollIntoView({ block: "end" });
}

// ---------------------------------------------------------------- consent (FR-8)

/** @type {number | undefined} */
let consentTimer;

function showConsent(name, seconds) {
  $("consent-body").textContent = t("consent.body", { name });
  let left = seconds;
  const tick = () => {
    $("consent-timer").textContent = t("consent.timer", { s: left });
    if (left-- <= 0) hideConsent();
  };
  tick();
  clearInterval(consentTimer);
  consentTimer = window.setInterval(tick, 1000);
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
  const ul = $("recent");
  ul.replaceChildren(
    ...r.map((x) => {
      const li = document.createElement("li");
      const pick = document.createElement("button");
      pick.className = "link mono";
      pick.textContent = /^\d{9}$/.test(x.target) ? groupId(x.target) : x.target;
      pick.addEventListener("click", () => {
        /** @type {HTMLInputElement} */ ($("target")).value = pick.textContent ?? "";
        $("password").focus();
      });
      const rm = document.createElement("button");
      rm.className = "icon";
      rm.textContent = "✕";
      rm.setAttribute("aria-label", t("control.forget", { target: x.target }));
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
  $("connect-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const target = /** @type {HTMLInputElement} */ ($("target")).value;
    const pw = /** @type {HTMLInputElement} */ ($("password"));
    const msg = $("connect-msg");
    msg.className = "msg";
    try {
      await invoke("connect", { target, password: pw.value });
      pw.value = "";
      msg.textContent = t("control.connecting", { target: target.trim() });
      loadRecent();
    } catch (err) {
      msg.className = "msg error";
      msg.textContent = String(err);
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

// ---------------------------------------------------------------- activity & settings

async function loadAudit() {
  try {
    const text = /** @type {string} */ (await invoke("audit_log"));
    $("audit").textContent = text.trim() || t("activity.empty");
  } catch (err) {
    $("audit").textContent = String(err);
  }
}

async function loadSettings() {
  /** @type {SettingRow[]} */
  const rows = await invoke("settings");
  for (const r of rows) {
    const el = document.querySelector(`[data-key="${r.key}"]`);
    if (el instanceof HTMLInputElement && el.type === "checkbox") el.checked = r.value === "true";
    else if (el instanceof HTMLInputElement || el instanceof HTMLSelectElement) el.value = r.value;
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
    let serverChanged = false;
    try {
      // Key before address, so the host never sees a new server without its key.
      for (const key of ["server_key", "server", "quality", "clipboard", "cmd_ctrl_swap", "start_with_os"]) {
        const el = document.querySelector(`[data-key="${key}"]`);
        const value =
          el instanceof HTMLInputElement && el.type === "checkbox"
            ? String(el.checked)
            : /** @type {HTMLInputElement} */ (el).value.trim();
        if (value === (old[key] ?? "")) continue;
        await invoke("set_setting", { key, value });
        if (key === "server" || key === "server_key") serverChanged = true;
      }
      if (serverChanged && !host.inSession) {
        await invoke("stop_host");
        host.id = null;
        await startHost();
        msg.textContent = t("settings.restartHost");
      } else {
        msg.textContent = t("settings.saved");
      }
    } catch (err) {
      msg.className = "msg error";
      msg.textContent = String(err);
    }
  });
  $("perm-screen").addEventListener("click", () => invoke("open_privacy_settings", { pane: "screen" }));
  $("perm-ax").addEventListener("click", () => invoke("open_privacy_settings", { pane: "accessibility" }));
}

// ---------------------------------------------------------------- start

async function startHost() {
  $("host-status").textContent = t("allow.starting");
  try {
    await invoke("start_host");
  } catch (err) {
    notice(String(err));
  }
}

function copy(id) {
  const text = $(id).textContent ?? "";
  navigator.clipboard?.writeText(text.trim()).catch(() => {});
}

async function main() {
  applyStrings();
  setupTabs();
  setupConnect();
  setupSettings();

  await listen("host", (e) => onHost(e.payload));
  await listen("viewer", (e) => onViewer(e.payload));
  await listen("host_exit", (e) => {
    host.inSession = false;
    hideConsent();
    $("creds").hidden = true;
    $("session").hidden = true;
    const p = e.payload ?? {};
    $("host-status").textContent = t("allow.stopped");
    if (/screen recording/i.test(p.raw ?? "")) {
      notice(t("perm.screen"), { label: t("perm.open"), run: () => invoke("open_privacy_settings", { pane: "screen" }) });
    } else {
      notice(p.message ?? "", { label: t("allow.restart"), run: startHost });
    }
  });

  $("copy-id").addEventListener("click", () => copy("my-id"));
  $("copy-pw").addEventListener("click", () => copy("my-pw"));
  $("new-pw").addEventListener("click", () => invoke("host_cmd", { cmd: { cmd: "new_password" } }));
  $("end-session").addEventListener("click", () => invoke("host_cmd", { cmd: { cmd: "end" } }));
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

  $("version").textContent = `Relay ${await invoke("version")}`;
  loadRecent();
  startHost();
}

main();
