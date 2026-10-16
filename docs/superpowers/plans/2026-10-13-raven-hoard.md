# Raven and the Hoard Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Spec section 18: a small Postman-style API client (**Raven**) whose requests are `.http` files under `.hulder/ravens/`, and one user-level auth store (**the Hoard**) that Raven, the Panda (section 6) and the Chrome picker (section 7) all draw on. The global HTTP client stays blocked; Raven owns a `reqwest_client` built on Send and dropped afterwards, and every send is a `Sent` entry in the Hollow Back.

**Architecture:** Two new crates.

- `crates/hoard` (lib root `src/hoard.rs`): `model.rs` is pure data (profile, kinds, cookies, host matching, https-only rule, expiry, redaction set); `store.rs` is `profiles.json` under `~/.config/hulder/hoard/` plus a `SecretStore` trait with a Keychain implementation (gpui `read_credentials`/`write_credentials`, which is the local Keychain on macOS; never Zed's cloud provider) and an in-memory fake for tests; `hoard.rs` holds the `Hoard` global entity and the two cross-crate APIs `apply_to_request` and `cookies_for_host`, plus a `ChromeBridge` hook so the Hoard can borrow cookies from the Chrome that `crates/browser` launched without depending on `browser` (browser registers the bridge, so the dependency points browser → hoard); `borrow.rs` is the CDP borrow (`Storage.getCookies` then `Network.clearBrowserCookies`); `modal.rs` is the `hoard::Toggle` modal.
- `crates/raven` (lib root `src/raven.rs`): `collection.rs` parses and serializes the REST Client `.http` format and `env.json`, and expands `{{var}}` with `$hoard:<profile>` left for send time; `store.rs` is synchronous `std::fs` under `<root>/.hulder/ravens/` with the saga containment rules (relative only, no `..`, symlink refusal on every component, temp-file-plus-rename writes) so Bifröst can call it from a background thread; `curl.rs` parses a cURL command line without ever running anything; `send.rs` builds the request, applies the Hoard, sends through a fresh `ReqwestClient`, enforces the 60 s timeout and 8 MiB cap, and records `Sent` in the Hollow Back; `log.rs` appends to `log.jsonl` with 2 MiB rotation, keeps the redacted per-request cache Bifröst reads, and writes "Save as Clipping" files; `view.rs` is the center `Item` modelled on `crates/herd_panel`.
- Shared crates get small additive edits: `hulder_net` (`NetEntryKind::Sent`, `record_sent`, a note on `Browsed`), `browser` (new `hoard_link.rs` registering the bridge and injecting cookies before navigation), `lightpanda` (new `hoard_fetch.rs`: probe `fetch --help` once per binary mtime, pass cookies as `--header`, else `serve` + CDP `Network.setCookies`), `bifrost` (new `raven_tools.rs` plus a `Routed::Raven` arm), `settings_content` (`raven.redact_headers`), `zed` (init order, namespace list, keymap).

**Tech Stack:** Rust, gpui, `reqwest_client`, `http_client` types, `cdp`, `serde_json`, `base64`, `chrono`, `util::command` for the Lightpanda child.

**Spec:** `docs/superpowers/specs/2026-10-13-hulder-design.md`, section 18 (and 5, 6, 7, 11, 12, 13).

## Global Constraints

- Secrets never reach logs, clippings, Bifröst, Saga files, toasts or Hollow Back entries. The Hollow Back `Sent` entry carries method, host, status and byte counts only. The log line carries no headers and no body. The last-response cache is stored already redacted.
- `env.json` is never written with a resolved `$hoard:` value; resolution happens in `send.rs` and the resolved map is dropped with the request.
- Every URL goes through `hulder_url::normalize_url(_, WEB_SCHEMES)` before it is sent, logged or handed to Lightpanda.
- Everything under `.hulder/ravens/` and `.hulder/clippings/` is written through `raven::store` (containment, symlink refusal, atomic rename).
- The cURL importer tokenizes text only; it never spawns a process or expands `$(...)`.
- The CDP borrow only ever uses the `CdpClient` the `ChromeBridge` hands over, which `crates/browser` takes from the `ChromeRunner` it launched itself.
- The Keychain is behind `hoard::store::SecretStore`; tests use `FakeSecretStore`.
- Follow repo CLAUDE.md: no `unwrap()` outside tests, no `let _ =`, no `mod.rs`, full-word names, `./script/clippy`, README marker stays.

## Review Focus

1. Redaction completeness: `hoard::model::redaction_set(profiles, extra)` must contain every header name a profile can produce (bearer and basic produce `authorization`, header produces its own configured name, cookies produce `cookie`), plus `set-cookie` and `x-api-key`. Test `every_profile_header_is_redacted` in Task 2.
2. The https-only rule: a profile must not be applied over `http` unless the host is `localhost`, `127.0.0.1` or `::1`. Test in Task 2.
3. `.http` round trip: parse then serialize reproduces names, methods, headers and bodies; `{{var}}` placeholders survive. Test in Task 3.
4. Store containment: `..`, absolute paths and a symlinked `ravens` directory are refused; writes go through a temp file and rename. Test in Task 4.
5. Send limits: body over the cap is truncated with a flag; a server that never answers trips the timeout and the client is dropped. Tests in Task 6 against a local TCP listener.
6. Log rotation at 2 MiB and bodies never present. Test in Task 6.
7. Bifröst `raven_*` tools never send: they read files and the redacted cache only. Test in Task 11.

---

### Task 1: Hollow Back learns `Sent`

**Files:** `crates/hulder_net/src/hulder_net.rs`, `crates/hulder_net/src/hollow_back.rs`

- [x] Failing tests: `NetLog::push` of a `Sent` entry bumps `sent`; `entry_line` prints `SENT`; `record_browsed_with_note` appends ` (with Hoard profile x)`.
- [x] Add `NetEntryKind::Sent`, `NetLog::sent`, `NetLog::record_sent(cx, method, host, status, request_bytes, response_bytes)` and `NetLog::record_browsed_with_note(cx, url, note)`; `status_tooltip` mentions sent requests.
- [x] `cargo test -p hulder_net`.

### Task 2: Hoard model and store

**Files:** `crates/hoard/Cargo.toml`, `crates/hoard/src/hoard.rs`, `crates/hoard/src/model.rs`, `crates/hoard/src/store.rs`, `Cargo.toml`

- [x] Failing tests in `model.rs`: host matching (`api.example.com` matches `*.example.com`, apex does not, case-insensitive exact), scheme rule, cookie expiry dropping, `Cookie` header assembly, `apply_secret` for each kind, redaction set coverage, `profiles.json` round trip.
- [x] Failing tests in `store.rs`: `FakeSecretStore` round trip through `Hoard` (`add_profile`, `delete_profile`, `secret_for`), `profiles.json` written 0600 under a 0700 directory.
- [x] Implement; `cargo test -p hoard`.

### Task 3: Raven collections and environments

**Files:** `crates/raven/Cargo.toml`, `crates/raven/src/raven.rs`, `crates/raven/src/collection.rs`

- [x] Failing tests: parse a two-request file with `### name` headers, headers and JSON body; a request without a `###` gets `request-N`; serialize round trip; `expand_variables` replaces `{{base}}` and leaves unknown names; `$hoard:` values are reported as pending and never substituted; `env.json` parse with several environments.
- [x] Implement; `cargo test -p raven`.

### Task 4: Raven store

**Files:** `crates/raven/src/store.rs`

- [x] Failing tests: list collections sorted; `resolve` refuses `..`, absolute, separators in a name and symlinks; `write_atomic` leaves no temp file and replaces content; `append_log` creates the directory.
- [x] Implement; `cargo test -p raven`.

### Task 5: cURL import

**Files:** `crates/raven/src/curl.rs`

- [x] Failing tests: `curl -X POST https://a.test/x -H 'A: b' -d '{"k":1}'` → method, URL, header, body; `-u u:p` → `Authorization: Basic dTpw`; `--data` without `-X` implies POST; quoting and line continuations; `$(rm -rf /)` is kept as literal text; non-curl input is an error.
- [x] Implement; `cargo test -p raven`.

### Task 6: Send, log, redaction, clipping

**Files:** `crates/raven/src/send.rs`, `crates/raven/src/log.rs`

- [x] Failing tests: `build_request` applies a bearer profile only on https and sets `Cookie` from a cookies profile; a local listener answers `200` with `Content-Type: application/json` and the response language is `JSON`; a response bigger than the cap is truncated with `truncated: true`; a listener that never answers hits a short timeout; `append_log` rotates to `log.1.jsonl` past the cap and lines never contain `body`; `redact` replaces every name from the set case-insensitively; `clipping_markdown` has the front matter and fenced `http` block and a capped body; `remember_last_response` stores the redacted form only.
- [x] Implement; `cargo test -p raven`.

### Task 7: `raven.redact_headers` setting

**Files:** `crates/settings_content/src/raven.rs`, `crates/settings_content/src/settings_content.rs`, `crates/settings/src/vscode_import.rs`, `assets/settings/default.json`, `crates/raven/src/raven.rs`

- [x] Add `RavenSettingsContent { redact_headers: Option<Vec<String>> }`, register it, default `[]`; `RavenSettings` in the raven crate.

### Task 8: Raven view, actions, keymap, init

**Files:** `crates/raven/src/view.rs`, `crates/raven/src/raven.rs`, `assets/keymaps/default-macos.json`, `crates/zed/src/main.rs`, `crates/zed/src/zed.rs`, `crates/zed/Cargo.toml`

- [x] Actions `raven::{Toggle, Send, SaveRequest, NewRequest, SaveAsClipping, ImportCurl, SelectNext, SelectPrevious}`; `RavenView` as a center `Item`: left list with filter, right editors, response pane with read-only body editor whose language follows `Content-Type`.
- [x] Keymap: `cmd-ctrl-shift-r` → `raven::Toggle` next to the other chords; context `RavenView` binds `cmd-enter` → `raven::Send`.
- [x] `raven::init` and `hoard::init` after `bifrost::init`; `raven`, `hoard` in the expected namespace list.

### Task 9: Hoard modal, borrow and the browser link

**Files:** `crates/hoard/src/modal.rs`, `crates/hoard/src/borrow.rs`, `crates/browser/src/hoard_link.rs`, `crates/browser/src/browser.rs`, `crates/browser/src/chrome_runner.rs`

- [x] Failing tests: `cookies_from_storage_get_cookies` parses the CDP shape and drops expired; `select_hosts` keeps only ticked hosts; `set_cookies_params` builds `Network.setCookies` params.
- [x] `hoard::Toggle` modal: list, add by hand, delete, Borrow from Chrome.
- [x] `browser::hoard_link` registers the `ChromeBridge`; `ChromeRunner::open` injects matching Hoard cookies through `Storage.setCookies` before navigating.

### Task 10: Panda fetch with the Hoard

**Files:** `crates/lightpanda/src/hoard_fetch.rs`, `crates/lightpanda/src/lightpanda.rs`, `crates/lightpanda/src/reader.rs`

- [x] Failing tests: `parse_fetch_help` finds `--header` and a cookie-file flag; capability cache keyed by mtime; `fetch_args_with_cookies` places `--header` before the URL.
- [x] Implement probe, header path and the `serve` + CDP fallback; `open_reader` uses it and records `Browsed` with the profile note.

### Task 11: Bifröst tools

**Files:** `crates/bifrost/src/raven_tools.rs`, `crates/bifrost/src/dispatch.rs`, `crates/bifrost/src/bifrost.rs`, `crates/bifrost/Cargo.toml`

- [x] Failing tests: `raven_list` lists names and methods from a temp root; `raven_last_response` returns the cached redacted response or `not_found`; methods route as `Routed::Raven`.
- [x] Implement.

### Task 12: README

- [x] Features entries for Raven and Hoard, key row, one privacy-model line.

### Verification

`cargo test -p raven -p hoard -p hulder_net -p lightpanda -p browser -p bifrost`, `./script/clippy -p raven -p hoard -p hulder_net -p lightpanda -p browser -p bifrost`, `cargo build -p zed`.
