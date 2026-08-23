// The pipeline: OBSERVED -> REPRODUCING -> REPRODUCED -> REPAIRING ->
// CANDIDATE_READY -> VERIFYING -> VERIFIED | FAILED | INCONCLUSIVE |
// REGRESSION_FOUND.
//
// The hard invariant, enforced in verdict(): VERIFIED requires
//   1. the original failure reproduced against HEAD,
//   2. a non-empty code change,
//   3. the SAME replay passing against the changed, rebooted app,
//   4. guardrails passing.
// Codex generates. Drums verifies. Codex saying "fixed" has zero authority.
const { spawn, execFileSync } = require("child_process");
const net = require("net");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { replayIncident } = require("./replay");

const say = (m) => console.log(m);

function freePort() {
  return new Promise((resolve) => {
    const s = net.createServer();
    s.listen(0, () => { const p = s.address().port; s.close(() => resolve(p)); });
  });
}

function git(dir, ...args) {
  return execFileSync("git", ["-C", dir, ...args], { encoding: "utf8" });
}

function bootApp(dir, startCmd, port, extraEnv = {}) {
  const child = spawn("sh", ["-c", startCmd], {
    cwd: dir,
    env: { ...process.env, PORT: String(port), ...extraEnv },
    stdio: ["ignore", "pipe", "pipe"],
    detached: true,
  });
  let out = "";
  child.stdout.on("data", (d) => (out += d));
  child.stderr.on("data", (d) => (out += d));
  child.tail = () => out.slice(-800);
  return child;
}

function killApp(child) {
  if (!child || child.killed) return;
  try { process.kill(-child.pid, "SIGKILL"); } catch {}
}

async function waitHealthy(base, healthPath, timeoutMs = 45000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const r = await fetch(base + healthPath, { signal: AbortSignal.timeout(2000) });
      if (r.status === 200) return true;
    } catch {}
    await new Promise((r) => setTimeout(r, 300));
  }
  return false;
}

function renderTrace(incident) {
  return (incident.trace || [])
    .map((s) => {
      if (s.kind === "fill") return `  fill ${s.selector} = ${JSON.stringify(s.value)}`;
      if (s.kind === "click") return `  click ${s.selector}${s.text ? ` ("${s.text}")` : ""}`;
      if (s.kind === "request") return `  -> ${s.method} ${s.path}${s.body ? ` body ${s.body}` : ""}`;
      return null;
    })
    .filter(Boolean)
    .join("\n");
}

function renderFailure(f) {
  if (!f) return "unknown failure";
  if (f.kind === "http")
    return `${f.method} ${f.path} returned ${f.status}. Response preview: ${f.responsePreview || "n/a"}`;
  return `Uncaught JS error: ${f.message}`;
}

function codexRepair(projectDir, prompt, timeoutMs = 600000) {
  return new Promise((resolve) => {
    const child = spawn(
      "codex",
      ["exec", "-C", projectDir, "-s", "workspace-write", "--skip-git-repo-check", "--color", "never", prompt],
      { stdio: ["ignore", "pipe", "pipe"], env: process.env }
    );
    let out = "";
    child.stdout.on("data", (d) => { out += d; process.stdout.write(d.toString().split("\n").map((l) => "    codex | " + l).join("\n").slice(0, 2000) + "\n"); });
    child.stderr.on("data", (d) => (out += d));
    const timer = setTimeout(() => { try { child.kill("SIGKILL"); } catch {} resolve({ ok: false, out, timedOut: true }); }, timeoutMs);
    child.on("exit", (code) => { clearTimeout(timer); resolve({ ok: code === 0, out, code }); });
  });
}

