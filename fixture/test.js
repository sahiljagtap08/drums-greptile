// Guardrail tests: the behaviors that must keep working after any repair.
const { spawn } = require("child_process");
const assert = require("assert");

const PORT = 4991;
const child = spawn(process.execPath, ["server.js"], {
  cwd: __dirname,
  env: { ...process.env, PORT: String(PORT) },
  stdio: "ignore",
});

async function post(email) {
  const r = await fetch(`http://localhost:${PORT}/api/signup`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ email }),
  });
  return { status: r.status, body: await r.json() };
}

async function main() {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/api/health`); break; }
    catch { await new Promise((r) => setTimeout(r, 100)); }
  }
  const ok = await post("alice@example.com");
  assert.equal(ok.status, 200, "plain email must sign up");
  assert.equal(ok.body.domain, "example.com", "domain must be extracted");
  const plus = await post("alice+greptile@example.com");
  assert.equal(plus.status, 200, "plus-addressed email must sign up");
  assert.equal(plus.body.domain, "example.com", "domain must be extracted from a plus address");
  const health = await fetch(`http://localhost:${PORT}/api/health`);
  assert.equal(health.status, 200, "health must stay up");
  console.log("guardrails pass");
}

main()
  .then(() => { child.kill(); process.exit(0); })
  .catch((e) => { console.error(e.message); child.kill(); process.exit(1); });
