# Security

This release covers Hulder source for Apple silicon macOS. The inherited collaboration server, deployment tools, Windows builds and Linux builds are not supported release targets. Do not deploy inherited server tooling from this snapshot. Their dependencies have not been cleared for deployment.

Hulder is independently maintained and derived from Zed. Preserve the upstream licenses and attribution. Report Hulder issues to its maintainers rather than treating this as an official Zed distribution.

## Reporting

Once the public repository is created, enable GitHub private vulnerability reporting and use its Security tab to submit reports. Until a private reporting channel is available, do not post credentials, private files or exploit details in a public issue. A public issue may request a private contact without including sensitive details.

## Release checks

Use the pinned Rust toolchain and Python 3.11 or newer:

```sh
python3 script/test-audit-release.py
python3 script/audit-release.py --output /tmp/hulder-security.json
cargo build --locked -p zed --bin Hulder --features util/debug-embed
```

The audit follows normal and build dependencies from the editor root for Apple silicon macOS. It queries the public OSV database with package names and versions. It uploads no source, credentials or application data. A failed query fails the check. New security and unsoundness advisories block the release. Git dependencies require source review and are listed separately in the report.

Eight currently reachable maintenance notices concern async-std, bincode, core2, paste, rustls-pemfile, rustybuzz, ttf-parser and unsafe-libyaml. These describe discontinued maintenance rather than a reported vulnerability in the checked versions. Their replacement is tracked maintenance work. The audit permits only the specific reviewed notice identifiers and only while they remain classified as unmaintained. It will block a reclassification to a security or unsoundness advisory. Reassess these dependencies before every release.

The fixes in this candidate include Wasmtime and WASI 48.0.6, rustls 0.23.46, cxx 1.0.195, patched memory and cache dependencies, and replacement of the inherited RSA implementation with AWS LC, which uses isolated native symbols. Authentication accepts OAEP with SHA256 and refuses legacy padding. The new cache dependency minimum is 0.18.2. Always build with the committed lockfile.

## Trust boundaries

The editor, its extensions, terminals and explicitly connected agents can access local files under their respective permissions. Veil is a convenience privacy feature, not a replacement for the operating system lock screen or a boundary against another local process.

Factory endpoints bind locally and require their documented authentication and approval controls. Do not expose them directly to the internet. Model providers and tools require their own credentials. Keep credentials and runtime state outside published source.

This is a source release. Public Mac binaries require the separate signing, notarization and clean installation checks in RELEASE.md. A dependency audit is not a guarantee that the complete application has no defects.

## Hulder installer updates

The dedicated updater contacts the public ChaosDeSelva/Hulder GitHub release feed and accepts only matching Apple silicon installer URLs. It validates the asset size and SHA256 digest, the Developer ID team 3LS9LD88JX and Apple notarization before revealing the download. It does not send project files or credentials, change the shared editor HTTP policy or replace the running application. Disable automatic checks with auto_update: false; manual checking remains available.
