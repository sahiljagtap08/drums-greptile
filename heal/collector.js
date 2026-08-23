// The collector is how Drums notices. It serves the capture snippet and
// receives incident reports from real user browsers.
const http = require("http");
const fs = require("fs");
const path = require("path");

function startCollector(port, onIncident) {
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
    res.writeHead(404); res.end();
  });
  return new Promise((resolve) => server.listen(port, () => resolve(server)));
}

module.exports = { startCollector };
