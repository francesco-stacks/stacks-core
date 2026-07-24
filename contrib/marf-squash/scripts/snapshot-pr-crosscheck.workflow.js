export const meta = {
  name: 'snapshot-pr-crosscheck',
  description: 'Cross-check review-comment themes from clarity/blocks PRs against all 5 snapshot PRs and adversarially verify each change-needed finding',
  phases: [
    { title: 'Analyze', detail: 'one deep-analysis agent per PR vs the theme catalog' },
    { title: 'Verify', detail: 'adversarially verify each change-needed finding against the real code' },
  ],
}

const THEME_CATALOG = `
# Comment-theme catalog (derived from review comments on the clarity #7307 and blocks #7320 PRs, plus each PR's own reviewer comments)

These snapshot PRs all add "squash"/offline-snapshot copy logic under stackslib/src/chainstate/stacks/db/snapshot/<domain>.rs (+ tests/<domain>.rs) and touch an owning module. Reviewers (cylewitruk-stacks, benjamin-stacks, federico-stacks, Copilot) raised recurring themes. For EACH theme below, determine if it applies to the code you are auditing.

T1 — READ-ONLY SOURCE OPEN (no write lock / no WAL sidecar on source DBs).
 Origin: clarity PR. filter_required_contracts() opened the SOURCE MARF via read-write MARF::from_path(), which forces journal_mode=WAL and creates *.sqlite-wal/*.sqlite-shm and a write lock on the source. Fixed by opening the source strictly read-only: TrieFileStorage::open_readonly(...) for the MARF and sqlite_open(path, OpenFlags::SQLITE_OPEN_READ_ONLY, false) for the side DB.
 CHECK: does this PR open any *source* DB or MARF for reading via a read-write path (sqlite_open without READ_ONLY, MARF::from_path, Connection::open, marf_sqlite_open, etc.)? Source files may be a live node's files and must NEVER be write-locked or get WAL/SHM sidecars. NOTE: reading a source by ATTACHing it inside with_offline_write_session is the accepted pattern; the concern is STANDALONE opens of a source path.

T2 — SQL / TABLE-NAME DOMAIN OWNERSHIP (foreign-domain SQL belongs in the owning module).
 Origin: clarity (move SQL into clarity sqlite.rs; expose DATA_TABLE_NAME/METADATA_TABLE_NAME constants), sortition (moved queries into sortdb.rs), blocks (squash SQL builders moved into a squash mod in staging_blocks.rs).
 CHECK: does snapshot/<domain>.rs (or its tests) contain raw SQL strings, or hardcoded table/column names, that belong to the owning subsystem (e.g. burnchains/db.rs, chainstate/burn/db/sortdb.rs, the SPV headers module)? Such SQL/identifiers should live in the owning module (as helpers or exposed name constants). EXCEPTION: the generic TableCopySpec declarations deliberately stay in the snapshot module — do NOT flag those.

T3 — NATIVE / OWNING ERROR TYPES (don't wrap everything in a foreign catch-all error).
 Origin: clarity (avoid returning clarity's VmExecutionError from snapshot helpers; prefer rusqlite::Error or the owning module's native error; the "one error to rule them all" breaks module isolation). Project standard: foreign-domain helpers should return rusqlite::Error or the owning module's Error, and use u64_to_sql(...) rather than "as i64" casts.
 CHECK: do owning-module helpers added by this PR return a foreign catch-all error inappropriately, or cast integers with "as i64"/"as u64" where u64_to_sql / typed conversion is expected?

T4 — NO HARDCODED SCHEMA VERSION (use the LATEST / source-of-truth constant).
 Origin: blocks Copilot — a test hardcoded the Nakamoto staging schema version 5; fixed to reference NAKAMOTO_STAGING_DB_SCHEMA_LATEST.
 CHECK: any test/assert/code that hardcodes a schema/db version NUMBER for which a *_SCHEMA_LATEST or version constant exists (SPV schema, sortition/sortdb version, burnchain db version, etc.)?

T5 — STREAM ROWS, don't collect a whole table into a Vec first.
 Origin: blocks Copilot — copy_epoch2_block_files loaded all (index_block_hash, height) rows into a Vec before the copy loop; fixed to iterate the query_map directly (no intermediate Vec).
 CHECK: any copy/scan loop that does .collect::<Vec<_>>() over an UNBOUNDED full-table query and then iterates, where direct streaming over the query_map iterator would avoid memory pressure? (Collecting a small/bounded set, or needing the full set at once for dedup/sort, is acceptable — only flag unbounded full-table materialization.)

T6 — INTEGER WIDTH CONSISTENCY for heights (u64, not u32).
 Origin: burnchain Copilot — expected_burn_height (and assert_sortition_tip_height's argument) typed u32; burn/block heights are u64 across the codebase, so u32 adds a narrowing conversion / truncation risk.
 CHECK: any burn/block height (or similar large counter) typed as u32 that should be u64? Any unnecessary "as u32"/"as u64" narrowing on heights?

T7 — IDEMPOTENCY / FAIL-IF-DESTINATION-EXISTS for new-file copy entrypoints.
 Origin: spv Copilot — copy_spv_headers was documented as copying into a NEW destination but would open a pre-existing dst and insert into it (constraint errors / duplicate / confusing). Fixed: it now returns an error if dst already exists (and a test asserts the stale dst is left untouched).
 CHECK: does this PR's copy entrypoint create/open a destination that, if it already exists (leftover from a prior failed squash), would append/duplicate/constraint-fail or silently corrupt? Should it fail (or remove) when dst already exists like spv now does? IMPORTANT JUDGMENT: functions that ADD side tables to an ALREADY-CREATED squashed index DB (created first by MARF::squash_to_path, then opened via with_offline_write_session) are SUPPOSED to find dst existing — that is by design, NOT a bug. Only flag entrypoints that are meant to create a brand-new standalone file (like spv headers). Distinguish these carefully and explain which case this is.

T8 — MISLEADING PARAM NAMES / STRINGLY-TYPED FOOTGUNS.
 Origin: clarity benjamin — contract_hash param wasn't a hash (renamed contract_id); a blockhash: &str actually carried a hex StacksBlockId (the DB column is literally named blockhash but stores a block id) — renamed/encapsulated as block_id in a MetadataRow struct; functions taking several same-typed &str args (order mistakes compile fine) were grouped into a struct.
 CHECK: params named *hash that actually carry ids; functions taking multiple same-typed string/primitive args where a wrong-order call would silently compile; could a rename or small struct help? (Pragmatic — only flag genuinely confusing/dangerous cases.)

T9 — NAMESPACE squash-specific free functions in a dedicated submodule.
 Origin: blocks cylewitruk — squash-specific free functions added to an owning module (staging_blocks.rs) polluted the namespace; fixed by grouping them in "pub(crate) mod squash { ... }" with documented ATTACH-alias constants (SRC_DB/IDX_DB/SRC_TABLE).
 CHECK: did THIS PR add squash/snapshot-specific free functions or constants to an owning module (sortdb.rs, burnchains/db.rs, SPV module) as BARE free functions (not grouped in a squash/snapshot submodule)? For cross-PR consistency, should they be grouped like blocks did? Cite the exact functions.

T10 — UNUSED IMPORTS / CLIPPY (-D warnings) CLEANLINESS.
 Origin: sortition Copilot — ClarityMarfTrieId imported but unused in a test module (would fail -D warnings).
 CHECK: any unused imports / obvious clippy violations in this PR's new files or test modules?

T11 — SILENT SKIP vs LOUD CORRUPTION ERROR on "impossible" data; and symmetric handling.
 Origin: clarity benjamin/federico — copy_required_metadata_rows silently skipped rows whose key didn't parse (all keys are clr-meta::, so a non-matching key means we'd silently DROP source data) while a sibling function returned an error — asymmetric. Fixed: both now return Error::CorruptionError on a non-parsing key (symmetric, loud).
 CHECK: any place that silently skips/continues on data that "should never happen" and whose presence means source data would be dropped? Should it be a loud corruption error instead? Also check symmetry between sibling functions handling the same condition.

T12 — DOCSTRING / COMMENT ACCURACY.
 Origin: spv Copilot (a doc claimed all schema versions have chain_work, but it was added in SPV_SCHEMA_2), blocks Copilot (a comment claimed read-only sqlite_open's WAL pragma rejects non-WAL files, but sqlite_open only forces WAL for non-read-only opens), clarity federico (docstring should note external_blobs is forced true because both archival and squashed Clarity MARFs always have an external blob).
 CHECK: docstrings/comments that overstate guarantees, cite the wrong schema version, or misdescribe pragma/open/lock behavior in this PR's files?
`

