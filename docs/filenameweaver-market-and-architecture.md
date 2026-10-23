# FilenameWeaver — market research, product direction, and architecture plan

**Research date:** 2026-10-20 (America/Chicago)  
**Name:** FilenameWeaver (renamed from the earlier working name)  
**Implementation checkpoint:** The accompanying `0.1.0` source implements the first core slice (planner, validation, staged executor, local ledger, and a limited desktop shell). It does **not** yet implement this entire plan. In particular, current rollback uses a conservative size/modified-time fingerprint rather than stable Windows file identity, the UI now exposes the recovery ledger, the full ordered rule stack, named rule presets, per-row inclusion, preview filters, CSV export of the reviewed list, and the plan handoff (save, revalidate, apply the unchanged subset), but it still does not expose move or copy destinations, sortable columns, mapping import, or a native file picker. Two plan details differ in the current code and are worth knowing when reading §4 and §7: the conflict policy **blocks** a job that has any blocking issue among the included rows; the planner never silently skips a row, although the shell now lets a user exclude a row explicitly (§4 item 17 describes the intended policy), and the implemented layout is a **single crate** with modules (`src/lib.rs` + `src/*.rs`, see the README's project layout), not the multi-crate workspace sketched in §7. See the README for exact current scope.  
**Evidence standard:** Product facts below come from official product pages, manuals, release notes, or official Microsoft documentation. Feature marketing claims have not been independently benchmarked. “Not verified” means the official sources reviewed did not establish the point—not that the product lacks it.

## 1. Product definition and working assumptions

### Purpose
A local-first Windows studio for safely renaming, moving, and copying large groups of files. It is intended to be comfortable for everyday file clean-up and sufficiently exact for photographers, librarians, and archivists who need reproducible naming, visible preview, auditable changes, and trustworthy rollback.

### Assumptions used for this plan

| Assumption | Why it is reasonable | Decision affected |
|---|---|---|
| Core work must operate with no network connection. | Batch file organization does not inherently need a service. | No account, telemetry, cloud, or online matching in the initial product. |
| “Rollback” means durable, operation-level reversal, not only the last Explorer undo item. | Users may discover errors long after a job or app restart. | A persistent job ledger is a core feature, not an afterthought. |
| Everyday users need templates; professional users need deterministic, inspectable rules. | The requested audience spans newcomers, photographers, and archivists. | Two-layer UX: guided templates plus an advanced ordered-rule builder. |
| Initial metadata support should prioritize local filesystem dates and JPEG/Exif data. | This is valuable for photographer workflows without opening arbitrary documents or requiring online lookups. | Add other media and document readers only after format-specific testing. |
| The initial app targets Windows 11 x64, with Windows 10 x64 as a deliberately tested compatibility target later. | The product target is Windows 11; the selected UI toolkit explicitly tests Windows 10/11 desktop targets. | Do **not** promise Windows 7/8 or ARM64 support until CI and installer testing cover them. |

## 2. Comparable programs

| Product | Platform | License / pricing model (verified) | Intended audience | Official product page |
|---|---|---|---|---|
| **Advanced Renamer** | Windows 10+ x64; also macOS | Casual/occasional personal use is free; frequent personal and commercial use require a perpetual, one-time license. The free tier limits a batch to 10,000 items and five methods. | General users and professionals managing photo, music, video, document, and folder names. | [Product](https://www.advancedrenamer.com/) · [license comparison](https://www.advancedrenamer.com/compare) |
| **Bulk Rename Utility** | Windows 7–11 and current Windows Server versions; installer includes 32/64-bit editions | Free for private personal home use; paid home/academic and commercial per-computer licenses. | Power users and organizations needing a dense, high-volume Windows renamer. | [Product](https://www.bulkrenameutility.co.uk/) · [licensing](https://www.bulkrenameutility.co.uk/Buy.php) |
| **PowerRename (Microsoft PowerToys)** | Windows 10 version 2004+ and Windows 11, x64/ARM64 | Free Microsoft PowerToys utility; PowerToys source is open source. | Windows users who want a quick File Explorer–initiated search/replace or regex batch rename. | [Microsoft documentation](https://learn.microsoft.com/en-us/windows/powertoys/powerrename) |
| **ReNamer** | Windows 2000 and later (vendor claim) | Proprietary Lite/Pro editions; Lite limits rules and presets, while the purchase page lists a US$29.95 product price. | Users who want reusable rule stacks, tags, mapping, or scripting. | [Product](https://www.den4b.com/products/renamer) · [edition comparison](https://www.den4b.com/compare/renamer) |
| **File Juggler** | Windows | Commercial: a full-featured 30-day trial, then a US$50 one-time license. | People who want rules that continuously organize downloads, documents, and incoming files. | [Product](https://www.filejuggler.com/) · [trial/licensing](https://www.filejuggler.com/faq/trial-and-licensing/) |
| **digiKam** | Windows 10+ x64, Linux, macOS | Free and open-source. | Photographers and enthusiasts needing digital asset management, photo metadata, and batch workflows. | [About](https://www.digikam.org/about/) · [features](https://www.digikam.org/about/features/) |
| **Ant Renamer** | Windows; older codebase maintained for compatibility | GPL-licensed free/open-source software. | People who need a lightweight, traditional bulk renamer with common operations. | [Product](https://www.antp.be/software/renamer) · [features/license](https://antp.be/software/renamer/features) |
| **FileBot** | Windows, macOS, Linux and other desktop platforms | Commercial evaluation; universal licenses are listed as US$8 for one year or US$80 lifetime. | Home-media curators who need database-assisted movie/TV/music organization and automation. | [Product](https://www.filebot.net/) · [purchase](https://www.filebot.net/purchase.html) |

### Market reading

1. **Raw rule power is abundant.** Advanced Renamer, Bulk Rename Utility, and ReNamer cover the expected transforms, regex, metadata tags, reusable recipes, and previews.
2. **Safety is uneven.** Preview and some undo exist, but vendor documentation generally frames undo as a batch convenience; it does not establish a persistent, conflict-aware recovery model across restarts and subsequent file changes.
3. **Automation products create a different risk class.** File Juggler shows the value of folder monitoring and content-driven workflows, but its own documentation warns that rules can move, rename, and delete files rapidly. This reinforces a safer “preview first; automation later” rollout.
4. **Photographers need both metadata and relationships.** digiKam is powerful but a full digital asset manager; a focused file studio can provide photo-centric organization without asking every user to adopt a library catalog.
5. **Professional-grade precision need not mean a dense, intimidating control panel.** Bulk Rename Utility exposes formidable capability but demonstrates the UX risk of presenting all controls at once.

## 3. Merged feature inventory (descriptive, not a roadmap)

“Products” records at least one verified provider. Plan/tier notes only appear where the vendor source clearly describes them.

| Category | Distinct feature | Products with verified evidence | Availability / meaningful differences |
|---|---|---|---|
| Intake and scale | Explorer context-menu entry | PowerRename; Advanced Renamer; Bulk Rename Utility | PowerRename begins from Explorer selection. Advanced Renamer and BRU offer optional Explorer integrations. |
| Intake and scale | Drag/drop, folder selection, recursive traversal, extension mask | Advanced Renamer; Ant Renamer; ReNamer | Advanced Renamer’s documented directory intake can include subdirectories and masks; ReNamer supports folders; Ant accepts files/folders. |
| Intake and scale | Large-batch performance | Bulk Rename Utility | Vendor states it can handle folders/disks with over 100,000 entries; this is a vendor claim, not a benchmark. |
| Intake and scale | Network/UNC locations | Bulk Rename Utility; ReNamer; File Juggler | BRU documents UNC paths; ReNamer’s manual includes network files; File Juggler can monitor network/external drives but falls back to scans when notifications are unavailable. |
| Rules | Ordered composable rule pipeline | Advanced Renamer; ReNamer; digiKam; File Juggler | ReNamer explicitly applies a stack top-to-bottom; Advanced Renamer has ordered methods; digiKam has rename workflows; File Juggler orders rules. |
| Rules | Basic text operations: replace, insert, remove, case, extension, trim | Advanced Renamer; BRU; ReNamer; Ant Renamer; PowerRename | Expected table-stakes capability. PowerRename concentrates on search/replace. |
| Rules | Deterministic numbering/counters | Advanced Renamer; BRU; ReNamer; Ant Renamer; digiKam | Rules differ in sort, padding, step, and persistent-counter support. Advanced Renamer’s named counters are licensed. |
| Rules | Regex and wildcard matching | Advanced Renamer; BRU; PowerRename; ReNamer; Ant Renamer; File Juggler | PowerRename uses ECMAScript regex and can optionally use Boost extensions; File Juggler also documents simpler patterns. |
| Rules | User code / scripting | Advanced Renamer (JavaScript); BRU (JavaScript); ReNamer (Pascal Script); FileBot (Groovy); File Juggler (run command) | Powerful but security, reviewability, and support burden are materially higher. |
| Rules | Explicit old-to-new mapping / list import / CSV | BRU; ReNamer; Ant Renamer; Advanced Renamer | BRU imports old/new pairs; ReNamer supports a Mapping rule and CSV; Ant takes names from a list; Advanced Renamer exports TXT/CSV/JSON. |
| Metadata | Filesystem properties and timestamps as tags | Advanced Renamer; BRU; ReNamer; File Juggler | Attributes, date fields, and size are used in patterns; BRU/Advanced Renamer can change timestamps. |
| Metadata | Photo metadata (Exif/XMP, dates, dimensions, camera) | Advanced Renamer; BRU; ReNamer; digiKam; Ant Renamer; FileBot | Advanced Renamer and BRU are broadest general renamers; digiKam is photo-first. |
| Metadata | Music, video, document, checksum metadata | Advanced Renamer; BRU; ReNamer; FileBot | ReNamer documents ID3, EXIF, OLE, AVI, and checksums; BRU can add optional PDF/MediaInfo components; FileBot adds media-specific sources. |
| Metadata | Embedded GPS → location names | Advanced Renamer; File Juggler | Advanced Renamer uses a city database/matching service; File Juggler supports GPS data in rules. Network/privacy behavior must be checked for every implementation. |
| Content inspection | Read document text/PDF properties to decide names or folders | File Juggler; Advanced Renamer | File Juggler documents content words/dates and PDF properties; Advanced Renamer documents PDF text reading and a metadata writer. Format coverage and resource cost vary. |
| Operations | Rename, copy, move; templated destination paths | Advanced Renamer; ReNamer; File Juggler; FileBot | Advanced Renamer supports paths that create folders; FileBot’s actions include rename/move/copy/link behavior. |
| Operations | Rename folders and files | Advanced Renamer; BRU; ReNamer; Ant Renamer | Advanced Renamer separates file and folder views; BRU can handle both; recursive folder ordering is a safety concern. |
| Operations | Alter timestamps/attributes | Advanced Renamer; BRU | Useful but distinct from filename changes and potentially surprising in archival workflows. |
| Operations | Write embedded metadata | Advanced Renamer; digiKam | Advanced Renamer uses ExifTool for supported formats; digiKam has metadata batch tools. This mutates file contents, so it has a different recovery story than a rename. |
| Operations | Delete/recycle / cleanup | File Juggler; BRU | File Juggler can send matches to Recycle Bin; BRU documents a Shift+Delete operation. |
| Safety | Live old → proposed preview | Advanced Renamer; BRU; PowerRename; ReNamer; Ant Renamer; File Juggler | Widely present; some allow excluding individual rows. |
| Safety | Collision, invalid-name, and long-path validation | Advanced Renamer; BRU; ReNamer; digiKam | ReNamer recently improved NTFS-equivalent clash and reserved-name detection; digiKam offers overwrite/different-name/skip behaviors. |
| Safety | Undo, revert, or generated undo artifact | Advanced Renamer; BRU; PowerRename; Ant Renamer; FileBot | BRU creates an undo batch file; FileBot has an internal history; Explorer provides PowerRename’s immediate undo pathway. Durability/resolution behavior has not been compared hands-on. |
| Safety | Operation log/status and error retention | BRU; File Juggler; ReNamer | File Juggler provides searchable logging; BRU logs activity; ReNamer can retain problematic items. |
| Safety | Dry-run/test mode | FileBot | FileBot exposes a `test` action on its CLI. |
| Safety | File-exists policy | File Juggler; digiKam; Advanced Renamer | File Juggler defaults to suffixing copied/moved names; digiKam documents store-different-name/overwrite/skip; Advanced Renamer recently added handling for existing targets. |
| Automation | Watch folders / event-driven rules | File Juggler; Advanced Renamer | File Juggler uses Windows change notification where available; Advanced Renamer lists Watch Folder as licensed. |
| Automation | CLI / scheduler / unattended jobs | Advanced Renamer; BRU (separate BRC); ReNamer; FileBot | Different products put the capability in a separate CLI, batch runner, or script interface. |
| Automation | External integrations | File Juggler; FileBot | File Juggler works against synced local cloud folders and can upload to Evernote; FileBot queries media databases and subtitles. |
| Photography | Batch queue and multi-core image workflow | digiKam | Queues can process items in parallel, chain plugins, preserve versions, and save workflows. |
| Media organization | Online media recognition and companion organization | FileBot | Match media against online sources, organize to Plex/Kodi/Jellyfin conventions, and revert via history. |
| UX / customization | Saved projects/presets and favorites | Advanced Renamer; BRU; ReNamer; FileBot; digiKam | Common professional acceleration; some plans limit quantity. |
| UX / customization | Dark mode | BRU; ReNamer; Advanced Renamer | ReNamer documents auto/light/dark choices on Windows 10+; BRU V4 added dark mode. |
| Accessibility / display | High contrast, high DPI, configurable text scale | BRU | BRU explicitly documents high contrast, high DPI, and larger/smaller UI font support. Comparable evidence was not established for every product. |
| Portability / privacy | Portable/no-install packaging | BRU; ReNamer; Advanced Renamer | BRU and ReNamer explicitly offer portable downloads; Advanced Renamer can keep settings next to its executable. |
| Privacy / networking | Explicitly local vs. online features | BRU; Advanced Renamer; File Juggler; FileBot | BRU says it does not communicate home; Advanced Renamer documents update/activation/GPS/TV-import data sharing; FileBot’s media matching is inherently online. |
| Assistance | Natural-language regex/JavaScript helper | Bulk Rename Utility | BRU’s FAQ describes an Assistant that generates regex or JavaScript from a natural-language request. The underlying service/privacy behavior was not verified from that FAQ. |

### Important source notes

- [Advanced Renamer feature and privacy guide](https://www.advancedrenamer.com/), [user guide](https://www.advancedrenamer.com/user_guide/using_aren), and [privacy statement in its guide](https://www.advancedrenamer.com/user_guide/complete_guide)
- [Bulk Rename Utility feature list](https://www.bulkrenameutility.co.uk/), [FAQ](https://www.bulkrenameutility.co.uk/Faq.php), and [optional metadata extensions](https://www.bulkrenameutility.co.uk/dll.php)
- [Microsoft PowerRename documentation](https://learn.microsoft.com/en-us/windows/powertoys/powerrename)
- [ReNamer product page](https://www.den4b.com/products/renamer), [manual](https://www.den4b.com/docs/books/renamer), and [recent reliability changes](https://www.den4b.com/news/3127/renamer-7-10)
- [File Juggler feature documentation](https://www.filejuggler.com/features/), [notifications vs. scans](https://www.filejuggler.com/documentation/introduction-to-file-juggler/), and [safe conflict behavior](https://www.filejuggler.com/documentation/actions/)
- [digiKam feature list](https://www.digikam.org/about/features/) and [Batch Queue Manager overview](https://docs.digikam.org/en/batch_queue/batchqueue_overview.html)
- [Ant Renamer features/license](https://antp.be/software/renamer/features)
- [FileBot product page](https://www.filebot.net/) and [CLI reference](https://www.filebot.net/cli.html)

## 4. Feature-by-feature fit assessment

The assessment below evaluates the merged feature inventory for **FilenameWeaver**, not whether a competitor made a good choice. Complexity assumes a safe Rust implementation with preview and regression tests.

| # | Feature | User problem / beneficiaries | Fit, advantages, drawbacks | Complexity, dependencies, maintenance | Decision |
|---:|---|---|---|---|---|
| 1 | Ordered, composable naming rules | Everyone needs simple rules; professionals need repeatable transformations. | Excellent fit. A visible pipeline explains compound transformations better than a monolithic pattern field. Risk: a long pipeline can become opaque. | **Medium**; pure rule engine, tests for composition and Unicode. | **Include early** |
| 2 | Basic text transforms | Everyday clean-up needs add/remove/replace/case/extension work. | Essential and discoverable through templates. Risk: none beyond normal name validation. | **Low**; no special dependency. | **Include early** |
| 3 | Deterministic numbering tied to explicit sort | Photographers and archivists need stable ordering. | Essential, but must show the sort key, direction, start, padding, and ties. Hidden order causes real damage. | **Medium**; natural sort, stable tie-breaker, date semantics tests. *(0.1.0 as planned: plain case-insensitive path sort only. Updated 2026-10-20: the planner now also offers Name, Modified-oldest-first, and Exif capture-date-oldest-first orders, all with the path key as tie-breaker, and the shell exposes the choice — natural sort is still not implemented.)* | **Include early** |
| 4 | Regex / wildcard rules | Power users clean inconsistent names more efficiently. | Strong fit as an advanced rule; never make it the default workflow. Provide inline examples and compile errors. | **Medium**; `regex` crate, escape/Unicode/error tests. Avoid catastrophic backtracking engines. | **Include early (advanced)** |
| 5 | Local file and JPEG/Exif metadata tokens | Camera and archival work needs capture date, camera, dimensions, file dates. *(0.1.0 implements only the JPEG capture date; no camera, dimension, or creation-time fields.)* | Central to the stated audience. Restrict initial format promise to tested formats and show missing-field behavior per row. | **Medium**; a well-maintained Exif reader, malformed-file tests. | **Include early** |
| 6 | Audio/video/PDF/document metadata | Media and document collectors want richer tags. | Useful, but broad format support bloats dependencies and produces difficult edge cases. | **High**; format parsers/external tooling; ongoing security updates. | **Defer** |
| 7 | GPS reverse geocoding | Travelers want location names in camera-file names. | Useful to some photographers, but exact place names can be wrong and services/data sets raise privacy/offline concerns. | **High**; offline gazetteer or explicit network service, location accuracy policy. | **Investigate** |
| 8 | Recursive intake with filters | Large collections need controlled selection by directory, extension, date, and exclusion. | Core fit. Default to non-recursive and make recursion explicit. | **Medium**; `walkdir`, symlink/reparse-point policy, permission tests. | **Include early** |
| 9 | Photo thumbnails | Visual confirmation avoids renaming the wrong camera exports. | Good fit for photos but should be optional and virtualized for thousands of rows. | **Medium**; image decoding/cache size limits, cancellation. | **Include early** |
| 10 | Folders alongside files | Organization often requires folder moves/renames. | Useful, but mixed recursive file/folder mutations are cycle-prone. Use a separate folder stage with depth-aware ordering. | **High**; rename graphs and path validation. | **Defer until rename core is proven** |
| 11 | Move/copy to template-derived folders | Users want “organize by year/month/camera” rather than only rename. | Core product fit and a point of differentiation over simple PowerRename. Cross-volume moves are not atomic, so risks must be explicit. *(0.1.0: one flat destination folder per job; no per-file template-derived paths.)* | **High**; safe copy-to-temp, byte verification, cleanup/recovery. | **Include early, carefully scoped** |
| 12 | Editable old→new mapping import/export | Archivists often receive a curator-approved list of names. | High fit. It makes corrections auditable and supports external review. | **Medium**; CSV/JSON parser, duplicate/missing path validation. | **Include early** |
| 13 | Saved templates / rule presets | Repeated jobs should take seconds. | Essential for both levels; template examples teach beginners. Avoid untrusted executable presets. | **Low**; JSON schema/version migration. | **Include early** |
| 14 | File conditions (extension, name, path, size, dates) | Avoid applying a rule to the wrong subset. | Strong fit and naturally supports safe preview. Avoid arbitrary file-content conditions in v1. | **Medium**; typed filters and date/locale tests. | **Include early** |
| 15 | Preview with row-level exclusion and filter | Users need to inspect every proposal without being overwhelmed. | Non-negotiable. Summaries plus filters should surface errors and “only changes.” | **Medium**; virtualized table, incremental plan recomputation. | **Include early** |
| 16 | Collision, reserved name, invalid path, and long-path validation | Prevent failed jobs and data loss. | Non-negotiable; must model Windows/NTFS behavior rather than only string comparisons. *(Requirement not yet met: 0.1.0 validation is string-based.)* | **High**; Windows-specific tests and long-path policy. | **Include early** |
| 17 | Safe conflict policies | Existing target names are common. | Default must be **Skip and explain**, never overwrite. Add “append counter” only after preview. *(Intended policy — 0.1.0 instead blocks the whole job when any blocking issue exists.)* | **Medium**; batch-wide collision graph tests. | **Include early** |
| 18 | Durable journal, rollback, and recovery | A bad batch may be spotted after restart; archivists need auditability. | Defining feature. Reversal must revalidate each file against its recorded fingerprint and stop on divergence rather than overwrite newer user work. | **High**; SQLite transaction journal, file-fingerprint checks, crash/failure tests. | **Include early** |
| 19 | Progress, cancellation, and per-item errors | Thousands of files cannot block the UI or become all-or-nothing. | Essential. “Stop safely” completes only the current irreversible step and preserves a recoverable journal. | **High**; worker isolation, cancellation state machine, UI messages. | **Include early** |
| 20 | Log and CSV/JSON operation export | Professionals need proof and troubleshooting. | Strong fit; no privacy cost because it is local and opt-in. | **Low**; serialization/redaction of local paths in shareable reports. | **Include early** |
| 21 | Timestamp/attribute mutation | People want dates corrected. | Useful but can compromise archival provenance and lacks easy filesystem rollback. | **High**; Windows attribute APIs plus reliable restoration. | **Defer** |
| 22 | Write embedded metadata | Correct embedded dates/titles, not only filenames. | Out of the focused rename/organization scope; file-content mutations need backups and format expertise. | **High**; ExifTool-like capability and corrupt-file risk. | **Reject for now** |
| 23 | Durable automated watched folders | Hands-free downloads organization. | Valuable later but mistakes repeat automatically, network folders poll, and rollback becomes complex. | **High**; Windows notification APIs, debounce, rule versioning, safety gates. | **Defer** |
| 24 | Command-line / Task Scheduler runner | Professionals need automation and CI-like reproducibility. | Valuable once the JSON rule format and job engine are stable. It must default to `--dry-run`. | **Medium**; stable public schema/versioning and exit-code contract. | **Later enhancement** |
| 25 | Explorer context menu | Quick access from the familiar file manager. | Good convenience; not required to learn the app. Avoid making a shell extension or resident process. | **Medium**; installer/registry integration and selected-path handoff. | **Later enhancement** |
| 26 | Scripting / arbitrary command execution | Specialists can solve exceptional transformations. | Poor early fit: it weakens safety, has difficult supportability, and may expose dangerous commands. | **High**; sandboxing is not realistic for arbitrary scripts. | **Reject for now** |
| 27 | Content extraction from documents | Invoices and scans could be organized by inner text. | A separate document-automation product surface; privacy/performance/format complexity are substantial. | **High**; PDF/Office/text extraction and possibly OCR. | **Reject for now** |
| 28 | Online media identification/subtitles | Media libraries benefit from authoritative titles. | Not a direct fit for the requested general file/photography/archival studio and breaks offline premise. | **High**; APIs, rate limits, matching accuracy, credentials. | **Reject for now** |
| 29 | Cloud/Evernote upload integrations | Some people automate handoff to services. | Out of scope and expands credentials/network risk. Local synced folders remain normal folders. | **High**; OAuth, sync failures, security review. | **Reject for now** |
| 30 | Network/UNC drive support | Offices and home NAS users need files outside local disks. | Include ordinary user-selected network paths but label availability and non-atomic cross-share risks. Do not add automatic watching initially. | **Medium**; transient access and permission error tests. | **Include early with limits** |
| 31 | Portable data mode | Archivists may work from a removable drive or controlled workstation. | Useful, but portable operation history needs clear data location and removable-drive failure recovery. | **Medium**; explicit mode selection, portable path resolver. | **Later enhancement** |
| 32 | Dark mode, high contrast, scaling, keyboard, screen reader access | Everyone benefits; some users require it. | Required, not cosmetic. Framework support is not a substitute for keyboard/focus/a11y test cases. | **Medium**; UI framework, manual accessibility review. | **Include early** |
| 33 | Natural-language rule assistant | Users may not know regex. | Interesting, but generated rules can be wrong and cloud AI conflicts with offline/privacy. | **High**; local model quality or remote disclosure, explanation/review requirement. | **Investigate, not v1** |
| 34 | Full photo batch editing/conversion queue | Photographers may also need image processing. | digiKam covers this well; adding it makes the product unfocused. | **High**; codecs/color/raw processing. | **Reject for now** |
| 35 | File hashes / duplicate grouping | Archivists may need evidence that two files match. | Valuable adjacent capability, but expensive at scale and should not delay safe renaming. | **Medium**; streaming hash, progress, cache invalidation. | **Later enhancement** |

## 5. Recommended scope

### Strong candidates for the first useful release

*(This shortlist governs. §4 records fit assessments; several §4 “Include early” items — thumbnails, mapping import/export, operation export — are not in this first-release list.)*

1. **Local files, with best-effort user-selected network paths; rename, copy, and move plans** — supports the actual organization need while retaining a focused offline product.
2. **Guided templates plus an ordered rule pipeline** — gives newcomers understandable starting points and experts deterministic composition.
3. **Filesystem + JPEG/Exif fields, explicit sorting and numbering** — directly serves photographers without prematurely promising every media format.
4. **Preview as a safety workspace** — virtualized old/proposed table, only-changes/errors filters, per-row exclusion, and a readable risk summary.
5. **Windows-aware validation and safe default conflicts** — block reserved names, malformed paths, and collisions; never overwrite (0.1.0 blocks a job containing any collision; the intended later policy is skip-and-explain, §4 #17).
6. **Durable job ledger and conflict-aware rollback** — the differentiator: record outcomes, revalidate before reversal, and give recovery guidance instead of silently overwriting changes.
7. **Responsive execution** — background worker, progress, safe cancellation, per-item results, and operation export.
8. **Accessibility baseline from day one** — keyboard navigation, visible focus, system theme/high-contrast-aware design, accessible labels/descriptions, text scaling, and screen-reader test passes.

### Useful later enhancements

- Folder renaming as a separately planned stage.
- Extra media/document metadata readers after format and maintenance review.
- Context-menu launcher without a shell extension.
- Read-only CLI with `--dry-run` default, then controlled execution.
- Portable data directory mode.
- SHA-256 hashes and duplicate grouping.
- Watched folders only after a strong “observe → preview → enable” safety design.

### Avoid for now

- Arbitrary user scripts/commands, metadata writes, delete workflows, document-content classification, online media databases, cloud accounts/uploads, and full image editing. They dilute the core or add irreversible/security/network risk disproportionate to initial value.

### Questions requiring user research or technical investigation

| Question | Why it matters | Recommended validation |
|---|---|---|
| Which three rename jobs dominate: camera ingestion, download cleanup, archive migration, or something else? | Determines first templates, metadata fields, and vocabulary. | Five 30-minute contextual interviews; ask users to bring a real non-sensitive folder. |
| What does “rollback” need to cover after manual moves, backups, or sync changes? | Establishes strictness for safe recovery. | Prototype divergent rollback scenarios and ask archivists whether stop-and-explain is acceptable. |
| Is folder renaming indispensable in v1? | It expands rename graph complexity dramatically. | Card-sort tasks using a file-only/move-to-folder prototype. |
| Is Windows 10 support commercially necessary? | Changes test matrix and installer commitment. | Survey target organizations; do not infer from developer machines. |
| Are local JPEG/Exif features enough before RAW/PDF/audio/video support? | Adds a significant dependency and malformed-file test surface. | Collect anonymized extension counts and sample workflows. |

## 6. Original feature concepts (hypotheses, not uniqueness claims)

The market review establishes that previews, undo, presets, metadata tokens, and automation are common. The ideas below combine those capabilities around trust and explainability. They are **not claimed to be unique**; comparable-product research should continue before branding any as novel.

| Rank | Concept | User experience and problem solved | What may be distinctive | Value / feasibility / risk / maintenance | Prototype or user test |
|---:|---|---|---|---|---|
| 1 | **Rename Ledger with divergence-aware recovery** | Each completed job becomes a receipt showing original path, target path, fingerprint check (size + modification time), rule snapshot, and result. “Undo” performs a preflight; if a file has been modified or replaced, it stops that row and offers locate/skip/export guidance. | Existing tools document undo/history, but a durable, per-item recovery workflow that explicitly detects divergence and helps resolve it is a promising distinction. | **Very high value; medium-high feasibility.** Risk: users may expect impossible reversal after external edits; wording must be exact. SQLite schema migrations and file identity edge cases are ongoing work. | Rename 200 files; manually move/edit/delete 12; evaluate whether five archivists understand the recovery report and avoid unsafe choices. |
| 2 | **Order Observatory** | Before numbering, the app states: “Sorted by Exif capture time ascending; ties by original filename; 18 items have no Exif date.” It lets users inspect ties and preview numbering changes. | Other apps sort and number; exposing the *reasoning*, ambiguities, and fallback sequence as first-class information directly addresses a common hidden failure. | **High value; high feasibility.** Maintenance is limited to sort definitions. Risk: too much detail for casual users; put it in a collapsible “Numbering order” card. | Give photographers files with identical/missing timestamps. Compare accurate results and task confidence against a traditional numbering dialog. |
| 3 | **Sidecar and sibling relationship groups** | When a user selects `IMG_0123.CR3`, FilenameWeaver can offer to include known sidecars such as `.xmp`, matching JPEGs, captions, or configured extensions, and previews the group together. | Some media tools organize companion files, but a configurable, visible relationship group for camera/archive workflows is a strong hypothesis. | **High value; medium feasibility.** Risk: false relationships; never auto-include by default and show every group member. Configurable patterns require careful documentation. | Observe photographers using RAW+XMP+JPEG folders; measure mistaken partial renames with and without the suggestion. |
| 4 | **Naming Contract** | A saved preset can include human-readable policies and executable checks: required date format, maximum name length, forbidden punctuation, sequence uniqueness, and sample input→expected output tests. Users see a green “contract satisfied” preview. | Competitors have validation and presets, but combining policy documentation, expected examples, and preflight checks supports institutional archives and repeatable handoffs. | **High professional value; medium feasibility.** Risk: it can feel like programming; offer plain templates first. JSON schema versioning is manageable. | Partner with an archive team; encode one real collection specification and see whether staff can audit a new import without a naming expert. |
| 5 | **Risk Budget summary** | Preview begins with a textual safety summary: “1,248 ready; 3 skipped conflicts; 6 missing metadata; 41 cross-volume copies; no overwrites.” Each total links to filtered rows. | Previews are ubiquitous; converting problems into a categorized, action-oriented decision summary could reduce overlooked warnings. | **High value; high feasibility.** Risk: color-only design; must include icons/text/counts and accessible live updates. | A/B task test: ask novices whether applying is safe and why; require them to locate all blocking issues. |
| 6 | **Rule-stage provenance** | Select a row and see each intermediate name: original → cleaned → date inserted → numbered → destination path. Professionals can toggle individual rules for a subset without losing the whole job. | Ordered stacks exist; per-row intermediate provenance makes complex pipelines explainable and debuggable. | **High value; medium feasibility.** Rule engine should preserve stage outputs; memory must be bounded/virtualized for huge batches. | Ask experienced users to diagnose three incorrect outcomes. Measure time-to-fix vs. final-output-only preview. |

### Innovation recommendation

Build **Rename Ledger**, **Order Observatory**, **Risk Budget**, and **Rule-stage provenance** into the initial architecture. They reinforce one coherent promise: “you can see, explain, apply, and recover every change.” Prototype **sidecar groups** and **Naming Contract** with photographers/archivists before committing them to first release UX.

## 7. Architecture & UX plan (before implementation)

> **Status note (0.1.0):** this section is the pre-implementation plan and is written in the present tense, but the shipped shell is smaller: a pasted folder path with optional subfolders, an ordered rule stack (prefix, suffix, literal replace, regex, case, whitespace, extension, date, counter), named presets, per-row inclusion, preview state filters, a risk budget, rule-stage provenance, CSV export of the reviewed list, plan handoff (save, revalidate, apply the unchanged rows), and a recovery ledger with confirmed rollback. Still absent: drag-and-drop intake, a native file picker, sortable columns, thumbnails, a progress bar, mapping import, a full job report, folder mutation, move/copy destinations in the UI, and theme support. Where this section and the README disagree, the README describes the shipped build.

### UI framework choice

| Option | Strengths | Material trade-offs | Decision |
|---|---|---|---|
| **Slint + Rust (selected)** | Rust-native application logic, declarative UI, Windows-fluent/native-style options, virtualizable models, and desktop accessibility integration through AccessKit/UI Automation. Slint documents Windows 10/11 testing and operating-system accessibility API support. | It is not a WinUI native-control application; each interaction and accessibility behavior still needs manual verification. The UI DSL is another language surface. | **Use for v1.** Best balance of Rust-first development, accessibility capability, small app architecture, and cross-version control. |
| WinUI 3 / Windows App SDK through Rust for Windows | Most native Windows visual language and UI Automation story; direct Windows integration. | Rust support has been experimental/preview in much of the ecosystem. The new `windows-reactor` is explicitly preview, and deployment requires Windows App SDK runtime. This is too much maturity/deployment risk for the initial product. | Do not select for v1; reconsider after framework stability and Windows-only product validation. |
| Tauri + HTML/CSS/TypeScript frontend | Excellent web UI tooling and mature packaging; Rust backend. | Adds a WebView2 runtime, a frontend supply chain, and a browser-style accessibility/security surface. It makes an offline, native-feeling file studio more complex than necessary. | Do not select for this focused, Rust-forward desktop product. |
| Raw Win32/common controls via `windows` crate | Lowest conceptual runtime surface and actual native controls. | Substantially more unsafe/platform plumbing, slower UX iteration, and expensive custom data-grid/pipeline UI work. | Not selected unless Slint proves unable to meet a tested accessibility/performance requirement. |

Framework evidence: [Slint desktop/native styles](https://slint.dev/desktop), [Slint Windows support](https://docs.slint.dev/latest/docs/slint/guide/platforms/desktop/windows/general/), [Slint accessibility](https://docs.slint.dev/latest/docs/rust/slint/docs/cargo_features/), [Microsoft Rust for Windows overview](https://learn.microsoft.com/en-us/windows/dev-environment/rust/rust-for-windows), [Windows Reactor preview notice](https://github.com/microsoft/windows-rs/blob/master/docs/crates/windows-reactor.md), and [Tauri’s WebView2 requirement](https://v2.tauri.app/start/prerequisites/).

### Application structure

> **Implementation note (0.1.0):** the sketch below is the planned multi-crate layout. The shipped code is a single crate — `filenameweaver_core` in `src/lib.rs` with modules per concern, plus the Slint shell in `src/main.rs` / `ui/main.slint` — and there are no `resources/templates/` or `docs/decisions/` directories yet. Treat the paths below as direction, not as a map of the current tree.

```text
filenameweaver/
  crates/
    core/          # Pure rule model, planning, validation, collision graph, unit tests
    metadata/      # Filesystem/JPEG-Exif reading behind typed capabilities
    journal/       # SQLite job ledger, recovery/undo preflight, export
    worker/        # Bounded background executor, progress events, cooperative cancellation
    desktop/       # Slint views, view models, keyboard navigation, a11y strings
  resources/
    templates/     # Versioned built-in JSON template definitions
  docs/
    decisions/     # Architecture decision records and threat/recovery notes
```

- **Pure planning path:** input snapshots + rules → immutable proposed actions + warnings/errors. It performs no writes and is unit-testable.
- **Execution path:** only validated, selected actions go to a bounded worker. It reports structured events to the UI and commits journal status after meaningful transitions. *(0.1.0: the shell can exclude rows, but there is no partial selection of rules, and the included actions run on one background thread per apply.)*
- **Journal/recovery path:** rollback records its outcome on the original job's item rows; it never “pretends” that a divergent file was restored.
- **UI path:** keeps file I/O, Exif parsing, and traversal off the UI event loop. Virtualized rows and a bounded thumbnail cache are design targets; 0.1.0 renders all preview rows directly, and has no thumbnails or hashing.

### Initial UX flow

1. **Add files or a folder** — prominent buttons plus drag/drop; recursive inclusion is an explicit checkbox with a concise explanation.
2. **Choose a starting point** — “Clean names,” “Camera date + sequence,” “Organize into date folders,” “Use a saved template,” or “Build rules.” Templates remain editable.
3. **Build and inspect** — left rule pipeline, center virtual preview, right Safety Summary. Advanced controls are collapsed until requested.
4. **Review risks** — blocking errors always have text, count, and a filter; color is supplementary. Users can uncheck individual rows without losing their configuration.
5. **Apply** — a final non-destructive review pane states operation counts, cross-volume moves, conflicts, and rollback limits. The action label includes the count (“Apply 842 changes”).
6. **Progress and recovery** — non-modal progress with “Stop safely.” Completion links to a report and “Undo this job.” Undo runs another review rather than immediately changing files.

### Accessibility commitments

- Logical Tab/Shift+Tab and arrow-key navigation; keyboard equivalents for add, preview filter, apply, cancel, and undo.
- Visible focus rings; no color-only warnings; reduced-motion-aware transitions.
- Every interactive widget receives an accessible name and short description; a tooltip is supplementary and reachable with focus as well as pointer.
- Important status changes are conveyed using a polite accessible live region; blocking errors are programmatically linked to the affected input/rule.
- Respect Windows theme and text scaling; test high-contrast and 200% scaling on the supported Windows matrix.
- The actual screen-reader quality will be tested with Windows Narrator and UI Automation inspection before claiming support.

### Minimum supported Windows version and compatibility statement

- **Initial shipping target:** Windows 11 x64.
- **Planned compatibility target, pending real testing:** Windows 10 x64. Slint’s current desktop documentation lists Windows 10 and Windows 11 as tested x64 targets.
- **No support claim:** Windows 8.1, Windows 7, Server editions, ARM64, and network file systems with unusual semantics are not supported claims yet. Network shares will be handled as best-effort user-selected paths with explicit availability/error recovery.
- The application will use standard Rust filesystem APIs where possible. A small `cfg(windows)` wrapper may use documented Windows APIs only to query stable file identity for safe recovery; any `unsafe` will be isolated, justified, and covered by Windows integration tests. (0.1.0 contains no `unsafe` code and no such wrapper.)

### Dependencies and installer strategy

| Need | Proposed choice | Rationale / runtime implication |
|---|---|---|
| UI | `slint` | Declarative Rust UI with accessible desktop integration. No WebView runtime. |
| Rule and data models | `serde`, `serde_json`, `thiserror` | Well-established typed serialization and error context. |
| Traversal/regex | `walkdir`, `regex` | Mature implementations; regex is non-backtracking. |
| Date/time | `time` | Explicit parsing/formatting and timezone-aware policy where needed. |
| JPEG Exif | `kamadak-exif` 0.6 (selected; used by `src/metadata.rs`) | Keep initial format scope small; malformed metadata is data, not an app-crash condition. |
| Durable journal | `rusqlite` with bundled SQLite | Transactional local recovery journal; SQLite is embedded, so end users install nothing separately for it. |
| Background work | Standard-library threads initially; results posted back to the Slint event loop (no channels) | Avoid a runtime dependency; use bounded work and cooperative cancellation. |

**Release installer:** Build a signed, per-user Windows installer (Inno Setup is a practical candidate) that contains the signed executable, static resources, and, only if the chosen MSVC build requires it, the Microsoft Visual C++ Redistributable. Confirm this on a clean Windows VM before release. No browser runtime, Java, Python, database server, administrator service, or account should be required for normal use.

### Local data, portability, and privacy

- Store user state under `%LOCALAPPDATA%\FilenameWeaver\`:
  - `operations.sqlite` — completed/in-progress job metadata and rollback records;
  - `presets/*.json` — saved rule stacks. Each file carries a format tag and version that are refused when unknown, is written through a temporary file plus a rename, and contains rules only (no file paths);
  - `settings.json` — non-sensitive UI preferences *(planned, not yet created)*.
- *(Planned)* Exports are opt-in CSV and JSON; they contain paths, so the export dialog will warn before sharing. No export exists in 0.1.0.
- A later portable mode will place the same data beside the executable only after removable-drive recovery behavior is tested.
- Essential functionality remains offline. No telemetry, accounts, synchronization, update checks, file uploads, or GPS/network lookups are included in the initial scope.
- Backup: close the app and copy its data directory. *(Planned)* Export of individual operation reports/presets for portable review, and a “Clear history” action that removes the local ledger only after an explicit confirmation explaining that rollback records will be lost. Neither exists in 0.1.0.

### Error recovery and undo semantics

- Input scanning reports unreadable directories, permissions, broken links/reparse points, malformed metadata, unavailable network shares, and overlong/invalid target paths as row-specific statuses.
- The plan does not run while blocking faults remain. Recoverable faults leave the rule settings and selection intact.
- Same-volume rename handles cycles/case-only changes via temporary unique names. Cross-volume “move” is implemented as copy to a unique temporary destination, verify byte count, rename into place, then remove source; the UI marks it non-atomic.
- Every applied item gets a durable journal row. Undo is a planned reverse operation that verifies each file's recorded size/modification-time fingerprint and expected location; mismatches are skipped and explained. The app never uses overwrite as the default recovery action.

### Verification plan before any “complete” claim

The lists below are the *planned* coverage, not the current suite: 0.1.0 has 51 unit tests (see the README's Build and verify section).

- Core unit tests: name parsing, sort determinism, rule composition, invalid names, collisions, counters, CSV import, Exif absence/corruption, and journal transitions.
- Integration tests in disposable directories: cycles, case-only changes, duplicate targets, cancellations, permissions, interrupted jobs, cross-volume simulations where feasible, and divergent rollback.
- Windows UI checks: keyboard-only workflow, Narrator/UIA tree inspection, high contrast, 100/150/200% scale, dark/light themes, and thousands-row responsiveness on Windows 10/11 VMs.
- Tooling: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and Windows release build. Results will be reported exactly; Linux-only checks will not be represented as Windows validation.

## 8. Limitations of this research

- This is a feature/positioning study, not a hands-on benchmark or legal license audit.
- Some vendor pages contain product claims (especially scale/compatibility) that should be independently tested if used as a numerical comparison.
- Accessibility verification was only explicit in the sources reviewed for Bulk Rename Utility; absence of a source is not proof another product is inaccessible.
- Pricing and version limits may change; release planning should recheck the cited official pages.
