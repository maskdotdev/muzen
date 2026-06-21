#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";

const PRODUCT_REVIEWER = "src/reviewer_kernel/autonomous_review.rs";
const DIRECT_SESSION_HARNESS = "bench/review-quality/run-production-review.mjs";
const DIRECT_SESSION_PROMPT = "bench/review-quality/prompts/direct-session-review.md";
const SYNTHETIC_ROOT = "bench/review-quality/synthetic";
const ANTI_CHEAT_ROOT = "bench/review-quality/anti-cheat";
const SYNTHETIC_MANIFEST = "bench/review-quality/synthetic-fixtures.json";
const ANTI_CHEAT_MANIFEST = "bench/review-quality/anti-cheat-fixtures.json";

const REQUIRED_LENSES = [
  {
    id: "concurrency_one_time_use",
    riskCategory: "one_time_use_race",
    syntheticFixture: "one-time-use-race",
    antiCheatFixture: "safe-one-time-use-race",
    productTerms: ["one-time-use", "atomic write", "compare-and-swap"],
    directHarnessTerms: ["one-time, limited-use", "atomic update", "compare-and-swap", "report-or-refute check"],
    positiveTerms: ["concurrent requests", "pre-consumed value"],
    antiCheatTerms: ["atomic", "concurrent requests"],
  },
  {
    id: "canonical_equivalence_normalization",
    riskCategory: "normalization_sensitive_comparison",
    syntheticFixture: "case-normalization-bypass",
    antiCheatFixture: "safe-case-normalization",
    productTerms: ["human-entered identifiers", "canonical form", "case or whitespace"],
    directHarnessTerms: ["human-entered identifiers", "case/whitespace canonical form"],
    positiveTerms: ["canonical-equivalent", "case or whitespace", "existing-record"],
    antiCheatTerms: ["canonicalization", "case or whitespace", "contract-equivalent"],
  },
  {
    id: "exact_repeat_idempotency",
    riskCategory: "duplicate_side_effect_batch",
    syntheticFixture: "duplicate-input-side-effects",
    antiCheatFixture: "safe-duplicate-input-side-effects",
    productTerms: ["exact repeated items in one caller batch", "both copies", "externally visible side effects"],
    directHarnessTerms: ["exact repeated items in one caller batch", "both copies", "externally visible side effects"],
    positiveTerms: ["exact repeated items in one caller batch", "both copies", "create records", "externally visible"],
    antiCheatTerms: ["deduplicated by stable key", "externally visible"],
  },
  {
    id: "interface_contract_drift",
    riskCategory: "boundary_contract_propagation",
    syntheticFixture: "boundary-contract-context-dropped",
    antiCheatFixture: "safe-boundary-contract-propagation",
    productTerms: ["changed contract carries required parameters", "reachable changed producer/consumer paths"],
    directHarnessTerms: [
      "changed contracts that carry required parameters",
      "reachable changed producer/consumer paths",
      "declare, implement, override, or call the same method symbol",
    ],
    positiveTerms: ["changed contract propagation paths", "reachable producers and consumers"],
    antiCheatTerms: ["reachable producers and consumers", "new required context"],
  },
  {
    id: "lookup_key_contract_drift",
    riskCategory: "lookup_key_contract_drift",
    syntheticFixture: "lookup-key-contract-drift",
    antiCheatFixture: "safe-lookup-key-contract-drift",
    productTerms: ["resolved identifier namespace", "original submitted key", "lookup_key_contract_drift"],
    directHarnessTerms: ["resolver converts a submitted identifier", "resolved identifier namespace", "original submitted key"],
    positiveTerms: ["submitted identifier", "resolved id", "original submitted identifier"],
    antiCheatTerms: ["resolved id", "original submitted identifier", "wrong lookup key"],
  },
  {
    id: "conditional_logic_inversion",
    riskCategory: "conditional_logic_inversion",
    syntheticFixture: "conditional-logic-inversion",
    antiCheatFixture: "safe-conditional-logic-inversion",
    productTerms: ["before/after truth table", "opposite wrong branch"],
    directHarnessTerms: ["before/after truth table", "opposite wrong branch", "true and false payload cases"],
    positiveTerms: ["before/after truth tables", "opposite wrong"],
    antiCheatTerms: ["before/after truth table", "opposite wrong"],
  },
  {
    id: "feature_flag_truth_table",
    riskCategory: "feature_flag_truth_table",
    syntheticFixture: "feature-flag-truth-table-inversion",
    antiCheatFixture: "safe-feature-flag-truth-table",
    productTerms: ["feature_flag_truth_table", "matching and non-matching requested values"],
    directHarnessTerms: ["feature flags, capability flags, or mode gates", "matching and non-matching requested values"],
    positiveTerms: ["feature flag", "truth table", "non-matching modes"],
    antiCheatTerms: ["feature decision", "requested-mode truth table", "matching flagged modes"],
  },
];