const FINDINGS_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  properties: {
    pr: { type: 'string' },
    own_comments_status: {
      type: 'string',
      description: 'Were the reviewer comments specific to THIS PR fully addressed (committed and/or uncommitted)? Note any that look unaddressed.',
    },
    findings: {
      type: 'array',
      items: {
        type: 'object',
        additionalProperties: false,
        properties: {
          theme: { type: 'string', description: 'e.g. "T6 — height width"' },
          title: { type: 'string' },
          status: { type: 'string', enum: ['change-needed', 'already-addressed', 'not-applicable'] },
          severity: { type: 'string', enum: ['must-fix', 'should-fix', 'nit', 'judgment-call'] },
          evidence: { type: 'string', description: 'file:line + short snippet proving the claim' },
          proposed_change: { type: 'string', description: 'concrete edit to make, or "none"' },
          confidence: { type: 'string', enum: ['high', 'medium', 'low'] },
        },
        required: ['theme', 'title', 'status', 'severity', 'evidence', 'proposed_change', 'confidence'],
      },
    },
  },
  required: ['pr', 'own_comments_status', 'findings'],
}

const VERDICT_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  properties: {
    real: { type: 'boolean', description: 'true if the issue genuinely exists in the real code AND is not already addressed' },
    reasoning: { type: 'string' },
    corrected_severity: { type: 'string', enum: ['must-fix', 'should-fix', 'nit', 'judgment-call', 'not-an-issue'] },
    apply_recommendation: { type: 'string', enum: ['apply', 'skip', 'needs-human-judgment'] },
    notes: { type: 'string', description: 'precise file:line and the exact change, or why it should be skipped' },
  },
  required: ['real', 'reasoning', 'corrected_severity', 'apply_recommendation', 'notes'],
}

