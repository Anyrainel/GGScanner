# Continuous capture API

Available on both manager servers, including the OCR edition (which returns the
`continuous_capture_disabled` error). In the Windows capture edition, enable
**Continuous Sync (Capture) / 持续同步（抓包）** before starting the manager server, then
log into the game. The listener runs until the server stops; OCR and manage jobs
do not replace this separate packet snapshot. Existing origin and preflight
rules apply. Responses use `Cache-Control: no-store`.

| HTTP | `error` | Meaning |
| --- | --- | --- |
| 409 | `continuous_capture_disabled` | This server session has no continuous listener. Enable it before starting the server. |
| 503 | `continuous_capture_no_data` | Enabled, but verified login identity and the initial character/equipment inventory are not ready yet. Keep polling and log in after starting the listener. |
| 503 | `continuous_capture_failed` | Listener initialization, decoding, or capture failed. `details` contains diagnostics. Restart the server after fixing the cause. |
| 200 | — | Latest parsed inventory is available. |

Success wraps the game's normal export, with no job ID:

```json
{
  "sessionId": "a4de65c5-2c68-4246-a04e-f451def99a47",
  "revision": 1,
  "updatedAt": 1793900000000,
  "data": { "format": "GOOD", "version": 3 }
}
```

`data` is a GOOD v3 export for Genshin and the existing HSR-Scanner export for
Star Rail; the example is abbreviated. `updatedAt` is Unix milliseconds when the
export content last changed. `revision` increases only on content changes and is
local to one login generation. `sessionId` changes on server start and each
observed login/account reset, including reconnecting to the same UID. This makes
a reset visible even when it occurs entirely between polls. Clients should poll
every few seconds and import when `(sessionId, revision)` changes. A new login or
disconnect clears the previous inventory to avoid mixing accounts; the endpoint
returns `continuous_capture_no_data` until the next initial inventory is ready.
No prior export file is loaded as live data.

Every approximately ten minutes, changed content replaces a single latest file
in the configured output directory (`genshin_export_latest.json` or
`star_rail_export_latest.json`). The file contains the export itself, without the
HTTP wrapper. Identical exports are not rewritten. File failures leave the
in-memory endpoint available and are retried at the next interval. The first
write occurs at the first interval with data; stopping does not force a write.

The listener reuses the game's existing structural packet decoders. Full
item-record updates replace records by instance ID, so enhancing an item does
not append another copy. Server-confirmed deletion notifications remove known
instances, including enhancement fodder and salvaged/recycled items. A ready
inventory remains available when its final item is removed; empty arrays mean
empty inventory, not missing capture data.

Genshin recognizes the strict StoreItemDelNotify shape: one packed GUID list
and one store-type value (1 or 2), with no other fields. Deletions require the
verified login UID and apply only to held GUIDs. Upgrade responses, strongbox
results and arbitrary GUID-bearing packets must not count as deletions. The
website replaces the supplied inventory sections from the resulting snapshot.

A single equipment family cannot establish Star Rail's initial inventory; the
structural matcher does not prove that a paired inventory batch is an
authoritative full baseline. Its website preserves missing items and applies
only explicit deletions from the reviewed PlayerSyncScNotify layout. The export
includes cumulative `generator.deletedInstanceIds` lists for `relics` and
`lightCones`, using the same account-scoped opaque hashes as `_uid`. These lists
survive missed polls, clear on a new login, and drop an ID if that instance is
observed again. They require `generator.itemIdentity: "account-instance-v1"`
and a verified UID; only that UID's matching captured items may be removed.
Legacy IDs and unrelated absent inventory remain intact. See `HSR_EXPORT.md`
for the extension and `CAPTURE_UPSTREAM_AUDIT.md` for supported protocol versions.

This is the latest **successfully decoded** inventory. Scalar-only
lock/equipment changes and other unsupported notification shapes are not
inferred from arbitrary packets. Fresh live-game validation is still needed;
packet regression tests establish decoder behavior, not every current server
layout. Re-login supplies a fresh baseline.
Genshin achievements appear if a recognized achievement packet arrives; they do
not block inventory availability. Star Rail continuous inventory excludes quests.


## 原神账号实机对账（Windows）

先在 GGScanner 抓包版的「管理器」里勾选「持续同步（抓包）」并启动服务器。等监听启动后，用同一账号进入游戏世界。游戏已经开着时，可以退回标题画面再进入。清除背包的套装筛选，扫描期间保持库存和锁定状态稳定，并把鼠标留给 GGScanner。

