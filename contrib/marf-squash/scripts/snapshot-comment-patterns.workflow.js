export const meta = {
  name: 'snapshot-comment-patterns',
  description: 'Check whether two clarity-PR review patterns (get_ref/as_str borrowing; owning-module table-name constants) apply elsewhere in the burnchain/blocks/sortition/spv snapshot PRs',
  phases: [
    { title: 'Analyze', detail: 'per-PR scan for both patterns' },
    { title: 'Verify', detail: 'adversarially verify each apply-recommended site' },
  ],
}

const PATTERNS = `
You are checking whether TWO specific review comments left on the clarity PR (#7307) apply to OTHER parts of the code in a different snapshot/squash PR. These are consensus-critical Rust files (Stacks blockchain; GPL-3.0; no rollback) — be precise and conservative.

CONTEXT: each PR adds offline "squash" copy logic under stackslib/src/chainstate/stacks/db/snapshot/<domain>.rs (+ tests) and touches its owning subsystem module. The clarity PR already applied BOTH patterns below to its own files; you are auditing a DIFFERENT domain to see if the same patterns apply to ITS code.

────────────────────────────────────────
PATTERN 1 — BORROW VIA get_ref().as_str() INSTEAD OF ALLOCATING A String.
Origin (clarity, cylewitruk): "For all of these visitor methods, since you're passing references to the visitor closures anyway, you should be able to use get_ref(n)?.as_str()? to get a reference into SQLite-owned memory instead of forcing an allocation/copy into String; then clone at the callsite if it needs ownership."
How clarity fixed it: visit_metadata_rows / visit_metadata_keys now do row.get_ref(0)?.as_str().map_err(rusqlite::Error::from)? and hand the visitor a borrowed &str (MetadataRow { key: &str, block_id: &str, value: &str }) instead of let key: String = row.get(0)?.

CHECK in THIS PR's row-reading code (any query_map closure, any \`while let Some(row) = rows.next()?\` loop, any custom visitor/iter helper added or modified by this PR — in the snapshot module AND in owning-module helpers this PR adds):
- Is a TEXT column read as \`let x: String = row.get(n)?\` / \`row.get::<_, String>(n)\` where the value is used TRANSIENTLY (passed to a closure, compared, parsed, format!-interpolated, or used to look up in a set) and NOT stored with ownership? Then get_ref(n)?.as_str()? would avoid the heap allocation.
DO NOT FLAG (not applicable):
- Values immediately moved into a Vec<String> / HashSet<String> / returned as owned String — borrowing gains nothing (you'd still .to_string()).
- Typed reads (StacksBlockId, BurnchainHeaderHash, SortitionId, ConsensusHash, u64, i64, bool, Vec<u8> blob) — these are not String allocations.
- Pure \`INSERT ... SELECT\` server-side copies (execute_copy_specs / TableCopySpec) — no Rust-side row materialization at all.
Be precise: cite the exact file:line and the read, and state whether the value is transient (flag) or owned/collected (skip).

────────────────────────────────────────
PATTERN 2 — KEEP TABLE NAMES WITH THE OWNING CODE (expose name constants in the owning module).
Origin (clarity, cylewitruk): on \`pub(super) const CLARITY_SIDE_TABLES: &[&str] = &["data_table","metadata_table"];\` — "I'd probably move this into clarity's sqlite.rs as well, or expose constants there like DATA_TABLE_NAME and METADATA_TABLE_NAME and compose them into the array slice here -- just to keep the table names with the owning code."
How clarity fixed it: added \`pub const DATA_TABLE_NAME\`/\`METADATA_TABLE_NAME\` in clarity's OWNING module (clarity/src/vm/database/sqlite.rs), re-exported them, and built \`CLARITY_SIDE_TABLES = &[DATA_TABLE_NAME, METADATA_TABLE_NAME]\` in the snapshot module.

CHECK in THIS PR:
- Does the snapshot module hardcode table-name string LITERALS for tables OWNED by another subsystem? Two places to look:
  (a) the clone-schema array — this PR's equivalent of CLARITY_SIDE_TABLES (e.g. \`REQUIRED_TABLES\`, \`NAKAMOTO_STAGING_TABLES\`) passed to clone_schemas_from_source / unclassified_tables.
  (b) the \`TableCopySpec { table: "<name>", ... }\` fields, and any other inline \`"<table>"\` / \`INSERT INTO <table>\` / \`FROM src.<table>\` literals.
- Does the OWNING module (burnchains/db.rs, chainstate/burn/db/sortdb.rs, burnchains/bitcoin/spv.rs, chainstate/nakamoto/staging_blocks.rs) ALREADY define name constants for these tables? (grep for the table name; check if it's only a literal inside CREATE TABLE schema SQL, or already a const.)
- Is exposing/using owning-module name constants a CLEAN small change (especially since this PR likely ALREADY modifies the owning module, so adding a \`pub const\` there is low-friction) or an INVASIVE one (e.g. the names only exist as literals buried in big schema SQL strings, used in many places)?
TENSION TO RESOLVE: a prior review decided the SQL inside TableCopySpec deliberately stays in the snapshot module. This comment is NARROWER — it's about the table NAME identifiers (constants), not moving the SQL. Decide whether centralizing just the names is warranted and consistent here, distinct from moving SQL. It is legitimate to conclude the clone-schema array (case a) should use constants while the TableCopySpec SQL (case b) keeps inline names — or that it's not worth it. Justify.
`

