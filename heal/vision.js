// Advisory vision check: after the deterministic replay has already decided,
// a vision model looks at what the user actually SAW before and after the
// repair. It never has authority over VERIFIED — the replay invariant does —
// it adds a human-shaped second look at the pixels, and a warning if the
// after-state does not visibly look like success.
const fs = require("fs");
const os = require("os");
const path = require("path");

function loadOpenAIKey() {
  if (process.env.OPENAI_API_KEY) return process.env.OPENAI_API_KEY;
  try {
    const auth = JSON.parse(fs.readFileSync(path.join(os.homedir(), ".codex", "auth.json"), "utf8"));
    return auth.OPENAI_API_KEY || null;
  } catch {
    return null;
  }
}

async function visionCheck(beforePng, afterPng, failureDescription) {
  const key = loadOpenAIKey();
  if (!key || !fs.existsSync(beforePng) || !fs.existsSync(afterPng)) return null;
  const img = (p) => ({
    type: "image_url",
    image_url: { url: "data:image/png;base64," + fs.readFileSync(p).toString("base64") },
  });
  const res = await fetch("https://api.openai.com/v1/chat/completions", {
    method: "POST",
    headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
    body: JSON.stringify({
      model: "gpt-4o-mini",
      max_tokens: 150,
      response_format: { type: "json_object" },
      messages: [
        {
          role: "user",
          content: [
            {
              type: "text",
              text:
                "Two screenshots of the same user interaction in a web app, before and after a code repair. " +
                "The failure was: " + failureDescription + ". " +
                'First image is BEFORE the repair, second is AFTER. Does the AFTER state visibly look like the user succeeded? ' +
                'Reply as JSON: {"looksFixed": true|false, "note": "<one short sentence about what the user now sees>"}',
            },
            img(beforePng),
            img(afterPng),
          ],
        },
      ],
    }),
    signal: AbortSignal.timeout(30000),
  });
  const data = await res.json();
  const text = data.choices && data.choices[0] && data.choices[0].message && data.choices[0].message.content;
  try { return JSON.parse(text); } catch { return null; }
}

module.exports = { visionCheck };
