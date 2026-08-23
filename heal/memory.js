// Drums' memory of the product evolving. Every incident — verified, failed,
// or inconclusive — is appended to an append-only record. On a new incident,
// Drums recalls related history and briefs the repair agent with it. Records,
// not instructions: the agent must verify each against the present code.
// (This is the hackathon-size port of the production engine's recall crate.)
const fs = require("fs");
const path = require("path");

function memoryPath(repoRoot) {
  return path.join(repoRoot, ".drums", "memory.jsonl");
}

function readAll(repoRoot) {
  try {
    return fs
      .readFileSync(memoryPath(repoRoot), "utf8")
      .split("\n")
      .filter(Boolean)
      .map((l) => { try { return JSON.parse(l); } catch { return null; } })
      .filter(Boolean);
  } catch {
    return [];
  }
}

function remember(repoRoot, record) {
  fs.mkdirSync(path.dirname(memoryPath(repoRoot)), { recursive: true });
  fs.appendFileSync(memoryPath(repoRoot), JSON.stringify(record) + "\n");
}

function anchorOf(failure) {
  if (!failure) return null;
  if (failure.kind === "http") return failure.method + " " + failure.path;
  if (failure.kind === "friction") return failure.selector;
  if (failure.kind === "jserror") return String(failure.message || "").slice(0, 60);
  return null;
}

// Related = same failure anchor, or same kind on the same page, newest first.
function recall(repoRoot, incident, limit = 3) {
  const anchor = anchorOf(incident.failure);
  const kind = incident.failure && incident.failure.kind;
  const scored = readAll(repoRoot)
    .map((r) => {
      let score = 0;
      if (anchor && r.anchor === anchor) score += 2;
      if (kind && r.kind === kind && r.url === incident.url) score += 1;
      return { r, score };
    })
    .filter((x) => x.score > 0)
    .sort((a, b) => b.score - a.score || (b.r.at || "").localeCompare(a.r.at || ""))
    .slice(0, limit);
  return scored.map(({ r }) => {
    const when = (r.at || "").slice(0, 10);
    const files = (r.filesChanged || []).join(", ");
    if (r.state === "VERIFIED")
      return `${when}: ${r.summary} — VERIFIED; the fix touched ${files || "unknown files"}${r.prUrl ? ` (${r.prUrl})` : ""}.`;
    return `${when}: ${r.summary} — outcome ${r.state}${files ? `; attempted change touched ${files}` : ""}.`;
  });
}

function summarize(failure) {
  if (!failure) return "an unclassified user failure";
  if (failure.kind === "http") return `${failure.method} ${failure.path} returned ${failure.status} for a real user`;
  if (failure.kind === "friction") return `users fought a dead ${failure.selector}${failure.text ? ` ("${failure.text}")` : ""} element`;
  return `uncaught error: ${String(failure.message || "").slice(0, 80)}`;
}

module.exports = { remember, recall, anchorOf, summarize };