const PRS = [
  {
    key: 'clarity',
    pr: '#7307',
    wt: '/home/franc/workspace/stacks-core-snapshot-clarity',
    files: 'snapshot/clarity.rs, snapshot/tests/clarity.rs, clarity/src/vm/database/sqlite.rs, clarity/src/vm/database/mod.rs',
    state: `Has UNCOMMITTED changes addressing its own comments. Addressed: contract_hash->contract_id rename; introduced MetadataRow struct (block_id field, documents blockhash column stores a StacksBlockId); visit_* now borrow via get_ref().as_str(); exposed DATA_TABLE_NAME/METADATA_TABLE_NAME consts in clarity sqlite.rs and used them; both copy_required_metadata_rows AND scan_metadata_contract_ids now return Error::CorruptionError on non-clr-meta key (symmetric, loud); visit_metadata_keys ascending-order documented as a contract + new test metadata_keys_visited_in_order; added metadata_key_make_and_parse test; open_readonly_marf docstring explains external_blobs forced true; removed visit_data_rows. Source MARF/DB already opened read-only (open_readonly_marf via TrieFileStorage::open_readonly, open_readonly_clarity_db via sqlite_open READ_ONLY). DEFERRED (out of scope, reviewer said "future"): rename SqliteConnection -> ClaritySqliteConnection.`,
  },
  {
    key: 'blocks',
    pr: '#7320',
    wt: '/home/franc/workspace/stacks-core-snapshot-blocks',
    files: 'snapshot/blocks.rs, snapshot/tests/blocks.rs, nakamoto/staging_blocks.rs, stacks/db/blocks.rs',
    state: `Has COMMITTED crc fixes for its 4 Copilot comments: microblock-confirm tip check, copy_epoch2_block_files now streams (no Vec), test uses NAKAMOTO_STAGING_DB_SCHEMA_LATEST (not hardcoded 5), stale WAL-pragma comment fixed. PLUS UNCOMMITTED changes for cylewitruk comments: squash-specific SQL builders grouped into "pub(crate) mod squash" in staging_blocks.rs (with SRC_DB/IDX_DB/SRC_TABLE consts; nakamoto_staging_block_columns renamed column_list_sql; predicates/source_select cleaned up); index_block_hash_to_rel_path moved to be an associated fn StacksChainState::index_block_hash_to_rel_path.`,
  },
  {
    key: 'burnchain',
    pr: '#7317',
    wt: '/home/franc/workspace/stacks-core-snapshot-burnchain',
    files: 'snapshot/burnchain.rs, snapshot/tests/burnchain.rs, burnchains/db.rs',
    state: `NO committed crc fixes and NO uncommitted changes yet. Its ONLY reviewer comment is Copilot on burnchain.rs:~126: expected_burn_height is u32 but should be u64 (and assert_sortition_tip_height's corresponding arg) for consistency/safety — this looks UNADDRESSED. This PR has NOT had a cleanup pass, so check ALL themes thoroughly.`,
  },
  {
    key: 'sortition',
    pr: '#7311',
    wt: '/home/franc/workspace/stacks-core-snapshot-sortition',
    files: 'snapshot/sortition.rs, snapshot/tests/sortition.rs, chainstate/burn/db/sortdb.rs',
    state: `Has COMMITTED fixes: DROP TABLE now qualified temp.<name>; boundary checks + test-fixture SQL + other queries moved into sortdb.rs. Reviewer comments: Copilot temp-table-drop (addressed), Copilot unused import ClarityMarfTrieId in tests/sortition.rs (verify it's gone). Pay special attention to T9 (are the squash functions MOVED into sortdb.rs grouped in a squash submodule like blocks did, or left as bare free functions?) and T2/T7.`,
  },
  {
    key: 'spv',
    pr: '#7319',
    wt: '/home/franc/workspace/stacks-core-snapshot-spv',
    files: 'snapshot/spv.rs, snapshot/tests/spv.rs, and the SPV headers owning module (find it, likely stackslib/src/burnchains/bitcoin/spv.rs)',
    state: `Has COMMITTED fixes: docstring corrected re chain_work / SPV schema versions; copy_spv_headers now errors if dst already exists (+ test). Pay special attention to T2 (does spv.rs hold raw SQL / table names that belong in the SPV owning module?), T4 (hardcoded SPV schema version?), T5, T9.`,
  },
]