const FINDINGS_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  properties: {
    pr: { type: 'string' },
    pattern1_summary: { type: 'string', description: 'One-paragraph verdict on PATTERN 1 for this PR' },
    pattern2_summary: { type: 'string', description: 'One-paragraph verdict on PATTERN 2 for this PR, incl. whether the owning module already has table-name constants and whether this PR already touches that module' },
    findings: {
      type: 'array',
      items: {
        type: 'object',
        additionalProperties: false,
        properties: {
          pattern: { type: 'string', enum: ['P1-borrow', 'P2-table-names'] },
          title: { type: 'string' },
          status: { type: 'string', enum: ['change-needed', 'not-applicable', 'judgment-call'] },
          evidence: { type: 'string', description: 'exact file:line + snippet' },
          proposed_change: { type: 'string', description: 'concrete edit, or "none"' },
          effort: { type: 'string', enum: ['trivial', 'small', 'invasive'] },
          confidence: { type: 'string', enum: ['high', 'medium', 'low'] },
        },
        required: ['pattern', 'title', 'status', 'evidence', 'proposed_change', 'effort', 'confidence'],
      },
    },
  },
  required: ['pr', 'pattern1_summary', 'pattern2_summary', 'findings'],
}

const VERDICT_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  properties: {
    real: { type: 'boolean', description: 'true if the pattern genuinely applies here and the change is warranted+safe' },
    reasoning: { type: 'string' },
    apply_recommendation: { type: 'string', enum: ['apply', 'skip', 'needs-human-judgment'] },
    notes: { type: 'string', description: 'precise file:line and the exact minimal edit, or why to skip' },
  },
  required: ['real', 'reasoning', 'apply_recommendation', 'notes'],
}

const PRS = [
  {
    key: 'burnchain', pr: '#7317', wt: '/home/franc/workspace/stacks-core-snapshot-burnchain',
    files: 'snapshot/burnchain.rs (REQUIRED_TABLES, burnchain_copy_specs, read_squashed_sortition_canonical_set, populate_canonical_burn_hashes), snapshot/tests/burnchain.rs; owning module stackslib/src/burnchains/db.rs (this PR adds count_canonical_burn_hashes_missing_from + test fixtures here)',
  },
  {
    key: 'blocks', pr: '#7320', wt: '/home/franc/workspace/stacks-core-snapshot-blocks',
    files: 'snapshot/blocks.rs (NAKAMOTO_STAGING_TABLES, copy_epoch2_block_files, microblock-stream reads get_confirmed_microblock_stream/derive_confirmed_microblock_set, copy_nakamoto_staging_blocks); owning module stackslib/src/chainstate/nakamoto/staging_blocks.rs (this PR adds pub(crate) mod squash here)',
  },
  {
    key: 'sortition', pr: '#7311', wt: '/home/franc/workspace/stacks-core-snapshot-sortition',
    files: 'snapshot/sortition.rs (TableCopySpec table names, populate_canonical_sortitions / bulk_read_squashed_blocks reads, any clone-schema array); owning module stackslib/src/chainstate/burn/db/sortdb.rs (this PR adds SortitionTipCopyBoundary + squash query methods here)',
  },
  {
    key: 'spv', pr: '#7319', wt: '/home/franc/workspace/stacks-core-snapshot-spv',
    files: 'snapshot/spv.rs (REQUIRED_TABLES = ["headers","db_config","chain_work"], spv_copy_specs); owning module stackslib/src/burnchains/bitcoin/spv.rs (this PR adds num_complete_chain_work_intervals + test_insert_chain_work here)',
  },
]

