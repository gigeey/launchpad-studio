#!/usr/bin/env node --test
//
// Tests for dev/generate-third-party-notices.mjs.
//
// WHY THIS EXISTS
// ---------------
// `--check` proves THIRD-PARTY-NOTICES.md matches what the generator produces
// today. It cannot prove the generator is right, because a bug in the licence
// classifier is baked into both sides of that comparison: if `classify` returned
// "permissive" for everything, `--check` would still pass, the file would still
// be internally consistent, and the sentence promising no GPL dependency would
// have quietly become an unchecked assertion again.
//
// So the tests that matter here are the ones that make the gate refuse. Most of
// this file feeds the classifier licences this project will not ship under and
// asserts that it says no.
//
// The rest checks the committed file against `frontend/package-lock.json`
// directly, without going through the generator at all. That half needs no
// cargo, no network and no `node_modules`, so it runs anywhere the repository is
// checked out.
//
// Run:  node --test dev/

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import {
  classify,
  compareVersions,
  fullTable,
  reviewFor,
  splice,
  summaryTable,
  NOTICES,
  PACKAGE_LOCK,
  REPO,
} from "./generate-third-party-notices.mjs";

const notices = readFileSync(NOTICES, "utf8");
const lock = JSON.parse(readFileSync(PACKAGE_LOCK, "utf8"));
const cargoLock = readFileSync(`${REPO}/Cargo.lock`, "utf8");

// --- The gate has to be able to refuse ---------------------------------------

test("a plain copyleft licence is not permissive", () => {
  for (const licence of [
    "GPL-3.0-only",
    "AGPL-3.0-or-later",
    "LGPL-2.1-or-later",
    "SSPL-1.0",
    "BUSL-1.1",
    "CDDL-1.0",
    "EPL-2.0",
    "OSL-3.0",
    "EUPL-1.2",
    "Elastic-2.0",
    "MPL-2.0",
  ]) {
    const { permissive } = classify(licence);
    assert.equal(permissive, false, `${licence} must not pass as permissive`);
  }
});

test("the forbidden families are recognised wherever they appear", () => {
  assert.deepEqual(classify("GPL-3.0-only").forbidden, ["GPL-3.0-only"]);
  assert.deepEqual(classify("MIT AND GPL-2.0-only").forbidden, ["GPL-2.0-only"]);
  // Present as an alternative, so it never attaches — but the classifier still
  // has to see it, because that is what puts the package in front of a human.
  assert.deepEqual(classify("MIT OR LGPL-2.1-or-later").forbidden, ["LGPL-2.1-or-later"]);
});

test("an OR expression passes on any permissive alternative", () => {
  assert.equal(classify("MIT OR Apache-2.0 OR LGPL-2.1-or-later").permissive, true);
  assert.equal(classify("(MPL-2.0 OR Apache-2.0)").permissive, true);
  // crates.io's pre-SPDX slash spelling means OR.
  assert.equal(classify("MIT/Apache-2.0").permissive, true);
  assert.equal(classify("Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT").permissive, true);
});

test("an AND expression requires every licence to be permissive", () => {
  assert.equal(classify("(MIT OR Apache-2.0) AND Unicode-3.0").permissive, true);
  assert.equal(classify("MIT AND MPL-2.0").permissive, false);
  // The strict rule applies to the whole expression, including the case where a
  // more careful parse would find the AND does not reach the copyleft term.
  assert.equal(classify("(MIT OR GPL-3.0-only) AND MIT").permissive, false);
});

test("an unknown licence is not assumed to be permissive", () => {
  assert.equal(classify("SomeCompany-Proprietary-1.0").permissive, false);
  assert.equal(classify("").permissive, false);
});

test("review entries match by exact name, and by family only where declared", () => {
  assert.ok(reviewFor("cssparser"), "an exact-name entry must match");
  assert.ok(reviewFor("lightningcss-linux-x64-musl"), "a prefix entry must match the family");
  assert.equal(reviewFor("cssparser-with-a-suffix"), undefined, "exact entries must not prefix-match");
  assert.equal(reviewFor("serde"), undefined);
});

test("a missing anchor is an error rather than a silent no-op", () => {
  const rendered = { coverage: "**Coverage: nothing.**", inventory: "## Rust crates (0)\n" };
  assert.throws(() => splice("# Notices\n\nNo anchors here.\n", rendered), /Coverage/);
  assert.throws(
    () => splice("**Coverage: x**\n\n## Rust crates (1)\n\n## Rust crates (2)\n", rendered),
    /Rust crates/,
  );
});

// --- Ordering has to be total and machine-independent ------------------------

test("versions order numerically, not as strings", () => {
  assert.ok(compareVersions("5.1.1", "11.3.3") < 0);
  assert.ok(compareVersions("0.9.0", "0.10.0") < 0);
  assert.ok(compareVersions("4.4.0", "5.14.0") < 0);
  assert.equal(compareVersions("1.2.3", "1.2.3"), 0);
  assert.ok(compareVersions("0.11.1+wasi-snapshot-preview1", "0.14.7") < 0);
});

