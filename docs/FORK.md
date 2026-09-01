# Running this fork

This is jsladerman's personal fork of [AprilNEA/OpenLogi], self-built and
self-signed as the daily driver replacing Logi Options+. This file is what's
*different* from upstream plus everything a fresh machine needs; general
build docs are [DEVELOPMENT.md](DEVELOPMENT.md).

[AprilNEA/OpenLogi]: https://github.com/AprilNEA/OpenLogi

## What the fork carries

Deliberately tiny so rebases stay clean:

- **`fix(hook)`: macOS key allowlist** — `translate_key` in
  `crates/openlogi-hook/src/macos.rs` drops every key outside Esc/F1–F19
  inside the hook crate, mirroring Windows, so ordinary typing never reaches
  the agent. This was the security finding that motivated the fork.
- **`feat(core)`: `Action::SmartZoom`** — synthesises the macOS trackpad
  two-finger double-tap (Options+ parity for the MX Master top button).
  Bumped `PROTOCOL_VERSION` to 30.
- **`build(gui)`: updater points here** — the default update manifest is this
  fork's GitHub releases, never `updates.openlogi.org`. The check errors
  harmlessly until a release with a `latest.json` asset exists
  (`cargo xtask release latest-json`); in-app installs would additionally
  need a minisign keypair (not set up — updates happen by rebuilding).
- **`fix(hook)`: sleep-safe tap watchdogs** — the CGEventTap watchdogs
  discount uptime that passed while the watchdog thread itself was off-CPU
  (sleep entry/exit, closed-lid dark wakes). Before this, one dark wake in a
  hundred exited the agent with "HID tap thread stopped making progress"
  (`elapsed_ms=2662`) and it stayed down until the next login.
- **`fix(agent)`: supervised self-relaunch** — when launchd runs the agent
  (the normal case), a binary update on disk exits non-zero so launchd
  respawns it, instead of `open`-launching a successor that escapes
  supervision. Without this, every reinstall silently produced an agent
  nothing would restart after a crash.

## Fresh-machine build (macOS)

1. Full Xcode (not just CLT) **plus the Metal toolchain**:
   `xcodebuild -downloadComponent MetalToolchain` — without it the GPUI
   shader compile fails mid-build.
2. `rustup` (the repo pins stable), `brew install create-dmg`.
3. An **Apple Development certificate** (free Apple account, via Xcode →
   Settings → Accounts → Manage Certificates). Ad-hoc signing voids the
   permission grants on every rebuild; a real cert keeps them stable.
4. Build the production bundle:

   ```sh
   export OPENLOGI_SIGN_IDENTITY="Apple Development: <you> (TEAMID)"  # security find-identity -v -p codesigning
   OPENLOGI_BUNDLE_ASSETS=1 cargo run -p xtask -- macos package
   ditto target/release/bundle/osx/OpenLogi.app /Applications/OpenLogi.app
   ```

5. Launch, then grant **both** permissions to **OpenLogi Agent** — the
   nested helper, not the app: System Settings → Privacy & Security →
   Input Monitoring *and* Accessibility. The + picker can't browse into
   bundles; use ⌘⇧G with
   `/Applications/OpenLogi.app/Contents/Library/LoginItems/OpenLogi Agent.app`.
   The agent only re-reads its grant on restart — if devices stay missing
   after granting, quit it from the menu bar (or `pkill -x openlogi-agent`)
   and relaunch the app.
6. Enable OpenLogi under System Settings → General → Login Items so the
   agent starts at login.

### Gotchas that cost real time

- **If the official OpenLogi (or another differently-signed build) ever ran
  on the machine**, its permission rows and login-item registration are
  bound to that signing identity and silently break this one (TN3127). Fix:
  `tccutil reset ListenEvent org.openlogi.agent`, same for `Accessibility`
  and `PostEvent`, then re-grant.
- **The agent must be launchd's job, or nothing restarts it.** Healthy state:
  `launchctl print gui/$(id -u)/org.openlogi.agent.service` shows
  `state = running` and the agent's environment carries
  `XPC_SERVICE_NAME=org.openlogi.agent.service`. If the GUI shows
  "the agent is not running" and `launchctl kickstart` says
  *Could not find service*, the `SMAppService` registration exists in
  Background Task Management but never reached launchd (seen after the
  stale-identity mess above). Force the re-register dance:
  `rm ~/.local/share/openlogi/registration-version` and relaunch the app —
  it unregisters, registers, and kickstarts the service. An agent started by
  hand with `open -g -n …` works but is unsupervised; quit it and kickstart
  the service instead.
- **Logi Options+ can't coexist** — it holds the HID++ devices exclusively.
  Stop `com.logi.cp-dev-mgr` (launchctl) or uninstall it first. Its
  uninstaller/AppCleaner leave behind `/Library/Application Support/Logi`,
  `Logitech.localized`, `LogiPluginService.app` in /Applications/Utilities,
  and `com.logitech.LogiRightSight.Agent.plist`.

## Config

Lives at `~/.config/openlogi/config.toml` (XDG, even on macOS). Device
entries are keyed by hardware serial (`serial:…`), so **copying the file to
a new machine carries every binding, DPI, and SmartShift setting with it** —
the agent re-applies them on connect (they live in device RAM and reset on
power-cycle otherwise). The one-time Logi Options+ → OpenLogi translation
notes and raw export live in `~/.config/openlogi/optionsplus-export/` on the
original machine (not in this repo).

Edit the file only while the GUI is closed (it refuses to save over
external edits, but won't see them until restart/reload either).

## Taking upstream updates

The point of the fork: updates are pulled deliberately, reviewed, rebuilt.

```sh
git fetch upstream master
git log --stat master..upstream/master   # review what changed (or hand to an agent)
git rebase upstream/master               # the three carried commits are small and rebase cleanly
# full local gate (AGENTS.md) — a rebase always takes the full tier
cargo run -p xtask -- macos package && ditto target/release/bundle/osx/OpenLogi.app /Applications/OpenLogi.app
```

Same signing cert ⇒ permission grants survive the reinstall. The running
agent notices the new binary on disk and, being launchd's job, exits so
launchd respawns it; confirm with the `launchctl print` check above (an
agent that was *not* supervised before the update relaunches itself via
`open` and stays unsupervised — quit it and kickstart the service).
