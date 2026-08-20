#!/usr/bin/env node
// Build Foxlite's ad/tracker blocklist: download the standard filter lists
// (the same ones uBlock Origin ships), convert the supported subset of the
// Adblock Plus filter syntax into WebKit content-blocker rules, and write them
// brotli-compressed to src-tauri/blocklist/rules.jsonl.br (embedded in the
// binary; WebKit compiles them once and caches the result).
//
//   node tools/blocklist/build.mjs            # standard lists (see SOURCES)
//   node tools/blocklist/build.mjs --ubo      # + uBlock Origin's own lists (GPLv3)
//   node tools/blocklist/build.mjs --offline  # reuse tools/blocklist/cache/*.txt
//
// Rules are grouped into categories the user can toggle separately:
//   ads         EasyList
//   privacy     EasyPrivacy, Peter Lowe's, NoCoin (cryptominers)
//   security    URLhaus malware hosts, phishing hosts (+ uBO badware with --ubo)
//   annoyances  EasyList Cookie List (cookie banners / consent pop-ups)
//
// Output format: line 1 = meta JSON (with `chunks: [{category, rules}]`);
// each following line = one JSON array of rules (≤ CHUNK rules; WebKit caps a
// compiled list at 150 000). Every chunk carries ALL exception rules at its
// end: `ignore-previous-rules` only sees rules in the same compiled list, and
// exceptions are what keep sites working.
//
// What is converted:
//   ||host^ / |http:… / plain patterns, * and ^ wildcards, $third-party,
//   $domain=, resource-type options, @@ exceptions, ##selector element hiding
//   (generic + per-domain, with #@# exceptions folded into unless-domain).
// What is skipped (no WebKit equivalent): regex filters, $csp/$redirect/
// $removeparam/$important-only semantics, scriptlets (##+js), extended
// selectors (:has, :-abp-*, :style, …), $document whitelists.

import { mkdirSync, readFileSync, writeFileSync, existsSync } from "node:fs";
import { brotliCompressSync, constants } from "node:zlib";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const cacheDir = join(here, "cache");
const outFile = join(here, "../../src-tauri/blocklist/rules.jsonl.br");
const CHUNK = 50_000;

const args = new Set(process.argv.slice(2));
const SOURCES = [
  { category: "ads", name: "EasyList", url: "https://easylist.to/easylist/easylist.txt", license: "CC BY-SA 3.0 / GPLv3" },
  { category: "privacy", name: "EasyPrivacy", url: "https://easylist.to/easylist/easyprivacy.txt", license: "CC BY-SA 3.0 / GPLv3" },
  { category: "privacy", name: "Peter Lowe's list",
    url: "https://pgl.yoyo.org/adservers/serverlist.php?hostformat=adblockplus&showintro=0&mimetype=plaintext",
    home: "https://pgl.yoyo.org/adservers/", license: "McRae General Public License" },
  { category: "privacy", name: "NoCoin", url: "https://raw.githubusercontent.com/hoshsadiq/adblock-nocoin-list/master/nocoin.txt",
    home: "https://github.com/hoshsadiq/adblock-nocoin-list", license: "MIT" },
  { category: "security", name: "URLhaus malware hosts", format: "hosts",
    url: "https://urlhaus.abuse.ch/downloads/hostfile/", license: "CC0" },
  { category: "security", name: "Malicious URL Blocklist (URLhaus)", format: "hosts",
    url: "https://malware-filter.gitlab.io/malware-filter/urlhaus-filter.txt", license: "CC0" },
  { category: "security", name: "Phishing URL Blocklist (OpenPhish/PhishTank)", format: "hosts",
    url: "https://malware-filter.gitlab.io/malware-filter/phishing-filter.txt", license: "CC0" },
  { category: "annoyances", name: "EasyList Cookie List", url: "https://secure.fanboy.co.nz/fanboy-cookiemonster.txt", license: "CC BY-SA 3.0 / GPLv3" },
];
if (args.has("--ubo")) {
  const ubo = (f, category) => ({ category, name: `uBlock filters – ${f}`, url: `https://raw.githubusercontent.com/uBlockOrigin/uAssets/master/filters/${f}.txt`, license: "GPLv3" });
  SOURCES.push(ubo("filters", "ads"), ubo("privacy", "privacy"), ubo("badware", "security"), ubo("quick-fixes", "ads"), ubo("unbreak", "ads"));
}
const CATEGORIES = ["ads", "privacy", "security", "annoyances"];

