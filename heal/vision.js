// Advisory vision check: after the deterministic replay has already decided,
// a vision model looks at what the user actually SAW before and after the
// repair. It never has authority over VERIFIED — the replay invariant does —
// it adds a human-shaped second look at the pixels, and a warning if the
// after-state does not visibly look like success.
//
// Provider order: Gemini (GEMINI_API_KEY) -> Vertex AI (gcloud token) ->
// OpenAI (OPENAI_API_KEY / codex auth). First one with a working credential
// does the check; the result names the model that judged.
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");

const PROMPT = (failureDescription) =>
  "Two screenshots of the same user interaction in a web app, before and after a code repair. " +
  "The failure was: " + failureDescription + ". " +
  "First image is BEFORE the repair, second is AFTER. Does the AFTER state visibly look like the user succeeded? " +
  'Reply as JSON only: {"looksFixed": true|false, "note": "<one short sentence about what the user now sees>"}';

function b64(p) {
  return fs.readFileSync(p).toString("base64");
}

function envLocalValue(name) {
  if (process.env[name]) return process.env[name];
  try {
    const env = fs.readFileSync(path.join(__dirname, "..", ".env.local"), "utf8");
    const m = env.match(new RegExp("^" + name + "=(.+)$", "m"));
    return m ? m[1].trim() : null;
  } catch {
    return null;
  }
}

function parseVerdict(text, model) {
  try {
    const clean = String(text).replace(/^```(json)?|```$/gm, "").trim();
    const v = JSON.parse(clean);
    if (typeof v.looksFixed === "boolean") return { looksFixed: v.looksFixed, note: v.note, model };
  } catch {}
  return null;
}

async function geminiCheck(beforePng, afterPng, failureDescription) {
  const key = envLocalValue("GEMINI_API_KEY");
  if (!key) return null;
  const model = "gemini-3.6-flash";
  const res = await fetch(
    `https://generativelanguage.googleapis.com/v1beta/models/${model}:generateContent`,
    {
      method: "POST",
      headers: { "x-goog-api-key": key, "content-type": "application/json" },
      body: JSON.stringify({
        contents: [{
          parts: [
            { text: PROMPT(failureDescription) },
            { inline_data: { mime_type: "image/png", data: b64(beforePng) } },
            { inline_data: { mime_type: "image/png", data: b64(afterPng) } },
          ],
        }],
        generationConfig: { responseMimeType: "application/json", maxOutputTokens: 2000 },
      }),
      signal: AbortSignal.timeout(30000),
    }
  );
  const data = await res.json();
  const text = data.candidates && data.candidates[0] && data.candidates[0].content
    && data.candidates[0].content.parts && data.candidates[0].content.parts.map((p) => p.text || "").join("");
  return parseVerdict(text, model);
}

async function vertexCheck(beforePng, afterPng, failureDescription) {
  let token, project;
  try {
    const quiet = { encoding: "utf8", timeout: 10000, stdio: ["ignore", "pipe", "ignore"] };
    token = execFileSync("gcloud", ["auth", "print-access-token"], quiet).trim();
    project = execFileSync("gcloud", ["config", "get-value", "project"], quiet).trim();
  } catch {
    return null;
  }
  if (!token || !project) return null;
  const model = "gemini-3.6-flash";
  const res = await fetch(
    `https://aiplatform.googleapis.com/v1/projects/${project}/locations/global/publishers/google/models/${model}:generateContent`,
    {
      method: "POST",
      headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
      body: JSON.stringify({
        contents: [{
          role: "user",
          parts: [
            { text: PROMPT(failureDescription) },
            { inlineData: { mimeType: "image/png", data: b64(beforePng) } },
            { inlineData: { mimeType: "image/png", data: b64(afterPng) } },
          ],
        }],
        generationConfig: { responseMimeType: "application/json", maxOutputTokens: 2000 },
      }),
      signal: AbortSignal.timeout(30000),
    }
  );
  const data = await res.json();
  const text = data.candidates && data.candidates[0] && data.candidates[0].content
    && data.candidates[0].content.parts && data.candidates[0].content.parts.map((p) => p.text || "").join("");
  return parseVerdict(text, model + " (vertex)");
}

async function openaiCheck(beforePng, afterPng, failureDescription) {
  let key = process.env.OPENAI_API_KEY;
  if (!key) {
    try {
      key = JSON.parse(fs.readFileSync(path.join(os.homedir(), ".codex", "auth.json"), "utf8")).OPENAI_API_KEY;
    } catch {}
  }
  if (!key) return null;
  const model = "gpt-4o-mini";
  const img = (p) => ({ type: "image_url", image_url: { url: "data:image/png;base64," + b64(p) } });
  const res = await fetch("https://api.openai.com/v1/chat/completions", {
    method: "POST",
    headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
    body: JSON.stringify({
      model,
      max_tokens: 150,
      response_format: { type: "json_object" },
      messages: [{ role: "user", content: [{ type: "text", text: PROMPT(failureDescription) }, img(beforePng), img(afterPng)] }],
    }),
    signal: AbortSignal.timeout(30000),
  });
  const data = await res.json();
  const text = data.choices && data.choices[0] && data.choices[0].message && data.choices[0].message.content;
  return parseVerdict(text, model);
}

async function visionCheck(beforePng, afterPng, failureDescription) {
  if (!fs.existsSync(beforePng) || !fs.existsSync(afterPng)) return null;
  for (const check of [geminiCheck, vertexCheck, openaiCheck]) {
    try {
      const v = await check(beforePng, afterPng, failureDescription);
      if (v) return v;
    } catch {}
  }
  return null;
}

module.exports = { visionCheck };
