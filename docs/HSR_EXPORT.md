# Star Rail export format / 《星穹铁道》导出格式

GGScanner writes **one** JSON file: `star_rail_capture_<timestamp>.json`.
The screenshot scanner writes the same v4 shape to `star_rail_scan_<timestamp>.json`.

The file is **HSR-Scanner format v4**, the same interchange used by
[HSR-Scanner](https://github.com/kel-z/HSR-Scanner) (`source: "HSR-Scanner"`)
and [Reliquary Archiver](https://github.com/IceDynamix/reliquary-archiver)
(`source: "reliquary_archiver"`). Fribbels Optimizer imports either source.
GGScanner uses `source: "HSR-Scanner"` because Fribbels allowlists that
string; `generator` retains the historical producer name `GOODScanner` for
compatibility with existing exports.

抓包与截图扫描写同一套 v4 JSON，但文件名前缀不同（`star_rail_capture_` /
`star_rail_scan_`），避免互相覆盖。GGScanner 的 `source` 固定为 `HSR-Scanner`，
真正的生成器写在 `generator` 里。截图扫描会读取画面上能看到的技能/行迹文字；
无法从画面读出的字段（成就数据包、`ability_version` 等）省略，不另起一套 schema。
截图扫描把左下角水印里的 UID 写入 `metadata.uid`（整数；需两个画面读数一致才采用，否则为 `null`）；抓包导出使用同一登录会话中验证得到的 UID，并可附带游戏昵称 `metadata.nickname`。

## Required v4 fields / 必填 v4 字段

| Field | Meaning |
|---|---|
| `source` | `"HSR-Scanner"` (format discriminator, not the binary name) |
| `build` | `"v1.2.0"` (Fribbels compatibility identifier) |
| `version` | `4` |
| `metadata.uid` | Screenshot scans: the account UID (integer) read from the bottom-left on-screen watermark, accepted once two panels read the same value, else `null`. Packet capture: the UID from the reviewed incoming token response, tied to the inventory’s KCP conversation; `null` if not verified. Continuous sync consumers may use it to select the matching game account. |
| `metadata.trailblazer` | `"Stelle"` or `"Caelus"`, else `null` |
| `characters[]` | `id` (string), `name`, `path`, `level`, `ascension`, `eidolon`. Packet captures also include `ability_version` and single-unlock `traces` flags. Screenshot OCR fills upgradeable `skills` (Basic, Skill, Ultimate, Talent, plus `elation` on Elation characters) and optional `memosprite` levels. Screenshot scans do not read or export one-time trace-node flags; `traces.special` is such a flag, separate from the upgradeable `skills.elation` level. `ability_version` is omitted because it is not on-screen text. |
| `light_cones[]` | `id` (string), `name`, `level`, `ascension`, `superimposition`, `location`, `lock`, `_uid` |
| `relics[]` | all six slots; `set_id`, `name`, `slot`, `rarity`, `level`, `mainstat`, `substats[{key,value}]`, `location`, `lock`, `discard`, `_uid` |

`path` uses Fribbels names: Destruction, Hunt, Erudition, Harmony, Nihility,
Preservation, Abundance, Remembrance, Elation.

`location` is the equipped character template ID as a string, or `""`.
`_uid` is never a raw server item ID. When `generator.itemIdentity` equals
`account-instance-v1`, the export has a verified UID and complete aligned capture
instance records; `_uid` is a stable lowercase SHA-256 hex string scoped to that
game account and item family. Upgrading an item or reconnecting the same account
preserves it; another account gets different IDs. Without that exact flag,
including OCR and anonymous exports, `_uid` remains a scan-order synthetic ID.
`metadata.nickname`, when present, is the same login’s in-game display name; it
is mutable and nonunique and must never be used as an account matching key.
Internal semantic observations and manager schemas remain anonymous.

Percent substats use a trailing `_` (`CRIT Rate_`, `HP_`). Flat stats do not.

## GGScanner extensions / 扩展（省略 = 未观察）

Unknown fields must be ignored by consumers. Omit a field when that data was
not captured; do not invent empty coverage.

未知字段必须忽略。某类数据未被抓取时**省略该字段**，不要写成空集合冒充“已观察”。

| Field | When present |
|---|---|
| `generator` | `{name:"GOODScanner", version, captureRevision, compatibility}` |
| `generator.deletedInstanceIds` | Optional `{relics:string[], lightCones:string[]}` containing cumulative confirmed deletions in this login, as the same opaque hashes used by `_uid`. Requires verified `metadata.uid` and `generator.itemIdentity: "account-instance-v1"`. Continuous consumers remove only matching captured instances for that UID, preserving unrelated absent records. A reappearing instance drops its deletion marker; a new login clears all markers. Manual imports retain their normal coverage semantics. |
| `coverage` | `{characters, lightCones, relics}` each `complete` / `unknown` / … |
| `metadata.current_trailblazer_path` | current Trailblazer path, same names as `characters[].path` |
| `quests` | `{schemaVersion:1, coverage:"complete", records:[{id,status,progress,counters}]}`; records sorted by unique positive ID. Omitted means no Quest snapshot was captured |

The complete Quest snapshot contains achievements and other objectives together.
Status values are 0 (None), 1 (Doing), 2 (Finish), 3 (Close), 4 (Delete).
GGStarRail derives completed achievements from catalog membership and statuses
2/3, and retains every record for the Objectives archive. Missing `quests`
leaves existing progress untouched. Legacy `achievements` exports remain importable.

`quests` 保留成就与其他目标的完整列表、状态、进度和计数，网站负责分类。
没有该字段时保留已有进度；出现完整快照时替换。完成时间不写入导出。

## Minimal example / 最小示例

```json
{
  "source": "HSR-Scanner",
  "build": "v1.2.0",
  "version": 4,
  "generator": {
    "name": "GOODScanner",
    "version": "0.0.0",
    "captureRevision": "reliquary-quest-4.6-v2",
    "compatibility": "HSR-Scanner v1.2.0 / format v4"
  },
  "metadata": {
    "uid": null,
    "trailblazer": "Stelle",
    "current_trailblazer_path": "Remembrance"
  },
  "coverage": {
    "characters": "complete",
    "lightCones": "complete",
    "relics": "complete"
  },
  "characters": [],
  "light_cones": [],
  "relics": [],
  "quests": {
    "schemaVersion": 1,
    "coverage": "complete",
    "records": [{"id": 4010101, "status": 3, "progress": 1, "counters": []}]
  }
}
```

Character, Light Cone, and Relic object shapes follow
[Fribbels' Kelz parser](https://github.com/fribbels/hsr-optimizer/blob/main/src/lib/importer/kelzFormatParser.tsx).
Reliquary's exporter is a second producer of the same v4 inventory objects;
it does not define `achievements` or `current_trailblazer_path`.