// ---- fetch ------------------------------------------------------------------
async function fetchList(src) {
  mkdirSync(cacheDir, { recursive: true });
  const file = join(cacheDir, src.name.replace(/[^a-z0-9]+/gi, "_") + ".txt");
  if (!args.has("--offline")) {
    const res = await fetch(src.url, { headers: { "user-agent": "Foxlite blocklist builder" } });
    if (!res.ok) throw new Error(`${src.name}: HTTP ${res.status}`);
    writeFileSync(file, await res.text());
  } else if (!existsSync(file)) {
    throw new Error(`--offline but no cache for ${src.name}`);
  }
  return readFileSync(file, "utf8");
}

// ---- conversion helpers -----------------------------------------------------
const TYPE_MAP = {
  script: "script", image: "image", stylesheet: "style-sheet", css: "style-sheet",
  font: "font", media: "media", xmlhttprequest: "raw", xhr: "raw", fetch: "raw",
  websocket: "raw", ping: "ping", beacon: "ping", other: "other", popup: "popup",
  subdocument: "document", frame: "document",
};
const ALL_TYPES = ["script", "image", "style-sheet", "font", "media", "raw", "ping", "other", "popup", "document"];
// Options that have no WebKit equivalent: rules carrying them are dropped.
const UNSUPPORTED = new Set(["csp", "redirect", "redirect-rule", "removeparam", "denyallow", "header",
  "method", "all", "document", "doc", "elemhide", "ehide", "generichide", "ghide", "genericblock",
  "specifichide", "shide", "badfilter", "replace", "permissions", "urltransform", "cookie", "inline-script",
  "inline-font", "object", "object-subrequest", "webrtc", "empty", "mp4", "rewrite", "strict1p", "strict3p",
  "to", "from", "ipaddress", "reason"]);

const RE_SPECIAL = /[.?+[\]{}()\\$/]/g;
function escapeLiteral(s) {
  return s.replace(RE_SPECIAL, (c) => "\\" + c);
}

/// ABP URL pattern → WebKit `url-filter` regex, or null if unsupported.
function patternToRegex(pat) {
  if (pat.startsWith("/") && pat.endsWith("/") && pat.length > 2) return null; // regex filter
  let re = "";
  let i = 0;
  if (pat.startsWith("||")) {
    re += "^https?://([^/]+\\.)?";
    i = 2;
  } else if (pat.startsWith("|")) {
    re += "^";
    i = 1;
  } else {
    // Unanchored patterns match anywhere. A bare "example" pattern that also
    // has no wildcards is far too broad on its own — ABP still allows it, and
    // WebKit handles it as an unanchored regex.
  }
  let end = pat.length;
  let anchoredEnd = false;
  if (pat.endsWith("|")) {
    end -= 1;
    anchoredEnd = true;
  }
  let literal = "";
  const flush = () => {
    re += escapeLiteral(literal);
    literal = "";
  };
  for (; i < end; i++) {
    const c = pat[i];
    if (c === "*") {
      flush();
      re += ".*";
    } else if (c === "^") {
      flush();
      re += "[^a-zA-Z0-9_.%-]";
    } else if (c === "|") {
      return null; // stray anchor mid-pattern
    } else {
      literal += c;
    }
  }
  flush();
  if (anchoredEnd) re += "$";
  // Collapse `.*.*`.
  re = re.replace(/(\.\*){2,}/g, ".*");
  if (re === "" || re === ".*" || re === "^" ) return null;
  return re;
}

function domainOk(d) {
  return /^[a-z0-9.-]+$/i.test(d) && !d.startsWith(".") && !d.endsWith(".");
}

