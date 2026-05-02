# ALVR Client Compatibility PRD

## Purpose
Define the remaining feature set needed for the Pimax Crystal OG client to reach practical parity with upstream ALVR client behavior.

## Goal
Deliver a stable standalone VR client that matches the ALVR server’s expected protocol, rendering, tuning, and device-integration behavior without requiring headset-specific workarounds.

## In Scope
- ALVR handshake and discovery
- Video stream decode and submission
- ViewsConfig / IPD / FOV negotiation
- Runtime tuning and persistence
- Pimax XR runtime integration
- Controller and head-tracking integration
- Audio and device lifecycle behavior
- Debugging, stats, and recovery tooling

## Current Baseline
Already implemented or validated:
- Discovery and control connection
- H.264/H.265 decode path
- UDP shard reassembly
- Zero-copy upload path
- Stereo eye submission
- Convergence correction
- Color correction
- Runtime tuning UI
- IPD sync
- FOV scaling baseline
- HDR negotiation and HDR render path

## Required Features

### 1. Protocol Compatibility
- Match ALVR protocol version and handshake expectations.
- Support discovery, control, keepalive, IDR requests, and stream reconfiguration.
- Preserve compatibility with current ALVR packet schemas.
- Handle reconnects cleanly after server restart or headset runtime reset.

### 2. Video Compatibility
- Support the codecs the server may negotiate for this device class.
- Keep decoder config and frame reconfiguration correct across stream restarts.
- Maintain zero-copy behavior where possible.
- Preserve correct buffer lifetime for decoded frames.
- Support HDR/10-bit paths when the server enables them.

### 3. Views and Projection
- Send correct `ViewsConfig` values for IPD, resolution, and FOV.
- Support live tuning for IPD and FOV scaling.
- Support eye render size tuning when runtime-reported sizes need adjustment.
- Prefer asymmetric frustum support only if the Pimax runtime exposes a client-accessible API.
- Keep default projection behavior stable when no extra runtime fields exist.

### 4. Tracking and Input
- Send head tracking at the expected update rate.
- Synchronize controller poses with ALVR’s input basis.
- Map controller buttons and interactions to ALVR profiles.
- Preserve IPD updates from headset hardware.
- Keep pose timestamps and prediction aligned with server expectations.

### 5. Runtime and Lifecycle
- Enter and exit Pimax XR mode reliably.
- Recover correctly from screen state changes, headset sleep, and focus loss.
- Reinitialize reference spaces when needed.
- Keep guardian/boundary behavior from breaking the streaming session.
- Avoid accidental shutdown behavior during development sessions.

### 6. Audio
- Support microphone and audio routing expected by ALVR.
- Handle device audio state transitions cleanly.
- Preserve session continuity when audio devices change.

### 7. Tuning and Persistence
- Persist runtime settings across launches.
- Keep the browser UI for live tuning.
- Expose the key rendering controls:
  - convergence shift
  - IPD scale
  - FOV scale
  - eye render scale
  - black crush
  - color gain
  - controller calibration
- Ensure tuning changes trigger live recomputation where applicable.

### 8. Diagnostics and Recovery
- Surface connection, decoder, and runtime errors clearly.
- Emit useful logs for stream negotiation and view updates.
- Provide controlled-launch validation for regression testing.
- Keep stats/reporting visible enough to diagnose packet loss, latency, and decoder issues.

## Explicit Non-Goals
- Reimplementing unsupported upstream features without evidence the Pimax runtime exposes them.
- Adding compatibility shims for APIs that are not used by the server or headset.
- Changing the proven FOV baseline without a device-backed reason.

## Acceptance Criteria
- The client can connect, stream, and recover without manual intervention in the common case.
- Video remains stable through reconnects and runtime restarts.
- View configuration matches the headset’s runtime-reported parameters within tuning tolerances.
- Controller and head-tracking behavior is functionally usable in SteamVR/OpenVR apps.
- Logs show clear negotiation, decode, and submission state.

## Open Questions
- Does the Pimax runtime expose any additional client-accessible frustum data beyond `targetFovXRad` / `targetFovYRad`?
- Are there any upstream ALVR features that the Pimax runtime cannot support by design?
- What audio path is required for full headset parity on this device?

## Suggested Priorities
1. Audio parity.
2. Full controller/input basis cleanup.
3. Runtime lifecycle hardening.
4. Better diagnostics and recovery.
5. Any remaining protocol-edge compatibility gaps.
