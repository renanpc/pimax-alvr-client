# Pimax ALVR Client

A native Rust client for the Pimax Crystal OG standalone headset, implementing the ALVR streaming protocol to receive VR content from a PC running ALVR Server.

## Features

### ALVR Protocol Support

| Feature | Status | Description |
|---------|--------|-------------|
| **Discovery** | ✅ | UDP broadcast on port 9943 to find ALVR servers |
| **Discovery Response** | ✅ | Receive and parse server hostname/IP |
| **TCP Control** | ✅ | Port 9944 - handshake, keepalive, configuration |
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
| **Two-Pass Blit** | ✅ | Pass 1: OES→RGBA, Pass 2: RGBA→eye |
| **Convergence Shift** | ✅ | Corrects Pimax compositor divergent warp |
| **Color Correction** | ✅ | BT.709 black crush and gain adjustment |
| **Foveation Shader** | ✅ | Un-distort foveated encoding |

### Tunable Parameters (via browser at http://headset-ip:7878/)

| Parameter | Range | Description |
|-----------|-------|-------------|
| `convergence_shift_ndc` | 0.0 - 0.5 | Pre-shift to cancel Pimax warp (~0.124 default) |
| `ipd_scale` | 0.0 - 2.0 | ALVR stereo strength (1.0 = full physical IPD) |
| `fov_scale` | 0.8 - 1.2 | Fine-tune headset FOV for Pimax warp alignment |
| `eye_render_scale` | 0.5 - 2.0 | Scale eye render targets for sharper/stabler output |
| `color_black_crush` | 0.0 - 0.3 | BT.709 black level (0.072 default) |
| `color_gain` | 0.5 - 2.0 | BT.709 contrast gain (1.22 default) |

### Platform Integration

| Feature | Status | Description |
|---------|--------|-------------|
| **Pimax XR Runtime** | ✅ | Enter VR mode via PxrApi |
| **Head Tracking** | ✅ | Receive poses from PxrServiceApi |
| **Proximity Sensor** | ⚠️ | Callback wired up but log-only; no functional response |
| **Screen State** | ⚠️ | Callback wired up but log-only; screen-off shutdown disabled for development |
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
     │ H.264/H.265/AV1 over UDP
     ▼
TCP Control (9943) ◄─── Server connects to client
UDP Video (9944) ───── Sharded video packets
     │
     ▼
Android MediaCodec (Hardware Decoder)
     │
     │ AHardwareBuffer
     ▼
GL_TEXTURE_EXTERNAL_OES ──► EGLImageKHR ──► GL Texture
     │
     ▼
Two-Pass Blit Shader
- Pass 1: OES → RGBA (color correction)
- Pass 2: RGBA → Eye (convergence shift + foveation)
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

```powershell
# Set up Android NDK
$env:ANDROID_NDK_ROOT='C:\Android\android-sdk\ndk\27.3.13750724'
$env:ANDROID_HOME='C:\Android\android-sdk'

# Build the APK
powershell -ExecutionPolicy Bypass -File scripts\build-android-client.ps1

# Install and launch
adb install -r target\debug\apk\pimax-alvr-client.apk
adb shell am start -n com.pimax.alvr.client/.VrRenderActivity

# View logs
adb logcat -v time | findstr PimaxALVR
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
  "version_string": "20.14.1",
  "generated_for_version": "20.14.1",
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
adb logcat -d -s PimaxALVR
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

### Check Config
```bash
adb shell cat /sdcard/Android/data/com.pimax.alvr.client/files/PimaxALVR/client.json
```

### Test Connection
1. Ensure ALVR Server is running on PC
2. Note the IP shown in ALVR dashboard
3. Enter IP in browser UI at http://192.168.x.x:7878/
4. Click "Connect"

## License

This project is licensed under the MIT License. See the [LICENSE](./LICENSE.md) file for details.

Some builds, integrations, or features may depend on third-party SDKs, drivers, or platform components that are licensed separately. Those components remain subject to their own respective license terms.