/// Parse `$options` into a WebKit trigger fragment; returns { trigger, ok }.
function parseOptions(opts) {
  const trigger = {};
  let types = new Set();
  let negTypes = new Set();
  let ifDomain = [];
  let unlessDomain = [];
  for (let raw of opts) {
    if (!raw) continue;
    let neg = false;
    let name = raw;
    let value = null;
    const eq = raw.indexOf("=");
    if (eq >= 0) {
      name = raw.slice(0, eq);
      value = raw.slice(eq + 1);
    }
    if (name.startsWith("~")) {
      neg = true;
      name = name.slice(1);
    }
    name = name.toLowerCase();
    if (name === "third-party" || name === "3p") {
      trigger["load-type"] = [neg ? "first-party" : "third-party"];
    } else if (name === "first-party" || name === "1p") {
      trigger["load-type"] = [neg ? "third-party" : "first-party"];
    } else if (name === "match-case") {
      trigger["url-filter-is-case-sensitive"] = true;
    } else if (name === "important") {
      // No override semantics in WebKit; keep the rule as a normal block.
    } else if (name === "domain" && value !== null) {
      for (const d of value.split("|")) {
        if (!d) continue;
        if (d.startsWith("~")) {
          const dd = d.slice(1).toLowerCase();
          if (domainOk(dd)) unlessDomain.push("*" + dd);
        } else {
          const dd = d.toLowerCase();
          if (domainOk(dd)) ifDomain.push("*" + dd);
          else return { ok: false }; // wildcard TLDs etc.
        }
      }
    } else if (name in TYPE_MAP) {
      (neg ? negTypes : types).add(TYPE_MAP[name]);
    } else if (UNSUPPORTED.has(name)) {
      return { ok: false };
    } else {
      return { ok: false }; // unknown option: be conservative
    }
  }
  if (ifDomain.length && unlessDomain.length) return { ok: false }; // WebKit can't mix
  if (ifDomain.length) trigger["if-domain"] = ifDomain;
  if (unlessDomain.length) trigger["unless-domain"] = unlessDomain;
  if (negTypes.size && !types.size) {
    types = new Set(ALL_TYPES.filter((t) => !negTypes.has(t)));
  }
  if (types.size) trigger["resource-type"] = [...types];
  return { ok: true, trigger };
}

