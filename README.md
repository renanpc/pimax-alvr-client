# Pimax ALVR Client

A native Rust client for the Pimax Crystal OG standalone headset, implementing the ALVR streaming protocol to receive VR content from a PC running ALVR Server.

## Features

### ALVR Protocol Support

| Feature | Status | Description |
|---------|--------|-------------|
| **Discovery** | ✅ | UDP broadcast on port 9943 to find ALVR servers |
| **Discovery Response** | ✅ | Receive and parse server hostname/IP |
| **TCP Control** | ✅ | Port 9943 - handshake, keepalive, configuration |
| **UDP Video Stream** | ✅ | Port 9944 - packet sharding and reassembly |
| **H.264 Codec** | ✅ | Hardware decoding via Android MediaCodec |
| **H.265/HEVC Codec** | ✅ | Hardware decoding via Android MediaCodec |
| **AV1 Codec** | ❌ | Codec enum defined but `encoder_av1: false` advertised; server never sends AV1 |
| **Head Tracking** | ✅ | Send pose updates to server at 90Hz+ |
| **Stereo Rendering** | ✅ | Left/right eye texture submission |
| **Foveated Encoding** | ✅ | Receive and un-distort foveated video streams |
| **ViewsConfig** | ✅ | Send IPD, FOV, resolution to server |
| **Statistics** | ✅ | Report frame timing, dropped packets |
| **KeepAlive** | ✅ | Periodic control packet exchange |
| **IDR Requests** | ✅ | Request keyframes on decoder reconfiguration |
| **Game Audio** | ✅ | Stream ALVR audio to headset output |
| **Microphone Capture** | ✅ | Capture headset mic and forward to PC |

### Video Pipeline

| Feature | Status | Description |
|---------|--------|-------------|
| **Zero-Copy Upload** | ✅ | EGLImageKHR from AHardwareBuffer |
| **Direct Zero-Copy Blit** | ✅ | OES→eye blit with BT.709 correction on the active zero-copy path |
| **Intermediate Fallback Path** | ✅ | OES/texture upload can still route through an RGBA intermediate path when needed |
| **Convergence Shift** | ✅ | Corrects Pimax compositor divergent warp |
| **Color Correction** | ✅ | BT.709 black crush and gain adjustment |
| **Foveation Shader** | ✅ | Un-distort foveated encoding |

### Tunable Parameters (via browser at http://headset-ip:7878/)

| Parameter | Range | Description |
|-----------|-------|-------------|
| `convergence_shift_ndc` | 0.0 - 0.5 | Pre-shift to cancel Pimax warp (~0.124 default) |
| `ipd_scale` | 0.0 - 2.0 | ALVR stereo strength (1.0 = full physical IPD) |
| `fov_scale` | 0.8 - 1.2 | Fine-tune headset FOV for Pimax warp alignment |
| `eye_render_scale` | 0.5 - 1.5 | Scale eye render targets for sharper/stabler output |
| `color_black_crush` | 0.0 - 0.3 | BT.709 black level (0.072 default) |
| `color_gain` | 0.5 - 2.0 | BT.709 contrast gain (1.22 default) |

### Platform Integration

| Feature | Status | Description |
|---------|--------|-------------|
| **Pimax XR Runtime** | ✅ | Enter VR mode via PxrApi |
| **Head Tracking** | ✅ | Receive poses from PxrServiceApi |
| **Proximity Sensor** | ✅ | Drives wake/sleep recovery and off-head display-sleep policy |
| **Screen State** | ✅ | Feeds lifecycle-aware presentation recovery and wake handling |
| **IPD Sync** | ✅ | Receive IPD from Pimax hardware |
| **EGL Context** | ✅ | Headset-backed context for rendering |
| **Texture Submission** | ✅ | Submit layers to Pimax compositor |

### Configuration & Persistence

| Feature | Status | Description |
|---------|--------|-------------|
| **Config Storage** | ✅ | JSON in app-specific storage |
| **Server IP Persistence** | ✅ | Auto-reconnect on restart |
| **Settings Persistence** | ✅ | Tuning values saved/restored |
| **HTTP Settings UI** | ✅ | Browser-accessible at port 7878 |

## Architecture

