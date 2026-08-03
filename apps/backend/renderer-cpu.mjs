// Renderer-only CPU for a profile over N seconds
import { execSync } from "child_process";
import { readFileSync } from "fs";
const profile = process.argv[2]; // e.g. lumen-clean-a
const seconds = +(process.argv[3] ?? 30);
const CLK = 100;
function ticks() {
  let t = 0;
  const out = execSync(`pgrep -f ${profile}`).toString().trim().split("\n");
  for (const pid of out) {
    try {
      const comm = readFileSync(`/proc/${pid}/comm`, "utf8").trim();
      if (comm !== "chrome" || !readFileSync(`/proc/${pid}/cmdline`, "utf8").includes("type=renderer")) continue;
      const st = readFileSync(`/proc/${pid}/stat`, "utf8");
      const parts = st.slice(st.lastIndexOf(")") + 1).trim().split(" ");
      t += (+parts[11] || 0) + (+parts[12] || 0);
    } catch {}
  }
  return t;
}
const t1 = ticks();
setTimeout(() => {
  const t2 = ticks();
  console.log(((t2 - t1) / CLK / seconds * 100).toFixed(2));
}, seconds * 1000);