// Conservative CSS selector validator (linear scan, no regex backtracking):
// compound selectors made of a type/`*`, `.class`, `#id`, `[attr op "value"]`
// and a few plain pseudo-classes, joined by descendant/child/sibling
// combinators. Anything else (extended/procedural selectors, escapes,
// namespaces, `:not` with nesting…) is dropped rather than risk failing the
// whole WebKit compile.
const IDENT_RE = /^-?[a-zA-Z_][\w-]*$/;
const PSEUDO_OK = new Set(["first-child", "last-child", "only-child", "empty", "first-of-type", "last-of-type"]);
const PSEUDO_ARG_OK = new Set(["nth-child", "nth-of-type", "nth-last-child", "not"]);
function selectorOk(sel) {
  if (sel.length > 400 || sel.length === 0) return false;
  if (sel.includes("\\") || sel.includes("|")) return false; // escapes / namespaces
  let i = 0;
  const n = sel.length;
  const isIdentChar = (c) => /[\w-]/.test(c);
  const readIdent = () => {
    const st = i;
    while (i < n && isIdentChar(sel[i])) i++;
    const id = sel.slice(st, i);
    return IDENT_RE.test(id) ? id : null;
  };
  let compounds = 0;
  while (i < n) {
    // one compound
    let parts = 0;
    if (sel[i] === "*") {
      i++;
      parts++;
    } else if (/[a-zA-Z]/.test(sel[i])) {
      if (!readIdent()) return false;
      parts++;
    }
    for (;;) {
      const c = sel[i];
      if (c === "." || c === "#") {
        i++;
        if (!readIdent()) return false;
        parts++;
      } else if (c === "[") {
        i++;
        if (!readIdent()) return false;
        while (sel[i] === " ") i++;
        if (sel[i] === "]") {
          i++;
        } else {
          if ("~|^$*".includes(sel[i])) i++;
          if (sel[i] !== "=") return false;
          i++;
          while (sel[i] === " ") i++;
          const q = sel[i];
          if (q === '"' || q === "'") {
            const close = sel.indexOf(q, i + 1);
            if (close < 0) return false;
            i = close + 1;
          } else {
            const st = i;
            while (i < n && /[\w./:%-]/.test(sel[i])) i++;
            if (i === st) return false;
          }
          while (sel[i] === " ") i++;
          if (sel[i] !== "]") return false;
          i++;
        }
        parts++;
      } else if (c === ":") {
        i++;
        if (sel[i] === ":") return false; // pseudo-elements: no
        const name = readIdent();
        if (!name) return false;
        if (sel[i] === "(") {
          if (!PSEUDO_ARG_OK.has(name)) return false;
          const close = sel.indexOf(")", i);
          if (close < 0) return false;
          const arg = sel.slice(i + 1, close);
          if (name === "not") {
            if (arg.includes("(") || !selectorOk(arg.trim())) return false;
          } else if (!/^[0-9n+ -]+$/.test(arg) && !["odd", "even"].includes(arg.trim())) {
            return false;
          }
          i = close + 1;
        } else if (!PSEUDO_OK.has(name)) {
          return false;
        }
        parts++;
      } else {
        break;
      }
    }
    if (parts === 0) return false;
    compounds++;
    // combinator or end
    let sawSpace = false;
    while (sel[i] === " ") {
      i++;
      sawSpace = true;
    }
    if (i >= n) break;
    if (">+~".includes(sel[i])) {
      i++;
      while (sel[i] === " ") i++;
      if (i >= n) return false;
    } else if (!sawSpace) {
      return false;
    }
  }
  return compounds > 0;
}

// ---- main -------------------------------------------------------------------
const stats = { lines: 0, network: 0, exceptions: 0, hide: 0, skipped: 0 };
// Per category: block rules and selector -> { generic, ifDomains, unlessDomains }.
const perCat = Object.fromEntries(CATEGORIES.map((c) => [c, { blockRules: [], hides: new Map() }]));
const exceptionRules = []; // global (appended to every chunk)
let cur = perCat.ads;

function addNetwork(line) {
  let exception = false;
  if (line.startsWith("@@")) {
    exception = true;
    line = line.slice(2);
  }
  let pattern = line;
  let opts = [];
  const dollar = line.lastIndexOf("$");
  if (dollar > 0 && !line.startsWith("/")) {
    pattern = line.slice(0, dollar);
    opts = line.slice(dollar + 1).split(",");
  }
  const re = patternToRegex(pattern);
  if (!re) return false;
  const { ok, trigger } = parseOptions(opts);
  if (!ok) return false;
  trigger["url-filter"] = re;
  const rule = { trigger, action: { type: exception ? "ignore-previous-rules" : "block" } };
  (exception ? exceptionRules : cur.blockRules).push(rule);
  if (exception) stats.exceptions++;
  else stats.network++;
  return true;
}

function addHide(domainsPart, selector, exception) {
  selector = selector.trim();
  if (!selectorOk(selector)) return false;
  let rec = cur.hides.get(selector);
  if (!rec) {
    rec = { generic: false, ifDomains: new Set(), unlessDomains: new Set() };
    cur.hides.set(selector, rec);
  }
  const domains = domainsPart ? domainsPart.split(",").map((d) => d.trim().toLowerCase()).filter(Boolean) : [];
  if (exception) {
    // `example.com#@#sel` — don't hide `sel` on example.com.
    for (const d of domains) if (domainOk(d)) rec.unlessDomains.add("*" + d);
    return true;
  }
  if (domains.length === 0) {
    rec.generic = true;
  } else {
    for (const d of domains) {
      if (d.startsWith("~")) {
        const dd = d.slice(1);
        if (domainOk(dd)) rec.unlessDomains.add("*" + dd);
        rec.generic = true; // "everywhere except" is a generic rule with exclusions
      } else if (domainOk(d)) {
        rec.ifDomains.add("*" + d);
      }
    }
  }
  return true;
}