```
ALVR Server (PC)
     │
     │ H.264/H.265 over UDP
     ▼
TCP Control (9943) ◄─── Server connects to client
UDP Video (9944) ───── Sharded video packets
     │
     ▼
Android MediaCodec (Hardware Decoder)
     │
     │ AHardwareBuffer
     ▼
GL_TEXTURE_EXTERNAL_OES ──► EGLImageKHR
     │
     ├── Primary path: direct eye blit
     │   - BT.709 black crush / gain correction
     │   - convergence shift
     │   - foveation handling
     │
     └── Fallback path: RGBA intermediate blit when required
     │
     ▼
Pimax Compositor (sxrSubmitFrame)
- Lens distortion
- Chromatic aberration
- Divergent warp (~0.124 NDC)
     │
     ▼
Display (Pimax Crystal lenses)
```

## Known Limitations

- **Guardian Boot Flow**: On first headset boot, Pimax Guardian takes focus. Complete the boundary setup once, then restart the app.
- **Diagnostic Pattern**: When not connected, shows simple test pattern without convergence shift (convergence correction requires ALVR video path)
- **Audio Routing**: Game audio and microphone capture are implemented, but PC-side virtual cable setup is still required
- **Decode Load Ceiling**: Crystal OG is currently most stable around `2400x2400 @ 72 Hz`; heavier profiles can trigger decoder backpressure and extra IDR recovery

## Audio Setup

For repeatable ALVR audio behavior on Crystal OG, use the following setup:

1. In ALVR Dashboard, enable both `Game Audio` and `Microphone`.
2. On the PC, choose the headset speaker/output you want ALVR to target from the ALVR audio settings.
3. If you want the headset microphone to appear in Windows apps or voice chat reliably, route the ALVR microphone output to a virtual input device such as VB-Cable, then select that input inside the target PC application.
4. Grant `RECORD_AUDIO` permission on the headset when the app asks for it. Game audio does not depend on microphone permission, but microphone forwarding does.
5. After changing PC-side audio routing, restart the ALVR session once so the new routing is negotiated cleanly.

Expected behavior after setup:
- headset game audio should start automatically when the ALVR stream starts
- microphone packets should continue flowing after reconnects
- permission can be granted after connection and the microphone capture thread will start once it becomes available

## Build

### Local debug build

```powershell
# Set up Android SDK / NDK
$env:ANDROID_NDK_ROOT='C:\Android\android-sdk\ndk\27.3.13750724'
$env:ANDROID_HOME='C:\Android\android-sdk'

# Build only
powershell -ExecutionPolicy Bypass -File scripts\build-android-client.ps1

# Build + install
powershell -ExecutionPolicy Bypass -File scripts\build-android-client.ps1 -Install

# Build + install + launch
powershell -ExecutionPolicy Bypass -File scripts\build-android-client.ps1 -Install -Launch
```

Default debug APK output:
- `target\debug\apk\pimax-alvr-client.apk`

### Local release build

```powershell
powershell -ExecutionPolicy Bypass -File scripts\build-android-client.ps1 -Profile release
```

Default release APK output:
- `target\release\apk\pimax-alvr-client.apk`

Note:
- Android release packaging depends on the native runtime libraries in [android/runtime-libs](D:/Code/pimax-alvr-client/android/runtime-libs)
- `cargo apk build --release` also requires release signing metadata; CI injects a temporary signing section during the release workflow

### Release workflow

GitHub Actions now includes a tag-driven APK release workflow:
- workflow file: [.github/workflows/release.yaml](D:/Code/pimax-alvr-client/.github/workflows/release.yaml)
- trigger: push a tag matching `v*`
- outputs:
  - release APK artifact
  - GitHub Release asset
  - `SHA256SUMS.txt`

## Test Workflow

### Controlled launch