test("table rendering is a pure function of the package set, not of its order", () => {
  const packages = [
    { name: "beta", version: "0.10.0", row: "MIT", summary: "MIT", backtick: true },
    { name: "beta", version: "0.9.0", row: "MIT", summary: "MIT", backtick: true },
    { name: "alpha", version: "1.0.0", row: "ISC", summary: "ISC", backtick: true },
  ];
  const shuffled = [packages[2], packages[0], packages[1]];

  assert.equal(fullTable(packages, "Crate"), fullTable(shuffled, "Crate"));
  assert.equal(summaryTable(packages, "Crates"), summaryTable(shuffled, "Crates"));

  const lines = fullTable(packages, "Crate").split("\n");
  assert.deepEqual(lines.slice(2), [
    "| `alpha` | 1.0.0 | ISC |",
    "| `beta` | 0.9.0 | MIT |",
    "| `beta` | 0.10.0 | MIT |",
  ]);
});

test("the summary sorts on the licence, not on the rendered cell", () => {
  const at = (licence) => ({ name: licence, version: "1", row: licence, summary: licence, backtick: true });
  const rows = summaryTable(
    [at("Apache-2.0"), at("Apache-2.0 OR ISC OR MIT")],
    "Crates",
  ).split("\n");
  // Descending by licence string: the longer expression precedes the bare one.
  // Sorting the backticked cells instead puts these the other way round.
  assert.deepEqual(rows.slice(2), [
    "| `Apache-2.0 OR ISC OR MIT` | 1 |",
    "| `Apache-2.0` | 1 |",
  ]);
});

// --- The committed file has to agree with the committed lockfiles ------------
//
// These read THIRD-PARTY-NOTICES.md as a reader would and check it against the
// inputs, rather than against the generator. A generator that stopped being run
// fails here.

function rowsOf(section) {
  return new Map(
    [...section.matchAll(/^\| `([^`]+)` \| ([^ |]+) \| ([^|]+) \|$/gm)].map((m) => [
      `${m[1]}@${m[2]}`,
      m[3].trim(),
    ]),
  );
}

const rustSection = notices.slice(notices.indexOf("## Rust crates"), notices.indexOf("## npm packages"));
const npmSection = notices.slice(notices.indexOf("## npm packages"));

test("the stated counts match the rows actually listed", () => {
  const coverage = notices.match(
    /^\*\*Coverage: (\d+) of (\d+) Rust crates and (\d+) of (\d+) npm packages\. No unknowns\.\*\*$/m,
  );
  assert.ok(coverage, "the coverage line must be present and in the expected form");

  const rust = rowsOf(rustSection).size;
  const npm = rowsOf(npmSection).size;

  assert.equal(Number(notices.match(/^## Rust crates \((\d+)\)$/m)[1]), rust);
  assert.equal(Number(notices.match(/^## npm packages \((\d+)\)$/m)[1]), npm);
  assert.deepEqual(coverage.slice(1).map(Number), [rust, rust, npm, npm]);
});

test("each summary table accounts for every row in its full list", () => {
  const total = (section) => {
    const table = section.slice(
      section.indexOf("### Summary by declared licence"),
      section.indexOf("### Full list"),
    );
    return [...table.matchAll(/\| (\d+) \|$/gm)].reduce((sum, m) => sum + Number(m[1]), 0);
  };

  assert.equal(total(rustSection), rowsOf(rustSection).size);
  assert.equal(total(npmSection), rowsOf(npmSection).size);
});

test("every npm package in the lockfile is listed, with the licence it declares", () => {
  const listed = rowsOf(npmSection);
  const missing = [];
  const wrong = [];
  const seen = new Set();

  for (const [path, entry] of Object.entries(lock.packages ?? {})) {
    const marker = path.lastIndexOf("node_modules/");
    if (marker === -1 || entry.link) continue;

    const key = `${path.slice(marker + "node_modules/".length)}@${entry.version}`;
    // A package can sit at more than one lockfile path at the same version when
    // a dependency bundles its own copy. The notices file lists it once, so
    // checking the first occurrence checks the whole of what it claims.
    if (seen.has(key)) continue;
    seen.add(key);

    if (!listed.has(key)) {
      missing.push(key);
      continue;
    }
    // A package with no declared licence is listed with a note saying where the
    // answer came from, so the row is checked to *contain* the licence rather
    // than to equal it.
    if (entry.license && !listed.get(key).includes(entry.license)) {
      wrong.push(`${key}: file says ${listed.get(key)}, lockfile says ${entry.license}`);
    }
    listed.delete(key);
  }

  assert.deepEqual(missing, [], "npm packages in the lockfile but not in the notices file");
  assert.deepEqual(wrong, [], "npm packages whose listed licence contradicts the lockfile");
  assert.deepEqual([...listed.keys()], [], "npm packages listed but no longer in the lockfile");
});

test("the counts quoted in README.md match the inventory", () => {
  // The same two numbers appear in the README's License section, where a reader
  // meets them first and where nothing else would notice them going stale.
  const readme = readFileSync(`${REPO}/README.md`, "utf8");
  const quoted = readme.match(/^(\d+) Rust crates and (\d+) npm packages,/m);
  assert.ok(quoted, "README.md must state both counts in the form the inventory uses");
  assert.equal(Number(quoted[1]), rowsOf(rustSection).size);
  assert.equal(Number(quoted[2]), rowsOf(npmSection).size);
});

test("the Rust crate count matches Cargo.lock", () => {
  // Every `[[package]]` with a `source` is a third-party crate; the ones without
  // are this workspace's own members, which are covered by LICENSE instead.
  const sourced = (cargoLock.match(/^source = /gm) ?? []).length;
  assert.equal(rowsOf(rustSection).size, sourced);
});
