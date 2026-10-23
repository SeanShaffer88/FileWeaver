# FileWeaver — innovation review (2026-10-20)

Companion to [`fileweaver-market-and-architecture.md`](fileweaver-market-and-architecture.md), whose §6 lists the original feature concepts. This review re-scores those concepts against the **implemented 0.1.0 code** (not the plan), adds new candidates that the current architecture makes unusually cheap or natural, and ranks everything by user value, distinctiveness, feasibility, and fit with the program's quality aims: dependable, frictionless, predictable, forgiving, transparent, offline, privacy-conscious, accessible, and out of the way.

**Honesty rules used here.** Distinctiveness is a *hypothesis*, not a claim — §3 of the architecture doc establishes what comparable products verifiably do, and nothing below asserts uniqueness beyond that evidence. "Status" statements were checked against the source on 2026-10-20.

**Progress since this review (2026-10-20):** #2 Risk Budget shipped (PR #1) — every preview shows its change/blocked/review/unchanged counts and zero-overwrite guarantee. #1 Rename Ledger completion shipped (PR #2) — the shell now lists recorded jobs, reviews each against the current disk state (unchanged / changed / missing / original-location-occupied), and rolls back behind a two-step confirmation, restoring only unchanged files. #3 Order Observatory shipped (PR #5) — selectable numbering order (Path / Name / Modified / Exif capture date, path tie-break), an Order line stating the order and assigned counter range with every preview, and sequence-gap warnings over the proposed names. #5 rule-stage provenance shipped (PR #6) — preview rows are selectable and show the selected name's stage-by-stage construction from the engine's recorded stages. Next in the ranking: #4 Reviewable Plan Handoff, which this review recommends paper-prototyping first (see the interview set in the architecture document).

**Progress since this review (2026-10-23):** the desktop shell now exposes the whole core rule set as an ordered rule stack (add/select/reorder/edit/remove, including regex, case, extension, and date rules), and the core gained a change-extension rule. This is not one of the ranked candidates; it closes the largest gap between the library's rule power and what a user can actually reach, and it keeps the product a renaming utility rather than a file organizer. Saved presets, per-row selection, and the plan-handoff shell are still missing.

## Where the code already stands

- The **Rename Ledger exists in the core**: the SQLite journal stores each job's request and rule pipeline as JSON plus per-item source/destination paths and size + modification-time fingerprints, and `Executor::rollback` preflights those fingerprints and skips diverged files. What is missing is any UI for it and any divergence *report*.
- **Risk data exists**: every `PlannedJob` carries a summary (ready count, blocking errors) and per-row issues. It is shown only as row text today.
- **Numbering order is path order only**: snapshots are sorted by case-insensitive full path. There is no Exif/date sort, no tie reporting, and no gap detection.
- The rule pipeline **already records intermediate names**: every `PlannedAction` carries its per-stage results (`stages`, populated by `RulePipeline::apply`). Per-stage provenance is display work, not engine work.
- Planning is pure and jobs are serializable (`pipeline_json`, `request_json`), which makes plan export/import realistic without new infrastructure.

## Ranked candidates

Scores: ● strong, ◐ moderate, ○ weak. Ranking weighs user value first, then fit and feasibility.

