# HSR Mission state research — 2026-10-10

The existing local capture contains independent Mission state. A real mission
archive is feasible without using Quest records as the mission catalog. Static
ownership comes from game source, not the dump. This research makes no UI changes.

## Sources and method

- Datamine repository: `F:/Codes/genshin/HoyoData/data/raw/honkai_star_rail/turn_based_game_data`,
  revision `724b139d8c9c32d12552eb95745a4fee72bfe48b`.
- Original private capture: `F:/Codes/genshin/irminsul/debug_capture/hsr/hsr_1791618385100436700`.
  All capture processing was local; no capture bytes were uploaded.
- Production replay: `experimental/hsr/examples/analyze_dump.rs`; 6,912 raw frames
  yielded 480 decrypted command bodies. Successful inventory and Quest decoding
  corroborates the transport/session decryption. The replay log also records a
  transport-layer error on a frame; this is not proof of exhaustive capture.
- Reviewed protocol: [Reliquary HSR 4.6 source at a pinned revision](https://github.com/IceDynamix/reliquary/tree/0b8c1d4f8eb5ae9722b5c3273e36a464b7188a38/src/network/command/proto).
  Reviewed generated `merge_from` code in `GetMissionDataScRsp.rs`,
  `GetMissionStatusScRsp.rs`, `Mission.rs`, `MainMission.rs`, `MissionStatus.rs`,
  `GetMissionStatusCsReq.rs`, `Quest.rs`, and `QuestStatus.rs`.
- Existing classification: `F:/Codes/genshin/HoyoData/docs/hsr-quest-classification.md`
  and `src/hsr_data/exporters/quests.py` in that repository.

Command dumps contain protobuf bodies without command IDs. Message identification
therefore rests on the reviewed complete field shapes, hundreds of catalog ID
joins, and inner record schemas, not an asserted command-number mapping. The audit
is research tooling, not a production decoder. It intentionally rejects sparse
list-only lookalikes as confirmed responses.

## Static ownership and conditional relationships

Read both `MissionInfo_<id>.json` and `MissionInfo_<id>.partial.json` through Git
objects, including files outside the sparse checkout. Exclude layout files.
The base file provides `MainMissionID`, `StartSubMissionList`, and
`FinishSubMissionList`; the partial file's `SubMissionList` gives each step's
explicit `ID` and `MainMissionID`, plus take/finish conditions and actions.

| Check | Result |
|---|---:|
| MainMission table rows | 2,184 |
| MainMission rows with explicit config/ownership | 2,158 (98.81%) |
| SubMission text table rows | 14,897 |
| Text-table steps with explicit owner | 14,729 (98.87%) |
| Unique configured steps, including steps absent from text table | 17,882 |
| Conflicting owners / conflicting duplicate step configs | 0 / 0 |
| Take conditions with multiple AnySequence dependencies | 200 |

The audit reads 4,370 base/partial files. Twenty-six MainMission rows lack the
matching config evidence; 168 SubMission text rows lack explicit ownership in
these files. Coverage refers to this source revision, not all historical missions
or every player's eligible content. The text table is not a complete step catalog.

Checkable examples:

1. `Config/Level/Mission/1000101/MissionInfo_1000101.json` starts at `100010102`
   and finishes at `100020103`. Its partial file explicitly assigns `100020103`
   to MainMission `1000101`. Numeric prefixes would assign it incorrectly.
2. `Config/Level/Mission/1000400/MissionInfo_1000400.partial.json` assigns step
   `100040108` to `1000400`, even though its `MissionJsonPath` is under folder
   `1000401`. `TakeType=AnySequence` carries `[100040106,100040107]`.
   Folder ownership is also unsafe.
3. `Config/Level/Mission/1000502/MissionInfo_1000502.partial.json`, step
   `100050213`, has `FinishType=SubMissionFinishCnt`, `ParamType=ListContain`,
   six IDs `100050251`–`100050256`, and `Progress=3`. Counting all six as
   mandatory steps would misstate the objective. Its take condition separately
   lists `[100050212,100050206]`.

The saved static graph preserves full take/finish conditions, custom values,
actions, main take/begin operations, script paths, and source file provenance.
Preserve typed condition groups; do not turn them into unconditional edges.
AnySequence, MultiSequence, CustomValue and Auto must remain distinct.
`Mission_*.json` contains runtime task sequences, and linked dialogue/scene
scripts may influence branch choices. This audit does not implement the game
interpreter or claim that all path choices can be evaluated from static data alone.

## Direct state already in the capture

The following names refer to the new isolated replay session
`F:/Codes/genshin/irminsul/debug_capture/hsr/mission_research_decrypted/hsr_1791688208816874500`.

`hsr_000056.bin` matches `GetMissionDataScRsp`:

| Reviewed field | Evidence |
|---|---|
| 9: finished_main_mission_id_list | 1,268 unique IDs; all match MainMission |
| 11: main_mission_list | 176 MainMission records; all IDs match; status 1 |
| 8: mission_list | 387 Mission records; status 1 |
| Mission inner fields | 1=status, 4=progress, 8=id |
| MainMission inner fields | 1=custom values, 6=id, 14=status |

Finished main types: 244 Main, 919 Branch, 61 Gap, 44 Companion.
Doing main types: 2 Main, 166 Branch, 1 Gap, 2 Companion, **5 Daily**.
Only 210/387 current steps occur in the SubMission text table; absence there
must not invalidate otherwise well-formed Mission records.

Twelve richer `GetMissionStatusScRsp` matches provide additional explicit states.
For example, `hsr_000117.bin` contains 116 sub records: 62 status 2, 53 status 0,
and one status 1; fields 3 and 14 contain 29 unfinished and 25 finished main IDs.
`hsr_000370.bin` contains 403 sub records: 322 status 2 and 81 status 0.
Reviewed response fields: 2=sub states, 3=unfinished main, 5=current-version
finished main, 7=retcode, 9=disabled main, 12=main custom values, 14=finished main.

Across the confirmed responses, the latest explicit sub states cover 1,235 IDs:
670 Finish, 387 Doing, 178 None. Static configs give owners for 1,218 of them;
17 remain unmapped. Queried main lists cover 657 Finished and 153 Unfinished
distinct IDs. These are queried subsets and overlap the initial response;
their counts must not be added to 1,268 or treated as a full status inventory.

Reviewed MissionStatus: 0=None, 1=Doing, 2=Finish, 3=Prepared. QuestStatus 3
instead means Close. Never share the enum interpretation across these systems.
None does not prove that a player must complete that branch.

The initial response also has 489 packed integers in obfuscated field 15, all
matching configured step IDs. Its reviewed name is `OLKALABEDOA`, not a verified
"finished sub list". Preserve it as unresolved evidence; do not label those
steps completed. Field 13 is likewise obfuscated. Recovering these semantics
needs additional source or controlled before/after captures.

## Quest joins: useful corroboration, sparse inference

The 3,656 Quest observations split into 1,950 achievements and 1,706 other
targets. Membership comes from AchievementData, never a numeric prefix.
There are **34 ID collisions with MainMission and zero with SubMission**.
For example Quest `4020701` has a GetItem condition (item 2, progress 1,000,000),
while MainMission `4020701` is a Branch mission. Equal numeric IDs are not a join.
Quest progress and counters describe its FinishWay objective; they do not encode
the main mission's full set of completed steps or a general step percentage.

Of 152 non-achievement Quest rows whose FinishWay is FinishMission, only 37 are
observed: 29 Finish, six Close, two Doing. All 35 completed/closed observed
conditions have a single target and threshold one. Thirty-one target references
occur in the direct initial finished-main list; four do not. These are counts of
conditions, not necessarily distinct target missions. The discrepant four
require investigation rather than overwriting direct Mission state. This is a
useful sparse inference channel, not a substitute for Mission snapshots.
A condition with multiple alternatives and a count threshold cannot identify
which individual alternatives the player completed. A missing Quest or an
unfinished Quest does not prove an associated mission unfinished.
LinearQuest has no multi-ID rows in this revision.

## Periodic exclusion and a defensible archive

Exclude MainMission Type=Daily (95 catalog rows). That leaves 2,089 non-Daily
catalog rows, but only 1,315 have a Name hash. Many rows are internal, hidden,
historical, unavailable, or mutually exclusive; 2,089 is not an honest universal
completion denominator.

Independently exclude Quest targets registered in DailyQuest, BattlePassQuest,
or CycleQuest without IsNonPeriodic=true. Explicitly one-time CycleQuest rows
remain eligible. This revision has 267 periodic target IDs; 51 occur among the
non-achievement observations, leaving 1,655 nonperiodic non-achievement targets.
Those 1,655 still do not constitute a story-mission catalog.

Build the future archive around curated named MainMission records, static
ownership/typed conditions, and direct state with provenance. Show Finished,
Doing, explicit None/Unfinished, Disabled, and Unknown separately. Unknown must
not become unfinished. Prefer a confirmed completion count and evidence coverage
until player eligibility, version availability and exclusive choices are resolved.
For a current step, show its own progress against its configured objective only
when that interpretation is verified. Do not sum step counts across branches.

No new capture is needed to prove feasibility or preserve the 1,268 direct
completed main missions. Next implementation work is a reviewed Mission decoder
and export schema, a static Mission catalog, and incremental snapshot merging.
Additional attended captures around accepting/completing a known branch, opening
the mission journal, and daily rollover are needed to validate transitions,
query scope, disabled states, field 15, and which historical sub states are
actually supplied. Record the game version and matched static revision.
Missing branch choices/custom-value semantics and the 17 unmapped observed steps
must stay unresolved until that evidence exists.

## Reproduce locally

Run from `F:/Codes/genshin/GGScanner` (paths below remain local):

```powershell
target/debug/examples/analyze_dump.exe F:/Codes/genshin/irminsul/debug_capture/hsr/hsr_1791618385100436700 --raw --dump-dir F:/Codes/genshin/irminsul/debug_capture/hsr/mission_research_decrypted --output-dir F:/Codes/genshin/irminsul/debug_capture/hsr/mission_research_export
python experimental/hsr/research/mission_audit.py F:/Codes/genshin/HoyoData/data/raw/honkai_star_rail/turn_based_game_data F:/Codes/genshin/irminsul/debug_capture/hsr/mission_research_decrypted/hsr_1791688208816874500 --output F:/Codes/genshin/irminsul/debug_capture/hsr/mission_research_audit.json --graph F:/Codes/genshin/irminsul/debug_capture/hsr/mission_research_static_graph.json
```

Each replay creates a fresh timestamped folder; substitute that actual folder in
the second command. The script reads Git objects without altering the sparse
checkout. A partial clone may fetch missing *public datamine* objects. Its report
contains aggregate player state and static examples; the static graph contains
only public configuration. Decrypted bodies stay outside the source repository.
Validation: repeated audit reproduced these totals; checked cross-prefix owner
100020103→1000101 and the six-candidate/three-required condition in the saved graph.
