#!/usr/bin/env node
// drums-heal: watch a running app for real user failures and close the loop.
//
//   node heal/cli.js watch <targetDir>            watch + auto-repair on incident
//   node heal/cli.js repair <targetDir> <incident.json>   run the loop on saved evidence
const fs = require("fs");
const path = require("path");
const { spawn } = require("child_process");
const { startCollector } = require("./collector");
const { runPipeline } = require("./pipeline");
const ui = require("./ui");

const COLLECTOR_PORT = 4600;

function loadCfg(targetDir) {
  const p = path.join(targetDir, "drums.json");
  if (!fs.existsSync(p)) {
    console.error(`No drums.json in ${targetDir}. Need: {"install","start","health","app","test"}`);
    process.exit(1);
  }
  return JSON.parse(fs.readFileSync(p, "utf8"));
}

async function main() {
  const [cmd, target, incidentFile] = process.argv.slice(2);
  if (!cmd || !target) {
    console.error("usage: cli.js watch <dir> | repair <dir> <incident.json>");
    process.exit(1);
  }
  const targetDir = path.resolve(target);
  const cfg = loadCfg(targetDir);

  if (cmd === "repair") {
    const incident = JSON.parse(fs.readFileSync(incidentFile, "utf8"));
    const result = await runPipeline(targetDir, incident, cfg);
    process.exit(result.state === "VERIFIED" ? 0 : 1);
  }

  if (cmd !== "watch") { console.error("unknown command " + cmd); process.exit(1); }

  // production instance, instrumented with the capture snippet
  const prodPort = cfg.port || 3000;
  const prod = spawn("sh", ["-c", cfg.start], {
    cwd: targetDir,
    env: { ...process.env, PORT: String(prodPort), DRUMS_COLLECTOR: `http://localhost:${COLLECTOR_PORT}` },
    stdio: "inherit",
    detached: true,
  });
  process.on("exit", () => { try { process.kill(-prod.pid, "SIGKILL"); } catch {} });
  process.on("SIGINT", () => process.exit(0));

  const { execFileSync } = require("child_process");
  const repoRoot = execFileSync("git", ["-C", targetDir, "rev-parse", "--show-toplevel"], { encoding: "utf8" }).trim();

  let busy = false;
  await startCollector(COLLECTOR_PORT, async (incident) => {
    if (busy) { console.log("(incident received while a repair is running — ignored)"); return; }
    busy = true;
    try {
      console.log("\n" + ui.rule("drums · user failure observed"));
      const result = await runPipeline(targetDir, incident, cfg);
      const tint = result.state === "VERIFIED" ? ui.c.green : ui.c.red;
      console.log(ui.rule("drums · " + result.state).replace(result.state, tint(result.state)) + "\n");
    } catch (e) {
      console.error("pipeline error:", e.message);
    } finally {
      busy = false;
    }
  }, { repoRoot });

  const lines = [
    { plain: "drums — the product that improves itself", styled: ui.head("drums") + ui.dim(" — the product that improves itself") },
    { plain: `app      http://localhost:${prodPort}${cfg.app || "/"}`, styled: ui.dim("app      ") + `http://localhost:${prodPort}${cfg.app || "/"}` },
    { plain: `console  http://localhost:${COLLECTOR_PORT}`, styled: ui.dim("console  ") + ui.c.cyan(`http://localhost:${COLLECTOR_PORT}`) },
  ];
  console.log(ui.banner(lines));
  console.log(ui.dim("Use the app like a real user. If something fails for you, drums will notice."));
}

main();