| # | Candidate | Value | Distinctiveness (hypothesis) | Feasibility | Fit |
|---:|---|:---:|:---:|:---:|:---:|
| 1 | Rename Ledger completion: recovery review UI + divergence report | ● | ● | ● | ● |
| 2 | Risk Budget summary | ● | ◐ | ● | ● |
| 3 | Order Observatory, with sequence-gap detection | ● | ● | ◐ | ● |
| 4 | Reviewable Plan Handoff (export / review / apply) | ● | ● | ◐ | ● |
| 5 | Rule-stage provenance | ◐ | ● | ● | ◐ |
| 6 | Collection sequence memory | ◐ | ● | ◐ | ◐ |
| 7 | Rehearsal Mode (sandbox apply) | ● | ● | ○ | ● |
| 8 | Collection Lint (read-only naming-health scan) | ◐ | ◐ | ● | ● |
| 9 | Sidecar & sibling relationship groups (§6 #3) | ◐ | ◐ | ◐ | ◐ |
| 10 | Naming Contract (§6 #4) | ◐ | ● | ◐ | ○ |

### 1. Rename Ledger completion — recovery as a review, not a button

- **Experience.** A History view lists past jobs as receipts: when, what rules, how many files, what state each file is in *now*. Choosing "Undo this job" opens the same kind of preview as a forward job — one row per file: *will restore*, *already back*, *changed since the job (size/date differ)*, *original location occupied*, *missing* — and only the safe rows are applied.
- **Problem.** Rollback that silently skips files teaches users not to trust undo; rollback that overwrites teaches them to fear it. The core already detects divergence; nobody can see it.
- **Distinctive hypothesis.** Comparable tools document undo/history (§3), but not a durable, per-item, divergence-aware recovery *review* that reuses the forward planning pipeline.
- **Quality traits.** Forgiving, transparent, predictable, dependable. This is the product's core promise made visible.
- **Feasibility / risk / maintenance.** High feasibility — journal schema, fingerprints, and rollback already exist; the work is a read model plus UI. Risk: users expecting reversal after heavy external edits; the review UI is exactly the mitigation. Maintenance: low-medium (schema already versioned by migrations discipline).
- **Assumptions to test.** That the size+mtime fingerprint produces few false "changed" verdicts in real collections (sync tools touching mtimes is the known threat).
- **Prototype / user test.** CLI or debug view that renders the recovery review for a real past job; then the §6 test: rename 200 files, manually move/edit/delete 12, and check whether five archivists make only safe choices from the report.

### 2. Risk Budget summary

- **Experience.** The preview opens with one plain-language block: "1,248 ready · 3 blocked by conflicts · 6 missing Exif dates · 41 cross-volume (non-atomic) · 0 overwrites." Each count filters the table.
- **Problem.** Row-by-row warnings are easy to miss at scale; people apply jobs they haven't actually reviewed.
- **Distinctive hypothesis.** Previews are universal; a categorized, countable decision summary is not (§6 #5).
- **Feasibility / risk / maintenance.** Highest feasibility on this list — `PlanSummary` and per-row issues already exist. Must never be color-only; every count needs text and a filter action. Maintenance: negligible.
- **Assumptions to test.** That novices can correctly answer "is this safe to apply, and why not?" faster with the summary than without (A/B task test from §6).

### 3. Order Observatory, with sequence-gap detection

- **Experience.** Before numbering, a "Numbering order" card states the sort in words — "By full path, A→Z, case-insensitive" today; later "By Exif capture time; 18 files have no Exif date and fall back to path order; 4 ties broken by name" — and warns about the *result*: "Numbers 7 and 19 are missing from the existing series" or "This folder already contains `_0042`; starting at 1 would collide."
- **Problem.** Wrong numbering is the most common quiet disaster in batch renaming: the names are valid, the order is wrong, and nobody notices until the archive is wrong.
- **Distinctive hypothesis.** Other tools sort and number; exposing the reasoning, the fallbacks, and the gaps as first-class information (§6 #2, extended with gap analysis) is the differentiator.
- **Feasibility / risk / maintenance.** Medium: Exif sort needs the metadata already read at plan time; gap detection is a small pure function over proposed/existing names. Risk is over-detailing for casual users — collapsible card, one-line summary by default.
- **Assumptions to test.** That photographers actually ingest in multiple batches into one folder (drives the collision warning's value).
- **Prototype / user test.** Static preview card over a folder with identical timestamps, missing Exif, and a pre-existing numbered series; compare numbering accuracy and confidence against a traditional dialog.

### 4. Reviewable Plan Handoff

- **Experience.** "Save plan…" exports one file containing the folder reference, the rule pipeline, the full old→new list, and the plan's risk summary. A colleague (or the same user, later, or on another machine) opens it in FileWeaver: the plan is **re-validated against the folder as it exists now**, drift is reported row-by-row ("3 source files changed since this plan was made; 1 target now exists"), and only then can it be applied — by someone who never built the rules.
- **Problem.** In archive and team workflows the person who *approves* a naming change is often not the person who *proposes* it, and today the review artifact is a screenshot or nothing. It also gives individuals an auditable "what exactly will happen" record before a big job.
- **Distinctive hypothesis.** Presets and CSV mappings exist elsewhere; a self-describing, re-validating plan artifact that separates propose → review → apply builds directly on this product's serializable plan + journal design.
- **Quality traits.** Transparent, predictable, interoperable, forgiving (drift is caught before, not during, execution).
- **Feasibility / risk / maintenance.** Medium: `PlannedJob` already serializes; the new work is a file format (versioned JSON), an import path that re-runs the planner and diffs, and UI. Risk: users treating a plan file as a backup — it is not one; label it clearly. Format versioning is the ongoing cost.
- **Assumptions to test.** That at least some target users really do hand naming work between people (validate in the §5 interview set before building the UI; the export alone is cheap).
- **Prototype / user test.** Export a plan, modify three files in the folder, import, and check whether a reviewer correctly identifies the drift and the safe subset to apply.

### 5. Rule-stage provenance

- **Experience.** Selecting a row shows the name's journey: original → after prefix → after replace → after counter → final, with the stage that introduced an issue highlighted (§6 #6).
- **Problem.** With stacked rules, the final name can be wrong for reasons invisible in a final-only preview; debugging means toggling rules blindly.
- **Feasibility / risk / maintenance.** High: the engine already stores each action's stage outputs (`PlannedAction.stages`); the work is a per-row inspector in the UI. Maintenance: negligible.
- **Prototype / user test.** Give experienced users three seeded wrong outcomes; measure time-to-diagnosis with and without stage view.

### 6. Collection sequence memory

- **Experience.** For a folder (or named collection), FileWeaver remembers the last counter value a job used there. Next ingest, the preview says "This collection's sequence reached 0142 on Oct 18 — continue at 0143?" with a one-click override. Stored in the local ledger; always shown, never silent.
- **Problem.** Photographers ingest in batches over weeks. Restarting at 1 collides or forces manual bookkeeping in a notebook; continuing by hand is error-prone.
- **Distinctive hypothesis.** Named counters exist in at least one competitor (§3 notes licensed named counters), but tying continuation to the *collection's own durable job history* — with the evidence shown in the preview — follows from this product's ledger rather than a global counter list.
- **Feasibility / risk / maintenance.** Medium: a small ledger query + UI state. The whole risk is hidden state, so the rule is: the source of the start value is always displayed ("starting at 143 because…") and editable.
- **Assumptions to test.** That multi-batch ingest into one folder is common among the target photographers (§5 interviews).
- **Prototype / user test.** Two-session task: ingest batch A, close the app, ingest batch B; count collisions and manual corrections with and without the prompt.

### 7. Rehearsal Mode (sandbox apply)

- **Experience.** "Rehearse" applies the plan to a throwaway copy of the folder (full copy for small sets; structure + names only for large ones) and opens the result: the user browses the *renamed tree itself*, not a table — then discards it or proceeds to the real apply with the same plan.
- **Problem.** Tables are abstract; some users only trust a result they can walk through. It also rehearses rollback: the rehearsal can run the undo and show that, too.
- **Feasibility / risk / maintenance.** Lowest feasibility here: copy cost, disk space, and cleanup discipline; a names-only simulation mode cuts most of the cost. Risk: users mistaking the rehearsal tree for the real one — watermark the folder name and location unmistakably.
- **Assumptions to test.** That a meaningful share of users distrust table previews (interview + prototype click test before building the full-copy path).
- **Prototype / user test.** Names-only rehearsal view (a tree rendering of the plan) shown to five users after a table preview; ask which they'd trust for a 5,000-file job and why.

### 8. Collection Lint

- **Experience.** A read-only "Check this folder" scan reports naming health without proposing a job: mixed date formats, sequence gaps, duplicate stems that differ only by case, names that will break the 259-unit path limit if moved one level deeper. Each finding offers "Turn into a rule" which seeds the pipeline.
- **Problem.** New users often don't know what to fix first, and the app currently offers no on-ramp between "paste a path" and "build rules."
- **Feasibility / risk / maintenance.** High-medium feasibility — it reuses the planner's validation and snapshot data read-only. Risk: lint noise; findings need severity discipline and a "why this matters" line each.
- **Prototype / user test.** Run it on three real-world messy folders; have users rate which findings they'd act on. Cut any finding type nobody acts on.

### 9–10. Held from §6

- **Sidecar & sibling groups (§6 #3)** stay a strong photographer hypothesis but rank below the trust features above: false groupings are a correctness risk, and the pattern configuration needs the documentation and testing budget the ledger work will consume first. Prototype per §6 before committing.
- **Naming Contract (§6 #4)** has real institutional value but the highest "feels like programming" risk; revisit after Plan Handoff exists, since a contract naturally travels *with* a plan file.

## Recommendation

Build in this order: **1 (Ledger UI + recovery review)** and **2 (Risk Budget)** — both mostly surface data the core already computes, and together they make the product's central promise visible. Then **3 (Order Observatory + gaps)**, the cheapest strong differentiator for the photographer audience. Prototype **4 (Plan Handoff)** on paper with the §5 interview set before writing the format; it is the best candidate for a genuinely distinctive professional workflow, and it composes with 1–3 (a handoff plan carries its risk budget and numbering explanation with it). Defer 9–10 until the trust core ships.

Explicitly *not* recommended, consistent with §5's avoid list: scripting, watched-folder automation, metadata writing, and online lookups — each spends the trust the features above are trying to earn.
