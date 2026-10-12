// ucgeocoder admin SPA. No build step, no dependencies.
// All server data is rendered through DOM nodes / textContent (never innerHTML):
// cache keys and addresses are user-supplied. No native dialogs (prompt/confirm).
"use strict";

const TOKEN_KEY = "ucg.adminToken";
const POLL_MS = 5000;
const TABS = ["dashboard", "entries", "jobs", "config", "docs"];

// ------------------------------------------------------------------ helpers

function $(id) {
  return document.getElementById(id);
}

/** el("td", {class: "x", onclick: fn}, "text" | Node | [children]) */
function el(tag, attrs = {}, children = []) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === undefined || v === null || v === false) continue;
    if (k.startsWith("on")) node.addEventListener(k.slice(2), v);
    else if (k === "class") node.className = v;
    else node.setAttribute(k, v === true ? "" : String(v));
  }
  for (const c of [].concat(children)) {
    if (c === undefined || c === null || c === false) continue;
    node.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return node;
}

/** Replace `node`'s children, skipping null/false placeholders. */
function clear(node, ...children) {
  node.replaceChildren(...children.filter((c) => c !== null && c !== undefined && c !== false));
}

function fmtBytes(n) {
  if (n == null) return "–";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n.toFixed(i ? 1 : 0)} ${units[i]}`;
}

function fmtTime(unix) {
  if (!unix) return "–";
  return new Date(unix * 1000).toLocaleString();
}

function fmtNum(n) {
  return n == null ? "–" : Number(n).toLocaleString();
}

let toastTimer;
function toast(msg, kind = "info") {
  const t = $("toast");
  t.textContent = msg;
  t.className = `show ${kind}`;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.className = ""), 5000);
}

/** A button that asks for a second click before running `action`. */
function confirmButton(label, action, confirmLabel = `Confirm ${label.toLowerCase()}`) {
  const b = el("button", { type: "button", class: "danger" }, label);
  let armed = false;
  let timer;
  b.addEventListener("click", async () => {
    if (!armed) {
      armed = true;
      b.textContent = confirmLabel;
      b.classList.add("armed");
      timer = setTimeout(() => {
        armed = false;
        b.textContent = label;
        b.classList.remove("armed");
      }, 4000);
      return;
    }
    clearTimeout(timer);
    b.disabled = true;
    try {
      await action();
    } catch (e) {
      report(e);
    } finally {
      // Always disarm: a failed action must not leave a one-click destructive button.
      armed = false;
      b.textContent = label;
      b.classList.remove("armed");
      b.disabled = false;
    }
  });
  return b;
}

// ---------------------------------------------------------------------- API

class AuthRequired extends Error {}

function token() {
  try {
    return localStorage.getItem(TOKEN_KEY);
  } catch {
    return null;
  }
}

function setToken(t) {
  try {
    if (t) localStorage.setItem(TOKEN_KEY, t);
    else localStorage.removeItem(TOKEN_KEY);
  } catch {
    /* storage unavailable: token lives for this page only */
  }
  memoryToken = t;
  $("signout").hidden = !t;
}
let memoryToken = null;

/** Census-shaped `{"errors":[...]}` or plain text, as one message. */
async function errorText(resp) {
  const text = await resp.text();
  try {
    const j = JSON.parse(text);
    if (Array.isArray(j.errors) && j.errors.length) return j.errors.join("; ");
  } catch {
    /* not JSON */
  }
  return text || `HTTP ${resp.status}`;
}

async function api(path, opts = {}) {
  const headers = new Headers(opts.headers || {});
  const t = token() || memoryToken;
  if (t) headers.set("Authorization", `Bearer ${t}`);
  const resp = await fetch(path, { ...opts, headers });
  if (resp.status === 401) {
    setToken(null);
    askToken("The token was not accepted.");
    throw new AuthRequired("authentication required");
  }
  if (!resp.ok) throw new Error(await errorText(resp));
  return resp;
}

async function apiJson(path, opts) {
  return (await api(path, opts)).json();
}

function sendJson(method, path, body) {
  return apiJson(path, {
    method,
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
}

function report(e) {
  if (!(e instanceof AuthRequired)) toast(e.message || String(e), "error");
}

// -------------------------------------------------------------------- token

let tokenResolve = null;

function askToken(message) {
  const dlg = $("token-dialog");
  const err = $("token-error");
  err.hidden = !message;
  err.textContent = message || "";
  if (!dlg.open) dlg.showModal();
  return new Promise((resolve) => (tokenResolve = resolve));
}

$("token-form").addEventListener("submit", (ev) => {
  const value = new FormData(ev.target).get("token").trim();
  ev.target.reset();
  setToken(value);
  if (tokenResolve) tokenResolve(value);
  tokenResolve = null;
  refreshCurrent();
});

$("signout").addEventListener("click", () => {
  setToken(null);
  askToken();
});

// ------------------------------------------------------------------ routing

let current = null;
let pollTimer = null;

function tabFromPath() {
  const seg = location.pathname.replace(/^\/admin\/?/, "").split("/")[0];
  return TABS.includes(seg) ? seg : "dashboard";
}

function show(tab, push = true) {
  if (push && tabFromPath() !== tab) history.pushState({}, "", `/admin/${tab}`);
  current = tab;
  for (const t of TABS) $(`tab-${t}`).hidden = t !== tab;
  for (const a of document.querySelectorAll("#nav a")) {
    a.classList.toggle("active", a.dataset.tab === tab);
  }
  clearInterval(pollTimer);
  pollTimer = null;
  enter(tab);
}

function enter(tab) {
  if (tab === "dashboard") {
    loadDashboard();
    loadDetail();
    pollTimer = setInterval(loadDashboard, POLL_MS);
  } else if (tab === "entries") {
    if (!entriesState.loaded) loadEntries();
  } else if (tab === "jobs") {
    loadJobs();
    pollTimer = setInterval(loadJobs, POLL_MS);
  } else if (tab === "config") {
    loadConfig();
  } else if (tab === "docs") {
    const f = $("docs-frame");
    if (!f.src) f.src = f.dataset.src;
  }
}

function refreshCurrent() {
  if (current) show(current, false);
}

document.querySelectorAll("#nav a").forEach((a) =>
  a.addEventListener("click", (ev) => {
    ev.preventDefault();
    show(a.dataset.tab);
  }),
);
window.addEventListener("popstate", () => show(tabFromPath(), false));
// Only the polling tabs refresh on focus (Config may hold unsaved edits).
document.addEventListener("visibilitychange", () => {
  if (!document.hidden && (current === "dashboard" || current === "jobs")) refreshCurrent();
});

// ---------------------------------------------------------------- dashboard

function card(label, value, hint) {
  return el("div", { class: "card" }, [
    el("div", { class: "card-label" }, label),
    el("div", { class: "card-value" }, value),
    hint ? el("div", { class: "card-hint" }, hint) : null,
  ]);
}

function kv(dl, pairs) {
  clear(dl, ...pairs.flatMap(([k, v]) => [el("dt", {}, k), el("dd", {}, v)]));
}

async function loadDashboard() {
  try {
    const [stats, up, running] = await Promise.all([
      apiJson("/api/v1/cache/stats"),
      apiJson("/api/v1/upstream"),
      apiJson("/api/v1/jobs?state=running&limit=50"),
    ]);
    const c = stats.counters;
    const active = ["queued", "running", "paused"].reduce((n, s) => n + (stats.jobs[s] || 0), 0);
    clear(
      $("dash-cards"),
      card("Entries", fmtNum(stats.entries)),
      card(
        "Hit ratio",
        stats.hit_ratio == null ? "–" : `${(stats.hit_ratio * 100).toFixed(1)}%`,
        `${fmtNum(c.hits)} hits · ${fmtNum(c.misses)} misses`,
      ),
      card("Database", fmtBytes(stats.file_bytes), `${fmtBytes(stats.entries_table.stored_bytes)} entries data`),
      card("Retry queue", fmtNum(stats.retry_queue_len), "NoMatch re-checks"),
      card("Active jobs", fmtNum(active)),
      card(
        "Breaker",
        up.breaker.open ? "OPEN" : "closed",
        up.breaker.open ? `retry in ${up.breaker.retry_after_secs}s` : `${up.breaker.consecutive_failures} recent failures`,
      ),
    );
    $("dash-cards").lastChild.classList.toggle("alert", up.breaker.open);
    kv($("dash-upstream"), [
      ["Single rate", `${up.single_rps}/s (burst ${up.single_burst})`],
      ["Single permits", `${up.single.available} of ${up.single.concurrency} free · ${up.single.sync_waiters} waiting`],
      ["Batch permits", `${up.batch.available} of ${up.batch.concurrency} free · ${up.batch.sync_waiters} waiting`],
      ["Batch gap", `${up.batch_min_interval_secs}s`],
      ["Coalesced fetches", fmtNum(up.singleflight_in_flight)],
      ["Last upstream OK", fmtTime(up.last_ok_at)],
      ["Upstream ok / err", `${fmtNum(c.upstream_ok)} / ${fmtNum(c.upstream_err)}`],
    ]);
    const breakerReset = up.breaker.open
      ? el("li", {}, confirmButton("Reset breaker", async () => {
          await api("/api/v1/upstream/breaker/reset", { method: "POST" });
          loadDashboard();
        }))
      : null;
    clear(
      $("dash-errors"),
      ...(up.recent_errors.length
        ? up.recent_errors
            .slice()
            .reverse()
            .map((e) => el("li", {}, [el("span", { class: "muted" }, fmtTime(e.at)), " ", e.message]))
        : [el("li", { class: "muted" }, "none")]),
      breakerReset,
    );
    clear($("dash-jobs"), jobsTable(running.jobs, false));
    $("dash-updated").textContent = `updated ${new Date().toLocaleTimeString()}`;
  } catch (e) {
    report(e);
  }
}

async function loadDetail() {
  const dl = $("dash-detail");
  clear(dl, el("dt", {}, "Loading…"));
  try {
    const d = (await apiJson("/api/v1/cache/stats?detail=full")).detail;
    const fmtMap = (m) =>
      Object.entries(m)
        .map(([k, v]) => `${k}: ${fmtNum(v)}`)
        .join(" · ") || "–";
    kv(dl, [
      ["By status", fmtMap(d.by_status)],
      ["By search type", fmtMap(d.by_searchtype)],
      ["By benchmark", fmtMap(d.by_benchmark)],
      [
        "Compression",
        d.compression
          ? `${d.compression.ratio.toFixed(2)}× (sample of ${fmtNum(d.compression.sampled)})`
          : "–",
      ],
      ["Undecodable", fmtNum(d.undecodable)],
    ]);
  } catch (e) {
    clear(dl);
    report(e);
  }
}
$("dash-detail-refresh").addEventListener("click", loadDetail);

// ------------------------------------------------------------------ entries

const entriesState = { loaded: false, query: {}, cursors: [null], page: 0, next: null, gen: 0 };

function entriesUrl(cursor) {
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(entriesState.query)) if (v) p.set(k, v);
  if (cursor) p.set("cursor", cursor);
  return `/api/v1/cache/entries?${p}`;
}

async function loadEntries() {
  // Only the latest request may update the list and the cursor.
  const gen = ++entriesState.gen;
  const cursor = entriesState.cursors[entriesState.page];
  $("entries-prev").disabled = true;
  $("entries-next").disabled = true;
  try {
    const page = await apiJson(entriesUrl(cursor));
    if (gen !== entriesState.gen) return;
    entriesState.loaded = true;
    entriesState.next = page.next_cursor;
    const list = $("entries-list");
    if (!page.entries.length) {
      clear(
        list,
        el(
          "p",
          { class: "muted" },
          page.next_cursor
            ? "No matches in this stretch of keys yet; use Next to keep scanning."
            : "No entries.",
        ),
      );
    } else {
      const rows = page.entries.map((e) =>
        el("tr", {}, [
          el("td", { class: "key" }, e.key),
          el("td", {}, el("span", { class: `badge ${e.status}` }, e.status)),
          el("td", {}, e.source),
          el("td", {}, e.views.join(", ")),
          el("td", {}, fmtTime(e.created_at)),
          el("td", {}, e.fresh ? "fresh" : el("span", { class: "muted" }, "expired")),
          el("td", {}, el("button", { type: "button", onclick: () => inspect(e.key) }, "Inspect")),
        ]),
      );
      clear(
        list,
        el("table", {}, [
          el("thead", {}, el("tr", {}, ["Key", "Status", "Source", "Views", "Created", "", ""].map((h) => el("th", {}, h)))),
          el("tbody", {}, rows),
        ]),
      );
    }
    $("entries-prev").disabled = entriesState.page === 0;
    $("entries-next").disabled = !page.next_cursor;
    $("entries-page").textContent = `page ${entriesState.page + 1}`;
  } catch (e) {
    if (gen !== entriesState.gen) return;
    $("entries-prev").disabled = entriesState.page === 0;
    report(e);
  }
}

$("entries-filter").addEventListener("submit", (ev) => {
  ev.preventDefault();
  entriesState.query = Object.fromEntries(new FormData(ev.target));
  entriesState.cursors = [null];
  entriesState.page = 0;
  loadEntries();
});
$("entries-next").addEventListener("click", () => {
  if (!entriesState.next) return;
  entriesState.cursors[entriesState.page + 1] = entriesState.next;
  entriesState.next = null;
  entriesState.page++;
  loadEntries();
});
$("entries-prev").addEventListener("click", () => {
  entriesState.page = Math.max(0, entriesState.page - 1);
  loadEntries();
});

async function inspect(key) {
  const panel = $("entry-detail");
  const path = `/api/v1/cache/entries/${encodeURIComponent(key)}`;
  try {
    const e = await apiJson(path);
    clear(
      panel,
      el("h2", {}, "Entry"),
      el("p", { class: "key" }, e.key),
      el("dl", { class: "kv" }, [
        ["Status", e.status],
        ["Source", e.source],
        ["Views", e.views.join(", ")],
        ["Benchmark / vintage", `${e.benchmark_name} / ${e.vintage_name}`],
        ["Created", fmtTime(e.created_at)],
        ["Verified", fmtTime(e.verified_at)],
        ["Expires", e.expires_at ? fmtTime(e.expires_at) : "never"],
        ["Stored size", fmtBytes(e.stored_bytes)],
      ].flatMap(([k, v]) => [el("dt", {}, k), el("dd", {}, v)])),
      e.batch ? el("h3", {}, "Batch row") : null,
      e.batch ? el("pre", {}, JSON.stringify(e.batch, null, 2)) : null,
      e.json ? el("h3", {}, "Upstream JSON") : null,
      e.json ? el("pre", {}, JSON.stringify(e.json, null, 2)) : null,
      el("div", { class: "bar" }, confirmButton("Delete", async () => {
        await api(path, { method: "DELETE" });
        toast("Entry deleted");
        clear(panel, el("p", { class: "muted" }, "Deleted."));
        loadEntries();
      })),
    );
  } catch (e) {
    report(e);
  }
}

// --------------------------------------------------------------------- jobs

function progressBar(pct) {
  const bar = el("div", { class: "progress", role: "progressbar", "aria-valuenow": pct, "aria-valuemin": 0, "aria-valuemax": 100 });
  bar.append(el("div", { class: "progress-fill" }));
  bar.firstChild.style.width = `${Math.max(0, Math.min(100, pct))}%`;
  return bar;
}

async function download(job, query, ext) {
  try {
    const blob = await (await api(`/api/v1/jobs/${encodeURIComponent(job.id)}/results${query}`)).blob();
    const url = URL.createObjectURL(blob);
    const a = el("a", { href: url, download: `job-${job.id}${ext}` });
    document.body.append(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 10000);
  } catch (e) {
    report(e);
  }
}

async function jobAction(job, action) {
  try {
    await api(`/api/v1/jobs/${encodeURIComponent(job.id)}/${action}`, { method: "POST" });
    loadJobs();
  } catch (e) {
    report(e);
  }
}

function jobsTable(jobs, actions) {
  if (!jobs.length) return el("p", { class: "muted" }, "No jobs.");
  const rows = jobs.map((j) => {
    const t = j.totals;
    const acts = [];
    if (actions) {
      if (j.state === "queued" || j.state === "running") {
        acts.push(el("button", { type: "button", onclick: () => jobAction(j, "pause") }, "Pause"));
      }
      if (j.state === "paused") {
        acts.push(el("button", { type: "button", onclick: () => jobAction(j, "resume") }, "Resume"));
      }
      if (["queued", "running", "paused"].includes(j.state)) {
        acts.push(confirmButton("Cancel", () => jobAction(j, "cancel")));
      }
      acts.push(
        el("button", { type: "button", onclick: () => download(j, "", ".csv") }, "CSV"),
        el("button", { type: "button", onclick: () => download(j, "?format=json", ".jsonl") }, "JSON"),
        el("button", { type: "button", onclick: () => download(j, "?only=unmatched", "-unmatched.csv") }, "Unmatched"),
        confirmButton("Delete", async () => {
          await api(`/api/v1/jobs/${encodeURIComponent(j.id)}`, { method: "DELETE" });
          loadJobs();
        }),
      );
    }
    return el("tr", {}, [
      el("td", { class: "key", title: j.id }, j.id.slice(0, 8) + "…"),
      el("td", {}, j.kind),
      el("td", {}, el("span", { class: `badge ${j.state}` }, j.state)),
      el("td", {}, `${j.search_type} · ${j.return_type}`),
      el("td", { class: "wide" }, [progressBar(j.progress), el("div", { class: "muted small" },
        `${fmtNum(t.total - t.pending)}/${fmtNum(t.total)} · ${fmtNum(t.matched)} match · ${fmtNum(t.no_match)} no match · ${fmtNum(t.tie)} tie · ${fmtNum(t.failed)} failed · ${fmtNum(t.cached)} cached`)]),
      el("td", {}, j.eta_secs == null ? "–" : `${j.eta_secs}s`),
      el("td", {}, fmtTime(j.created_at)),
      actions ? el("td", { class: "actions" }, acts) : null,
      j.error ? el("td", { class: "error small" }, j.error) : null,
    ]);
  });
  const head = ["Id", "Kind", "State", "Type", "Progress", "ETA", "Created"].concat(actions ? [""] : []);
  return el("table", {}, [
    el("thead", {}, el("tr", {}, head.map((h) => el("th", {}, h)))),
    el("tbody", {}, rows),
  ]);
}

async function loadJobs() {
  try {
    // The API lists oldest first: collect every page, then show newest first.
    const jobs = [];
    let cursor = null;
    do {
      const q = cursor ? `&cursor=${encodeURIComponent(cursor)}` : "";
      const page = await apiJson(`/api/v1/jobs?limit=1000${q}`);
      jobs.push(...page.jobs);
      cursor = page.next_cursor;
    } while (cursor);
    clear($("jobs-list"), jobsTable(jobs.reverse(), true));
  } catch (e) {
    report(e);
  }
}

$("job-upload").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const f = new FormData(ev.target);
  const search = f.get("searchtype");
  const file = f.get("file");
  const form = new FormData();
  form.append(search === "coordinates" ? "coordinatesFile" : "addressFile", file, file.name);
  for (const k of ["searchtype", "returntype", "benchmark", "vintage", "priority"]) {
    if (f.get(k)) form.append(k, f.get(k));
  }
  try {
    const r = await apiJson("/api/v1/jobs", { method: "POST", body: form });
    toast(`Job ${r.id.slice(0, 8)}… created: ${r.counts.total} rows, ${r.counts.cached} cached`);
    ev.target.reset();
    loadJobs();
  } catch (e) {
    report(e);
  }
});

$("job-json").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  let body;
  try {
    body = JSON.parse(new FormData(ev.target).get("body"));
  } catch (e) {
    toast(`Invalid JSON: ${e.message}`, "error");
    return;
  }
  try {
    const r = await sendJson("POST", "/api/v1/jobs", body);
    toast(`Job ${r.id.slice(0, 8)}… created: ${r.counts.total} rows, ${r.counts.cached} cached`);
    loadJobs();
  } catch (e) {
    report(e);
  }
});

$("jobs-refresh-negatives").addEventListener("click", async () => {
  try {
    const r = await sendJson("POST", "/api/v1/jobs/refresh-negatives", {});
    toast(r.jobs.length ? `${r.jobs.length} refresh job(s) created` : "Nothing due for a re-check");
    loadJobs();
  } catch (e) {
    report(e);
  }
});

// ------------------------------------------------------------------- config

const configEdits = new Map();

async function loadConfig() {
  configEdits.clear();
  $("config-save").disabled = true;
  try {
    const cfg = await apiJson("/api/v1/config");
    const sections = new Map();
    for (const f of cfg.fields) {
      const s = f.path.split(".")[0];
      if (!sections.has(s)) sections.set(s, []);
      sections.get(s).push(f);
    }
    const blocks = [];
    for (const [section, fields] of sections) {
      const rows = fields.map((f) => {
        let value;
        if (f.overridable) {
          value = el("input", { value: f.value, "aria-label": f.path });
          value.addEventListener("input", () => {
            if (value.value === f.value) configEdits.delete(f.path);
            else configEdits.set(f.path, value.value);
            value.classList.toggle("changed", configEdits.has(f.path));
            $("config-save").disabled = configEdits.size === 0;
          });
        } else {
          value = el("code", {}, f.value || "(empty)");
        }
        const reset =
          f.source === "override"
            ? el("button", { type: "button", class: "link", onclick: () => resetConfig(f.path) }, "reset")
            : null;
        return el("tr", {}, [
          el("td", { class: "key", title: f.doc }, f.path),
          el("td", {}, value),
          el("td", {}, [el("span", { class: `badge src-${f.source}` }, f.source), " ", reset]),
          el("td", { class: "muted small" }, [f.doc, f.env ? ` (${f.env}, default ${f.default || "unset"})` : ""]),
        ]);
      });
      blocks.push(el("h2", {}, section), el("table", { class: "config" }, el("tbody", {}, rows)));
    }
    clear($("config-list"), ...blocks);
  } catch (e) {
    report(e);
  }
}

async function resetConfig(path) {
  try {
    await api(`/api/v1/config/${encodeURIComponent(path)}`, { method: "DELETE" });
    toast(`${path} reset to its startup value`);
    loadConfig();
  } catch (e) {
    report(e);
  }
}

$("config-save").addEventListener("click", async () => {
  // Values go as strings: the server parses them per key (durations, numbers, bools).
  const body = Object.fromEntries(configEdits);
  try {
    await sendJson("PUT", "/api/v1/config", body);
    toast(`Applied ${configEdits.size} change(s)`);
    loadConfig();
  } catch (e) {
    report(e);
  }
});

// -------------------------------------------------------------------- start

async function start() {
  setToken(token());
  // Probe once: an open server needs no token; otherwise ask before polling.
  const probe = await fetch("/api/v1/cache/stats", {
    headers: token() ? { Authorization: `Bearer ${token()}` } : {},
  }).catch(() => null);
  if (probe && probe.status === 401) {
    const hadToken = Boolean(token());
    setToken(null);
    await askToken(hadToken ? "The stored token was not accepted." : "");
  }
  show(tabFromPath(), false);
}

start();
