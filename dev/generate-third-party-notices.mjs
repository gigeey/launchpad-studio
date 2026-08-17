#!/usr/bin/env node
//
// Regenerate the machine-derived sections of THIRD-PARTY-NOTICES.md, and check
// the dependency tree against this project's outbound licensing.
//
// WHY THIS EXISTS
// ---------------
// THIRD-PARTY-NOTICES.md states that nothing in it was transcribed by hand. For
// a while that was true of how it was first produced and false of the
// repository, because the program that produced it was never committed. A file
// that describes a process no reader can run is worse than one that admits to
// being hand-written: the claim cannot be checked, and the first dependency
// change silently makes the file wrong while it still reads as authoritative.
//
// So this script is the process that file describes. It is the only supported
// way to change the generated sections.
//
// WHAT IT DERIVES, AND FROM WHERE
// -------------------------------
// Rust:  `cargo metadata --locked`, every package carrying a `source`. That is
//        the resolved graph for *all* target platforms, not just this machine's,
//        so Linux- and Windows-only crates (`wayland-*`, `zbus*`,
//        `secret-service`, `linux-keyutils`) are covered. Each `license` string
//        is the one in that crate's own manifest.
//
// npm:   `frontend/package-lock.json`, every entry under `node_modules/`. The
//        lockfile rather than the installed tree, for the same reason: an
//        install only ever materializes the optional binaries matching the host
//        platform, so a walk of `node_modules/` on macOS omits ~80 packages a
//        Linux contributor has on disk. The lockfile carries a `license` field
//        for every package, so no filesystem read and no network call is needed.
//
// Neither input is this machine's state. Both are committed files, which is what
// makes the output reproducible by anyone who clones the repository.
//
// WHAT IT DOES NOT TOUCH
// ----------------------
// Everything above `## Rust crates (N)` except the coverage line: the summary of
// obligations, the elections between dual licences, the reasoning about MPL-2.0
// section 3.3. That is judgement, not data, and a generator that rewrote it
// would either delete it or fake it. This script rewrites exactly two regions
// and fails loudly if it cannot find either one.
//
// THE POLICY CHECK
// ----------------
// The notices file and NOTICE both assert that no dependency is under the GPL,
// AGPL, SSPL, BUSL, CDDL, EPL, OSL, EUPL, the Commons Clause or the Elastic
// License, and that a specific handful carry obligations beyond attribution.
// Those are the two claims most likely to quietly stop being true. Both are
// checked here on every run, so a dependency that breaks either one fails this
// script instead of reaching a reader.
//
// Usage:
//   dev/generate-third-party-notices.mjs            rewrite THIRD-PARTY-NOTICES.md in place
//   dev/generate-third-party-notices.mjs --check    do not write; exit 1 if it is out of date
//   dev/generate-third-party-notices.mjs --out PATH write elsewhere (leaves the original alone)
//
// Exit: 0 = up to date / written, 1 = out of date or policy violation,
//       2 = could not read an input or find an anchor.

import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const NOTICES = resolve(REPO, "THIRD-PARTY-NOTICES.md");
const PACKAGE_LOCK = resolve(REPO, "frontend/package-lock.json");

// --- Licence policy ---------------------------------------------------------

// Licences that impose nothing beyond attribution, so a package under one needs
// no entry in "Obligations we carry". CDLA-Permissive-2.0 is deliberately absent
// despite the name: it is permissive, but it covers certificate *data* rather
// than code and the notices file calls it out, so it is treated here as
// requiring an acknowledgement like any other non-plain licence.
const PERMISSIVE = new Set([
  "0BSD",
  "Apache-2.0",
  "Apache-2.0 WITH LLVM-exception",
  "BSD-2-Clause",
  "BSD-3-Clause",
  "BlueOak-1.0.0",
  "BSL-1.0",
  "CC0-1.0",
  "ISC",
  "MIT",
  "MIT-0",
  "Python-2.0",
  "Unicode-3.0",
  "Unlicense",
  "Zlib",
]);