const errors = [];
const productText = fs.readFileSync(PRODUCT_REVIEWER, "utf8");
const directHarnessText = [
  fs.readFileSync(DIRECT_SESSION_HARNESS, "utf8"),
  fs.readFileSync(DIRECT_SESSION_PROMPT, "utf8"),
].join("\n");
const syntheticManifest = readJson(SYNTHETIC_MANIFEST);
const antiCheatManifest = readJson(ANTI_CHEAT_MANIFEST);
const syntheticByDir = new Map((syntheticManifest.fixtures || []).map((fixture) => [fixture.fixtureDir, fixture]));
const antiCheatByDir = new Map((antiCheatManifest.fixtures || []).map((fixture) => [fixture.fixtureDir, fixture]));

for (const lens of REQUIRED_LENSES) {
  checkLens(lens);
}

if (errors.length > 0) {
  console.error("Required review lens coverage check failed:");
  for (const error of errors) console.error(`- ${error}`);
  process.exit(1);
}

console.log(`Required review lens coverage passed: ${REQUIRED_LENSES.length} lens(es).`);

function checkLens(lens) {
  requireText(productText, lens.riskCategory, `${lens.id}.riskCategory`);
  for (const term of lens.productTerms) {
    requireText(productText, term, `${lens.id}.productTerm`);
  }
  for (const term of lens.directHarnessTerms) {
    requireText(directHarnessText, term, `${lens.id}.directHarnessTerm`);
  }

  const synthetic = syntheticByDir.get(lens.syntheticFixture);
  if (!synthetic) {
    errors.push(`${lens.id}.syntheticFixture missing: ${lens.syntheticFixture}`);
    return;
  }
  requireFixtureFiles(SYNTHETIC_ROOT, synthetic.fixtureDir, `${lens.id}.syntheticFixture`);
  if (synthetic.antiCheatFixture !== lens.antiCheatFixture) {
    errors.push(
      `${lens.id}.syntheticFixture points to ${synthetic.antiCheatFixture}, expected ${lens.antiCheatFixture}`,
    );
  }
  const syntheticText = fixtureText(synthetic);
  for (const term of lens.positiveTerms) {
    requireText(syntheticText, term, `${lens.id}.positiveTerm`);
  }

  const antiCheat = antiCheatByDir.get(lens.antiCheatFixture);
  if (!antiCheat) {
    errors.push(`${lens.id}.antiCheatFixture missing: ${lens.antiCheatFixture}`);
    return;
  }
  requireFixtureFiles(ANTI_CHEAT_ROOT, antiCheat.fixtureDir, `${lens.id}.antiCheatFixture`);
  const antiCheatText = fixtureText(antiCheat);
  for (const term of lens.antiCheatTerms) {
    requireText(antiCheatText, term, `${lens.id}.antiCheatTerm`);
  }
}

function requireFixtureFiles(root, fixtureDir, label) {
  for (const side of ["base", "head"]) {
    const sideRoot = path.join(root, fixtureDir, side);
    if (!fs.existsSync(sideRoot)) {
      errors.push(`${label}.${side} missing directory: ${sideRoot}`);
      continue;
    }
    if (listFiles(sideRoot).length === 0) {
      errors.push(`${label}.${side} has no fixture files: ${sideRoot}`);
    }
  }
}

function fixtureText(fixture) {
  const goldenText = fixture.golden && fs.existsSync(fixture.golden)
    ? fs.readFileSync(fixture.golden, "utf8")
    : "";
  return [
    fixture.id,
    fixture.fixtureDir,
    fixture.risk,
    fixture.mustNotDo,
    ...(fixture.requiredReviewerBehavior || []),
    goldenText,
  ]
    .filter(Boolean)
    .join("\n")
    .toLowerCase();
}

function requireText(text, term, label) {
  if (!text.toLowerCase().includes(term.toLowerCase())) {
    errors.push(`${label} missing ${JSON.stringify(term)}`);
  }
}

function listFiles(root) {
  if (!fs.existsSync(root)) return [];
  const stat = fs.statSync(root);
  if (stat.isFile()) return [root];
  const files = [];
  for (const entry of fs.readdirSync(root, { withFileTypes: true })) {
    const child = path.join(root, entry.name);
    if (entry.isDirectory()) files.push(...listFiles(child));
    else if (entry.isFile()) files.push(child);
  }
  return files;
}

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, "utf8"));
}
