// Publish the incident console as a static site: the same dashboard, reading
// data.json instead of the live API. Run after a demo session, deploy the
// site/ directory anywhere static.
const fs = require("fs");
const path = require("path");
const { listIncidents } = require("./collector");

const repoRoot = path.join(__dirname, "..");
const out = path.join(repoRoot, "site");
// keep the Vercel project link across re-exports
let vercelLink = null;
try { vercelLink = fs.readFileSync(path.join(out, ".vercel", "project.json")); } catch {}
fs.rmSync(out, { recursive: true, force: true });
fs.mkdirSync(out, { recursive: true });
if (vercelLink) {
  fs.mkdirSync(path.join(out, ".vercel"), { recursive: true });
  fs.writeFileSync(path.join(out, ".vercel", "project.json"), vercelLink);
}

const incidents = listIncidents(repoRoot).map((it) => ({
  ...it,
  // A run that died mid-flight has no verdict; label it honestly.
  state: it.state === "RUNNING" ? "INCONCLUSIVE" : it.state,
}));
fs.writeFileSync(path.join(out, "data.json"), JSON.stringify(incidents));
fs.copyFileSync(path.join(__dirname, "dashboard.html"), path.join(out, "index.html"));

let shots = 0;
for (const it of incidents) {
  const src = path.join(repoRoot, ".drums", "incidents", it.id);
  const dst = path.join(out, "artifact", it.id);
  for (const f of ["before.png", "after.png"]) {
    const p = path.join(src, f);
    if (fs.existsSync(p)) {
      fs.mkdirSync(dst, { recursive: true });
      fs.copyFileSync(p, path.join(dst, f));
      shots++;
    }
  }
}
console.log(`site/ exported: ${incidents.length} incidents, ${shots} screenshots`);