// Licence families this project will not ship under, in any position. Matched
// against whole SPDX identifiers, case-insensitively, by prefix.
const FORBIDDEN_PREFIXES = [
  "AGPL",
  "GPL",
  "LGPL",
  "SSPL",
  "BUSL",
  "CDDL",
  "EPL",
  "OSL",
  "EUPL",
  "Elastic",
  "Commons-Clause",
];

// Packages whose licence is not plainly permissive and which have been reviewed.
// The reason recorded here must stay consistent with the "Obligations we carry"
// table in THIRD-PARTY-NOTICES.md and with NOTICE. A package that reaches a
// non-permissive licence and is not listed here fails the run.
//
// `prefix` covers a family that npm publishes as one package per platform. Those
// are the same project, from the same source, under the same licence, and only
// the entry matching the host is ever installed — but all of them are in the
// lockfile, so all of them are listed and all of them need a reason.
const REVIEWED = [
  { name: "cssparser", reason: "MPL-2.0, used unmodified; file-level copyleft only" },
  { name: "cssparser-macros", reason: "MPL-2.0, used unmodified; file-level copyleft only" },
  { name: "dtoa-short", reason: "MPL-2.0, used unmodified; file-level copyleft only" },
  { name: "option-ext", reason: "MPL-2.0, used unmodified; file-level copyleft only" },
  { name: "selectors", reason: "MPL-2.0, used unmodified; file-level copyleft only" },
  { name: "webpki-roots", reason: "CDLA-Permissive-2.0 certificate data, used unmodified" },
  { name: "webpki-root-certs", reason: "CDLA-Permissive-2.0 certificate data, used unmodified" },
  { name: "r-efi", reason: "we elect MIT OR Apache-2.0; the LGPL alternative is not taken" },
  { name: "dompurify", reason: "we elect Apache-2.0; the MPL alternative is not taken" },
  { prefix: "lightningcss", reason: "MPL-2.0, build-time only, not in the shipped bundle" },
  { name: "caniuse-lite", reason: "CC-BY-4.0 data, build-time only, not in the shipped bundle" },
];

function reviewFor(name) {
  return REVIEWED.find(
    (entry) => entry.name === name || (entry.prefix && name.startsWith(entry.prefix)),
  );
}

// Packages whose manifest declares no licence at all, resolved by reading the
// licence text the package itself ships. Adding an entry here is a deliberate
// act: it records a human having opened the file and read it.
const NO_DECLARED_LICENCE = new Map([
  [
    "khroma",
    {
      // frontend/node_modules/khroma/license — MIT, Copyright 2019-present
      // Fabio Spampinato, Andrew Maney.
      spdx: "MIT",
      row: "MIT (from bundled `license` file; no `license` field declared)",
      summary: "MIT, declared only in a bundled `license` file (`khroma`)",
    },
  ],
]);

/**
 * Split a licence expression into the identifiers it mentions.
 *
 * This is not an SPDX expression parser and does not try to be one. It answers
 * one question — which licence names appear — and the acceptability rule below
 * is written so that an imprecise answer errs towards rejection rather than
 * towards a silent pass.
 *
 * `/` is crates.io's pre-SPDX spelling of OR and is normalized to it.
 */