For repeatable headset validation, use:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\pimax-controlled-launch-test.ps1 -Serial bf18c368 -RebootBeforeRun -NetworkWaitTimeoutSeconds 60
```

Useful flags:
- `-RecoverAfterRun`: reboot and restore normal headset runtime state after the test
- `-SnapshotSeconds 5 20 45`: change snapshot timing
- `-SkipSteamVRRestart`: skip the PC-side SteamVR restart step
- `-LeaveRunningWhenDisplayOff`: keep the app running if the panel turns off during the run

Outputs:
- artifact directory under `.tmp\pimax_controlled_launch_<timestamp>`
- `logcat-app.txt`
- `diagnostic-summary.txt`
- display/power/activity snapshots for each capture point

### Simple manual launch

```powershell
adb shell am start -n com.pimax.alvr.client/.VrRenderActivity
```

## Configuration

### Config File Location
```
/sdcard/Android/data/com.pimax.alvr.client/files/PimaxALVR/client.json
```

### Config Format
```json
{
  "client_name": "pimax-crystal-og",
  "version_string": "21.0.0-dev13",
  "generated_for_version": "21.0.0-dev13",
  "discovery_port": 9943,
  "stream_port": 9944,
  "last_server_ip": "192.168.1.100",
  "convergence_shift_ndc": 0.124,
  "ipd_scale": 1.0,
  "color_black_crush": 0.072,
  "color_gain": 1.22,
  "fov_scale": 0.95,
  "eye_render_scale": 1.0
}
```

### Settings UI

Open `http://<headset-ip>:7878/` in a browser on the same network to access:
- Server IP configuration
- Server discovery scan
- Video tuning sliders
- Stereo/FOV tuning sliders
- Connection status

## Ports

| Port | Protocol | Purpose |
|------|----------|---------|
| 9943 | UDP/TCP | ALVR discovery and control |
| 9944 | TCP/UDP | ALVR video streaming |
| 7878 | TCP | HTTP settings UI |
| 9950 | TCP | Debug RGBA stream (testing) |

## Debugging

### View Logs
```bash
adb logcat -d -s PimaxALVR PimaxALVRActivity
```

### Controlled-Launch Diagnostics

Each controlled launch artifact now includes `diagnostic-summary.txt`, which gives a quick
classification of the session without hand-grepping the whole logcat capture.

Important fields:
- `negotiated_stream`: the final ALVR stream config accepted by the client
- `idr_requests`: how often the client had to ask for a fresh keyframe
- `decoder_backpressure`: MediaCodec input-buffer pressure on the headset
- `waiting_for_idr_drops`: video packets discarded while the client waited for a clean keyframe
- `packet_gap`: UDP continuity gaps detected after packet reassembly
- `compositor_failures`: zero-copy render / GL submission failures
- `dominant_failure_class`: the most likely failure family for the run

Current failure classes:
- `no-stream-observed`: the artifact did not capture ALVR negotiation or stream markers at all
- `compositor-submit`: render path / GL submission failure
- `decoder-config-timing`: video arrived before the decoder was configured
- `decoder-backpressure`: MediaCodec could not accept input quickly enough
- `stream-waiting-for-idr`: stream continuity was reset and the client is waiting for a fresh keyframe
- `network-packet-gap`: missing or out-of-order UDP video packets
- `control-connection`: the ALVR TCP control loop disconnected
- `none-observed`: no dominant failure class was seen in the captured window

For Crystal OG, the current known-good baseline remains:
- `2400x2400`
- `72 Hz`
- optional `HDR` / `10-bit` based on visual preference

That baseline is especially helpful when deciding whether a failure is transport-related or simply a
decode-load issue on the headset.

### Release-build troubleshooting

If a release build fails in CI with a missing runtime-libs path, verify that:
- [android/runtime-libs](D:/Code/pimax-alvr-client/android/runtime-libs) is present in the checkout
- the `arm64-v8a` loader libraries and `libpxrapi.so` are included
- the release workflow is using the current [.github/workflows/release.yaml](D:/Code/pimax-alvr-client/.github/workflows/release.yaml)

### Check Config
```bash
adb shell cat /sdcard/Android/data/com.pimax.alvr.client/files/PimaxALVR/client.json
```

### Test Connection
1. Ensure ALVR Server is running on PC
2. Note the IP shown in ALVR dashboard
3. Enter the PC's IP in the browser UI at `http://<headset-ip>:7878/`
4. Click "Connect"

## License

This project is licensed under the MIT License. See the [LICENSE](./LICENSE.md) file for details.

Some builds, integrations, or features may depend on third-party SDKs, drivers, or platform components that are licensed separately. Those components remain subject to their own respective license terms.
