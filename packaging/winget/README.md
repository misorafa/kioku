# winget manifests (misorafa.kioku)

Source of the manifests submitted to https://github.com/microsoft/winget-pkgs under
`manifests/m/misorafa/kioku/<version>/`. The package is the Windows release zip
(`kioku-v<version>-x86_64-pc-windows-msvc.zip`, kioku.exe at its root) installed as a
portable command `kioku`. Update PackageVersion, InstallerUrl, InstallerSha256 (uppercase
hex from the release's `.zip.sha256`), ReleaseDate and ReleaseNotesUrl for each version.

## Automation

From the second version on, `.github/workflows/winget.yml` submits each release: release.yml
calls it after the GitHub Release (not for pre-release tags), and it can be run by hand
(Actions → winget → Run workflow, tag `v<version>`) for a tag that is already released. It
uses winget-releaser (komac), which starts from the previous version's manifests in
winget-pkgs, so the files here are only needed for the first submission. It needs the
`WINGET_TOKEN` repository secret: a classic personal access token of ShinichiroGoto (the
owner of the winget-pkgs fork) with the `public_repo` scope. Without it the job is skipped.
