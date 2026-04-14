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
| **AV1 Codec** | ✅ | Hardware decoding via Android MediaCodec |
| **Head Tracking** | ✅ | Send pose updates to server at 90Hz+ |
| **Stereo Rendering** | ✅ | Left/right eye texture submission |
| **Foveated Encoding** | ✅ | Receive and un-distort foveated video streams |
| **ViewsConfig** | ✅ | Send IPD, FOV, resolution to server |
| **Statistics** | ✅ | Report frame timing, dropped packets |
| **KeepAlive** | ✅ | Periodic control packet exchange |
| **IDR Requests** | ✅ | Request keyframes on decoder reconfiguration |

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
| `color_black_crush` | 0.0 - 0.3 | BT.709 black level (0.072 default) |
| `color_gain` | 0.5 - 2.0 | BT.709 contrast gain (1.22 default) |

### Platform Integration

| Feature | Status | Description |
|---------|--------|-------------|
| **Pimax XR Runtime** | ✅ | Enter VR mode via PxrApi |
| **Head Tracking** | ✅ | Receive poses from PxrServiceApi |
| **Proximity Sensor** | ✅ | Detect headset on/off face |
| **Screen State** | ✅ | Respond to screen on/off events |
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
- **Audio**: Not yet implemented (video-only streaming)

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
  "color_gain": 1.22
}
```

### Settings UI

Open `http://<headset-ip>:7878/` in a browser on the same network to access:
- Server IP configuration
- Server discovery scan
- Video tuning sliders
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

### Check Config
```bash
adb shell cat /sdcard/Android/data/com.pimax.alvr.client/files/PimaxALVR/client.json
```

### Test Connection
1. Ensure ALVR Server is running on PC
2. Note the IP shown in ALVR dashboard
3. Enter IP in browser UI at http://192.168.x.x:7878/
4. Click "Connect"