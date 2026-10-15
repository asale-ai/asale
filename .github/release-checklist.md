## Before publishing this draft

Check all of the below against the built artifacts, then publish the draft by hand.

- [ ] the macOS `.dmg` is signed and notarized — otherwise other machines report it as damaged
- [ ] the Windows installer is Authenticode-signed. The build asserts this when the Azure credentials are configured, but on a fresh Windows machine also check that `Get-AuthenticodeSignature` reports Valid and that SmartScreen stays quiet — a brand-new certificate has no reputation yet and can still warn for a while
- [ ] every bundle has a matching `.sig` next to it (required for auto-update)
- [ ] installed once, and both signing in and going on the market work


## Desktop update regression checks

- [ ] Start an older, root-owned macOS installation as a standard user. Leave automatic updates enabled for two check intervals: the running client must stay open, settings must explain that authorization is needed, and the same release must not retry automatically.
- [ ] Click Install update in that client. The native authorization dialog must appear after download and verification. Cancel it: the app and selling must keep running, and another automatic check must not reopen the dialog.
- [ ] Retry and authorize. Confirm that the app and both CLI binaries reach the requested version, the desktop reopens as the original user, and the update error clears.
- [ ] Repeat from a user-owned installation: it must update without an authorization prompt.
- [ ] Force a CLI/app replacement failure. Confirm rollback, reopening of the previous client, a visible error, and no automatic retry of the failed release after restarting the app again.
- [ ] Check Windows and Linux bundles for persisted failed-release suppression; also verify their existing package/task update paths on their native platforms.

Older clients whose updater already cannot replace the app cannot acquire this fix through that broken path. Offer the signed graphical installer as the recovery path for this release; users should not need a terminal or ownership commands. Do not describe native authorization testing as complete until it has been exercised with the packaged app.
