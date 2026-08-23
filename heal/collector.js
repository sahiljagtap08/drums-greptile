// The collector is how Drums notices — and now also how you watch Drums.
// It serves the capture snippet to target apps, receives incident reports
// from real user browsers, and hosts the incident console at /.
const http = require("http");
const fs = require("fs");
const path = require("path");

const ARTIFACT_FILES = new Set([
  "before.png", "after.png", "patch.diff", "incident.json",
  "result.json", "codex-output.txt", "reproduction.json", "verification.json",
]);

function readJson(p) {
  try { return JSON.parse(fs.readFileSync(p, "utf8")); } catch { return null; }
}

function listIncidents(repoRoot) {
  const base = path.join(repoRoot, ".drums", "incidents");
  let ids = [];
  try { ids = fs.readdirSync(base).filter((d) => /^[\w-]+$/.test(d)).sort().reverse(); } catch {}
  return ids.slice(0, 50).map((id) => {
    const dir = path.join(base, id);
    const incident = readJson(path.join(dir, "incident.json")) || {};
    const result = readJson(path.join(dir, "result.json"));
    const status = readJson(path.join(dir, "status.json"));
    const state = result ? result.state : (status ? status.state : "RUNNING");
    let diff = null;
    try {
      const d = fs.readFileSync(path.join(dir, "patch.diff"), "utf8");
      if (d.trim()) diff = d.slice(0, 6000);
    } catch {}
    return {
      id,
      state: result ? state : "RUNNING",
      incident: { failure: incident.failure, url: incident.url, trace: incident.trace },
      result,
      memory: (status && status.memory) || (result && result.memory) || [],
      diff,
      hasShots: fs.existsSync(path.join(dir, "before.png")) && fs.existsSync(path.join(dir, "after.png")),
    };
  });
}

function startCollector(port, onIncident, opts = {}) {
  const repoRoot = opts.repoRoot;
  const server = http.createServer((req, res) => {
    res.setHeader("access-control-allow-origin", "*");
    res.setHeader("access-control-allow-headers", "content-type");
    if (req.method === "OPTIONS") { res.writeHead(204); res.end(); return; }
    if (req.method === "GET" && req.url === "/snippet.js") {
      res.writeHead(200, { "content-type": "application/javascript" });
      res.end(fs.readFileSync(path.join(__dirname, "snippet.js")));
      return;
    }
    if (req.method === "POST" && req.url === "/incident") {
      let body = "";
      req.on("data", (c) => (body += c));
      req.on("end", () => {
        res.writeHead(202, { "content-type": "application/json" });
        res.end('{"received":true}');
        try { onIncident(JSON.parse(body)); }
        catch (e) { console.error("bad incident payload:", e.message); }
      });
      return;
    }
    if (repoRoot && req.method === "GET" && (req.url === "/" || req.url === "/index.html")) {
      res.writeHead(200, { "content-type": "text/html" });
      res.end(fs.readFileSync(path.join(__dirname, "dashboard.html")));
      return;
    }
    if (repoRoot && req.method === "GET" && req.url === "/api/incidents") {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify(listIncidents(repoRoot)));
      return;
    }
    const art = repoRoot && req.method === "GET" && req.url.match(/^\/artifact\/([\w-]+)\/([\w.-]+)$/);
    if (art && ARTIFACT_FILES.has(art[2])) {
      const p = path.join(repoRoot, ".drums", "incidents", art[1], art[2]);
      if (fs.existsSync(p)) {
        const type = art[2].endsWith(".png") ? "image/png"
          : art[2].endsWith(".json") ? "application/json" : "text/plain";
        res.writeHead(200, { "content-type": type });
        res.end(fs.readFileSync(p));
        return;
      }
    }
    res.writeHead(404); res.end();
  });
  return new Promise((resolve) => server.listen(port, () => resolve(server)));
}

module.exports = { startCollector };