function identifiers(expression) {
  return expression
    .replace(/\//g, " OR ")
    .replace(/[()]/g, " ")
    .split(/\s+OR\s+|\s+AND\s+/)
    .map((part) => part.trim())
    .filter(Boolean);
}

/**
 * Decide whether a licence expression is acceptable without review.
 *
 * Two rules, chosen so that the imprecision above cannot produce a false pass:
 *
 *   - A pure OR expression is acceptable if *any* alternative is permissive,
 *     because we may take that alternative. This is what lets `r-efi`
 *     (`MIT OR Apache-2.0 OR LGPL-2.1-or-later`) through without the LGPL ever
 *     attaching.
 *   - An expression containing AND requires *every* identifier to be permissive,
 *     because the conjunction means all of them apply at once. Since the split
 *     above does not preserve precedence, any AND anywhere triggers the strict
 *     rule even where a more careful parse might not need it.
 */
function classify(expression) {
  const ids = identifiers(expression);
  const conjunctive = /\sAND\s/.test(expression);
  const permissive = conjunctive
    ? ids.every((id) => PERMISSIVE.has(id))
    : ids.some((id) => PERMISSIVE.has(id));

  const forbidden = ids.filter((id) =>
    FORBIDDEN_PREFIXES.some((prefix) =>
      id.toLowerCase().startsWith(prefix.toLowerCase()),
    ),
  );

  return { ids, permissive, conjunctive, forbidden };
}

// --- Inputs -----------------------------------------------------------------

function rustPackages() {
  let raw;
  try {
    raw = execFileSync(
      "cargo",
      ["metadata", "--format-version", "1", "--locked"],
      { cwd: REPO, encoding: "utf8", maxBuffer: 256 * 1024 * 1024 },
    );
  } catch (error) {
    fail(
      2,
      "`cargo metadata` failed. It needs a working cargo and, on a cold registry\n" +
        "cache, network access: reading a manifest requires having downloaded the\n" +
        "crate, including the Linux- and Windows-only ones this machine never builds.\n\n" +
        String(error.stderr || error.message).trim(),
    );
  }

  const { packages } = JSON.parse(raw);
  const external = packages.filter((pkg) => pkg.source);

  // The notices file states that every crate's origin is a published crates.io
  // release. A git dependency would make that false, and would also mean a crate
  // whose licence nobody can check at a stable URL.
  const nonRegistry = external.filter((pkg) => !pkg.source.startsWith("registry+"));
  if (nonRegistry.length > 0) {
    fail(
      1,
      "These crates do not come from a registry, which contradicts the notices\n" +
        "file's statement that every crate's origin is a published crates.io release:\n" +
        nonRegistry.map((p) => `  ${p.name} ${p.version} — ${p.source}`).join("\n"),
    );
  }

  const unlicensed = external.filter((pkg) => !pkg.license);
  if (unlicensed.length > 0) {
    fail(
      1,
      "These crates declare no `license` field. Establish what each one is licensed\n" +
        "under, then either record it in NO_DECLARED_LICENCE with the evidence or\n" +
        "remove the dependency:\n" +
        unlicensed
          .map((p) => `  ${p.name} ${p.version} — license_file: ${p.license_file ?? "none"}`)
          .join("\n"),
    );
  }

  return external.map((pkg) => ({
    name: pkg.name,
    version: pkg.version,
    ...renderLicence(pkg.name, pkg.license),
  }));
}

function npmPackages() {
  let lock;
  try {
    lock = JSON.parse(readFileSync(PACKAGE_LOCK, "utf8"));
  } catch (error) {
    fail(2, `Could not read ${PACKAGE_LOCK}: ${error.message}`);
  }

  const out = new Map();
  const unlicensed = [];

  for (const [path, entry] of Object.entries(lock.packages ?? {})) {
    const marker = path.lastIndexOf("node_modules/");
    if (marker === -1) continue; // the root project itself
    if (entry.link) continue; // workspace symlink, not a third-party package

    const name = path.slice(marker + "node_modules/".length);

    if (!entry.license) {
      if (!NO_DECLARED_LICENCE.has(name)) {
        unlicensed.push(`  ${name} ${entry.version ?? "?"}`);
        continue;
      }
    }

    // One package can occupy several lockfile paths at the same version: a
    // dependency that bundles its own copy appears both as `node_modules/x` and
    // as `node_modules/host/node_modules/x`. That is one package under one
    // licence, so it earns one row. Keying on name@version rather than on the
    // path is what keeps the count this file states equal to the rows it lists.
    const key = `${name}@${entry.version}`;
    if (out.has(key)) continue;

    out.set(key, { name, version: entry.version, ...renderLicence(name, entry.license) });
  }

  if (unlicensed.length > 0) {
    fail(
      1,
      "These npm packages declare no `license` field. Open the package's own licence\n" +
        "file, then either record it in NO_DECLARED_LICENCE with the evidence or\n" +
        "remove the dependency:\n" +
        unlicensed.join("\n"),
    );
  }

  return [...out.values()];
}

/**
 * Produce the two strings a package contributes — its row in the full list and
 * its bucket in the summary — plus the SPDX expression the policy check reads.
 * They differ only for a package with no declared licence, where the row has to
 * say where the answer came from.
 */
function renderLicence(name, declared) {
  const override = declared ? undefined : NO_DECLARED_LICENCE.get(name);
  if (override) {
    return { spdx: override.spdx, row: override.row, summary: override.summary, backtick: false };
  }
  return { spdx: declared, row: declared, summary: declared, backtick: true };
}

// --- Policy -----------------------------------------------------------------

function checkPolicy(all) {
  const violations = [];
  const unreviewed = [];
  const staleReviews = new Set(REVIEWED);

  for (const pkg of all) {
    const { permissive, forbidden } = classify(pkg.spdx);
    const review = reviewFor(pkg.name);

    if (!permissive) {
      if (forbidden.length > 0 && !review) {
        violations.push(
          `  ${pkg.name} ${pkg.version} — ${pkg.spdx} (matches ${forbidden.join(", ")})`,
        );
        continue;
      }
      if (!review) {
        unreviewed.push(`  ${pkg.name} ${pkg.version} — ${pkg.spdx}`);
        continue;
      }
    }

    if (review) staleReviews.delete(review);
  }

  if (violations.length > 0) {
    fail(
      1,
      "A dependency is under a licence this project does not ship. THIRD-PARTY-NOTICES.md\n" +
        "and NOTICE both state that none is; one of the two has to change:\n" +
        violations.join("\n"),
    );
  }

  if (unreviewed.length > 0) {
    fail(
      1,
      "These packages are not under a plainly permissive licence and have not been\n" +
        "reviewed. Work out what obligation each one carries, write it into the\n" +
        '"Obligations we carry" table in THIRD-PARTY-NOTICES.md and into NOTICE, then\n' +
        "add it to REVIEWED in this script with the same reason:\n" +
        unreviewed.join("\n"),
    );
  }

  // A review entry for a package that is no longer a dependency, or that has
  // changed to a permissive licence, means the prose in the notices file is now
  // describing an obligation this project does not carry. That is the same class
  // of error as a missing entry, so it is reported the same way.
  if (staleReviews.size > 0) {
    fail(
      1,
      "REVIEWED lists packages that are no longer a non-permissive dependency. Remove\n" +
        'them here and from the "Obligations we carry" table and NOTICE:\n' +
        [...staleReviews].map((entry) => `  ${entry.name ?? `${entry.prefix}*`}`).join("\n"),
    );
  }
}

// --- Rendering --------------------------------------------------------------

// Every ordering in this file compares strings by code unit rather than with
// `localeCompare`. Collation is the property that matters here, not readability:
// `localeCompare` resolves against whatever ICU data and default locale the
// runtime has, so the same input can produce a different order on a contributor's
// machine and on a CI runner. A --check that byte-compares would then fail for a
// reason nobody can reproduce. Code-unit order is uglier around `-` and `_` and
// is identical everywhere.
const byCodeUnit = (a, b) => (a < b ? -1 : a > b ? 1 : 0);

// Descending by count, then descending by licence. The second key is arbitrary
// but has to be *some* total order, or two licences with equal counts swap
// places between runs and produce a diff that means nothing. It compares the
// licence itself, not the rendered cell: sorting on the backticked text puts
// `Apache-2.0` on the wrong side of `Apache-2.0 OR ISC OR MIT`.
function summaryTable(packages, unit) {
  const counts = new Map();
  const cells = new Map();
  for (const pkg of packages) {
    counts.set(pkg.summary, (counts.get(pkg.summary) ?? 0) + 1);
    cells.set(pkg.summary, pkg.backtick ? `\`${pkg.summary}\`` : pkg.summary);
  }

  const rows = [...counts.entries()].sort((a, b) =>
    b[1] !== a[1] ? b[1] - a[1] : byCodeUnit(b[0], a[0]),
  );

  return [
    `| Licence | ${unit} |`,
    "|---|---|",
    ...rows.map(([licence, count]) => `| ${cells.get(licence)} | ${count} |`),
  ].join("\n");
}

// Ascending by name, then by version. Version ordering is numeric where both
// sides are numeric, so 4.4.0 sorts before 5.14.0 and 5.1.1 before 11.3.3 — a
// plain string sort gets the second of those backwards.
function fullTable(packages, unit) {
  const sorted = [...packages].sort(
    (a, b) => byCodeUnit(a.name, b.name) || compareVersions(a.version, b.version),
  );

  return [
    `| ${unit} | Version | Licence |`,
    "|---|---|---|",
    ...sorted.map((p) => `| \`${p.name}\` | ${p.version} | ${p.row} |`),
  ].join("\n");
}

function compareVersions(a = "", b = "") {
  const pa = a.split(/[.+-]/);
  const pb = b.split(/[.+-]/);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const x = pa[i] ?? "";
    const y = pb[i] ?? "";
    const nx = Number(x);
    const ny = Number(y);
    if (Number.isInteger(nx) && Number.isInteger(ny) && x !== "" && y !== "") {
      if (nx !== ny) return nx - ny;
    } else if (x !== y) {
      return x < y ? -1 : 1;
    }
  }
  return 0;
}

