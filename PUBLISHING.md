# Publishing InventoryX

Everything that names the GitHub account uses the placeholder `@OWNER@` or the `&owner;`
entity. Set it once:

```sh
./scripts/set-owner.sh YOUR_GITHUB_USERNAME
```

## Release a version

1. `./scripts/package.sh 2026.12.01` builds `dist/inventoryx-2026.12.01.tgz` and writes
   `inventoryx.plg` in the repository root with the version and checksum filled in.
2. Commit `inventoryx.plg` and push to `main`.
3. On GitHub create a release tagged `2026.12.01` and attach the `.tgz` from `dist/`.
   The plugin file downloads the bundle from that release.

Unraid's "check for updates" reads `inventoryx.plg` from `main`, so pushing a new one is
what offers the update to users. Push it only after the release with the bundle exists.

## Community Applications (the Unraid app store)

1. The repository must be public, with `ca_profile.xml`, `plugins/inventoryx.xml`, a
   license and a README (all present).
2. Recommended: start a support thread on the Unraid forums and put its URL in
   `plugins/inventoryx.xml` (`<Support>`), `ca_profile.xml` (`<Forum>`) and the `support`
   attribute in `plugin/inventoryx.plg.in`. Until then they point at GitHub issues.
3. Submit the repository at https://ca.unraid.net/submit: run Scan and Validate, fix what it
   reports, then submit. Plugins are reviewed by hand.

## Try a build on your own server before publishing

`./scripts/install-plugin.sh root@your-server` copies the bundle to the flash drive and runs
Unraid's plugin installer, without needing a release.
