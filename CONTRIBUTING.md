# Contributing to frenchetv

Thanks for considering a contribution. frenchetv is an IPTV client for French
operators, written in Rust, targeting Linux/Windows/macOS desktop and Android
TV / Fire TV.

By contributing you agree that your work is licensed under the project's MIT
license.

---

## Ground rules

- **Never commit credentials.** No real account identifiers, passwords, tokens,
  session cookies, or license-server responses — including in test fixtures,
  captured traffic, and commit messages. Fixtures must be redacted or synthetic.
- **MSRV is 1.97.** Do not use language or standard-library features newer than
  that. CI's quality gate is pinned to 1.97.1; keep `CLAUDE.md`, the workflow
  pin, and `Cargo.toml`'s `rust-version` in sync when bumping.
- **No `unsafe`** except at FFI boundaries (libmpv, JNI).
- **Business logic belongs in `crates/core`.** The UI crates should stay
  presentation-layer wherever practical.

---

## Building

### Desktop

Requires `libmpv-dev` (Linux) or `mpv` via Homebrew (macOS).

```bash
cargo build -p ui-desktop
cargo run   -p ui-desktop
```

### Core only

```bash
cargo test -p frenchetv-core               # unit tests (wiremock, no network)
cargo test -p frenchetv-core -- --ignored  # integration tests (real network/VPN)
```

### Android TV / Fire TV

One-time setup:

```bash
rustup target add aarch64-linux-android armv7-linux-androideabi
cargo install cargo-ndk
```

You also need three toolchain pieces that are **not** interchangeable with
whatever your distro ships:

- **Android NDK r26d**, exported as `ANDROID_NDK_HOME`. This is what CI uses;
  other versions may work but are untested. Needed to compile the Rust `.so`.
- **JDK 17 or 21.** Gradle 8.6 / AGP 8.2.2 do not run on newer JDKs — a JDK 25+
  system default will fail before it compiles anything. If your distro only
  ships a newer JDK, unpack a Temurin 17 tarball somewhere local and point
  `JAVA_HOME` at it rather than changing your system default.
- **Android SDK** with `platforms;android-34` and `build-tools;34.0.0`,
  exported as `ANDROID_HOME`. Needed to package the APK — the NDK alone is not
  enough, and Gradle fails with "SDK location not found" without it. Install it
  without Android Studio via the command-line tools:

  ```bash
  # unpack commandlinetools-linux-*.zip to $ANDROID_HOME/cmdline-tools/latest
  sdkmanager --licenses
  sdkmanager "platforms;android-34" "build-tools;34.0.0" "platform-tools"
  ```

Then:

```bash
export ANDROID_NDK_HOME=/path/to/android-ndk-r26d
export ANDROID_HOME=/path/to/android-sdk
export JAVA_HOME=/path/to/jdk-17

# Rust .so for both shipped ABIs
cargo ndk -t arm64-v8a -t armeabi-v7a \
  -o crates/ui-android/android/app/src/main/jniLibs \
  build -p ui-android --release

cd crates/ui-android/android
./gradlew assembleDebug     # debug-signed, installable via `adb install`
./gradlew assembleRelease   # unsigned unless signing env vars are set
```

`assembleRelease` produces an **unsigned** APK unless the release signing
environment variables are present (see `.github/workflows/release.yml`).
Android refuses to install an unsigned APK — if sideloading fails with
"App not installed" or a parse error, that is usually why. Use `assembleDebug`
for local testing.

#### Testing on a real Fire TV

Enable *Settings → My Fire TV → Developer Options → ADB debugging*, then:

```bash
adb connect <device-ip>:5555     # accept the prompt on the TV
adb install -r app-debug.apk
adb shell monkey -p com.frenchetv -c android.intent.category.LEANBACK_LAUNCHER 1
adb logcat -d | grep -iE "frenchetv|AndroidRuntime"
adb shell screencap -p /sdcard/s.png && adb pull /sdcard/s.png
```

A screencap is worth taking. Several Android-only bugs have been invisible
from the host side and obvious in one frame — a crash that returns silently to
the launcher, or a layout that renders off the bottom of the screen.

