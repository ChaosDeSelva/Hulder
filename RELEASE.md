# Hulder releases

## Source first

Hulder is independently maintained and derived from Zed. Preserve all upstream copyright and license notices, component-specific licenses and dependency notices. Publish the complete corresponding source and build instructions for each distributed binary, including local modifications; a patch alone is not the source release. Do not describe Hulder as an official Zed release.

Project Pages deployment is manually triggered. A source security workflow checks dependency changes and can also be run manually. Inherited upstream deployment, telemetry, Slack, issue automation and release workflows have been removed. No public repository URL is assumed. GitHub publishing is a separate operator action.

## Build and package on macOS

Use the pinned Rust toolchain, Xcode, Python 3.11 or newer and CMake. Authentication uses the same AWS LC provider already used by the TLS stack. From the source root:

```sh
cargo build --locked -p zed --bin Hulder --features util/debug-embed
script/bundle-mac --binary target/debug/Hulder --output dist/Hulder-Dev.app --channel dev
```

The package command requires Python 3.11+, refuses to overwrite an existing bundle, performs no downloads and creates an ad-hoc signed development app. Match the packaging channel to `crates/zed/RELEASE_CHANNEL` and the build configuration. The internal Cargo package name remains `zed` for source compatibility. A production build uses `cargo build --release --locked -p zed --bin Hulder` and the corresponding release executable; generate dependency licenses with `script/generate-licenses` before that build.

## Optimized local production build

Generate the dependency notices before compiling so they are embedded in the app. Set the stable channel for the compiler as well as the package. A stable channel identifies the build; it does not certify that Apple has notarized it.

```sh
script/generate-licenses
ZED_RELEASE_CHANNEL=stable RELEASE_CHANNEL=stable CARGO_PROFILE_RELEASE_DEBUG=0 cargo build --release --locked -p zed --bin Hulder
script/bundle-mac --binary target/release/Hulder --output dist/Hulder.app --channel stable
```

This uses optimized compilation with debug assertions disabled and embedded assets. The package initially has a local signature. Complete the Developer ID and notarization steps below before publishing binary downloads.

## Public Mac binary gate

Use your own Apple Developer ID Application certificate already installed in Keychain. Never commit certificate keys, passwords or notarization credentials. No Zed signing identity or team is used.

```sh
script/sign-hulder dist/Hulder.app "Developer ID Application: YOUR NAME (YOURTEAM)"
ditto -c -k --keepParent dist/Hulder.app dist/Hulder-notarization.zip
xcrun notarytool submit dist/Hulder-notarization.zip --keychain-profile hulder-notary --wait
xcrun stapler staple dist/Hulder.app
xcrun stapler validate dist/Hulder.app
spctl --assess --type execute --verbose=2 dist/Hulder.app
```

Configure the `hulder-notary` profile locally using your Apple account before submission. Require an Accepted result before stapling. Recreate the downloadable ZIP after stapling and record its SHA-256 checksum. Signing or submitting to Apple is not performed by the source cleanup or package script.

Before publication, test a clean install, file editing/save/reopen, project search, terminal, language-server diagnostics, Git diff, settings persistence and optional factory connection/disconnection. Verify launch without any Zed account, separate Hulder data directories, and visible license notices. Test the notarized download on a clean Mac. Windows and Linux packaging inherited from upstream is not supported by this release process.

## Remaining compatibility references

Internal crate names, action IDs, `.zed` project configuration, upstream documentation for inherited features, and third-party schema URLs may retain the upstream name. These do not represent a Zed cloud subscription or an official Zed distribution. Avoid mechanical renames that break project compatibility. Factory services and external tools can make explicitly requested network connections; the shared editor HTTP client is blocked. The extension catalogue has a separate restricted HTTPS client for public catalogue queries and extension archives.

## Source repository preparation

Use a reviewed source snapshot with fresh Git history. Do not push the original checkout history, local build outputs, acceptance logs or credential stores. Before creating a release tag, run the checks in SECURITY.md and retain the resulting report with the release evidence. The initial supported target is Apple silicon macOS.

## Native update feed

Before building a public installer, set `VERSION` in `crates/zed/src/update_release.rs` to its distribution version. This is separate from the inherited Cargo compatibility version. Publish a matching `vVERSION` GitHub tag and `Hulder-VERSION-macos-arm64.dmg` asset with a GitHub SHA256 digest. The updater includes preview releases, selects the newest compatible version and ignores drafts.

Sign and notarize both the app and the DMG. The updater pins the current publisher team ID and requires Apple notarization before offering the downloaded file. A signing team change requires an explicit updater migration. Check the native menu and download verification before publishing. The updater never replaces a running app automatically.