async function runPipeline(targetDir, incident, cfg) {
  const id = new Date().toISOString().replace(/[:.]/g, "-").slice(0, 19) + "-" + Math.random().toString(36).slice(2, 6);
  const repoRoot = git(targetDir, "rev-parse", "--show-toplevel").trim();
  const rel = path.relative(repoRoot, path.resolve(targetDir));
  const artifacts = path.join(repoRoot, ".drums", "incidents", id);
  fs.mkdirSync(artifacts, { recursive: true });
  fs.writeFileSync(path.join(artifacts, "incident.json"), JSON.stringify(incident, null, 2));

  const result = { id, state: "OBSERVED", reproduced: false, diffNonEmpty: false, replayPassed: false, guardrailsPassed: false, artifacts };
  const verdict = () => {
    if (result.reproduced && result.diffNonEmpty && result.replayPassed && result.guardrailsPassed) {
      result.state = "VERIFIED";
    } else if (result.state !== "REGRESSION_FOUND" && result.state !== "INCONCLUSIVE") {
      result.state = "FAILED";
    }
    return result;
  };

  say("");
  say("Observed failure");
  say("  " + renderFailure(incident.failure));
  say("");
  say("Captured");
  say(renderTrace(incident) || "  (no interaction trace)");
  say("");

  // --- isolated workspace at HEAD ---
  const wtRoot = fs.mkdtempSync(path.join(os.tmpdir(), "drums-wt-"));
  git(repoRoot, "worktree", "add", "--detach", wtRoot, "HEAD");
  const projectDir = rel ? path.join(wtRoot, rel) : wtRoot;
  result.worktree = projectDir;

  let app = null;
  const cleanup = () => killApp(app);

  try {
    // --- reproduce against HEAD ---
    result.state = "REPRODUCING";
    say("Reproducing against HEAD (isolated worktree)...");
    if (cfg.install) execFileSync("sh", ["-c", cfg.install], { cwd: projectDir, stdio: "ignore" });
    const portA = await freePort();
    app = bootApp(projectDir, cfg.start, portA);
    if (!(await waitHealthy(`http://localhost:${portA}`, cfg.health))) {
      say("✗ HEAD app failed to boot: " + app.tail());
      result.state = "INCONCLUSIVE";
      return verdict();
    }
    const repro = await replayIncident(incident, `http://localhost:${portA}`);
    fs.writeFileSync(path.join(artifacts, "reproduction.json"), JSON.stringify(repro, null, 2));
    killApp(app); app = null;
    if (!repro.completed || !repro.originalFailureObserved) {
      say("✗ could not reproduce the user's failure against HEAD — refusing to proceed as if the diagnosis were certain");
      result.state = "INCONCLUSIVE";
      return verdict();
    }
    result.reproduced = true;
    result.state = "REPRODUCED";
    say("✓ original failure reproduced against HEAD");
    say("");

    // --- repair with Codex ---
    result.state = "REPAIRING";
    say("Repairing with Codex (isolated worktree, workspace-write sandbox)...");
    const prompt = [
      "You are repairing a web application. A real user hit a failure in production. Nobody filed a bug; this evidence was captured from the user's browser.",
      "",
      "URL: " + (incident.url || "/"),
      "What the user did:",
      renderTrace(incident) || "  (no trace)",
      "",
      "Failure: " + renderFailure(incident.failure),
      incident.consoleErrors && incident.consoleErrors.length ? "Console errors:\n" + incident.consoleErrors.map((e) => "  " + e).join("\n") : "",
      "",
      "Drums has already replayed this exact interaction against the current code in this workspace and confirmed it fails the same way.",
      "",
      "Your job: find the root cause in this repository and make the smallest reasonable fix so this same interaction succeeds, while existing valid behavior keeps working.",
      "The app is started with: " + cfg.start + " (PORT env variable selects the port; health check at " + cfg.health + ").",
      "",
      "Rules:",
      "- Edit only what is needed. Do not refactor unrelated code.",
      "- Do not modify or delete tests to make them pass.",
      "- Do not claim success. Drums will independently boot the changed app, replay the exact user interaction, and run guardrail tests. Your work is judged only by that replay.",
    ].join("\n");
    const codex = await codexRepair(projectDir, prompt);
    fs.writeFileSync(path.join(artifacts, "codex-output.txt"), codex.out || "");
    if (codex.timedOut) say("✗ codex timed out");

    const diff = git(wtRoot, "diff");
    const numstat = git(wtRoot, "diff", "--numstat").trim();
    fs.writeFileSync(path.join(artifacts, "patch.diff"), diff);
    if (!numstat) {
      say("✗ codex produced no code change");
      return verdict();
    }
    result.diffNonEmpty = true;
    result.state = "CANDIDATE_READY";
    let add = 0, del = 0;
    numstat.split("\n").forEach((l) => { const [a, d] = l.split("\t"); add += +a || 0; del += +d || 0; });
    say(`✓ patch generated (+${add} -${del} across ${numstat.split("\n").length} file(s))`);
    say("");

    // --- verify: reboot the changed app, replay the SAME evidence ---
    result.state = "VERIFYING";
    say("Restarting candidate app from the changed worktree...");
    const portB = await freePort();
    app = bootApp(projectDir, cfg.start, portB);
    if (!(await waitHealthy(`http://localhost:${portB}`, cfg.health))) {
      say("✗ candidate app failed to boot after the patch: " + app.tail());
      return verdict();
    }
    say("Verifying: replaying the original user interaction...");
    const verify = await replayIncident(incident, `http://localhost:${portB}`);
    fs.writeFileSync(path.join(artifacts, "verification.json"), JSON.stringify(verify, null, 2));
    killApp(app); app = null;
    if (!verify.completed) {
      say("✗ replay could not complete against the candidate: " + verify.error);
      return verdict();
    }
    if (verify.originalFailureObserved) {
      say("✗ the same failure still happens after the patch — NOT verified");
      return verdict();
    }
    if (verify.otherFailures && verify.otherFailures.length) {
      say("✗ original failure is gone but a DIFFERENT failure appeared:");
      verify.otherFailures.forEach((f) => say("    " + f));
      result.state = "REGRESSION_FOUND";
      return verdict();
    }
    result.replayPassed = true;
    say("✓ original interaction now passes");

    // --- guardrails ---
    if (cfg.test) {
      say("Running guardrail tests...");
      try {
        execFileSync("sh", ["-c", cfg.test], { cwd: projectDir, stdio: "pipe", timeout: 120000 });
        result.guardrailsPassed = true;
        say("✓ guardrail tests pass");
      } catch (e) {
        say("✗ original failure resolved but guardrail tests FAIL — NOT verified");
        say("    " + String(e.stdout || e.message).slice(0, 300));
        result.state = "REGRESSION_FOUND";
        return verdict();
      }
    } else {
      result.guardrailsPassed = true; // no tests configured; replay + health are the bar
    }

    return verdict();
  } finally {
    cleanup();
    say("");
    if (result.state === "VERIFIED") {
      say("✓ VERIFIED");
      say("  the same interaction that failed for the user now passes, and guardrails pass");
    } else {
      say("Outcome: " + result.state);
    }
    say("");
    say("Diff:");
    const diffStat = (() => { try { return git(wtRoot, "diff", "--stat").trim(); } catch { return "(worktree gone)"; } })();
    say(diffStat || "  (no change)");
    say("");
    say("Artifacts: " + artifacts);
    say("Candidate worktree (for human review): " + projectDir);
    say(result.state === "VERIFIED" ? "Ready for human approval. Drums does not merge or deploy." : "No merge candidate.");
    fs.writeFileSync(path.join(artifacts, "result.json"), JSON.stringify(result, null, 2));
  }
}

module.exports = { runPipeline };