Note that the Fire TV Stick reports `armeabi-v7a` (32-bit) and a 2.0 display
density: a 1920x1080 panel gives egui only ~960x540 **logical** points. Layouts
that fit on a desktop window can overflow the TV. Put tall screens in a
`ScrollArea` and `scroll_to_me()` the focused widget.

---

## The quality gate

CI runs exactly this, and it must pass before a PR can merge:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
```

Run it locally before pushing. `cargo fmt --all` (no `--check`) fixes
formatting in place.

**Android code needs a second, separate clippy run.** Everything in
`crates/ui-android` is `#[cfg(target_os = "android")]`-gated, so a host-target
`cargo clippy --workspace` walks straight past all of it and reports success no
matter what the crate contains. Lints only reach it through an Android target:

```bash
cargo ndk -t arm64-v8a -t armeabi-v7a clippy -p ui-android --all-targets -- -D warnings
```

CI does this in the `build + clippy (android …)` job. If you touch
`crates/ui-android`, run it yourself first.

---

## Adding an operator

Every operator implements `Operator` in `crates/core/src/operator/traits.rs`.
Checklist:

1. **Implement the trait** in `crates/core/src/operator/<name>.rs`.

   Required: `name`, `requires_auth`, `authenticate`, `fetch_channels`,
   `resolve_stream`, `fetch_epg`.

   Optional, with defaults — override only what the operator actually needs:
   - `uses_phased_auth` + `begin_auth` / `complete_auth_password` /
     `wait_for_push_auth` / `submit_otp`, for out-of-band factors. The driver
     loops on the returned `AuthPhase` until it sees `AuthPhase::Done`.
   - `set_extra_credential`, if setup needs a third field beyond
     username/password (Bouygues needs the account holder's last name).
   - `session_token` + `restore_session`, to persist a session across restarts.

2. **Register it** in `crates/core/src/operator/mod.rs`:
   - add an `OperatorKind` variant,
   - handle it in `display_name`, `config_str`, `from_config_str`, and
     `extra_credential_label`,
   - add it to `OperatorRegistry::all()` and `OperatorRegistry::build()`.

   `config_str` is a **stable** identifier — it keys both the config file and
   the OS keyring. Changing it later orphans existing users' saved sessions.

3. **Ship a fallback M3U** at `assets/channels/<name>.m3u`, pulled in with
   `include_str!`. This is mandatory, not optional: when the operator's API
   fails, the app falls back to the static list instead of showing an error.
   See `bouygues.rs` for the pattern — log a `tracing::warn!` on each fallback
   path so failures stay diagnosable.

4. **Write tests with `wiremock`.** No live network in the default test run;
   real-network tests go behind `#[ignore]`. Cover at least the happy path and
   the API-failure-falls-back-to-M3U path.

5. **Document the API** in `docs/operators.md` — endpoints, auth flow, token
   lifetimes, and anything reverse-engineered. This file is the reason the next
   person can fix the operator when it breaks, which it will. Credit prior art
   (e.g. a Kodi addon) and note its license.

6. **Wire up the UI** if the operator needs anything unusual at setup time; the
   generic username/password path needs no UI change.

### Credentials handling

Passwords go to the OS keyring (`keyring` crate) on desktop and the Android
Keystore on Android. **Never** write a password to `config.toml`.

---

## Pull requests

Keep PRs focused — one logical change. Conventional-commit subjects
(`fix(ui-android): …`, `feat(core): …`, `docs: …`) are used throughout the
history.

Copy this into the PR description:

```markdown
## What
<!-- One or two sentences. What changes and why. -->

## Operator impact
<!-- Which operators does this touch? "None" is a fine answer. -->

## Testing
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace --locked`
- [ ] `cargo ndk … clippy -p ui-android …` (only if `crates/ui-android` changed)
- [ ] Verified on a real device (desktop / Android TV / Fire TV — say which)

## Notes
<!-- Anything reviewers should know: follow-ups, known gaps, docs updated. -->
```

If a change affects an operator's API surface, update `docs/operators.md` in
the same PR.

### Verifying UI changes

A change to a screen is not done when it compiles. Android UI in particular
has shipped broken twice in ways the build could not catch — an activity theme
that crashed on launch, and a form that rendered below the bottom of the
screen. If you change a screen, run it and look at it, and say in the PR which
device you looked at it on.
