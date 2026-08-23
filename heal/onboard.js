#!/usr/bin/env node
// One-command onboarding for a stranger's repo:
//   node heal/onboard.js /path/to/their-repo
// Writes drums.json (guessed from package.json), injects the capture snippet
// into their HTML entry if one is found, and tells you exactly what to check.
const fs = require("fs");
const path = require("path");
const { execFileSync } = require("child_process");

const target = path.resolve(process.argv[2] || "");
if (!target || !fs.existsSync(target)) {
  console.error("usage: node heal/onboard.js /path/to/their-repo");
  process.exit(1);
}

const notes = [];
const ok = (m) => console.log("✓ " + m);
const warn = (m) => { console.log("⚠ " + m); notes.push(m); };

// git repo?
try { execFileSync("git", ["-C", target, "rev-parse", "--show-toplevel"], { stdio: "pipe" }); ok("git repo"); }
catch { console.error("✗ not a git repo — isolation needs git worktrees. git init && commit first."); process.exit(1); }

// guess commands from package.json
let pkg = null;
try { pkg = JSON.parse(fs.readFileSync(path.join(target, "package.json"), "utf8")); } catch {}
const scripts = (pkg && pkg.scripts) || {};
const start = scripts.dev ? "npm run dev" : scripts.start ? "npm start" : pkg && pkg.main ? `node ${pkg.main}` : null;
if (!start) { console.error("✗ no obvious start command (no dev/start script). Add one, or edit drums.json by hand."); process.exit(1); }
const test = scripts.test && !/no test specified/.test(scripts.test) ? "npm test" : "";

const cfgPath = path.join(target, "drums.json");
if (fs.existsSync(cfgPath)) {
  ok("drums.json already present — left untouched");
} else {
  fs.writeFileSync(cfgPath, JSON.stringify({ install: pkg ? "npm install" : "", start, health: "/", app: "/", test }, null, 2) + "\n");
  ok(`drums.json written (start: "${start}"${test ? `, test: "${test}"` : ""})`);
}

// PORT discipline — the requirement that kills candidates
let respectsPort = false;
try {
  respectsPort = !!execFileSync("grep", ["-rl", "--include=*.js", "--include=*.ts", "--include=*.mjs", "process.env.PORT", target, "--exclude-dir=node_modules"], { encoding: "utf8" }).trim();
} catch {}
if (respectsPort || scripts.dev) ok("app looks PORT-aware (or framework dev server, which honors PORT)");
else warn("could not find process.env.PORT — if the app hardcodes its port, isolated candidates cannot boot. Fix that first.");

// snippet injection into an HTML entry
const SNIPPET = [
  '<script>window.__DRUMS_COLLECTOR__ = "http://localhost:4600"</script>',
  '<script src="http://localhost:4600/snippet.js"></script>',
].join("\n");
const candidates = [];
for (const dir of ["", "public", "src", "static", "views"]) {
  const d = path.join(target, dir);
  try {
    for (const f of fs.readdirSync(d)) if (f.endsWith(".html")) candidates.push(path.join(d, f));
  } catch {}
}
const entry = candidates.find((f) => fs.readFileSync(f, "utf8").includes("</head>"));
if (entry) {
  const html = fs.readFileSync(entry, "utf8");
  if (html.includes("__DRUMS_COLLECTOR__")) ok("capture snippet already present in " + path.relative(target, entry));
  else {
    fs.writeFileSync(entry, html.replace("</head>", SNIPPET + "\n</head>"));
    ok("capture snippet injected into " + path.relative(target, entry));
  }
} else {
  warn("no .html entry with a </head> found (JSX/template app?) — add these two lines to the main layout yourself:\n" + SNIPPET);
}

console.log("");
console.log("Next:");
console.log(`  node heal/cli.js watch ${target}`);
console.log("  then use the app at http://localhost:3000 like a real user,");
console.log("  and watch the console at http://localhost:4600");
if (notes.length) console.log("\nCheck the ⚠ items above before promising anything.");
