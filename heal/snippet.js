// Drums capture snippet. One script tag in the target app. Records what the
// user actually did, and reports the moment something fails for them.
(function () {
  var COLLECTOR = window.__DRUMS_COLLECTOR__;
  if (!COLLECTOR) return;
  var trace = [];
  var consoleErrors = [];
  var reported = false;

  function selectorFor(el) {
    if (!el || !el.tagName) return null;
    if (el.id) return "#" + el.id;
    if (el.getAttribute && el.getAttribute("name"))
      return el.tagName.toLowerCase() + '[name="' + el.getAttribute("name") + '"]';
    var sibs = el.parentNode ? Array.prototype.filter.call(el.parentNode.children, function (c) { return c.tagName === el.tagName; }) : [el];
    var idx = sibs.indexOf(el);
    return el.tagName.toLowerCase() + (sibs.length > 1 ? ":nth-of-type(" + (idx + 1) + ")" : "");
  }

  function redact(el, value) {
    var name = ((el.getAttribute && (el.getAttribute("name") || el.id)) || "").toLowerCase();
    if ((el.type || "").toLowerCase() === "password") return "•••";
    if (/password|token|secret|card|cvv|ssn/.test(name)) return "•••";
    return value;
  }

  function push(step) {
    step.at = Date.now();
    trace.push(step);
    if (trace.length > 50) trace.shift();
  }

  // Behavioral friction: a click on an interactive element that produces no
  // network request, no DOM change, and no navigation is a dead click. Three
  // of those on the same element within seconds is a user fighting a silently
  // broken feature — no error will ever fire, but the product is failing them.
  var mutations = 0;
  try {
    new MutationObserver(function (ms) { mutations += ms.length; })
      .observe(document.documentElement, { subtree: true, childList: true, attributes: true, characterData: true });
  } catch (e) {}
  var requestCount = 0;
  var deadClicks = {};

  document.addEventListener("click", function (e) {
    var el = e.target.closest ? (e.target.closest("button,a,[role=button],input[type=submit]") || e.target) : e.target;
    var sel = selectorFor(el);
    var text = (el.textContent || "").trim().slice(0, 40);
    push({ kind: "click", selector: sel, text: text });
    var interactive = el.matches && el.matches("button,a,[role=button],input[type=submit]");
    if (!interactive) return;
    var m0 = mutations, r0 = requestCount, href0 = location.href;
    setTimeout(function () {
      if (mutations !== m0 || requestCount !== r0 || location.href !== href0) return; // the click did something
      var now = Date.now();
      var arr = (deadClicks[sel] || []).filter(function (t) { return now - t < 4000; });
      arr.push(now);
      deadClicks[sel] = arr;
      if (arr.length >= 3) {
        report({
          kind: "friction", selector: sel, text: text, clicks: arr.length,
          note: "repeated clicks produced no network request, no DOM change, no navigation, and no error",
        });
      }
    }, 700);
  }, true);

  document.addEventListener("change", function (e) {
    var el = e.target;
    if (!el || !("value" in el)) return;
    push({ kind: "fill", selector: selectorFor(el), value: redact(el, String(el.value).slice(0, 200)) });
  }, true);

  var origError = console.error;
  console.error = function () {
    consoleErrors.push(Array.prototype.map.call(arguments, String).join(" ").slice(0, 500));
    if (consoleErrors.length > 20) consoleErrors.shift();
    return origError.apply(console, arguments);
  };

  function report(failure) {
    if (reported) return; // one incident per page visit is enough
    reported = true;
    var incident = {
      url: location.pathname + location.search,
      userAgent: navigator.userAgent,
      at: new Date().toISOString(),
      trace: trace.slice(),
      consoleErrors: consoleErrors.slice(),
      failure: failure,
    };
    try {
      fetch(COLLECTOR + "/incident", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(incident),
      });
    } catch (e) {}
  }

  var origFetch = window.fetch;
  window.fetch = function (input, init) {
    var method = ((init && init.method) || "GET").toUpperCase();
    var url = typeof input === "string" ? input : (input && input.url) || "";
    var path;
    try { path = new URL(url, location.origin).pathname; } catch (e) { path = url; }
    var bodyPreview = init && typeof init.body === "string" ? init.body.slice(0, 500) : null;
    requestCount++;
    push({ kind: "request", method: method, path: path, body: bodyPreview });
    return origFetch.apply(window, arguments).then(function (res) {
      if (res.status >= 500) {
        res.clone().text().then(function (text) {
          report({ kind: "http", method: method, path: path, status: res.status, responsePreview: text.slice(0, 500), requestBody: bodyPreview });
        }).catch(function () {
          report({ kind: "http", method: method, path: path, status: res.status, requestBody: bodyPreview });
        });
      }
      return res;
    });
  };

  window.addEventListener("error", function (e) {
    report({ kind: "jserror", message: String(e.message).slice(0, 500), source: e.filename, line: e.lineno });
  });
  window.addEventListener("unhandledrejection", function (e) {
    report({ kind: "jserror", message: String(e.reason && e.reason.message || e.reason).slice(0, 500) });
  });
})();
