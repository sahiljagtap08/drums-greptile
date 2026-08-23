// Tiny signup app. Zero dependencies. One real bug a real user can hit.
const http = require("http");
const fs = require("fs");
const path = require("path");

const PORT = process.env.PORT || 3000;
const signups = [];

const server = http.createServer((req, res) => {
  if (req.method === "GET" && (req.url === "/" || req.url === "/index.html")) {
    const html = fs
      .readFileSync(path.join(__dirname, "index.html"), "utf8")
      .replace("__DRUMS_COLLECTOR__", process.env.DRUMS_COLLECTOR || "");
    res.writeHead(200, { "content-type": "text/html" });
    res.end(html);
    return;
  }
  if (req.method === "GET" && req.url === "/api/health") {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ ok: true }));
    return;
  }
  if (req.method === "POST" && req.url === "/api/signup") {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      try {
        const { email } = JSON.parse(body);
        // BUG: this regex has no "+" (or uppercase) in the local part, so a
        // plus-addressed email like sahil+test@gmail.com makes match() return
        // null and reading [2] throws — the user gets a 500.
        const domain = email.match(/^([a-z0-9.]+)@([a-z0-9.]+)$/)[2];
        signups.push({ email, domain, at: Date.now() });
        res.writeHead(200, { "content-type": "application/json" });
        res.end(JSON.stringify({ ok: true, domain }));
      } catch (err) {
        res.writeHead(500, { "content-type": "application/json" });
        res.end(JSON.stringify({ error: "internal error", detail: String(err && err.message) }));
      }
    });
    return;
  }
  res.writeHead(404);
  res.end("not found");
});

server.listen(PORT, () => console.log(`fixture app on http://localhost:${PORT}`));