function render(rust, npm) {
  const coverage =
    `**Coverage: ${rust.length} of ${rust.length} Rust crates and ` +
    `${npm.length} of ${npm.length} npm packages. No unknowns.**`;

  const inventory = [
    `## Rust crates (${rust.length})`,
    "",
    "### Summary by declared licence",
    "",
    summaryTable(rust, "Crates"),
    "",
    "### Full list",
    "",
    fullTable(rust, "Crate"),
    "",
    `## npm packages (${npm.length})`,
    "",
    "### Summary by declared licence",
    "",
    summaryTable(npm, "Packages"),
    "",
    "### Full list",
    "",
    fullTable(npm, "Package"),
    "",
  ].join("\n");

  return { coverage, inventory };
}

// --- Splicing ---------------------------------------------------------------

// The two generated regions are located by the text already in the file rather
// than by inserted markers, so the published document carries no scaffolding. A
// missing or duplicated anchor is an error, never a silent no-op: rewriting the
// wrong region, or none, would be invisible in the output.
function splice(existing, { coverage, inventory }) {
  const coverageMatches = existing.match(/^\*\*Coverage:.*$/gm) ?? [];
  if (coverageMatches.length !== 1) {
    fail(
      2,
      `Expected exactly one line starting with "**Coverage:" in THIRD-PARTY-NOTICES.md, ` +
        `found ${coverageMatches.length}. The anchors this script relies on have moved; ` +
        `fix them here before regenerating.`,
    );
  }

  const inventoryStarts = [...existing.matchAll(/^## Rust crates \(\d+\)$/gm)];
  if (inventoryStarts.length !== 1) {
    fail(
      2,
      `Expected exactly one "## Rust crates (N)" heading in THIRD-PARTY-NOTICES.md, ` +
        `found ${inventoryStarts.length}. The anchors this script relies on have moved; ` +
        `fix them here before regenerating.`,
    );
  }

  return (
    existing.slice(0, inventoryStarts[0].index).replace(/^\*\*Coverage:.*$/m, coverage) + inventory
  );
}

// --- Reporting --------------------------------------------------------------

// Reads the committed tables back out so that --check can say which package
// changed, rather than which line changed. A dependency bump moves hundreds of
// lines in the summary tables for one real edit; a line diff buries it.
function parseCommitted(markdown, heading, until) {
  const start = markdown.indexOf(heading);
  if (start === -1) return null;
  const end = until ? markdown.indexOf(until, start) : markdown.length;
  const section = markdown.slice(start, end === -1 ? markdown.length : end);
  const rows = new Map();
  for (const m of section.matchAll(/^\| `([^`]+)` \| ([^ |]+) \| ([^|]+) \|$/gm)) {
    rows.set(`${m[1]}@${m[2]}`, m[3].trim());
  }
  return rows;
}

function reportDrift(existing, rust, npm) {
  const sections = [
    ["Rust", parseCommitted(existing, "## Rust crates", "## npm packages"), rust],
    ["npm", parseCommitted(existing, "## npm packages", null), npm],
  ];

  for (const [label, committed, generated] of sections) {
    if (!committed) continue;
    const now = new Map(generated.map((p) => [`${p.name}@${p.version}`, p.row]));

    const added = [...now.keys()].filter((k) => !committed.has(k));
    const removed = [...committed.keys()].filter((k) => !now.has(k));
    const changed = [...now.entries()].filter(
      ([k, v]) => committed.has(k) && committed.get(k) !== v,
    );

    if (added.length + removed.length + changed.length === 0) continue;

    console.error(`\n${label}: ${added.length} added, ${removed.length} removed, ${changed.length} relicensed`);
    for (const k of added.slice(0, 20)) console.error(`  + ${k} — ${now.get(k)}`);
    if (added.length > 20) console.error(`  + … and ${added.length - 20} more`);
    for (const k of removed.slice(0, 20)) console.error(`  - ${k}`);
    if (removed.length > 20) console.error(`  - … and ${removed.length - 20} more`);
    for (const [k, v] of changed.slice(0, 20)) {
      console.error(`  ~ ${k} — ${committed.get(k)} -> ${v}`);
    }
    if (changed.length > 20) console.error(`  ~ … and ${changed.length - 20} more`);
  }
}

// Throwing rather than exiting, so that the checks above can be exercised by
// dev/generate-third-party-notices.test.mjs. A gate nobody can watch fail is a
// gate nobody should trust; `process.exit` inside a library function would take
// the test runner down with it and make that impossible to write.
class Refusal extends Error {
  constructor(code, message) {
    super(message);
    this.code = code;
  }
}

function fail(code, message) {
  throw new Refusal(code, message);
}

// --- Main -------------------------------------------------------------------

export function main(argv) {
  const check = argv.includes("--check");
  const outIndex = argv.indexOf("--out");

  if (outIndex !== -1 && !argv[outIndex + 1]) fail(2, "--out needs a path.");
  if (check && outIndex !== -1) fail(2, "--check and --out are mutually exclusive.");

  const outPath = outIndex === -1 ? NOTICES : resolve(argv[outIndex + 1]);

  const rust = rustPackages();
  const npm = npmPackages();
  checkPolicy([...rust, ...npm]);

  let existing;
  try {
    existing = readFileSync(NOTICES, "utf8");
  } catch (error) {
    fail(2, `Could not read ${NOTICES}: ${error.message}`);
  }

  const generated = splice(existing, render(rust, npm));

  console.log(
    `${rust.length} Rust crates (Cargo.lock, all platforms), ` +
      `${npm.length} npm packages (package-lock.json, all platforms). ` +
      `${REVIEWED.length} reviewed non-permissive entries, 0 unknown.`,
  );

  if (check) {
    if (generated === existing) {
      console.log("THIRD-PARTY-NOTICES.md is up to date.");
      return 0;
    }
    console.error("\nTHIRD-PARTY-NOTICES.md is out of date.");
    reportDrift(existing, rust, npm);
    console.error("\nRun dev/generate-third-party-notices.mjs to update it.");
    return 1;
  }

  writeFileSync(outPath, generated);
  console.log(`Wrote ${outPath}.`);
  return 0;
}

// Run only when invoked as a program. Imported — by the test beside it — this
// file defines functions and does nothing else.
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    process.exit(main(process.argv.slice(2)));
  } catch (error) {
    if (error instanceof Refusal) {
      console.error(`${error.message}\n`);
      process.exit(error.code);
    }
    throw error;
  }
}

export { Refusal, classify, compareVersions, fullTable, npmPackages, reviewFor, splice, summaryTable, PERMISSIVE, REVIEWED, NOTICES, PACKAGE_LOCK, REPO };
