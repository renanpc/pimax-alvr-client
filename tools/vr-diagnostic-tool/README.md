# VR Diagnostic Tool

A SteamVR application for visually diagnosing rendering issues in VR integrations (e.g. ALVR on Pimax Crystal OG).

## Installation

```bash
pip install -r requirements.txt
```

**Requirements:**
- Python 3.10+
- SteamVR running with a connected headset
- OpenGL 3.3+ compatible GPU

## Running

```bash
python vr_diagnostic.py
```

A mirror window will appear on your PC showing both eyes side-by-side. Put on the headset to see the diagnostic frames.

## Controls (mirror window)

| Key | Mode | What to look for |
|-----|------|----------------|
| `1` | Eye Alignment | Crosshairs/rings should merge between eyes |
| `2` | Distortion Reference | Straight lines through lens curvature |
| `3` | Screen Door Effect | Individual pixels should be visible |
| `4` | Corner Acuity | Text/lines readable at screen edges |
| `5` | FOV Boundary | Content reaches exact render edge |
| `6` | Projection Asymmetry | Optical centre vs render centre |
| `D` | Export JSON | Save diagnostic data to file |
| `Q` | Quit | Exit the application |

---

## Test Modes Explained

### 1: Eye Alignment
**Purpose:** Detect eye swap, horizontal dislocation, IPD issues.

- **Left eye:** Red background, filled centre dot
- **Right eye:** Blue background, hollow centre dot
- **What to look for:**
  - Crosshairs should **perfectly overlap** at the centre
  - Concentric rings should appear as **single set**, not double
  - If crosshairs are 3+ grid squares apart (~252px), there's a problem
  - If left eye shows a blue image, eyes are swapped

### 2: Distortion Reference
**Purpose:** Verify barrel/pincushion distortion correction.

- **In the headset:** Lines will appear **curved** (normal - that's lens distortion)
- **In the mirror:** Lines should be **perfectly straight**
- **What to look for:**
  - If mirror shows curved lines = rendering distortion bug
  - If both eyes show different curvature = asymmetric distortion correction

### 3: Screen Door Effect
**Purpose:** Check pixel filtering and SDE visibility.

- **What to look for:**
  - 1px grid: should see distinct individual lines, not blur
  - Solid colour squares: should be uniformly solid
  - Gradient bars: should be smooth, not banded
  - If pixels look "fuzzy" or "shimmering" = texture filtering issue

### 4: Corner Acuity
**Purpose:** Verify content visibility at extreme FOV edges.

- **What to look for:**
  - Text at 6pt, 8pt, 10pt, 12pt in all 4 corners
  - Thin diagonal lines corner-to-corner
  - If corners are blurry or content disappears early = FOV mismatch or distortion correction over-correcting

### 5: FOV Boundary
**Purpose:** Verify render target FOV matches display FOV.

- **What to look for:**
  - Solid border = exact render edge (2px from actual edge)
  - Orange dashed rectangle = 90% safe area
  - Cyan dashed rectangle = 80% safe area
  - If you see black space beyond the solid border = FOV is too narrow
  - If content clips at the orange box = 90% of render is visible (expected for some headsets)

### 6: Projection Asymmetry
**Purpose:** Verify asymmetric frustum setup (critical for Pimax Crystal OG and similar headsets).

- Shows **optical centre** (where the lens looks) vs **render centre** (geometric centre)
- Frustum values from SteamVR are displayed
- **What to look for:**
  - Optical centre should be near the lens centre, not necessarily the render centre
  - If optical centre is far from render centre, your integration must use `getProjectionRaw` values
  - Asymmetry values should match your known headset specs

---

## Grid Reading Guide

In Mode 1, the numbered grid shows **pixel offset from render centre**:

```
          -960   -480    0   +480   +960
            |      |     |      |      |
   -960 ────┼──────┼─────┼──────┼──────┼────
            │      │     │      │      │
   -480 ────┼──────┼─────┼──────┼──────┼────
            │      │     │      │      │
      0 ────┼──────┼──── X ─────┼──────┼────   ← centre crosshair
            │      │     │      │      │
   +480 ────┼──────┼─────┼──────┼──────┼────
            │      │     │      │      │
   +960 ────┼──────┼─────┼──────┼──────┼────
```

**Example:** If the right-eye crosshair lands at `+252`, the dislocation is 252 horizontal pixels.

---

## Exporting Diagnostic Data

Press `D` to export a JSON file containing:
- Headset vendor/model/serial
- Render target size (per eye)
- Per-eye eye-to-head transforms (IPD calculation)
- Per-eye projection matrices and frustum values
- Calculated FOV (horizontal/vertical)
- Asymmetry offsets
- Raw driver information

**Share this JSON with an AI assistant** when asking for help diagnosing issues. It contains all the numerical data needed to understand your setup.

---

## Interpreting Results

### Horizontal dislocation only (~252px = 3 grid squares)
- Likely **IPD offset issue** - eyes not using per-eye `eyeToHeadTransform`
- Or both eyes using the same view matrix instead of per-eye matrices

### Vertical dislocation
- Display physical misalignment, or wrong Y offset in projection

### Both horizontal and vertical
- General transform/matrix application bug

### Asymmetric distortion between eyes
- One eye's distortion correction is wrong
- Different render target sizes per eye

### Content clips early in corners (FOV test)
- FOV mismatch between render and display
- Distortion correction parameters don't match lens profile

---

## Tips for Pimax Crystal OG + ALVR

1. **Use `getProjectionRaw`** - not a symmetric FOV. Pimax has asymmetric frustums.
2. **Apply per-eye transforms** - `getEyeToHeadTransform` gives eye offset in head space
3. **Verify IPD** - if your IPD setting is wrong, eyes will appear swapped or misaligned
4. **Frustum asymmetry** - Pimax typically has ~0.15 asymmetry on X axis, ~0.05 on Y
