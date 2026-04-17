# Project Context

This is a Rust/Android project for the Pimax Crystal OG headset that implements an ALVR client.

## Test Launch Script

Use the controlled launch test script for automated testing:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\pimax-controlled-launch-test.ps1 -Serial bf18c368 -RebootBeforeRun -NetworkWaitTimeoutSeconds 60
```

Parameters:
- `-Serial` - Headset serial number (required for multi-device setups)
- `-RebootBeforeRun` - Reboot before testing
- `-RecoverAfterRun` - Recover headset after test (reboot + reassert Guardian)
- `-SnapshotSeconds` - Capture snapshots at these intervals (default: 5, 20, 45)
- `-NetworkWaitTimeoutSeconds` - Timeout for network readiness (default: 90)

Note: The app stays running after the test (no shutdown broadcast is sent).

## Build & Deploy

```powershell
# Build APK and Install
powershell -ExecutionPolicy Bypass -File scripts\build-android-client.ps1 -Install

# Launch
adb shell am start -n com.pimax.alvr.client/.VrRenderActivity

# View logs
adb logcat -d -s PimaxALVR
```

## Key Files

- `src/android.rs` - Android entry point
- `src/client.rs` - ALVR protocol implementation
- `src/video_receiver.rs` - Video decoding and blit pipeline
- `src/pimax.rs` - Pimax XR runtime integration
- `src/tune.rs` - Runtime tuning parameters (HTTP server on port 7878)
- `src/config.rs` - Configuration persistence

## Tuning Parameters (port 7878)

Access at `http://<headset-ip>:7878/`:
- `convergence_shift_ndc` (0.0-0.5) - Pre-shift to cancel Pimax compositor divergent warp (~0.124 default)
- `ipd_scale` (0.0-2.0) - ALVR stereo strength (1.0 = full physical IPD)
- `color_black_crush` (0.0-0.3) - BT.709 black level (0.072 default)
- `color_gain` (0.5-2.0) - BT.709 contrast gain (1.22 default)

## Config Location

```
/sdcard/Android/data/com.pimax.alvr.client/files/PimaxALVR/client.json
```

## Ports

| Port | Purpose |
|------|---------|
| 9943 | ALVR discovery/control |
| 9944 | ALVR video stream |
| 7878 | HTTP settings UI |
| 9950 | Debug RGBA stream |

## Important Notes

- Screen-off no longer triggers shutdown (disabled for development)
- Tuning parameters are persisted to config on change
- Server IP can be configured via browser UI and auto-reconnects on restart

<!-- code-review-graph MCP tools -->
## MCP Tools: code-review-graph

**IMPORTANT: This project has a knowledge graph. ALWAYS use the
code-review-graph MCP tools BEFORE using Grep/Glob/Read to explore
the codebase.** The graph is faster, cheaper (fewer tokens), and gives
you structural context (callers, dependents, test coverage) that file
scanning cannot.

### When to use graph tools FIRST

- **Exploring code**: `semantic_search_nodes` or `query_graph` instead of Grep
- **Understanding impact**: `get_impact_radius` instead of manually tracing imports
- **Code review**: `detect_changes` + `get_review_context` instead of reading entire files
- **Finding relationships**: `query_graph` with callers_of/callees_of/imports_of/tests_for
- **Architecture questions**: `get_architecture_overview` + `list_communities`

Fall back to Grep/Glob/Read **only** when the graph doesn't cover what you need.

### Key Tools

| Tool | Use when |
|------|----------|
| `detect_changes` | Reviewing code changes — gives risk-scored analysis |
| `get_review_context` | Need source snippets for review — token-efficient |
| `get_impact_radius` | Understanding blast radius of a change |
| `get_affected_flows` | Finding which execution paths are impacted |
| `query_graph` | Tracing callers, callees, imports, tests, dependencies |
| `semantic_search_nodes` | Finding functions/classes by name or keyword |
| `get_architecture_overview` | Understanding high-level codebase structure |
| `refactor_tool` | Planning renames, finding dead code |

### Workflow

1. The graph auto-updates on file changes (via hooks).
2. Use `detect_changes` for code review.
3. Use `get_affected_flows` to understand impact.
4. Use `query_graph` pattern="tests_for" to check coverage.
