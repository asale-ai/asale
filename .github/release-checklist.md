## Before publishing this draft

Check all of the below against the built artifacts, then publish the draft by hand.

- [ ] the macOS `.dmg` is signed and notarized — otherwise other machines report it as damaged
- [ ] the Windows installer is Authenticode-signed. The build asserts this when the Azure credentials are configured, but on a fresh Windows machine also check that `Get-AuthenticodeSignature` reports Valid and that SmartScreen stays quiet — a brand-new certificate has no reputation yet and can still warn for a while
- [ ] every bundle has a matching `.sig` next to it (required for auto-update)
- [ ] installed once, and both signing in and going on the market work
