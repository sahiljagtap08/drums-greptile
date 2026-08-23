# Onboard your app onto Drums (hackathon edition)

Five minutes, three steps. Your app keeps running like normal; Drums watches
how users actually behave in it and repairs what fails them.

## Your app qualifies if

- It is a **git repo** and a **Node-family web app** (Vite, Next, Express,
  plain node — anything `npm`/`node` can start).
- One command boots it, and it **respects the `PORT` env variable**
  (Drums boots isolated copies on random ports — a hardcoded port breaks that).
- It has a page a user can click around in.

## Step 1 — drums.json in your repo root

```json
{
  "install": "npm install",
  "start": "npm run dev",
  "health": "/",
  "app": "/",
  "test": "npm test"
}
```

`health` is any URL that returns 200 when the app is up. Leave `test` empty
if you have no tests (the behavioral replay is still the bar).

## Step 2 — the capture snippet in your main HTML

Two lines, before your other scripts:

```html
<script>window.__DRUMS_COLLECTOR__ = "http://localhost:4600"</script>
<script src="http://localhost:4600/snippet.js"></script>
```

It records interactions (passwords and secret-looking fields are redacted)
and reports the moment something fails for a user: a 5xx, an uncaught error,
or a button that people keep clicking while nothing happens at all.

## Step 3 — run Drums

```bash
node heal/cli.js watch /path/to/your-repo
```

Now use your app like a real user. When something fails for you, Drums will:
reproduce your exact interaction against HEAD in an isolated worktree, hand
the evidence to Codex, reboot the changed app, replay the same interaction,
and refuse to say VERIFIED unless the thing that failed for you no longer
fails. If you have push access and Greptile on the repo, the verified fix
also opens a PR and gets an independent Greptile review. You decide the merge.