在仓库根目录运行以下命令。它只提交一次全量圣遗物 OCR。已有管理器监听在其他本地端口时，可传 `-Port`。

```powershell
./scripts/check-genshin-artifact-parity.ps1
```

加 `-Account` 可在一次 OCR 任务中扫描角色、武器和圣遗物。抓包已有成就清单时也扫描成就。账号模式默认最多等 180 分钟。每一类都按本次任务的结果和同一登录前后两份抓包核对；缺少成就清单时，报告保留其他类别的结果，并将成就标为无法验证。

每轮结果在 `test-results/artifact-parity/` 的独立目录下。脚本先保存抓包快照，用本次 OCR 的 `jobId` 取回圣遗物，再保存第二份抓包快照。两份抓包必须属于同一登录会话、同一 UID，且圣遗物清单一致。其他类别更新可以改变 `revision`。`report.txt` 记录匹配件数、缺件、多件、逐字段差异和示例。`metadata.json` 记录数据来源和任务状态。

退出码 `0` 表示比较范围内一致，`1` 表示有差异，`2` 表示本轮无法完整对账。报告按件数保留同词条的重复件，背包顺序和副词条位置不影响匹配。星标和祝圣状态按输入文件的字段覆盖率比较，初始值、总条数和逐次强化记录单列。库存 OCR 和抓包都包含五星套装里的四星件。可用 `python tools/compare_artifact_exports.py ocr.json capture.json --min-rarity 5` 对保存的两份文件单独复查五星。比较复杂的重复件时需要 Python 的 SciPy。

### 同一台测试机上反复启停

开发版使用仓库已有的 `Codex GGScanner Start` 和 `Codex GGScanner Stop` 计划任务。首次配置使用 `scripts/register-ggscanner-agent-tasks.ps1`。之后从普通 PowerShell 启停仓库的 `target/release/GGScanner.exe`。

```powershell
./scripts/ggscanner-agent.ps1 -Action Start -Game Genshin
./scripts/ggscanner-agent.ps1 -Action Stop
```

若原神已打开并已登录，可用 `./scripts/ggscanner-agent.ps1 -Action Restart -Game Genshin -RefreshGenshinWorld` 重新获取本次登录的背包。开发版在抓包监听就绪后识别游戏界面，按固定顺序退回标题并进入同一账号。遇到陌生画面会停下，命令会给出诊断截图路径。

一次登录可连续运行多轮 OCR。需要反复冷启动游戏时，可先运行 `./scripts/genshin-test-session.ps1` 并亲自确认一次 UAC。默认两小时内，开发版管理器开始监听后，助手会启动已安装的国际服原神。开发版 GGScanner 退出时，助手会请求关闭自己在本轮打开的游戏。手动打开的游戏由玩家控制。

`-Check` 可在普通权限下核对路径和游戏发行商签名。`-Minutes` 可改会话时长。关闭助手窗口、按 Ctrl+C 或到期会结束监督，当时打开的游戏可继续使用。此助手适用于 HoYoPlay 安装在系统 Program Files 下的 BigPC 布局。

## Account routing

Enabling automatic sync authorizes the websites to apply subsequent snapshots
without repeated session approval. Snapshots route by their game UID to the
matching account, creating one when necessary, regardless of the currently
viewed profile. Default and unrelated accounts are retained. Data without a UID
routes to Default. This is the websites' import fallback; successful GGScanner
packet captures always have a verified UID.

Genshin capture provides `metadata.uid` from its verified incoming login
response. It clears identity together with inventory on a new login, including
reconnects to the same UID, and waits for identity plus the initial inventory
before publishing. Unsupported login identity layouts report a capture error.
No player nickname is currently exported. No UID is guessed from
arbitrary scalar fields or character names. See `GENSHIN_EXPORT.md` for the
reviewed protocol layout and validation boundary.

Star Rail's reviewed login layout provides UID and optional nickname from the
same incoming KCP conversation as its inventory. Both one-time and continuous
capture require verified identity and carry the UID in `metadata`. If the login
layout is unsupported, a capture error explains how to update and log in again;
complete inventory alone cannot produce a successful anonymous export.
Public UID is account identity, not a credential.
No authentication tokens or key material are exported.

Packet captures with a validated UID and aligned instance identities mark
`generator.itemIdentity` as `account-instance-v1`; their `_uid` fields are stable,
opaque per-account equipment identifiers. Legacy/OCR exports retain scan-order
IDs and omit this marker. Clients must not treat those legacy IDs as stable.