function analysisPrompt(p) {
  return `You are auditing PR ${p.pr} (${p.key}) — one of several MARF snapshot/squash PRs for the Stacks blockchain (consensus-critical; no rollback).

Worktree (read with ABSOLUTE paths under here; you may also \`git -C ${p.wt} diff\` / \`git -C ${p.wt} log\`): ${p.wt}
Relevant files: ${p.files}

The clarity PR (#7307) received two review comments and fixed both in its own files. The user wants to know if the SAME two patterns apply to OTHER parts of THIS PR's code. Evaluate both patterns below against the ACTUAL current code (committed + uncommitted) in this worktree.

${PATTERNS}

RULES:
- READ-ONLY. Do not modify any file.
- Cite exact file:line + a 1-3 line snippet for every finding.
- Status: "change-needed" (pattern clearly applies + worth doing), "judgment-call" (applies but a taste/scope decision), or "not-applicable" (with a one-line why).
- Be conservative — prefer accuracy over volume; this is consensus-critical code. It is a perfectly good answer that one or both patterns do NOT apply to this PR.
- Distinguish effort: trivial / small / invasive.
Return the structured object (pattern1_summary, pattern2_summary, and a findings array).`
}

function verifyPrompt(p, f) {
  return `Adversarial verifier for consensus-critical Rust PR ${p.pr} (${p.key}). A first-pass audit claims a clarity-PR review pattern applies here. REFUTE if wrong/overstated/already-handled/not-worth-it. Default skeptical.

Worktree (ABSOLUTE paths): ${p.wt}

CLAIM:
- pattern: ${f.pattern}
- title: ${f.title}
- status: ${f.status}
- evidence: ${f.evidence}
- proposed change: ${f.proposed_change}
- effort: ${f.effort}

Read the real code at the cited location and decide:
- For P1-borrow: is the read genuinely TRANSIENT (so get_ref().as_str() avoids an allocation), or is the value collected/owned (borrowing gains nothing → skip)? Does the closure/return signature actually allow a borrowed &str, or would the lifetime force a clone anyway? Is it on a hot/unbounded path (matters) or a one-row/test path (cosmetic)?
- For P2-table-names: are these tables owned by another subsystem? Does the owning module already have constants? Is adding/using them clean (esp. if this PR already edits that module) or invasive? Does it conflict with the deliberate "TableCopySpec SQL stays in snapshot" exception, or is it the narrower name-constant centralization that's fine? Be consistent with how clarity did it.
- Is the change safe + behavior-preserving for consensus-critical code?
apply_recommendation: "apply" (clear, safe, warranted, low-risk), "skip" (not real / not worth it), "needs-human-judgment" (real but a taste/scope call). In notes give precise file:line + exact minimal edit, or why to skip.
Return the structured verdict.`
}

phase('Analyze')
const analyses = await pipeline(
  PRS,
  (p) => agent(analysisPrompt(p), { label: `scan:${p.key}`, phase: 'Analyze', schema: FINDINGS_SCHEMA }),
  (analysis, p) => {
    if (!analysis) return { pr: p.key, prNum: p.pr, wt: p.wt, pattern1_summary: 'ANALYSIS FAILED', pattern2_summary: '', verified: [] }
    const actionable = (analysis.findings || []).filter((f) => f.status === 'change-needed' || f.status === 'judgment-call')
    if (actionable.length === 0) {
      return { pr: p.key, prNum: p.pr, wt: p.wt, pattern1_summary: analysis.pattern1_summary, pattern2_summary: analysis.pattern2_summary, all_findings: analysis.findings, verified: [] }
    }
    return parallel(
      actionable.map((f) => () =>
        agent(verifyPrompt(p, f), { label: `verify:${p.key}:${f.pattern}`, phase: 'Verify', schema: VERDICT_SCHEMA })
          .then((v) => ({ finding: f, verdict: v })),
      ),
    ).then((verdicts) => ({
      pr: p.key, prNum: p.pr, wt: p.wt,
      pattern1_summary: analysis.pattern1_summary,
      pattern2_summary: analysis.pattern2_summary,
      all_findings: analysis.findings,
      verified: verdicts.filter(Boolean),
    }))
  },
)

return analyses.filter(Boolean)