function analysisPrompt(p) {
  return `You are auditing one of five related "MARF snapshot/squash" pull requests for the Stacks blockchain (consensus-critical Rust; GPL-3.0; no rollback from a bad deploy, so be precise and conservative).

## Your target: PR ${p.pr} (${p.key})
- Worktree (read files with ABSOLUTE paths under here): ${p.wt}
- Primary files: ${p.files}
- Current state of this PR: ${p.state}

## Your job
Read the ACTUAL current code in the worktree (use Read/Grep/Bash with absolute paths under ${p.wt}; you may also "git -C ${p.wt} diff" and "git -C ${p.wt} log" to see committed vs uncommitted state). For the snapshot file, its tests, and the owning module(s) this PR touches, evaluate EVERY theme in the catalog below. The catalog distills the review comments left on the clarity (#7307) and blocks (#7320) PRs plus each PR's own comments — the user wants to know which of those same issues exist in THIS PR and should get the same fix.

${THEME_CATALOG}

## Rules
- READ-ONLY: do NOT modify any file. This is analysis only.
- For each theme, return a finding with status:
  - "change-needed": the issue genuinely exists here and a concrete edit is warranted.
  - "already-addressed": the issue existed but this PR already handles it correctly (say how).
  - "not-applicable": the theme doesn't apply to this code (say why, briefly).
- Be CONSERVATIVE about "change-needed". This is consensus-critical code. Only mark change-needed when you can cite the exact file:line and a snippet proving it. Prefer accuracy over volume. Distinguish must-fix (correctness/CI failure) from nit/judgment-call.
- For T7 especially: carefully determine whether a copy entrypoint creates a brand-new file (fail-if-exists makes sense) vs adds tables to an already-created squashed index (existing dst is BY DESIGN — not a bug). Explain which.
- Also report own_comments_status: were this PR's OWN reviewer comments fully addressed? Flag any that appear unaddressed (e.g. burnchain's u32->u64).
- Cover all 12 themes (one finding each is fine; you may add extra findings if you spot additional instances). Keep evidence snippets short (1-3 lines).

Return the structured object.`
}

