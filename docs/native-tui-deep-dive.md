# Native TUI Deep Dive

## Containment: Screen Corruption

### Severity 0: native TUI log output could corrupt the alternate screen
- Scope: native `--tui`, including `--tui --api` and `--tui --web`.
- Native-only: the web bridge child already sets `OCTO_TUI_LOG_ADDR` in [src/tui/terminal.rs](/home/mjc/projects/octo-dl/src/tui/terminal.rs:141), so its logs are forwarded off the PTY before xterm playback.
- Root cause: [src/bin/octo.rs](/home/mjc/projects/octo-dl/src/bin/octo.rs:55) initialized `env_logger` before mode parsing. When `OCTO_TUI_LOG_*` was absent, `log::info!/warn!/error!` from the interactive runtime wrote directly to the controlling terminal while `ratatui` owned the alternate screen.
- Deterministic reproduction before the fix:
  - Run `cargo run -- --tui`.
  - Trigger any path that logs during interactive ownership: session resume, login, file start/complete, scope error, periodic progress summary.
  - Observe raw log lines punch through the TUI because they bypass [src/tui/terminal.rs](/home/mjc/projects/octo-dl/src/tui/terminal.rs:314) and write outside the draw loop.
- Containment fix implemented here:
  - Parse CLI args before logger setup.
  - If `--tui` is active and no forwarded log sink is present, route logs to `native-tui.log` beside the session state directory instead of the terminal.
  - Preserve the existing socket/FD forwarding path for the web bridge child.
- Regression coverage:
  - `native_tui_log_detachment_only_applies_to_unforwarded_tui` in [src/bin/octo.rs](/home/mjc/projects/octo-dl/src/bin/octo.rs:274)

### Reproduction matrix
- Native `--tui`: previously unsafe, fixed by detached logging.
- Native `--tui --api`: same unsafe path, fixed by detached logging.
- Native `--tui --web`: same native containment fix; child PTY logging was already forwarded safely.
- Primary `--web` or headless `--api`: no alternate-screen ownership, so terminal corruption is out of scope.

### Other terminal writes
- `eprintln!` in [src/bin/octo.rs](/home/mjc/projects/octo-dl/src/bin/octo.rs:30) is limited to help/argument errors before TUI ownership.
- Alternate-screen cleanup is guarded by RAII in [src/tui/terminal.rs](/home/mjc/projects/octo-dl/src/tui/terminal.rs:80). That covers unwind paths, not hard aborts.

## Add Mode: Alignment, Focus, Cursor

### Severity 1: add mode had no visible cursor
- Root cause: [src/tui/draw.rs](/home/mjc/projects/octo-dl/src/tui/draw.rs:85) rendered the URL box but never called `frame.set_cursor_position`, so the backend hid the cursor every frame.
- Reproduction before the fix:
  - Press `a`.
  - Type any character.
  - The mode changes, but the input field has no caret or focus location.
- Fix implemented here:
  - Render a focused input viewport and place the cursor explicitly inside the URL field.

### Severity 1: long or pasted URLs lost the insertion point
- Root cause: the URL field always rendered from character 0, with no horizontal tracking.
- Reproduction before the fix:
  - Open add mode in a narrow terminal.
  - Paste a long URL.
  - The newest characters fall off-screen and there is still no cursor to show where input is happening.
- Fix implemented here:
  - Keep the trailing slice of the input visible and reserve the last cell for the cursor.
- Regression coverage:
  - `scenario_add_mode_keeps_cursor_visible_during_live_updates` in [src/tui/tests.rs](/home/mjc/projects/octo-dl/src/tui/tests.rs:686)

### Remaining add-mode issue
- `a`, then `Enter` on empty input is a silent no-op.
- Likely subsystem: [src/tui/input.rs](/home/mjc/projects/octo-dl/src/tui/input.rs:287)
- Confidence: high
- Fix direction: either exit add mode on empty submit or set a status message so the user is not left in a dead-feeling state.
- Missing regression: input-level unit test plus harness scenario.

## Ranked Bug Map

### 1. Screen Corruption
- Fixed: native logger output escaping the draw loop.
- Likely subsystem: [src/bin/octo.rs](/home/mjc/projects/octo-dl/src/bin/octo.rs:55)
- Confidence: high
- Root-cause pattern: terminal ownership started after the logger was already pointed at the terminal.
- Missing regression: PTY-level panic/abort coverage while alternate-screen ownership is active.

### 2. Mode Drift
- Remaining: empty add-mode submit gives no feedback.
- Remaining: narrow controls text is truncated from the end, so the most important escape semantics can disappear on small terminals.
- Likely subsystem: [src/tui/input.rs](/home/mjc/projects/octo-dl/src/tui/input.rs:287) and [src/tui/draw.rs](/home/mjc/projects/octo-dl/src/tui/draw.rs:230)
- Confidence: high
- Fix direction: add width-banded legends and explicit empty-submit behavior.
- Missing regression: narrow-width render snapshots and input-mode submit scenarios.

### 3. Selection and Targeting
- Severity 1 fixed: when a selected child row disappeared because an auto-expanded package collapsed, selection could jump to the next visible package instead of staying with the original package.
- Deterministic reproduction before the fix:
  - Create two packages.
  - Start one file in package A so A auto-expands.
  - Select A’s child row.
  - Let that file complete so package A collapses.
  - The selected index can land on package B, so the next delete/retry/reset targets the wrong row.
- Fix implemented here:
  - If the exact child row disappears, fall back to the parent package row before using numeric-index fallback.
- Regression coverage:
  - `scenario_selection_falls_back_to_parent_package_after_auto_collapse` in [src/tui/tests.rs](/home/mjc/projects/octo-dl/src/tui/tests.rs:706)
- Likely subsystem: [src/tui/visible.rs](/home/mjc/projects/octo-dl/src/tui/visible.rs:391)
- Confidence: high

### 4. Layout and Rendering
- Fixed: long add-mode input no longer hides the active insertion point.
- Remaining: status and controls lines still use coarse end-truncation, which drops priority information instead of reflowing by width band.
- Likely subsystem: [src/tui/draw.rs](/home/mjc/projects/octo-dl/src/tui/draw.rs:230)
- Confidence: high
- Fix direction: define explicit compact legends/status variants for narrow widths instead of truncating a single long sentence.
- Missing regression: width-banded draw tests around 20, 30, and 40 columns.

### 5. Session and Late-Event Reconciliation
- Covered by the selection fix above: completion-driven row collapse no longer retargets the user to a different package.
- Fixed: delete now re-suppresses late `FileError` the same way it already re-suppressed late `FileComplete`, so removed rows do not revive as error rows.
- Fixed: retry/reset now assign a per-file attempt ID to `ResumeFileIds`, and the runtime ignores any `FileStart`, `Progress`, `ResumeReused`, `FileComplete`, `FileCancelled`, or `FileError` whose attempt ID does not match the current file attempt.
- Likely subsystem: [src/tui/app/actions.rs](/home/mjc/projects/octo-dl/src/tui/app/actions.rs:95)
- Confidence: high

## Regression Test Plan by Layer
- `app`: state projection and late-event suppression rules.
- `input`: empty-submit semantics and popup/mode transitions.
- `draw`: width-banded legends, URL viewport behavior, and cursor placement.
- `tui::tests`: multi-step scenarios with keys, paste, download events, tick boundaries, rendered text, selected row, popup, and status.
