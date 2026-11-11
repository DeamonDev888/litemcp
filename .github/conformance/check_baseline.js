#!/usr/bin/env node
/**
 * Gate de conformance — compare les échecs SCORÉS de la suite officielle
 * (`--requirements 2026-07-28`) à la baseline datée
 * `.github/conformance/baseline-2026-07-28.json`.
 *
 * Usage : node check_baseline.js <results-dir>
 *
 * Le <results-dir> est le `--output-dir` du harness : un sous-dossier par
 * scénario (`server-<scenario>-<timestamp>/checks.json`, tableau de checks
 * `SUCCESS|FAILURE|WARNING|SKIPPED|INFO`). Un scénario est en échec dès
 * qu'un de ses checks porte `status === "FAILURE"` — c'est exactement le
 * critère d'exit code du harness (`src/index.ts`).
 *
 * La distinction scoré / non-scoré vient de la liste `not_scored_scenarios`
 * de la baseline, copie gelée de la section `not_scored` du fichier
 * `requirements/2026-07-28.yaml` du harness ÉPINGLÉ
 * (`@modelcontextprotocol/conformance@0.2.0-alpha.12`) — mise à jour
 * uniquement si la version du harness change.
 *
 * Le gate échoue si :
 *   1. un scénario scoré échoue sans être dans `expected_failures`
 *      (NOUVELLE RÉGRESSION) ;
 *   2. une entrée de `expected_failures` ne correspond plus à un échec
 *      (BASELINE PÉRIMÉE — elle doit rester vide ou strictement décroissante) ;
 *   3. un scénario scoré gelé (`scored_scenarios`, copie de la section
 *      `server:` de `requirements/2026-07-28.yaml`) est ABSENT des résultats
 *      (exécution tronquée — pas de vert falsifié par un run interrompu) ;
 *   4. un scénario exécuté n'appartient ni à la liste scorée gelée ni à
 *      `not_scored_scenarios` (désynchronisation harness ↔ baseline — le
 *      harness est épinglé par version exacte, toute divergence est un
 *      changement volontaire qui exige une revue de la baseline).
 *
 * Les entrées de baseline sans `date` ou sans `reason` sont refusées
 * (aucune baseline d'échecs permanente sans justification datée).
 */
'use strict';

const fs = require('fs');
const path = require('path');

const resultsDir = process.argv[2];
if (!resultsDir || !fs.existsSync(resultsDir)) {
  console.error('usage: node check_baseline.js <results-dir>');
  process.exit(2);
}

const baselinePath = path.join(__dirname, 'baseline-2026-07-28.json');
// Tolérant au BOM (éditeurs Windows).
const baseline = JSON.parse(
  fs.readFileSync(baselinePath, 'utf8').replace(/^\uFEFF/, '')
);

// 1. Baseline valide : chaque entrée est datée et justifiée.
for (const entry of baseline.expected_failures || []) {
  if (!entry.scenario || !entry.date || !entry.reason) {
    console.error(
      `BASELINE INVALIDE : l'entrée ${JSON.stringify(
        entry
      )} doit porter scenario, date et reason (aucune baseline d'échecs permanente sans justification datée).`
    );
    process.exit(1);
  }
}

const notScored = new Set(baseline.not_scored_scenarios || []);

// 2. Échecs effectifs par scénario, depuis les checks.json du dossier.
const failing = new Set();
const seen = new Set();
for (const dir of fs.readdirSync(resultsDir)) {
  const m = dir.match(/^server-([a-z0-9-]+)-\d{4}-\d{2}-\d{2}T/);
  if (!m) continue;
  const scenario = m[1];
  seen.add(scenario);
  const checksFile = path.join(resultsDir, dir, 'checks.json');
  if (!fs.existsSync(checksFile)) continue;
  let checks;
  try {
    checks = JSON.parse(fs.readFileSync(checksFile, 'utf8'));
  } catch (error) {
    console.error(`checks.json illisible pour ${scenario} : ${error.message}`);
    process.exit(2);
  }
  if (Array.isArray(checks) && checks.some((c) => c && c.status === 'FAILURE')) {
    failing.add(scenario);
  }
}

