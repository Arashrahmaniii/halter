// Terminal output helpers. Colors are skipped when NO_COLOR is set or output is not a TTY.
const useColor = process.stdout.isTTY && !process.env.NO_COLOR;
const c = (code) => (s) => (useColor ? `\x1b[${code}m${s}\x1b[0m` : String(s));

export const color = {
  red: c("31"),
  green: c("32"),
  yellow: c("33"),
  blue: c("34"),
  magenta: c("35"),
  cyan: c("36"),
  gray: c("90"),
  bold: c("1"),
};

const verdictColor = {
  allow: color.green,
  warn: color.yellow,
  block: color.red,
  Settled: color.green,
  Dispatched: color.green,
  Pending: color.yellow,
  Blocked: color.red,
  Failed: color.red,
  Rejected: color.red,
  Expired: color.red,
  REJECTED: color.red,
  refused: color.red,
};

export function tag(word) {
  const f = verdictColor[word] || color.cyan;
  return f(`[${word}]`);
}

export function step(title) {
  console.log("\n" + color.bold(color.blue(`== ${title}`)));
}

export function info(...args) {
  console.log("  " + args.join(" "));
}

export function detail(lines) {
  for (const l of lines) console.log(color.gray("    · " + l));
}

export function link(label, url) {
  if (url) console.log(color.gray(`    ${label}: ${url}`));
}
