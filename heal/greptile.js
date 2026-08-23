// Greptile is the independent staff engineer: after Drums verifies a repair
// behaviorally, the candidate PR is handed to Greptile for a code review a
// human can read before merging. Codex generates, Drums verifies behavior,
// Greptile reviews the code. Nobody grades their own homework.
const fs = require("fs");
const path = require("path");

function loadKey(repoRoot) {
  if (process.env.GREPTILE_API_KEY) return process.env.GREPTILE_API_KEY;
  try {
    const env = fs.readFileSync(path.join(repoRoot, ".env.local"), "utf8");
    const m = env.match(/^GREPTILE_API_KEY=(.+)$/m);
    return m ? m[1].trim() : null;
  } catch {
    return null;
  }
}

async function mcpCall(key, tool, args) {
  const res = await fetch("https://api.greptile.com/mcp", {
    method: "POST",
    headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name: tool, arguments: args } }),
    signal: AbortSignal.timeout(45000),
  });
  const data = await res.json();
  if (data.error) throw new Error(data.error.message || "greptile mcp error");
  const text = data.result && data.result.content && data.result.content[0] && data.result.content[0].text;
  try { return JSON.parse(text); } catch { return text; }
}

async function triggerReview(key, { repoSlug, defaultBranch, branch, prNumber }) {
  return mcpCall(key, "trigger_code_review", {
    name: repoSlug,
    remote: "github",
    defaultBranch: defaultBranch || "main",
    branch,
    prNumber,
  });
}

module.exports = { loadKey, triggerReview };