const scoredFailures = [...failing].filter((s) => !notScored.has(s)).sort();
const expected = new Set((baseline.expected_failures || []).map((e) => e.scenario));

const newRegressions = scoredFailures.filter((s) => !expected.has(s));
const stale = [...expected].filter((s) => !failing.has(s)).sort();

// 3. Complétude de l'exécution : tout scénario scoré gelé doit avoir tourné.
const scoredFrozen = baseline.scored_scenarios || [];
const missing = scoredFrozen.filter((s) => !seen.has(s));
// 4. Désynchronisation harness ↔ baseline : tout scénario exécuté doit être
// connu (scoré gelé ou not_scored).
const unknown = [...seen].filter(
  (s) => !scoredFrozen.includes(s) && !notScored.has(s)
);
const expectedCount = baseline.expected_server_scenario_count;
const countMismatch =
  typeof expectedCount === 'number' && seen.size !== expectedCount;

console.log(
  `Baseline ${baseline.revision} (harness ${baseline.harness}, mise à jour ${baseline.updated}) — ${baseline.expected_failures.length} entrée(s) justifiée(s), ${seen.size}/${expectedCount ?? '?'} scénario(s) attendu(s) exécuté(s), ${scoredFrozen.length} scénario(s) scoré(s).`
);
console.log(
  `Échecs scorés effectifs : ${scoredFailures.length ? scoredFailures.join(', ') : 'aucun'}`
);

let failed = false;
if (missing.length > 0) {
  failed = true;
  console.error(
    `\nGATE ÉCHOUÉ — EXÉCUTION TRONQUÉE (${missing.length} scénario(s) scoré(s) absent(s) des résultats) :`
  );
  for (const s of missing) {
    console.error(
      `  ! ${s} — le run s'est-il interrompu ? Aucun vert ne peut être prononcé sur un run incomplet.`
    );
  }
}
if (unknown.length > 0) {
  failed = true;
  console.error(
    `\nGATE ÉCHOUÉ — SCÉNARIO(S) INCONNU(S) DE LA BASELINE (${unknown.length}) :`
  );
  for (const s of unknown) {
    console.error(
      `  ? ${s} — le harness épinglé a-t-il changé ? Geler la nouvelle liste dans la baseline (revue obligatoire).`
    );
  }
}
if (countMismatch) {
  failed = true;
  console.error(
    `\nGATE ÉCHOUÉ — DÉCOMPTE INCOHÉRENT : ${seen.size} scénario(s) exécuté(s), ${expectedCount} attendu(s) pour ${baseline.revision}.`
  );
}
if (newRegressions.length > 0) {
  failed = true;
  console.error(
    `\nGATE ÉCHOUÉ — NOUVELLE(S) RÉGRESSION(S) SCORÉE(S) (${newRegressions.length}) :`
  );
  for (const s of newRegressions) {
    console.error(
      `  ✗ ${s} — absente de ${path.basename(baselinePath)} ; corriger la lib ou ajouter une entrée datée et justifiée.`
    );
  }
}
if (stale.length > 0) {
  failed = true;
  console.error(
    `\nGATE ÉCHOUÉ — BASELINE PÉRIMÉE (${stale.length} entrée(s) devenue(s) verte(s)) :`
  );
  for (const s of stale) {
    console.error(
      `  ✓ ${s} — retirer l'entrée de la baseline (elle doit rester vide ou strictement décroissante).`
    );
  }
}

if (!failed) {
  console.log(
    '\nGATE OK — aucun échec scoré hors baseline, baseline sans entrée périmée.'
  );
  process.exit(0);
}
process.exit(1);