/// `hosts`-format lists (`0.0.0.0 host` / `127.0.0.1 host`) → `||host^`.
function hostsToAbp(text) {
  const out = [];
  for (let line of text.split("\n")) {
    line = line.trim();
    if (!line || line.startsWith("#")) continue;
    const parts = line.split(/\s+/);
    const host = (parts.length > 1 ? parts[1] : parts[0]).toLowerCase();
    if (host && host !== "localhost" && host !== "localhost.localdomain" && host !== "broadcasthost" && domainOk(host)) {
      out.push(`||${host}^`);
    }
  }
  return out.join("\n");
}

function ingest(text, src) {
  cur = perCat[src.category];
  if (src.format === "hosts") text = hostsToAbp(text);
  for (let line of text.split("\n")) {
    line = line.trim();
    stats.lines++;
    if (!line || line.startsWith("!") || line.startsWith("[") || line.startsWith("#")) continue;
    if (line.includes("$$") || line.includes("##+js") || line.includes("#%#") || line.includes("#$#")) {
      stats.skipped++;
      continue;
    }
    // Element hiding: domains##selector, domains#@#selector (skip #?# extended)
    let m = /^([^#]*)#@#(.+)$/.exec(line);
    if (m) {
      if (!addHide(m[1], m[2], true)) stats.skipped++;
      continue;
    }
    if (line.includes("#?#")) {
      stats.skipped++;
      continue;
    }
    m = /^([^#]*)##(.+)$/.exec(line);
    if (m) {
      if (addHide(m[1], m[2], false)) stats.hide++;
      else stats.skipped++;
      continue;
    }
    if (!addNetwork(line)) stats.skipped++;
  }
}

const used = [];
for (const src of SOURCES) {
  const text = await fetchList(src);
  ingest(text, src);
  used.push({ category: src.category, name: src.name, url: src.url, home: src.home, license: src.license });
  console.error(`✓ ${src.name}`);
}

// Materialise hide rules per category.
function hideRulesOf(hides) {
  const out = [];
  for (const [selector, rec] of hides) {
    const base = { action: { type: "css-display-none", selector } };
    if (rec.generic) {
      const trigger = { "url-filter": ".*" };
      if (rec.unlessDomains.size) trigger["unless-domain"] = [...rec.unlessDomains];
      out.push({ trigger, ...base });
    } else if (rec.ifDomains.size) {
      // Domain-specific hide; per-domain exceptions can't combine with if-domain
      // in WebKit, so drop those (very rare).
      const ifDomains = [...rec.ifDomains].filter((d) => !rec.unlessDomains.has(d));
      if (ifDomains.length) out.push({ trigger: { "url-filter": ".*", "if-domain": ifDomains }, ...base });
    }
  }
  return out;
}

// Dedupe exceptions once (they are appended to every chunk).
const exSeen = new Set();
const exceptions = exceptionRules.filter((r) => {
  const k = JSON.stringify(r);
  if (exSeen.has(k)) return false;
  exSeen.add(k);
  return true;
});

const chunkMeta = [];
const chunkLines = [];
const catCounts = {};
let totalRules = 0;
let totalHide = 0;
let totalBlock = 0;
for (const cat of CATEGORIES) {
  const { blockRules, hides } = perCat[cat];
  const hideRules = hideRulesOf(hides);
  // Order: blocks, then hides, then exceptions (ignore-previous-rules must
  // follow what it exempts). Dedupe within the category.
  const seen = new Set();
  const rules = [];
  for (const r of [...blockRules, ...hideRules]) {
    const key = JSON.stringify(r);
    if (seen.has(key)) continue;
    seen.add(key);
    rules.push(r);
  }
  catCounts[cat] = { block: blockRules.length, hide: hideRules.length, rules: rules.length };
  totalBlock += blockRules.length;
  totalHide += hideRules.length;
  totalRules += rules.length;
  if (rules.length === 0) continue;
  const per = CHUNK - exceptions.length;
  for (let i = 0; i < rules.length; i += per) {
    const slice = rules.slice(i, i + per).concat(exceptions);
    chunkMeta.push({ category: cat, rules: slice.length });
    chunkLines.push(JSON.stringify(slice));
  }
}

const meta = {
  generated: new Date().toISOString().slice(0, 10),
  rules: totalRules,
  network: totalBlock,
  hide: totalHide,
  exceptions: exceptions.length,
  categories: catCounts,
  chunks: chunkMeta,
  sources: used,
};
let out = JSON.stringify(meta) + "\n" + chunkLines.join("\n") + "\n";
const br = brotliCompressSync(Buffer.from(out), {
  params: { [constants.BROTLI_PARAM_QUALITY]: 11, [constants.BROTLI_PARAM_SIZE_HINT]: out.length },
});
mkdirSync(dirname(outFile), { recursive: true });
writeFileSync(outFile, br);

// Attribution: the compiled rules stay under the source lists' licenses, so the
// notices file is regenerated from the same `used` table that goes into meta.
// Keep it committed alongside rules.jsonl.br — it is what satisfies CC BY-SA.
{
  const link = (s) => {
    const u = s.home || s.url;
    return `[${new URL(u).hostname}](${u})`;
  };
  const rows = used
    .map((s) => `| ${s.name} | ${s.category} | ${s.license} | ${link(s)} |`)
    .join("\n");
  const ubo = used.some((s) => s.license === "GPLv3")
    ? "This blocklist was built with `--ubo`, so it **includes uBlock Origin's own\n" +
      "filter lists (GPLv3)**. A binary carrying these rules is not covered by the\n" +
      "arrangement above and should not be redistributed under these notices."
    : "uBlock Origin's own filter lists (GPLv3) are **not** in the shipped blocklist.\n" +
      "`npm run blocklist:ubo` adds them for local builds only — a build made that way\n" +
      "should not be redistributed under these notices.";
  writeFileSync(
    join(here, "../../THIRD-PARTY-NOTICES.md"),
    `<!-- Generated by tools/blocklist/build.mjs — do not edit by hand. -->

# Third-party notices

Foxlite is licensed under the Apache License 2.0 (see [\`LICENSE\`](LICENSE)).
The material recorded here is **not**: it is redistributed under its own terms,
which are unaffected by Foxlite's license.

## Bundled filter lists

\`src-tauri/blocklist/rules.jsonl.br\` is compiled from the filter lists below and
embedded in the Foxlite binary as a data resource. The compiled rule set is a
derivative work of those lists and stays subject to their licenses — the Apache
License covers the converter (\`tools/blocklist/build.mjs\`), not the rules it
produces.

Generated ${meta.generated} · ${totalRules.toLocaleString("en-US")} rules.

| List | Category | License | Source |
| --- | --- | --- | --- |
${rows}

EasyList, EasyPrivacy and the EasyList Cookie List are dual-licensed under
CC BY-SA 3.0 **or** GPLv3. Foxlite takes the CC BY-SA 3.0 option and satisfies
it with the attribution in this table; the rules travel as data inside the
application, not as part of its code.

${ubo}

## Dependencies

Foxlite's Rust and JavaScript dependencies are permissively licensed
(MIT, Apache-2.0 or BSD-3-Clause). The authoritative list is
\`src-tauri/Cargo.lock\` and \`package-lock.json\`; run \`cargo license\` in
\`src-tauri/\` for a current per-crate breakdown.

The browser engine is the operating system's own WebView (WKWebView on macOS)
and is not redistributed with Foxlite.
`
  );
}

console.error(
  `rules: ${totalRules} (block ${totalBlock}, hide ${totalHide}, exceptions ${exceptions.length}) in ${chunkLines.length} chunks ` +
    `${JSON.stringify(catCounts)}; ` +
    `skipped ${stats.skipped} of ${stats.lines} lines; ${(out.length / 1e6).toFixed(1)} MB JSON → ${(br.length / 1e6).toFixed(2)} MB brotli → ${outFile}`
);
