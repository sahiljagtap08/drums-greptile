// Terminal presentation. Zero dependencies: raw ANSI, degrades to plain text
// when not a TTY or when NO_COLOR is set. The output stays a sequential log
// on purpose — the scrollback IS the story of the loop.
const tty = process.stdout.isTTY && !process.env.NO_COLOR;
const wrap = (open, close) => (s) => (tty ? `\x1b[${open}m${s}\x1b[${close}m` : String(s));

const c = {
  bold: wrap(1, 22),
  dim: wrap(2, 22),
  green: wrap(32, 39),
  red: wrap(31, 39),
  yellow: wrap(33, 39),
  cyan: wrap(36, 39),
  blue: wrap(34, 39),
  cream: wrap(97, 39),
};

const ok = (s) => c.green("✓") + " " + s;
const no = (s) => c.red("✗") + " " + s;
const adv = (s) => c.blue("◆") + " " + s;
const warn = (s) => c.yellow("⚠") + " " + s;
const head = (s) => c.bold(c.cream(s));
const dim = (s) => c.dim(s);

function rule(label) {
  const width = Math.min((process.stdout.columns || 80), 74);
  if (!label) return dim("─".repeat(width));
  const pad = Math.max(2, width - label.length - 6);
  return dim("── ") + c.bold(label) + " " + dim("─".repeat(pad));
}

function banner(lines) {
  const width = Math.max(...lines.map((l) => l.plain.length)) + 4;
  const top = dim("╭" + "─".repeat(width) + "╮");
  const bot = dim("╰" + "─".repeat(width) + "╯");
  const body = lines.map((l) => dim("│") + "  " + l.styled + " ".repeat(width - l.plain.length - 2) + dim("│"));
  return [top, ...body, bot].join("\n");
}

// A single-line spinner with elapsed time. Only spins on a real terminal;
// otherwise prints the label once and stays quiet.
function spin(label) {
  const started = Date.now();
  if (!tty) {
    console.log(label + "...");
    return { done() {}, clear() {} };
  }
  const frames = ["◐", "◓", "◑", "◒"];
  let i = 0;
  const draw = () => {
    const s = Math.round((Date.now() - started) / 1000);
    const t = s >= 60 ? `${Math.floor(s / 60)}m${String(s % 60).padStart(2, "0")}s` : `${s}s`;
    process.stdout.write(`\r\x1b[2K${c.cyan(frames[i++ % 4])} ${label} ${c.dim(t)}`);
  };
  draw();
  const timer = setInterval(draw, 250);
  const clear = () => { clearInterval(timer); process.stdout.write("\r\x1b[2K"); };
  return {
    clear,
    done(finalLine) { clear(); if (finalLine) console.log(finalLine); },
  };
}

module.exports = { c, ok, no, adv, warn, head, dim, rule, banner, spin, tty };
