# GGScanner rename rollout

The editions are `GGScanner.exe` (capture, OCR, and manager) and
`GGScannerOCR.exe` (OCR and manager, with capture dependencies excluded).

## Same-day publication

Publishing is blocked if the daily release or tag already exists. Ordinary CI
builds remain available. The workflow never automatically deletes or overwrites
a published release. To deliberately replace it, an operator must manually remove
the release and its tag, then dispatch publication again. Remove both together
with `gh release delete <tag> --yes --cleanup-tag` so the new tag points at the
new build's commit.

## Publish the updater bridge first

The bridge was published under the existing `GOODScanner.exe` and
`GOODCapture.exe` download names. Its internal Cargo targets and branding already
used the GG names. That stage is complete.

Publish this bridge before renaming the GitHub repository or public downloads.
The bridge has already shipped in v2026.10.16. The rename can proceed before
every installed copy has adopted it; older users may need a fresh download.
The bridge updater tries both `Anyrainel/GGScanner` and `Anyrainel/GOODScanner`, follows
repository-rename redirect chains, and uses stable edition roles in `update.json`:

```json
{
  "tag": "v2026.10.07",
  "revision": 125,
  "assets": {
    "scanner": "GOODScanner.exe",
    "capture": "GOODCapture.exe"
  }
}
```

The example revision is illustrative; CI writes its actual run number. These
filenames are complete uploaded asset names. Build revisions stay in metadata;
each edition is uploaded once, without a revision suffix.
Old manifests without `assets` remain supported. Existing updaters ignore the
additional fields.

## Rename and retain compatibility downloads

At the rename cutoff, rename the repository in GitHub Settings and update
local remotes to `https://github.com/Anyrainel/GGScanner.git`. Do not reuse the old
repository name: GitHub's redirect is needed by older installed versions.

At that cutoff, change CI's manifest roles to
`scanner: GGScannerOCR.exe` and `capture: GGScanner.exe`.
For one calendar month after the first release with the new public executable
names, continue uploading copies under both old names
(`GOODScanner.exe`, `GOODCapture.exe`). Record the actual first publication
date and its one-month cutoff in this document when publishing.

The first renamed release is v2026.10.21 (2026-10-21); compatibility downloads
expire on 2026-11-21. `.github/release-policy.json` controls packaging and cleanup.
The release workflow uploads canonical names first and maps `update.json` only
to them. Once the cutoff is reached, new releases omit compatibility copies.
The scheduled `retire-legacy-downloads.yml` workflow removes only the two old-name
assets from transition releases on November 21 at 16:00 UTC. Its script is
idempotent and can also be dispatched manually. These aliases are temporary.
At the cutoff, remove the old-name assets from the transition releases and stop
uploading them in subsequent releases. Historical pre-rename releases can retain
their original assets; do not rewrite their manifests or tags.
During the transition, aliases must preserve edition: old GOODScanner receives the OCR build; old
GOODCapture receives the capture build.

Users who miss the transition and cannot auto-update should download the current
edition again from either companion website's `/download-scanner` page. Include
that manual recovery route in the rename announcement. Automatic updates from
all historical versions are not a requirement after the compatibility cutoff.

The update replaces the executable at its existing local path and restarts that
path. It deliberately preserves the installed filename and shortcuts; a fresh
download uses the new names. Existing settings filenames and wire-format IDs
are independent of the branding and should remain compatible.
This includes the restart environment variable, `goodscanner.hsr.manager-instructions`,
GOOD's `yas-GOODScanner` source, and Star Rail's historical `generator.name`.

Both companion websites link to the GGScanner repository and canonical
GGScanner/GGScannerOCR assets. The updater prefers canonical assets when the
API inventory is available, including when a cached manifest names a legacy
alias. Without inventory, the manifest remains authoritative for older releases;
without either, canonical names are used. The old repository URL remains only
as a compatibility discovery fallback. The local checkout folder is `GGScanner`;
related repositories use `../GGScanner`. Remove obsolete worktrees before moving
the checkout, or repair retained worktree links with `git worktree repair` afterward.

The updater checks the API's asset inventory for the matching edition. If a
newer release has no matching executable, it shows a manual-download notice
instead of offering a broken automatic download. Download and installation
failures also offer a GitHub release-page link. With API access unavailable,
the manifest or legacy filename convention is used, and a missing download
is handled by the same manual fallback. A newer manifest can supersede a
cached API inventory; in that case download failure provides the fallback.

Old versions whose API access fails may not handle an extra repository-rename
redirect through a mirror. The bridge fixes this for updated installations;
legacy asset aliases cannot fix code already installed. If an old version cannot
discover the renamed repository, a manual download is required once.

## Possible Linux release

Linux support may land before the next release; it is not yet a release promise.
Before adding Linux downloads, define supported architectures and display/window
requirements, verify capture/OCR/manager support per game and edition, and choose
clear platform-specific assets. Ensure release metadata and updater asset selection
distinguish operating system, architecture, and edition before publication;
Windows clients must never select a Linux binary. Linux may initially use manual
downloads with self-update explicitly disabled. Both websites must show platform
choices only once corresponding tested assets are available. The old Windows
filename compatibility window remains one month and does not restart when Linux
support is added.

## Validation

- Test legacy manifests and renamed manifest roles, including edition mapping.
- Test missing executables without switching an OCR installation to capture.
- Test redirect chains and bounded redirect loops with a local HTTP server.
- Check both editions build and keep capture dependencies feature-gated.
- Inspect release uploads for canonical and compatibility names before publishing.
