// Management web UI (specs/122-management-webui). No framework, no build
// step. Every node is built with createElement/textContent: SMS bodies and
// caller IDs are attacker-controlled, so no HTML string ever reaches the DOM.
(function () {
  "use strict";

  var app = document.getElementById("app");

  // ---------------------------------------------------------------- DOM --

  // el("div", {class: "x", onclick: fn, title: "t"}, child, "text", [more]...)
  function el(tag, attrs) {
    var node = document.createElement(tag);
    if (attrs) {
      Object.keys(attrs).forEach(function (key) {
        var value = attrs[key];
        if (value === null || value === undefined || value === false) return;
        if (key.slice(0, 2) === "on" && typeof value === "function") {
          node.addEventListener(key.slice(2), value);
        } else if (key === "class") {
          node.className = value;
        } else if (key === "text") {
          node.textContent = value;
        } else {
          node.setAttribute(key, value === true ? "" : value);
        }
      });
    }
    for (var i = 2; i < arguments.length; i++) append(node, arguments[i]);
    return node;
  }

  function append(node, child) {
    if (child === null || child === undefined || child === false) return;
    if (Array.isArray(child)) {
      child.forEach(function (c) { append(node, c); });
    } else if (child instanceof Node) {
      node.appendChild(child);
    } else {
      node.appendChild(document.createTextNode(String(child)));
    }
  }

  function clear(node) {
    while (node.firstChild) node.removeChild(node.firstChild);
    return node;
  }

  // ---------------------------------------------------------------- API --

  function ApiError(status, body) {
    this.status = status;
    this.body = body;
  }

  function api(path) {
    return fetch(path, { credentials: "same-origin", headers: { Accept: "application/json" } })
      .then(function (response) {
        if (response.status === 401) {
          window.location.assign("/login");
          throw new ApiError(401, null);
        }
        return response.json().catch(function () { return null; }).then(function (body) {
          if (!response.ok) throw new ApiError(response.status, body);
          return body;
        });
      });
  }

  function query(params) {
    var parts = [];
    Object.keys(params).forEach(function (key) {
      var v = params[key];
      if (v !== "" && v !== null && v !== undefined) {
        parts.push(encodeURIComponent(key) + "=" + encodeURIComponent(v));
      }
    });
    return parts.length ? "?" + parts.join("&") : "";
  }

  // ------------------------------------------------------------ formats --

  function pad(n) { return n < 10 ? "0" + n : String(n); }

  function fmtTime(iso) {
    if (!iso) return "—";
    var d = new Date(iso);
    if (isNaN(d.getTime())) return "—";
    return d.getFullYear() + "-" + pad(d.getMonth() + 1) + "-" + pad(d.getDate()) +
      " " + pad(d.getHours()) + ":" + pad(d.getMinutes()) + ":" + pad(d.getSeconds());
  }

  function fmtUnix(sec) {
    return sec ? fmtTime(new Date(sec * 1000).toISOString()) : "—";
  }

  function fmtDuration(total) {
    if (total === null || total === undefined || isNaN(total)) return "—";
    total = Math.max(0, Math.round(total));
    var h = Math.floor(total / 3600);
    var m = Math.floor((total % 3600) / 60);
    var s = total % 60;
    if (h > 0) return h + "h " + m + "m";
    if (m > 0) return m + "m " + s + "s";
    return s + "s";
  }

  function fmtNum(v, digits, unit) {
    if (v === null || v === undefined) return "—";
    return Number(v).toFixed(digits) + (unit || "");
  }

  function maskNumber(n) {
    if (!n) return "—";
    return n.length <= 4 ? n : "•••• " + n.slice(-4);
  }

  // -------------------------------------------------------------- pages --

  // A page is {title, mount(container) -> {refresh()}}. `refresh` is called
  // every 5s while the tab is visible.
  var pages = {};
  var current = null;
  var mainEl = null;
  var navEl = null;

  var NAV = [
    ["overview", "Overview"],
    ["calls", "Calls"],
    ["sms", "SMS"],
    ["events", "Events"],
    ["config", "Config"]
  ];

  function route() {
    var name = (window.location.hash || "#/overview").replace(/^#\//, "").split("?")[0];
    return pages[name] ? name : "overview";
  }

  function show() {
    var name = route();
    Array.prototype.forEach.call(navEl.children, function (a) {
      if (a.getAttribute("data-page") === name) a.setAttribute("aria-current", "page");
      else a.removeAttribute("aria-current");
    });
    if (current && current.destroy) current.destroy();
    clear(mainEl);
    current = pages[name].mount(mainEl);
    if (current && current.refresh) current.refresh();
  }

  function tick() {
    if (document.visibilityState === "visible" && current && current.refresh) current.refresh();
  }

  function errorBanner(container, err) {
    clear(container);
    var text = "Could not load this page.";
    if (err && err.status === 503) text = "The message store is unavailable right now.";
    else if (err && err.status) text = "The server answered " + err.status + ".";
    container.appendChild(el("div", { class: "banner bad", text: text }));
  }

  function renderApp() {
    clear(app);
    navEl = el("nav", { class: "nav" });
    NAV.forEach(function (entry) {
      navEl.appendChild(el("a", { href: "#/" + entry[0], "data-page": entry[0], text: entry[1] }));
    });
    mainEl = el("main");
    app.appendChild(
      el("div", { class: "shell" },
        el("header", { class: "topbar" },
          el("span", { class: "brand", text: "Bridge" }),
          navEl,
          el("button", { class: "btn link", type: "button", text: "Log out", onclick: logout })),
        mainEl));
    window.addEventListener("hashchange", show);
    document.addEventListener("visibilitychange", tick);
    window.setInterval(tick, 5000);
    show();
  }

  function logout() {
    fetch("/api/logout", { method: "POST", credentials: "same-origin" }).then(function () {
      window.location.assign("/login");
    });
  }

  // -------------------------------------------------------------- login --

  function renderLogin() {
    var error = el("p", { class: "form-error", role: "alert" });
    var input = el("input", { type: "password", name: "password", autocomplete: "current-password", required: true, autofocus: true });
    var button = el("button", { class: "btn primary", type: "submit", text: "Sign in" });
    var form = el("form", { class: "login" },
      el("h1", { text: "Bridge" }),
      el("label", { class: "muted small" }, "Password", input),
      error,
      button);

    form.addEventListener("submit", function (event) {
      event.preventDefault();
      error.textContent = "";
      button.disabled = true;
      fetch("/api/login", {
        method: "POST",
        credentials: "same-origin",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ password: input.value })
      }).then(function (response) {
        if (response.status === 204) {
          window.location.assign("/");
          return null;
        }
        return response.json().catch(function () { return {}; }).then(function (body) {
          if (response.status === 429) {
            var wait = body && body.retry_after_seconds;
            var minutes = wait ? Math.ceil(wait / 60) : null;
            error.textContent = "Too many attempts. Try again" +
              (minutes ? " in about " + minutes + " minute" + (minutes === 1 ? "" : "s") : " later") + ".";
          } else if (response.status === 401) {
            error.textContent = "That password is not right.";
          } else {
            error.textContent = "Sign-in failed (" + response.status + ").";
          }
          button.disabled = false;
          return null;
        });
      }).catch(function () {
        error.textContent = "Could not reach the server.";
        button.disabled = false;
      });
    });

    clear(app).appendChild(form);
  }

  // ----------------------------------------------------------- pages go --
  // PAGES-BEGIN

  // ----------------------------------------------------------- overview --

  var REASONS = {
    stale: "Not reporting",
    unregistered: "Not registered",
    tunnel_down: "Tunnel down",
    child_sa_down: "IPsec SA down",
    pdn_down: "LTE data bearer down",
    gm_down: "Gm connection down",
    uplink_held: "Uplink outage",
    flapping: "Tunnel flapping",
    initializing: "Starting up",
    recovering: "Recovering",
    given_up: "Gave up"
  };

  var TRANSPORT = { cs: "GSM", vowifi: "VoWiFi", volte: "VoLTE" };

  function healthBadge(line) {
    var cls = { ok: "ok", degraded: "warn", down: "bad", stale: "stale" }[line.health] || "stale";
    var label = { ok: "Healthy", degraded: "Degraded", down: "Down", stale: "Stale" }[line.health] || line.health;
    return el("span", { class: "badge " + cls, text: label });
  }

  function chip(label, up) {
    if (up === null || up === undefined) return null;
    return el("span", { class: "chip " + (up ? "ok" : "bad"), text: label + (up ? " up" : " down") });
  }

  function signalBars(rssi) {
    var on = 0;
    if (rssi !== null && rssi !== undefined) {
      on = rssi >= -70 ? 4 : rssi >= -85 ? 3 : rssi >= -100 ? 2 : rssi >= -109 ? 1 : 0;
    }
    var cls = "bars" + (on <= 1 ? " poor" : on === 2 ? " weak" : "");
    var bars = el("span", { class: cls, "aria-hidden": "true" });
    for (var i = 1; i <= 4; i++) bars.appendChild(el("i", { class: i <= on ? "on" : "" }));
    return bars;
  }

  var SIGNAL_NOTES = {
    not_available_pcsc: "not available (card reader)",
    not_polled_yet: "not read yet",
    polling_disabled: "polling is off"
  };

  function signalRow(line) {
    var sig = line.signal;
    if (!sig || sig.rssi_dbm === null || sig.rssi_dbm === undefined) {
      var note = SIGNAL_NOTES[line.signal_note] || (sig ? "no reading" : "not read yet");
      return el("div", { class: "row" }, el("span", { class: "k", text: "Signal" }), el("span", { class: "muted", text: note }));
    }
    var lte = [];
    if (sig.rsrp_dbm !== null && sig.rsrp_dbm !== undefined) lte.push("RSRP " + sig.rsrp_dbm);
    if (sig.rsrq_db !== null && sig.rsrq_db !== undefined) lte.push("RSRQ " + sig.rsrq_db);
    if (sig.sinr_db !== null && sig.sinr_db !== undefined) lte.push("SINR " + sig.sinr_db);
    var main = el("span", null, signalBars(sig.rssi_dbm), sig.rssi_dbm + " dBm", sig.rat ? " · " + sig.rat : "");
    var rows = [el("div", { class: "row" }, el("span", { class: "k", text: "Signal" }), main)];
    if (lte.length) {
      var tail = line.transport === "vowifi" ? " · doesn't affect calls" : "";
      rows.push(el("div", { class: "row small " + (line.transport === "vowifi" ? "dim" : "") },
        el("span", { class: "k", text: "LTE quality" }),
        el("span", { text: lte.join(" · ") + tail })));
    }
    return rows;
  }

  function countdown(iso) {
    if (!iso) return "—";
    var left = Math.round((new Date(iso).getTime() - Date.now()) / 1000);
    if (isNaN(left)) return "—";
    return left >= 0 ? "in " + fmtDuration(left) : "lapsed " + fmtDuration(-left) + " ago";
  }

  function sinceText(iso) {
    if (!iso) return "—";
    var ago = Math.round((Date.now() - new Date(iso).getTime()) / 1000);
    return isNaN(ago) ? "—" : fmtDuration(ago) + " ago";
  }

  function lineCard(line, revealed, rerender) {
    var number = line.msisdn;
    var shown = revealed[line.id] ? number : maskNumber(number);
    var numberEl = el("span", null, shown || "—");
    if (number) {
      numberEl.appendChild(el("button", {
        class: "btn link small", type: "button",
        text: revealed[line.id] ? "hide" : "show",
        onclick: function () { revealed[line.id] = !revealed[line.id]; rerender(); }
      }));
    }

    var chips = el("div", { class: "chips" },
      line.transport === "vowifi" ? [chip("Tunnel", line.tunnel_up), chip("IPsec", line.child_sa_up), chip("Gm", line.gm_connection_up),
        line.uplink ? el("span", { class: "chip " + (line.uplink === "up" ? "ok" : "warn"), text: "Uplink " + line.uplink.replace("_", " ") }) : null] : null,
      line.transport === "volte" ? [chip("LTE data", line.pdn_up), chip("Gm", line.gm_connection_up)] : null,
      line.network ? el("span", { class: "chip", text: line.network }) : null);

    var sub = [TRANSPORT[line.transport] || line.transport];
    if (line.carrier) sub.push(line.carrier);

    var body = [
      el("div", { class: "card-head" },
        el("div", null,
          el("div", { class: "card-title", text: line.id }),
          el("div", { class: "card-sub", text: sub.join(" · ") })),
        healthBadge(line))
    ];

    if (line.stale) {
      body.push(el("div", { class: "banner", text: "No report for " + fmtDuration(line.last_report_age_seconds) + ". The values below may be out of date." }));
    } else if (line.health_reasons && line.health_reasons.length) {
      body.push(el("div", { class: "banner" + (line.health === "down" ? " bad" : ""), text:
        line.health_reasons.map(function (r) { return REASONS[r] || r; }).join(" · ") }));
    }

    body.push(
      el("div", { class: "row" }, el("span", { class: "k", text: "Number" }), numberEl),
      el("div", { class: "row" }, el("span", { class: "k", text: "Registration" }),
        el("span", null, line.registration_state || "unknown")));
    if (line.expires_at) {
      body.push(el("div", { class: "row" }, el("span", { class: "k", text: "Renews" }),
        el("span", { "data-countdown": line.expires_at, text: countdown(line.expires_at) })));
    }
    if (line.registered_at) {
      body.push(el("div", { class: "row" }, el("span", { class: "k", text: "Registered" }), el("span", { text: sinceText(line.registered_at) })));
    }
    if (chips.childNodes.length) body.push(chips);
    body.push(signalRow(line));
    body.push(el("div", { class: "row" }, el("span", { class: "k", text: "Calls now" }),
      el("span", { text: String(line.active_call_count || 0) })));
    if (line.last_report_age_seconds !== null && line.last_report_age_seconds !== undefined) {
      body.push(el("div", { class: "row small muted" }, el("span", { class: "k", text: "Last report" }),
        el("span", { text: fmtDuration(line.last_report_age_seconds) + " ago" })));
    }
    return el("article", { class: "panel card health-" + line.health }, body);
  }

  function statBlock(label, value) {
    return el("div", { class: "stat" }, el("div", { class: "k", text: label }), el("div", { class: "v" }, value));
  }

  pages.overview = {
    mount: function (container) {
      var header = el("div", { class: "stats" });
      var alerts = el("div");
      var cards = el("div", { class: "cards" });
      var revealed = {};
      var last = null;

      container.appendChild(el("h1", { text: "Lines" }));
      container.appendChild(header);
      container.appendChild(alerts);
      container.appendChild(cards);

      function draw(data) {
        var b = data.bridge;
        clear(header);
        header.appendChild(statBlock("Uptime", b.uptime_seconds === null ? "—" : fmtDuration(b.uptime_seconds)));
        header.appendChild(statBlock("Version", b.version + (b.git_sha ? " (" + String(b.git_sha).slice(0, 7) + ")" : "")));
        header.appendChild(statBlock("PBX", b.pbx_registered === null
          ? el("span", { class: "muted", text: "n/a" })
          : el("span", { class: "badge " + (b.pbx_registered ? "ok" : "bad"), text: b.pbx_registered ? "registered" : "not registered" })));
        if (b.grafana_url) {
          header.appendChild(statBlock("History", el("a", { href: b.grafana_url, target: "_blank", rel: "noopener noreferrer", text: "Open Grafana" })));
        }

        clear(alerts);
        if (data.active_alerts.length) {
          var list = el("div", { class: "chips" });
          data.active_alerts.forEach(function (a) {
            list.appendChild(el("span", { class: "badge bad", text:
              a.category.replace(/_/g, " ") + (a.module_id ? " · " + a.module_id : "") + (a.since ? " · since " + fmtTime(a.since) : "") }));
          });
          alerts.appendChild(el("div", { class: "banner bad" }, "Active alerts", list));
        }

        clear(cards);
        if (!data.lines.length) {
          cards.appendChild(el("div", { class: "empty" }, "No lines yet. Cards appear once a modem or an agent reports in."));
        }
        data.lines.forEach(function (line) { cards.appendChild(lineCard(line, revealed, function () { draw(last); })); });
      }

      var timer = window.setInterval(function () {
        Array.prototype.forEach.call(container.querySelectorAll("[data-countdown]"), function (node) {
          node.textContent = countdown(node.getAttribute("data-countdown"));
        });
      }, 1000);

      return {
        refresh: function () {
          api("/api/overview").then(function (data) { last = data; draw(data); }).catch(function (err) {
            if (!last) errorBanner(cards, err);
          });
        },
        destroy: function () { window.clearInterval(timer); }
      };
    }
  };

  // ---------------------------------------------------------------- SMS --

  // A standalone 4-8 digit number, preferring one within 40 characters of a
  // word that usually introduces a code (research R10, FR-043).
  function findOtp(body) {
    var numbers = [];
    var re = /\b\d{4,8}\b/g;
    var m;
    while ((m = re.exec(body)) !== null) numbers.push({ value: m[0], at: m.index });
    if (!numbers.length) return null;
    var keywords = [];
    var kre = /otp|code|verification|password|pin/gi;
    while ((m = kre.exec(body)) !== null) keywords.push({ start: m.index, end: m.index + m[0].length });
    var best = null;
    numbers.forEach(function (n) {
      keywords.forEach(function (k) {
        var gap = n.at >= k.end ? n.at - k.end : k.start - (n.at + n.value.length);
        if (gap <= 40 && (best === null || gap < best.gap)) best = { value: n.value, gap: gap };
      });
    });
    if (best) return best.value;
    return numbers[0].value;
  }

  function copyCode(code, codeNode, button) {
    function selectText() {
      var range = document.createRange();
      range.selectNodeContents(codeNode);
      var selection = window.getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
      button.textContent = "Selected";
    }
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(code).then(function () {
        button.textContent = "Copied";
        window.setTimeout(function () { button.textContent = "Copy code"; }, 1500);
      }, selectText);
    } else {
      selectText();
    }
  }

  var FORWARD = {
    sent: ["ok", "Forwarded"],
    failed: ["bad", "Forward failed"],
    pending: ["info", "Forwarding"],
    skipped: ["stale", "Not forwarded"]
  };

  function smsCard(m) {
    var fwd = FORWARD[m.forwarding_status] || ["stale", m.forwarding_status];
    var code = findOtp(m.body);
    var head = el("div", { class: "sms-head" },
      el("span", { class: "from", text: m.sender }),
      el("span", { text: "→ " + m.module_id }),
      el("span", { class: "chip", text: TRANSPORT[m.transport] || m.transport }),
      el("span", { text: fmtTime(m.received_at) }),
      el("span", { class: "badge " + fwd[0], text: fwd[1] }));
    var body = el("div", { class: "sms-body", text: m.body });
    var parts = [head, body];
    if (code) {
      var codeNode = el("code", { class: "mono", text: code });
      var button = el("button", { class: "btn small", type: "button", text: "Copy code" });
      button.addEventListener("click", function () { copyCode(code, codeNode, button); });
      parts.push(el("div", { class: "otp" }, el("span", { class: "muted small", text: "Code" }), codeNode, button));
    }
    return el("article", { class: "panel sms" + (m.forwarding_status === "failed" ? " failed" : "") }, parts);
  }

  // A page can empty out from under the reader (retention, a narrower
  // filter). Step back to the last page that exists instead of leaving them on
  // a blank one with no way to go back. True when the caller should reload.
  function pastTheEnd(state, data) {
    if (data.items.length || state.page <= 1 || !data.total) return false;
    state.page = Math.max(1, Math.ceil(data.total / data.per_page));
    return true;
  }

  function pager(info, goto) {
    var pagesTotal = Math.max(1, Math.ceil(info.total / info.per_page));
    return el("div", { class: "pager" },
      el("span", { text: info.total + " total · page " + info.page + " of " + pagesTotal }),
      el("button", { class: "btn", type: "button", text: "Previous", disabled: info.page <= 1 ? true : null,
        onclick: function () { goto(info.page - 1); } }),
      el("button", { class: "btn", type: "button", text: "Next", disabled: info.page >= pagesTotal ? true : null,
        onclick: function () { goto(info.page + 1); } }));
  }

  function lineSelect(onchange) {
    var select = el("select", { "aria-label": "Line" }, el("option", { value: "", text: "All lines" }));
    select.addEventListener("change", onchange);
    api("/api/overview").then(function (data) {
      data.lines.forEach(function (l) { select.appendChild(el("option", { value: l.id, text: l.id })); });
    }).catch(function () { /* the filter just stays at "All lines" */ });
    return select;
  }

  function debounce(fn, ms) {
    var t = null;
    return function () {
      window.clearTimeout(t);
      t = window.setTimeout(fn, ms);
    };
  }

  pages.sms = {
    mount: function (container) {
      var state = { page: 1 };
      var results = el("div");
      var line = lineSelect(function () { state.page = 1; load(); });
      var search = el("input", { type: "search", placeholder: "Search sender or text", "aria-label": "Search" });
      search.addEventListener("input", debounce(function () { state.page = 1; load(); }, 300));

      container.appendChild(el("h1", { text: "SMS" }));
      container.appendChild(el("div", { class: "filters" }, line, search));
      container.appendChild(results);

      var seq = 0;
      function load() {
        var mine = ++seq;
        api("/api/sms" + query({ line: line.value, q: search.value, page: state.page })).then(function (data) {
          if (mine !== seq) return;
          if (pastTheEnd(state, data)) { load(); return; }
          clear(results);
          if (!data.items.length) {
            results.appendChild(el("div", { class: "empty", text: "No messages match." }));
            return;
          }
          var list = el("div", { class: "sms-list" });
          data.items.forEach(function (m) { list.appendChild(smsCard(m)); });
          results.appendChild(list);
          results.appendChild(pager(data, function (p) { state.page = p; load(); }));
        }).catch(function (err) { if (mine === seq) errorBanner(results, err); });
      }

      return {
        refresh: function () {
          // Leave the list alone while it is being read or copied from.
          var selecting = window.getSelection && !window.getSelection().isCollapsed;
          if (state.page === 1 && !selecting && document.activeElement !== search) load();
          else if (!results.firstChild) load();
        }
      };
    }
  };

  // -------------------------------------------------------------- calls --

  var STATUS_BADGE = { answered: "ok", missed: "warn", failed: "bad" };

  function tile(title, counts) {
    return el("div", { class: "panel tile" },
      el("div", { class: "k muted small", text: title }),
      el("div", { class: "big", text: String(counts.answered + counts.missed + counts.failed) }),
      el("div", { class: "split", text:
        counts.answered + " answered · " + counts.missed + " missed · " + counts.failed + " failed" }),
      el("div", { class: "split", text: "Average answered: " + (counts.avg_answered_seconds === null ? "—" : fmtDuration(counts.avg_answered_seconds)) }));
  }

  function directionLine(label, w) {
    return el("div", { class: "split small muted" },
      label + ": " + w.today.answered + "/" + w.today.missed + "/" + w.today.failed + " today · " +
      w.last_7d.answered + "/" + w.last_7d.missed + "/" + w.last_7d.failed + " in 7 days");
  }

  function qualityText(c) {
    if (c.loss_percent === null && c.jitter_ms === null && c.rtt_ms === null) return "—";
    var parts = [];
    if (c.loss_percent !== null) parts.push("loss " + fmtNum(c.loss_percent, 1, "%"));
    if (c.jitter_ms !== null) parts.push("jitter " + fmtNum(c.jitter_ms, 0, " ms"));
    if (c.rtt_ms !== null) parts.push("rtt " + fmtNum(c.rtt_ms, 0, " ms"));
    return parts.join(" · ");
  }

  function dayBoundary(value, endOfDay) {
    if (!value) return "";
    var d = new Date(value + (endOfDay ? "T23:59:59" : "T00:00:00"));
    return isNaN(d.getTime()) ? "" : d.toISOString();
  }

  function inProgress(lines) {
    var rows = [];
    lines.forEach(function (line) {
      if (line.active_calls && line.active_calls.length) {
        line.active_calls.forEach(function (c) { rows.push({ line: line, call: c }); });
      } else if (line.active_call_count > 0) {
        rows.push({ line: line, call: null });
      }
    });
    var box = el("div");
    if (!rows.length) {
      box.appendChild(el("div", { class: "muted small", text: "No calls in progress." }));
      return box;
    }
    var tbody = el("tbody");
    rows.forEach(function (r) {
      var c = r.call;
      tbody.appendChild(el("tr", null,
        el("td", { text: r.line.id }),
        el("td", { text: c ? c.direction : "—" }),
        el("td", { text: c ? c.remote : "details unavailable" }),
        el("td", { text: c ? fmtDuration(Date.now() / 1000 - c.started_at_unix) : "—" }),
        el("td", { text: c ? qualityText(c) : "unavailable" })));
    });
    box.appendChild(el("div", { class: "table-wrap" }, el("table", null,
      el("thead", null, el("tr", null, ["Line", "Direction", "Remote", "Running", "Quality"].map(function (h) { return el("th", { text: h }); }))),
      tbody)));
    return box;
  }

  pages.calls = {
    mount: function (container) {
      var state = { page: 1 };
      var summary = el("div");
      var live = el("div");
      var results = el("div");
      var line = lineSelect(function () { state.page = 1; loadLog(); });
      function select(label, options, onchange) {
        var node = el("select", { "aria-label": label }, options.map(function (o) { return el("option", { value: o[0], text: o[1] }); }));
        node.addEventListener("change", onchange);
        return node;
      }
      var direction = select("Direction", [["", "Any direction"], ["inbound", "Inbound"], ["outbound", "Outbound"], ["unknown", "Before direction was recorded"]], function () { state.page = 1; loadLog(); });
      var transport = select("Transport", [["", "Any transport"], ["cs", "GSM"], ["vowifi", "VoWiFi"], ["volte", "VoLTE"]], function () { state.page = 1; loadLog(); });
      var status = select("Status", [["", "Any status"], ["answered", "Answered"], ["missed", "Missed"], ["failed", "Failed"]], function () { state.page = 1; loadLog(); });
      var from = el("input", { type: "date", "aria-label": "From" });
      var to = el("input", { type: "date", "aria-label": "To" });
      from.addEventListener("change", function () { state.page = 1; loadLog(); });
      to.addEventListener("change", function () { state.page = 1; loadLog(); });

      container.appendChild(el("h1", { text: "Calls" }));
      container.appendChild(summary);
      container.appendChild(el("h2", { text: "In progress" }));
      container.appendChild(live);
      container.appendChild(el("h2", { text: "History" }));
      container.appendChild(el("div", { class: "filters" }, line, direction, transport, status, from, to));
      container.appendChild(results);

      function loadSummary() {
        api("/api/calls/summary").then(function (s) {
          clear(summary);
          // "Today" is counted from the bridge host's midnight, which need not
          // be this browser's, so say whose.
          var offset = s.host_utc_offset && s.host_utc_offset !== "+00:00" ? "UTC" + s.host_utc_offset : "UTC";
          var today = tile("Today (bridge host time, " + offset + ")", s.today);
          today.appendChild(directionLine("In", s.by_direction.inbound));
          today.appendChild(directionLine("Out", s.by_direction.outbound));
          var u = s.by_direction.unknown;
          if (u && (u.last_7d.answered + u.last_7d.missed + u.last_7d.failed) > 0) {
            today.appendChild(directionLine("Older", u));
          }
          summary.appendChild(el("div", { class: "tiles" }, today, tile("Last 7 days", s.last_7d)));
        }).catch(function (err) { errorBanner(summary, err); });
      }

      function loadLive() {
        api("/api/overview").then(function (data) { clear(live); live.appendChild(inProgress(data.lines)); })
          .catch(function () { /* the section keeps its last content */ });
      }

      var seq = 0;
      function loadLog() {
        var mine = ++seq;
        api("/api/calls" + query({
          line: line.value, direction: direction.value, transport: transport.value, status: status.value,
          from: dayBoundary(from.value, false), to: dayBoundary(to.value, true), page: state.page
        })).then(function (data) {
          if (mine !== seq) return;
          if (pastTheEnd(state, data)) { loadLog(); return; }
          clear(results);
          if (!data.items.length) {
            results.appendChild(el("div", { class: "empty", text: "No calls match." }));
            return;
          }
          var tbody = el("tbody");
          data.items.forEach(function (c) {
            tbody.appendChild(el("tr", null,
              el("td", { text: fmtTime(c.started_at) }),
              el("td", { text: c.module_id || "—" }),
              el("td", { text: c.direction === "outbound" ? "Out" : c.direction === "inbound" ? "In" : "—", title: c.direction === "unknown" ? "Recorded before call direction was tracked" : null }),
              el("td", { class: "mono", text: c.remote || "—" }),
              el("td", { text: c.status === "answered" ? fmtDuration(c.duration_seconds) : "—" }),
              el("td", null, el("span", { class: "badge " + (STATUS_BADGE[c.status] || "stale"), text: c.status })),
              el("td", { text: TRANSPORT[c.transport] || c.transport }),
              el("td", { class: "small muted", text: qualityText(c) })));
          });
          results.appendChild(el("div", { class: "table-wrap" }, el("table", null,
            el("thead", null, el("tr", null, ["Started", "Line", "Dir", "Remote", "Length", "Status", "Via", "Quality"].map(function (h) { return el("th", { text: h }); }))),
            tbody)));
          results.appendChild(pager(data, function (p) { state.page = p; loadLog(); }));
        }).catch(function (err) { if (mine === seq) errorBanner(results, err); });
      }

      return {
        refresh: function () {
          loadSummary();
          loadLive();
          if (state.page === 1 || !results.firstChild) loadLog();
        }
      };
    }
  };

  // ------------------------------------------------------------- events --

  var EVENT_KINDS = [
    ["", "All events"],
    ["alert_raised", "Alerts raised"],
    ["alert_resolved", "Alerts resolved"],
    ["tunnel_recovered", "Tunnel recoveries"],
    ["dead_peer_loss", "Dead-peer losses"],
    ["uplink_hold_started", "Uplink holds started"],
    ["uplink_hold_ended", "Uplink holds ended"],
    ["module_initialized", "Modules initialised"],
    ["module_init_failed", "Module init failures"],
    ["scheduled_restart", "Scheduled restarts"]
  ];

  function eventTitle(kind) {
    var parts = kind.split(":");
    var what = parts[1] ? parts[1].replace(/_/g, " ") : "";
    if (parts[0] === "alert_raised") return "Alert: " + what;
    if (parts[0] === "alert_resolved") return "Resolved: " + what;
    var text = parts[0].replace(/_/g, " ");
    return text.charAt(0).toUpperCase() + text.slice(1);
  }

  function eventItem(e) {
    var head = el("div", { class: "ev-head" },
      el("span", { class: "ev-kind", text: eventTitle(e.kind) }),
      e.module_id ? el("span", { class: "chip", text: e.module_id }) : null,
      el("span", { class: "muted small", text: fmtTime(e.at) }),
      e.duration_seconds !== null && e.duration_seconds !== undefined
        ? el("span", { class: "badge info", text: (e.kind.indexOf("alert_resolved") === 0 ? "lasted " : "") + fmtDuration(e.duration_seconds) })
        : null);
    return el("li", null, el("span", { class: "dot " + e.severity, title: e.severity }),
      el("div", null, head, e.detail ? el("div", { class: "ev-detail", text: e.detail }) : null));
  }

  function healthPanel(h) {
    function stat(label, value, bad) {
      return el("div", { class: "stat" }, el("div", { class: "k", text: label }),
        el("div", { class: "v" }, el("span", { class: bad ? "badge bad" : "", text: String(value) })));
    }
    return el("div", { class: "stats" },
      stat("Modules active", h.cs_enabled ? h.modules_active : "n/a", false),
      stat("Modules failed", h.cs_enabled ? h.modules_failed : "n/a", h.cs_enabled && h.modules_failed > 0),
      stat("Store queue", h.store_queue_depth, h.store_queue_depth > 100),
      stat("Dropped reports", h.observability_events_dropped, h.observability_events_dropped > 0));
  }

  pages.events = {
    mount: function (container) {
      var state = { page: 1 };
      var health = el("div");
      var results = el("div");
      var line = lineSelect(function () { state.page = 1; load(); });
      var kind = el("select", { "aria-label": "Kind" }, EVENT_KINDS.map(function (k) { return el("option", { value: k[0], text: k[1] }); }));
      kind.addEventListener("change", function () { state.page = 1; load(); });

      container.appendChild(el("h1", { text: "Events" }));
      container.appendChild(health);
      container.appendChild(el("div", { class: "filters" }, line, kind));
      container.appendChild(results);

      function loadHealth() {
        api("/api/health").then(function (h) { clear(health); health.appendChild(healthPanel(h)); })
          .catch(function () { /* the panel keeps its last content */ });
      }

      var seq = 0;
      function load() {
        var mine = ++seq;
        api("/api/events" + query({ line: line.value, kind: kind.value, page: state.page })).then(function (data) {
          if (mine !== seq) return;
          if (pastTheEnd(state, data)) { load(); return; }
          clear(results);
          if (!data.items.length) {
            results.appendChild(el("div", { class: "empty", text: "Nothing recorded yet." }));
            return;
          }
          var list = el("ul", { class: "timeline panel" });
          data.items.forEach(function (e) { list.appendChild(eventItem(e)); });
          results.appendChild(list);
          results.appendChild(pager(data, function (p) { state.page = p; load(); }));
        }).catch(function (err) { if (mine === seq) errorBanner(results, err); });
      }

      return {
        refresh: function () {
          loadHealth();
          if (state.page === 1 || !results.firstChild) load();
        }
      };
    }
  };

  // ------------------------------------------------------------- config --

  function isPlainObject(v) { return v !== null && typeof v === "object" && !Array.isArray(v); }

  function scalarText(v) {
    if (Array.isArray(v)) return "[" + v.map(scalarText).join(", ") + "]";
    return typeof v === "string" ? v : JSON.stringify(v);
  }

  // One table: its scalar keys as rows, its sub-tables as nested sections.
  function configTable(table, open) {
    var kv = el("div", { class: "kv" });
    var nested = [];
    Object.keys(table).forEach(function (key) {
      var value = table[key];
      var tables = Array.isArray(value) && value.length && value.every(isPlainObject);
      if (isPlainObject(value) || tables) {
        nested.push([key, value, tables]);
      } else {
        var redacted = value === "[REDACTED]";
        kv.appendChild(el("div", { class: "row" },
          el("span", { class: "k", text: key }),
          el("span", { class: "val" + (redacted ? " redacted" : ""), text: scalarText(value) })));
      }
    });
    var box = el("div");
    if (kv.childNodes.length) box.appendChild(kv);
    nested.forEach(function (n) {
      if (n[2]) {
        n[1].forEach(function (entry, i) { box.appendChild(configSection(n[0] + " #" + (i + 1), entry, false)); });
      } else {
        box.appendChild(configSection(n[0], n[1], false));
      }
    });
    return box;
  }

  function configSection(name, table, open) {
    return el("details", { class: "cfg", open: open ? true : null },
      el("summary", { text: name }),
      el("div", { class: "kv" }, configTable(table)));
  }

  pages.config = {
    mount: function (container) {
      var body = el("div");
      container.appendChild(el("h1", { text: "Configuration" }));
      container.appendChild(body);
      api("/api/config").then(function (data) {
        body.appendChild(el("div", { class: "banner info", text:
          "As loaded at startup; later edits to the file are not shown. Loaded " + fmtTime(data.loaded_at) + " from " + data.source + "." }));
        var doc = data.document;
        Object.keys(doc).forEach(function (name) {
          if (isPlainObject(doc[name])) body.appendChild(configSection(name, doc[name], false));
        });
        var loose = {};
        Object.keys(doc).forEach(function (name) { if (!isPlainObject(doc[name])) loose[name] = doc[name]; });
        if (Object.keys(loose).length) body.appendChild(configSection("(top level)", loose, true));
      }).catch(function (err) { errorBanner(body, err); });
      return null;
    }
  };
  // PAGES-END

  // -------------------------------------------------------------- start --

  if (window.location.pathname === "/login") renderLogin();
  else renderApp();
})();