function verifyPrompt(p, f) {
  return `You are an adversarial verifier for a consensus-critical Rust PR (Stacks blockchain, no rollback). A first-pass audit of PR ${p.pr} (${p.key}) claims this change is needed. Your job is to REFUTE it: read the real code and decide whether the issue is genuinely present and NOT already handled. Default to skeptical — if the claim is wrong, overstated, or already addressed, say real=false.

Worktree (use ABSOLUTE paths under here): ${p.wt}

CLAIMED FINDING:
- theme: ${f.theme}
- title: ${f.title}
- severity (claimed): ${f.severity}
- evidence (claimed): ${f.evidence}
- proposed change: ${f.proposed_change}

Verify by reading the actual code at the cited location (Read/Grep/Bash, e.g. "git -C ${p.wt} diff"). Consider:
- Is the cited code actually present as described (not stale/misquoted)?
- Is it already addressed elsewhere (committed or uncommitted)?
- For T7: is an existing destination actually a bug here, or is it by-design (side tables added to an already-squashed index)? Be careful.
- For T2/T9: is moving SQL / namespacing genuinely warranted and consistent with how the OTHER PRs did it, or is it an over-reach / does it conflict with the deliberate TableCopySpec exception?
- Is the proposed change safe and correct for consensus-critical code, or could it change behavior?
Give apply_recommendation: "apply" (clear, safe, warranted), "skip" (not real / not worth it), or "needs-human-judgment" (real but a design/taste call the author should decide). In notes, give the precise file:line and the exact minimal change (or why to skip).

Return the structured verdict.`
}

phase('Analyze')
const analyses = await pipeline(
  PRS,
  (p) => agent(analysisPrompt(p), { label: `analyze:${p.key}`, phase: 'Analyze', schema: FINDINGS_SCHEMA }),
  (analysis, p) => {
    if (!analysis) return { pr: p.key, prNum: p.pr, wt: p.wt, own_comments_status: 'ANALYSIS FAILED', verified: [] }
    const changeNeeded = (analysis.findings || []).filter((f) => f.status === 'change-needed')
    if (changeNeeded.length === 0) {
      return { pr: p.key, prNum: p.pr, wt: p.wt, own_comments_status: analysis.own_comments_status, all_findings: analysis.findings, verified: [] }
    }
    return parallel(
      changeNeeded.map((f) => () =>
        agent(verifyPrompt(p, f), {
          label: `verify:${p.key}:${f.theme.split(' ')[0]}`,
          phase: 'Verify',
          schema: VERDICT_SCHEMA,
        }).then((v) => ({ finding: f, verdict: v })),
      ),
    ).then((verdicts) => ({
      pr: p.key,
      prNum: p.pr,
      wt: p.wt,
      own_comments_status: analysis.own_comments_status,
      all_findings: analysis.findings,
      verified: verdicts.filter(Boolean),
    }))
  },
)

return analyses.filter(Boolean)
