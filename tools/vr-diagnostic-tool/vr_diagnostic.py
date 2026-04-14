#!/usr/bin/env python3
"""
VR Diagnostic Tool - Visual Reference Edition
=============================================
Renders diagnostic frames with clear visual reference objects for AI-assisted debugging.

Each frame has objects that SHOULD appear a specific way. You describe what you actually
see, and AI can diagnose the issue from your description.

Usage:
  1. Start SteamVR and connect your headset
  2. pip install openvr PyOpenGL PyOpenGL_accelerate Pillow glfw numpy
  3. python vr_diagnostic.py

Controls (mirror window):
  - 1 : Eye Alignment Frame (red/blue per eye, check merge)
  - 2 : Distortion Reference (straight lines, circles)
  - 3 : Screen Door / Pixel Grid (fine grid for SDE visibility)
  - 4 : Corner Acuity Test (text/lines at extreme edges)
  - 5 : FOV Boundary Test (shows exact render limits)
  - 6 : Projection Asymmetry (frustum visualization)
  - D : Export diagnostic JSON data
  - Q : Quit

What each frame tests:
----------------------
FRAME 1 (Eye Alignment):
  - Red background (left eye) / Blue background (right eye)
  - White crosshair at centre - SHOULD merge perfectly if aligned
  - Concentric rings - SHOULD appear as single set, not double
  - Grid with pixel numbers - read offset if crosshairs dont merge

FRAME 2 (Distortion Reference):
  - Perfectly straight horizontal/vertical lines across full image
  - Perfect circles at centre and corners
  - THROUGH LENS: Lines will appear curved (barrel distortion is normal)
  - MIRROR VIEW: Lines should be perfectly straight
  - If mirror shows curved lines = rendering distortion bug

FRAME 3 (Screen Door Effect):
  - Fine 1-pixel grid in high contrast
  - Solid colour squares (10x10 pixels)
  - Gradient bars (smooth transition check)
  - You should see individual pixels clearly
  - If grid looks "fuzzy" or "shimmering" = SDE or filtering issue

FRAME 4 (Corner Acuity):
  - Small text (6pt, 8pt, 10pt) at all 4 corners
  - Thin diagonal lines corner-to-corner
  - SHOULD be readable/sharp at all positions
  - If corners are blurry = focus or distortion correction issue

FRAME 5 (FOV Boundary):
  - Bright border showing exact render edge
  - 90% and 80% safe-area markers
  - Content SHOULD extend to the very edge of visible area
  - If you see black beyond the border = FOV mismatch

FRAME 6 (Projection Asymmetry):
  - Shows optical centre (where lens is aimed) vs render centre
  - Asymmetric frame showing left/right/top/bottom extents
  - Numbers showing actual frustum values from SteamVR
  - Optical centre SHOULD match the marked centre dot
"""

import sys
import ctypes
import time
import json
import os
from datetime import datetime
import numpy as np
from PIL import Image, ImageDraw, ImageFont

import glfw
import OpenGL.GL as gl
import openvr


# ──────────────────────────────────────────────────────────────────────────────
# Mode constants
# ──────────────────────────────────────────────────────────────────────────────

MODE_EYE_ALIGNMENT     = 0
MODE_DISTORTION_REF    = 1
MODE_SCREEN_DOOR       = 2
MODE_CORNER_ACUITY     = 3
MODE_FOV_BOUNDARY      = 4
MODE_PROJECTION_ASYM   = 5

MODE_NAMES = {
    MODE_EYE_ALIGNMENT:   "1: Eye Alignment",
    MODE_DISTORTION_REF:  "2: Distortion Reference",
    MODE_SCREEN_DOOR:     "3: Screen Door / Pixel Grid",
    MODE_CORNER_ACUITY:   "4: Corner Acuity",
    MODE_FOV_BOUNDARY:    "5: FOV Boundary",
    MODE_PROJECTION_ASYM: "6: Projection Asymmetry",
}


# ──────────────────────────────────────────────────────────────────────────────
# OpenGL helpers
# ──────────────────────────────────────────────────────────────────────────────

VERT_SRC = """
#version 330 core
layout(location = 0) in vec2 a_pos;
layout(location = 1) in vec2 a_uv;
out vec2 v_uv;
void main() {
    gl_Position = vec4(a_pos, 0.0, 1.0);
    v_uv = a_uv;
}
"""

FRAG_SRC = """
#version 330 core
in vec2 v_uv;
out vec4 f_color;
uniform sampler2D u_tex;
void main() {
    f_color = texture(u_tex, v_uv);
}
"""


def _compile_shader(src: str, kind: int) -> int:
    shader = gl.glCreateShader(kind)
    gl.glShaderSource(shader, src)
    gl.glCompileShader(shader)
    if not gl.glGetShaderiv(shader, gl.GL_COMPILE_STATUS):
        raise RuntimeError("Shader compile error:\n" +
                           gl.glGetShaderInfoLog(shader).decode())
    return shader


def build_program() -> int:
    vs = _compile_shader(VERT_SRC, gl.GL_VERTEX_SHADER)
    fs = _compile_shader(FRAG_SRC, gl.GL_FRAGMENT_SHADER)
    prog = gl.glCreateProgram()
    gl.glAttachShader(prog, vs)
    gl.glAttachShader(prog, fs)
    gl.glLinkProgram(prog)
    if not gl.glGetProgramiv(prog, gl.GL_LINK_STATUS):
        raise RuntimeError("Shader link error:\n" +
                           gl.glGetProgramInfoLog(prog).decode())
    gl.glDeleteShader(vs)
    gl.glDeleteShader(fs)
    return prog


def make_quad_vao(x0: float, y0: float, x1: float, y1: float,
                  flip_v: bool = False):
    """Return (vao, vbo, ebo) for a textured quad in NDC coords."""
    u0, v0, u1, v1 = 0.0, 0.0, 1.0, 1.0
    if flip_v:
        v0, v1 = 1.0, 0.0
    verts = np.array([
        x0, y0,  u0, v0,
        x1, y0,  u1, v0,
        x1, y1,  u1, v1,
        x0, y1,  u0, v1,
    ], dtype=np.float32)
    indices = np.array([0, 1, 2, 2, 3, 0], dtype=np.uint32)

    vao = gl.glGenVertexArrays(1)
    vbo = gl.glGenBuffers(1)
    ebo = gl.glGenBuffers(1)

    gl.glBindVertexArray(vao)
    gl.glBindBuffer(gl.GL_ARRAY_BUFFER, vbo)
    gl.glBufferData(gl.GL_ARRAY_BUFFER, verts.nbytes, verts, gl.GL_STATIC_DRAW)
    gl.glBindBuffer(gl.GL_ELEMENT_ARRAY_BUFFER, ebo)
    gl.glBufferData(gl.GL_ELEMENT_ARRAY_BUFFER, indices.nbytes, indices,
                    gl.GL_STATIC_DRAW)

    stride = 4 * 4
    gl.glVertexAttribPointer(0, 2, gl.GL_FLOAT, gl.GL_FALSE, stride,
                             ctypes.c_void_p(0))
    gl.glEnableVertexAttribArray(0)
    gl.glVertexAttribPointer(1, 2, gl.GL_FLOAT, gl.GL_FALSE, stride,
                             ctypes.c_void_p(8))
    gl.glEnableVertexAttribArray(1)

    gl.glBindVertexArray(0)
    return vao, vbo, ebo


def upload_pil_texture(img: Image.Image) -> int:
    """Upload a PIL image as an OpenGL texture. Supports RGB and RGBA."""
    img_flipped = img.transpose(Image.FLIP_TOP_BOTTOM)
    data = img_flipped.tobytes()

    if img.mode == 'RGBA':
        internal_fmt = gl.GL_RGBA8
        pixel_fmt = gl.GL_RGBA
    elif img.mode == 'RGB':
        internal_fmt = gl.GL_RGB8
        pixel_fmt = gl.GL_RGB
    else:
        raise ValueError(f"Unsupported image mode: {img.mode} (use RGB or RGBA)")

    tex = gl.glGenTextures(1)
    gl.glBindTexture(gl.GL_TEXTURE_2D, tex)
    gl.glTexImage2D(gl.GL_TEXTURE_2D, 0, internal_fmt,
                    img.width, img.height, 0,
                    pixel_fmt, gl.GL_UNSIGNED_BYTE, data)
    gl.glTexParameteri(gl.GL_TEXTURE_2D, gl.GL_TEXTURE_MIN_FILTER, gl.GL_LINEAR)
    gl.glTexParameteri(gl.GL_TEXTURE_2D, gl.GL_TEXTURE_MAG_FILTER, gl.GL_LINEAR)
    gl.glTexParameteri(gl.GL_TEXTURE_2D, gl.GL_TEXTURE_WRAP_S, gl.GL_CLAMP_TO_EDGE)
    gl.glTexParameteri(gl.GL_TEXTURE_2D, gl.GL_TEXTURE_WRAP_T, gl.GL_CLAMP_TO_EDGE)
    gl.glBindTexture(gl.GL_TEXTURE_2D, 0)
    return tex


def make_vr_texture(gl_id: int) -> openvr.Texture_t:
    """Wrap a GL texture ID in an OpenVR Texture_t struct."""
    t = openvr.Texture_t()
    t.handle = int(gl_id)
    t.eType = openvr.TextureType_OpenGL
    t.eColorSpace = openvr.ColorSpace_Gamma
    return t


# ──────────────────────────────────────────────────────────────────────────────
# Font loading
# ──────────────────────────────────────────────────────────────────────────────

def _load_font(size: int) -> ImageFont.FreeTypeFont:
    candidates = [
        "C:/Windows/Fonts/arialbd.ttf",
        "C:/Windows/Fonts/arial.ttf",
        "C:/Windows/Fonts/verdanab.ttf",
        "C:/Windows/Fonts/consola.ttf",
    ]
    for path in candidates:
        try:
            return ImageFont.truetype(path, size)
        except OSError:
            pass
    return ImageFont.load_default()


# ──────────────────────────────────────────────────────────────────────────────
# Diagnostic frame generators
# ──────────────────────────────────────────────────────────────────────────────

def frame_eye_alignment(eye: str, width: int, height: int) -> Image.Image:
    """
    FRAME 1: Eye Alignment Test

    Visual references:
    - LEFT eye = RED background, RIGHT eye = BLUE background
    - Centre crosshair should merge perfectly between eyes
    - Concentric rings should appear as single set
    - Grid numbers show pixel offset from centre
    """
    is_left = eye.upper() == 'LEFT'
    bg = (15, 3, 3) if is_left else (3, 5, 18)
    accent = (220, 50, 50) if is_left else (55, 90, 230)
    accent_dim = (70, 18, 18) if is_left else (18, 25, 75)
    white = (200, 200, 200)

    img = Image.new('RGB', (width, height), bg)
    draw = ImageDraw.Draw(img)

    cx, cy = width // 2, height // 2
    grid_px = max(20, width // 24)
    label_every = 4

    font_tiny = _load_font(max(10, height // 65))
    font_med = _load_font(max(18, height // 22))

    # Header
    bar_h = max(28, height // 12)
    draw.rectangle([0, 0, width, bar_h], fill=accent)
    draw.text((cx, bar_h // 2), f"{'LEFT' if is_left else 'RIGHT'} EYE - Alignment Test",
              fill=white, font=font_med, anchor='mm')

    # Grid lines
    for x in range(0, width + 1, grid_px):
        is_ctr = (x == cx)
        draw.line([(x, 0), (x, height)],
                  fill=(accent if is_ctr else accent_dim),
                  width=(2 if is_ctr else 1))

    for y in range(0, height + 1, grid_px):
        is_ctr = (y == cy)
        draw.line([(0, y), (width, y)],
                  fill=(accent if is_ctr else accent_dim),
                  width=(2 if is_ctr else 1))

    # Grid labels (pixel offset from centre)
    for x in range(grid_px, width, grid_px * label_every):
        if x != cx:
            draw.text((x + 2, cy + 3), str((x - cx)), fill=white, font=font_tiny)
    for y in range(grid_px, height, grid_px * label_every):
        if y != cy:
            draw.text((cx + 3, y + 2), str((y - cy)), fill=white, font=font_tiny)

    # Concentric rings (should merge between eyes)
    for r in [40, 80, 120, 160, 200]:
        draw.ellipse([(cx - r, cy - r), (cx + r, cy + r)],
                     outline=white, width=2)

    # Centre crosshair
    arm = min(width, height) // 4
    draw.line([(cx - arm, cy), (cx + arm, cy)], fill=accent, width=3)
    draw.line([(cx, cy - arm), (cx, cy + arm)], fill=accent, width=3)

    # Centre dot (filled left, hollow right)
    dot_r = 10
    if is_left:
        draw.ellipse([(cx - dot_r, cy - dot_r), (cx + dot_r, cy + dot_r)],
                     fill=accent, outline=white, width=2)
    else:
        draw.ellipse([(cx - dot_r, cy - dot_r), (cx + dot_r, cy + dot_r)],
                     fill=bg, outline=white, width=3)

    # Corner markers
    cs = 40
    corners = ['TL', 'TR', 'BL', 'BR']
    positions = [(0, 0), (width - cs, 0), (0, height - cs), (width - cs, height - cs)]
    for (x, y), label in zip(positions, corners):
        draw.rectangle([x, y, x + cs, y + cs], fill=accent)
        draw.text((x + cs // 2, y + cs // 2), label, fill=white,
                  font=font_med, anchor='mm')

    # Instructions at bottom
    draw.rectangle([0, height - 50, width, height], fill=(0, 0, 0))
    draw.text((cx, height - 25),
              "CHECK: Crosshair/rings should merge. Read grid number if offset.",
              fill=white, font=font_med, anchor='mm')

    return img


def frame_distortion_reference(eye: str, width: int, height: int) -> Image.Image:
    """
    FRAME 2: Distortion Reference

    Visual references:
    - Perfectly straight lines horizontal/vertical (full width)
    - Perfect circles at centre and 4 corners
    - These lines SHOULD be straight in mirror view
    - Through lens they will appear curved (normal barrel distortion)
    - If mirror shows curves = rendering bug
    """
    is_left = eye.upper() == 'LEFT'
    bg = (10, 10, 15) if is_left else (10, 12, 18)
    line_color = (200, 200, 255)
    circle_color = (255, 100, 100) if is_left else (100, 150, 255)

    img = Image.new('RGB', (width, height), bg)
    draw = ImageDraw.Draw(img)

    cx, cy = width // 2, height // 2
    font_med = _load_font(max(16, height // 20))
    font_sm = _load_font(max(12, height // 35))

    # Header
    draw.rectangle([0, 0, width, 40], fill=(30, 30, 40))
    draw.text((cx, 20), "DISTORTION REFERENCE - Lines SHOULD be straight",
              fill=(255, 255, 255), font=font_med, anchor='mm')

    # Full-width straight lines (every 10% of screen)
    for i in range(1, 10):
        x = width * i // 10
        y = height * i // 10
        draw.line([(x, 0), (x, height)], fill=line_color, width=1)
        draw.line([(0, y), (width, y)], fill=line_color, width=1)

    # Centre crosshair
    draw.line([(cx, 0), (cx, height)], fill=(255, 100, 100), width=2)
    draw.line([(0, cy), (width, cy)], fill=(255, 100, 100), width=2)

    # Perfect circles - centre and corners
    circle_radii = [30, 60, 90]
    # Centre circles
    for r in circle_radii:
        draw.ellipse([(cx - r, cy - r), (cx + r, cy + r)],
                     outline=circle_color, width=2)

    # Corner circles
    corner_offset = min(width, height) // 4
    corner_positions = [
        (corner_offset, corner_offset),
        (width - corner_offset, corner_offset),
        (corner_offset, height - corner_offset),
        (width - corner_offset, height - corner_offset),
    ]
    for px, py in corner_positions:
        draw.ellipse([(px - 25, py - 25), (px + 25, py + 25)],
                     outline=circle_color, width=2)

    # Diagonal reference lines
    draw.line([(0, 0), (width, height)], fill=(100, 255, 100), width=1)
    draw.line([(width, 0), (0, height)], fill=(100, 255, 100), width=1)

    # Labels
    draw.text((10, 50), "MIRROR VIEW: Lines should be STRAIGHT",
              fill=(100, 255, 100), font=font_sm)
    draw.text((10, 70), "THROUGH LENS: Curvature is NORMAL (barrel distortion)",
              fill=(255, 200, 100), font=font_sm)

    return img


def frame_screen_door(eye: str, width: int, height: int) -> Image.Image:
    """
    FRAME 3: Screen Door Effect Test

    Visual references:
    - 1-pixel grid (high contrast)
    - 10x10 pixel solid colour squares
    - Smooth gradient bars
    - Individual pixels SHOULD be distinguishable
    - Fuzzy/shimmering = SDE or filtering issue
    """
    is_left = eye.upper() == 'LEFT'
    bg = (5, 5, 5)
    grid_color = (255, 255, 255) if is_left else (100, 200, 255)

    img = Image.new('RGB', (width, height), bg)
    draw = ImageDraw.Draw(img)

    cx, cy = width // 2, height // 2
    font_med = _load_font(max(16, height // 20))

    # Header
    draw.rectangle([0, 0, width, 40], fill=(30, 30, 40))
    draw.text((cx, 20), "SCREEN DOOR TEST - Pixels SHOULD be distinct",
              fill=(255, 255, 255), font=font_med, anchor='mm')

    # Fine 1-pixel grid (top-left quadrant)
    grid_area_w, grid_area_h = width // 2, height // 2
    for x in range(0, grid_area_w, 2):
        draw.line([(x, 40), (x, 40 + grid_area_h)], fill=grid_color, width=1)
    for y in range(40, 40 + grid_area_h, 2):
        draw.line([(0, y), (grid_area_w, y)], fill=grid_color, width=1)

    # 10x10 pixel solid squares (top-right quadrant)
    sq_size = 10
    colors = [(255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 0),
              (255, 0, 255), (0, 255, 255), (255, 255, 255), (128, 128, 128)]
    for i, color in enumerate(colors):
        x = width // 2 + (i % 4) * sq_size * 3
        y = 50 + (i // 4) * sq_size * 3
        draw.rectangle([x, y, x + sq_size * 2, y + sq_size * 2], fill=color)

    # Horizontal gradient bars (bottom half)
    grad_y_start = height // 2 + 50
    bar_h = 30
    for i in range(5):
        y = grad_y_start + i * (bar_h + 10)
        for x in range(width):
            intensity = int(255 * x / width)
            if i == 0:  # Red gradient
                color = (intensity, 0, 0)
            elif i == 1:  # Green gradient
                color = (0, intensity, 0)
            elif i == 2:  # Blue gradient
                color = (0, 0, intensity)
            elif i == 3:  # Gray gradient
                color = (intensity, intensity, intensity)
            else:  # Checkerboard edge test
                color = (255, 255, 255) if (x // 5 + i) % 2 == 0 else (0, 0, 0)
            draw.line([(x, y), (x, y + bar_h)], fill=color)

    # Labels
    draw.text((10, 50), "1px grid: Should see individual lines",
              fill=(200, 200, 200), font=font_med)
    draw.text((10, 50 + 20), "Squares: Should be solid, not dithered",
              fill=(200, 200, 200), font=font_med)
    draw.text((10, 50 + 40), "Gradients: Should be smooth, no banding",
              fill=(200, 200, 200), font=font_med)

    return img


def frame_corner_acuity(eye: str, width: int, height: int) -> Image.Image:
    """
    FRAME 4: Corner Acuity Test

    Visual references:
    - Small text at 6pt, 8pt, 10pt, 12pt in all 4 corners
    - Thin diagonal lines corner-to-corner
    - Fine spiral pattern at corners
    - Text SHOULD be readable, lines SHOULD be sharp
    - Blurry corners = focus or distortion correction issue
    """
    is_left = eye.upper() == 'LEFT'
    bg = (8, 8, 12) if is_left else (10, 8, 12)
    text_color = (255, 255, 255)
    line_color = (255, 100, 100) if is_left else (100, 255, 100)

    img = Image.new('RGB', (width, height), bg)
    draw = ImageDraw.Draw(img)

    cx, cy = width // 2, height // 2
    font_6 = _load_font(6)
    font_8 = _load_font(8)
    font_10 = _load_font(10)
    font_12 = _load_font(12)
    font_16 = _load_font(16)

    # Header
    draw.rectangle([0, 0, width, 40], fill=(30, 30, 40))
    draw.text((cx, 20), "CORNER ACUITY - Text SHOULD be readable at edges",
              fill=(255, 255, 255), font=font_16, anchor='mm')

    # Diagonal corner-to-corner lines
    draw.line([(0, 40), (width, height)], fill=line_color, width=1)
    draw.line([(width, 40), (0, height)], fill=line_color, width=1)

    # Corner text blocks
    margin = 20
    corner_data = [
        (margin, 60, "TOP-LEFT"),
        (width - margin, 60, "TOP-RIGHT"),
        (margin, height - 100, "BOTTOM-LEFT"),
        (width - margin, height - 100, "BOTTOM-RIGHT"),
    ]

    for x, y, label in corner_data:
        # Label box
        draw.rectangle([x - 10, y - 20, x + 100, y + 80], fill=(0, 0, 0))
        draw.rectangle([x - 10, y - 20, x + 100, y + 80], outline=line_color, width=2)

        # Text at different sizes
        draw.text((x, y), label, fill=text_color, font=font_12)
        draw.text((x, y + 20), "12pt text", fill=(200, 200, 200), font=font_12)
        draw.text((x, y + 38), "10pt text", fill=(180, 180, 180), font=font_10)
        draw.text((x, y + 54), "8pt text", fill=(160, 160, 160), font=font_8)
        draw.text((x, y + 68), "6pt", fill=(140, 140, 140), font=font_6)

    # Centre acuity target
    for r in range(5, 100, 5):
        draw.ellipse([(cx - r, cy - r), (cx + r, cy + r)],
                     outline=text_color, width=1)

    draw.line([(cx - 100, cy), (cx + 100, cy)], fill=(255, 0, 0), width=1)
    draw.line([(cx, cy - 100), (cx, cy + 100)], fill=(255, 0, 0), width=1)

    return img


def frame_fov_boundary(eye: str, width: int, height: int) -> Image.Image:
    """
    FRAME 5: FOV Boundary Test

    Visual references:
    - Bright border showing exact render edge
    - 90% and 80% safe-area rectangles
    - Content SHOULD extend to visible edge
    - Black beyond border = FOV mismatch
    """
    is_left = eye.upper() == 'LEFT'
    bg = (5, 5, 8)
    border_color = (255, 50, 50) if is_left else (50, 255, 100)
    safe_90_color = (255, 200, 0)
    safe_80_color = (0, 200, 255)

    img = Image.new('RGB', (width, height), bg)
    draw = ImageDraw.Draw(img)

    cx, cy = width // 2, height // 2
    font_med = _load_font(max(16, height // 20))

    # Header
    draw.rectangle([0, 0, width, 40], fill=(30, 30, 40))
    draw.text((cx, 20), "FOV BOUNDARY - Content SHOULD reach the edge",
              fill=(255, 255, 255), font=font_med, anchor='mm')

    # Exact render boundary (2px from actual edge)
    boundary_margin = 2
    draw.rectangle([boundary_margin, 42, width - boundary_margin, height - boundary_margin],
                   outline=border_color, width=3)

    # 90% safe area
    safe_90_l = width * 0.05
    safe_90_t = 40 + height * 0.05
    safe_90_r = width * 0.95
    safe_90_b = height * 0.95
    draw.rectangle([safe_90_l, safe_90_t, safe_90_r, safe_90_b],
                   outline=safe_90_color, width=2, dash=(4, 4))

    # 80% safe area
    safe_80_l = width * 0.10
    safe_80_t = 40 + height * 0.10
    safe_80_r = width * 0.90
    safe_80_b = height * 0.90
    draw.rectangle([safe_80_l, safe_80_t, safe_80_r, safe_80_b],
                   outline=safe_80_color, width=2, dash=(8, 4))

    # Edge markers with coordinates
    marker_positions = [
        (width // 2, 45, "TOP"),
        (width // 2, height - 15, "BOTTOM"),
        (15, height // 2, "LEFT"),
        (width - 15, height // 2, "RIGHT"),
    ]
    for x, y, label in marker_positions:
        draw.text((x, y), label, fill=border_color, font=font_med, anchor='mm')

    # Corner coordinate readout
    corners = [
        (5, 55, f"(0,0)"),
        (width - 5, 55, f"({width},{0})"),
        (5, height - 5, f"(0,{height})"),
        (width - 5, height - 5, f"({width},{height})"),
    ]
    for x, y, label in corners:
        draw.text((x, y), label, fill=(150, 150, 150), font=font_med,
                  anchor=['lm', 'rm', 'lm', 'rm'][corners.index((x, y, label))])

    # Legend
    legend_y = 70
    draw.text((10, legend_y), "Solid:", fill=border_color, font=font_med)
    draw.text((80, legend_y), "= Render boundary", fill=(200, 200, 200), font=font_med)
    draw.text((10, legend_y + 20), "Orange dashed:", fill=safe_90_color, font=font_med)
    draw.text((140, legend_y + 20), "= 90% safe area", fill=(200, 200, 200), font=font_med)
    draw.text((10, legend_y + 40), "Cyan dashed:", fill=safe_80_color, font=font_med)
    draw.text((140, legend_y + 40), "= 80% safe area", fill=(200, 200, 200), font=font_med)

    return img


def frame_projection_asymmetry(eye: str, width: int, height: int,
                                vr_data: dict = None) -> Image.Image:
    """
    FRAME 6: Projection Asymmetry Test

    Visual references:
    - Shows optical centre vs render centre
    - Asymmetric frame from getProjectionRaw values
    - Frustum values displayed
    - Optical centre SHOULD align with lens centre
    """
    is_left = eye.upper() == 'LEFT'
    bg = (8, 5, 10) if is_left else (5, 8, 10)

    img = Image.new('RGB', (width, height), bg)
    draw = ImageDraw.Draw(img)

    cx, cy = width // 2, height // 2
    font_med = _load_font(max(16, height // 20))
    font_sm = _load_font(max(12, height // 30))
    font_mono = _load_font(max(14, height // 25))

    # Header
    draw.rectangle([0, 0, width, 40], fill=(30, 30, 40))
    draw.text((cx, 20), "PROJECTION ASYMMETRY - Optical centre vs Render centre",
              fill=(255, 255, 255), font=font_med, anchor='mm')

    # Render centre (geometric centre)
    draw.line([(cx - 20, cy), (cx + 20, cy)], fill=(200, 200, 200), width=2)
    draw.line([(cx, cy - 20), (cx, cy + 20)], fill=(200, 200, 200), width=2)
    draw.text((cx, cy + 35), "RENDER CENTRE", fill=(200, 200, 200),
              font=font_sm, anchor='mm')

    # If we have VR data, show optical centre from projection
    optical_x, optical_y = cx, cy
    if vr_data and 'eye_data' in vr_data:
        eye_key = 'left' if is_left else 'right'
        if eye_key in vr_data['eye_data']:
            frustum = vr_data['eye_data'][eye_key].get('projection_frustum_raw')
            if frustum and isinstance(frustum, dict):
                l, r, t, b = (frustum.get(k, 0) for k in ['left', 'right', 'top', 'bottom'])

                # Asymmetry = (l + r) for x, (t + b) for y
                asym_x = (l + r) / 2
                asym_y = (t + b) / 2

                # Convert to pixel offset (approximate)
                optical_x = cx + int(asym_x * width * 0.5)
                optical_y = cy + int(asym_y * height * 0.5)

                # Draw optical centre
                draw.line([(optical_x - 15, optical_y), (optical_x + 15, optical_y)],
                         fill=(255, 100, 100) if is_left else (100, 255, 100), width=3)
                draw.line([(optical_x, optical_y - 15), (optical_x, optical_y + 15)],
                         fill=(255, 100, 100) if is_left else (100, 255, 100), width=3)
                draw.text((optical_x, optical_y + 30), "OPTICAL",
                         fill=(255, 100, 100) if is_left else (100, 255, 100),
                         font=font_sm, anchor='mm')

                # Frustum values display
                fx, fy = 10, 60
                draw.rectangle([fx - 5, fy - 5, fx + 280, fy + 115], fill=(0, 0, 0, 180))
                draw.rectangle([fx - 5, fy - 5, fx + 280, fy + 115], outline=(100, 100, 100), width=1)
                draw.text((fx, fy), f"Frustum values ({eye_key} eye):",
                         fill=(255, 255, 255), font=font_sm)
                draw.text((fx, fy + 18), f"  Left:   {l:+.4f}", fill=(200, 200, 200), font=font_mono)
                draw.text((fx, fy + 40), f"  Right:  {r:+.4f}", fill=(200, 200, 200), font=font_mono)
                draw.text((fx, fy + 62), f"  Top:    {t:+.4f}", fill=(200, 200, 200), font=font_mono)
                draw.text((fx, fy + 84), f"  Bottom: {b:+.4f}", fill=(200, 200, 200), font=font_mono)
                draw.text((fx, fy + 106), f"  AsymX:  {asym_x:+.4f}", fill=(255, 150, 150), font=font_mono)

    # Draw offset indicator line between centres
    if optical_x != cx or optical_y != cy:
        draw.line([(cx, cy), (optical_x, optical_y)],
                 fill=(255, 255, 0), width=1, dash=(4, 4))

        # Offset measurement
        offset_px = int(((optical_x - cx) ** 2 + (optical_y - cy) ** 2) ** 0.5)
        draw.text((cx + 10, (cy + optical_y) // 2), f"{offset_px}px",
                 fill=(255, 255, 0), font=font_sm)

    # CHECK label
    draw.rectangle([0, height - 40, width, height], fill=(20, 20, 25))
    draw.text((cx, height - 20),
              "CHECK: Optical centre should be near lens centre (not necessarily render centre)",
              fill=(200, 200, 200), font=font_sm, anchor='mm')

    return img


# Dispatch table for frame generators
FRAME_GENERATORS = {
    MODE_EYE_ALIGNMENT: frame_eye_alignment,
    MODE_DISTORTION_REF: frame_distortion_reference,
    MODE_SCREEN_DOOR: frame_screen_door,
    MODE_CORNER_ACUITY: frame_corner_acuity,
    MODE_FOV_BOUNDARY: frame_fov_boundary,
    MODE_PROJECTION_ASYM: frame_projection_asymmetry,
}


# ──────────────────────────────────────────────────────────────────────────────
# Diagnostic data export
# ──────────────────────────────────────────────────────────────────────────────

def _matrix_to_list(mat):
    """Convert OpenVR HmdMatrix44_t to flat list (row-major)."""
    return [
        mat.m[0][0], mat.m[0][1], mat.m[0][2], mat.m[0][3],
        mat.m[1][0], mat.m[1][1], mat.m[1][2], mat.m[1][3],
        mat.m[2][0], mat.m[2][1], mat.m[2][2], mat.m[2][3],
        mat.m[3][0], mat.m[3][1], mat.m[3][2], mat.m[3][3],
    ]


def export_diagnostic_data(vr_system, width, height, output_dir=None):
    """
    Collect all diagnostic data and export to JSON.
    Share this file with AI for analysis.
    """
    if output_dir is None:
        output_dir = os.getcwd()

    timestamp = datetime.now().isoformat()
    filename = os.path.join(output_dir, f"vr_diagnostic_{timestamp.replace(':', '-')}.json")

    data = {
        'exported_at': timestamp,
        'headset': {'vendor': '', 'model': '', 'serial': ''},
        'render_target': {'width_per_eye': width, 'height_per_eye': height},
        'eye_data': {},
        'ipd_info': {},
    }

    # Headset identification
    try:
        data['headset']['vendor'] = vr_system.getStringTrackedDeviceProperty(
            openvr.k_unTrackedDeviceIndex_Hmd, openvr.Prop_Vendor_String_TrackedDeviceProperty)
        data['headset']['model'] = vr_system.getStringTrackedDeviceProperty(
            openvr.k_unTrackedDeviceIndex_Hmd, openvr.Prop_ModelNumber_String_TrackedDeviceProperty)
        data['headset']['serial'] = vr_system.getStringTrackedDeviceProperty(
            openvr.k_unTrackedDeviceIndex_Hmd, openvr.Prop_SerialNumber_String_TrackedDeviceProperty)
    except:
        pass

    # Per-eye data
    for eye_name, eye_const in [('left', openvr.Eye_Left), ('right', openvr.Eye_Right)]:
        eye_data = {}

        # Eye-to-head transform
        try:
            e2h = vr_system.getEyeToHeadTransform(eye_const)
            eye_data['eye_to_head_transform'] = _matrix_to_list(e2h)
            eye_data['eye_position_mm'] = {
                'x': round(e2h.m[0][3] * 1000, 2),
                'y': round(e2h.m[1][3] * 1000, 2),
                'z': round(e2h.m[2][3] * 1000, 2),
            }
        except Exception as e:
            eye_data['eye_to_head_transform'] = f'error: {e}'

        # Projection frustum (raw)
        try:
            l, r, t, b = vr_system.getProjectionRaw(eye_const)
            eye_data['projection_frustum_raw'] = {'left': l, 'right': r, 'top': t, 'bottom': b}
            eye_data['asymmetry'] = {
                'x_offset': round(l + r, 4),
                'y_offset': round(t + b, 4),
                'is_asymmetric': abs(l + r) > 0.01 or abs(t + b) > 0.01,
            }
            # FOV calculation
            fov_h = np.degrees(np.arctan(r) - np.arctan(l))
            fov_v = np.degrees(np.arctan(t) - np.arctan(b))
            eye_data['fov_degrees'] = {'horizontal': round(fov_h, 2), 'vertical': round(fov_v, 2)}
        except Exception as e:
            eye_data['projection_frustum_raw'] = f'error: {e}'

        data['eye_data'][eye_name] = eye_data

    # IPD
    try:
        left_x = vr_system.getEyeToHeadTransform(openvr.Eye_Left).m[0][3]
        right_x = vr_system.getEyeToHeadTransform(openvr.Eye_Right).m[0][3]
        ipd = abs(right_x - left_x)
        data['ipd_info'] = {
            'ipd_mm': round(ipd * 1000, 2),
            'left_eye_x_mm': round(left_x * 1000, 2),
            'right_eye_x_mm': round(right_x * 1000, 2),
        }
    except Exception as e:
        data['ipd_info'] = {'error': str(e)}

    with open(filename, 'w') as f:
        json.dump(data, f, indent=2)

    print(f"\n[EXPORT] Diagnostic data: {filename}")
    return filename, data


# ──────────────────────────────────────────────────────────────────────────────
# Main application
# ──────────────────────────────────────────────────────────────────────────────

def main() -> None:
    print("=" * 62)
    print("  VR Diagnostic Tool - Visual Reference Edition")
    print("=" * 62)

    # IMPORTANT: Init GLFW/OpenGL BEFORE OpenVR to avoid segfaults
    # OpenVR queries the context and can conflict if initialized first
    print("[1/6] Initialising GLFW...")
    if not glfw.init():
        raise RuntimeError("GLFW init failed")

    glfw.window_hint(glfw.CONTEXT_VERSION_MAJOR, 3)
    glfw.window_hint(glfw.CONTEXT_VERSION_MINOR, 3)
    glfw.window_hint(glfw.OPENGL_PROFILE, glfw.OPENGL_CORE_PROFILE)
    glfw.window_hint(glfw.OPENGL_FORWARD_COMPAT, gl.GL_TRUE)

    mirror_w, mirror_h = 1600, 500
    window = glfw.create_window(mirror_w, mirror_h, "VR Diagnostic - Mirror View", None, None)
    if not window:
        glfw.terminate()
        raise RuntimeError("Failed to create GLFW window")

    glfw.make_context_current(window)
    glfw.swap_interval(0)
    print(f"      OpenGL {gl.glGetString(gl.GL_VERSION).decode()}")

    # NOW init SteamVR (after OpenGL context exists)
    print("[2/6] Initialising SteamVR...")
    try:
        vr_system = openvr.init(openvr.VRApplication_Scene)
        print("[OK] SteamVR initialised.")
    except openvr.OpenVRError as exc:
        print(f"[ERROR] Cannot connect to SteamVR: {exc}")
        print("        Make sure SteamVR is running and a headset is connected.")
        glfw.terminate()
        sys.exit(1)

    compositor = openvr.VRCompositor()
    vr_width, vr_height = vr_system.getRecommendedRenderTargetSize()
    print(f"[3/6] VR Render target: {vr_width} x {vr_height} per eye")

    # Use smaller render size for diagnostic images to avoid memory/texture issues
    # The VR compositor will scale anyway, and visual diagnostics work fine at lower res
    width, height = 1920, 1920
    print(f"      Using diagnostic image size: {width} x {height}")

    # Export diagnostic data immediately
    print("[4/6] Exporting diagnostic data...")
    _, vr_data = export_diagnostic_data(vr_system, width, height)

    print("[5/6] Building shader program...")
    prog = build_program()
    u_tex = gl.glGetUniformLocation(prog, "u_tex")
    print("      Shader program OK")

    # State
    current_mode = MODE_EYE_ALIGNMENT
    textures = {}
    vaos = {}
    vbos = {}
    ebos = {}
    vr_textures = {}

    print("[6/6] Generating diagnostic images...")
    sys.stdout.flush()

    def regenerate_images():
        nonlocal textures, vaos, vbos, ebos, vr_textures

        print(f"   Generating for mode {current_mode}...")
        sys.stdout.flush()

        # Generate new (don't delete old - OS cleans up on exit)
        textures = {}
        vaos = {}
        vbos = {}
        ebos = {}
        vr_textures = {}

        gen = FRAME_GENERATORS[current_mode]
        for eye_name, eye_key in [('left', 'LEFT'), ('right', 'RIGHT')]:
            print(f"     {eye_name} eye image...", end=" ")
            sys.stdout.flush()
            if current_mode == MODE_PROJECTION_ASYM:
                img = gen(eye_key, width, height, vr_data)
            else:
                img = gen(eye_key, width, height)
            print(f"generated ({img.size})", end=" ")
            sys.stdout.flush()

            print(f"-> texture...", end=" ")
            sys.stdout.flush()
            tex = upload_pil_texture(img)
            textures[eye_name] = tex
            vr_textures[eye_name] = make_vr_texture(tex)
            print(f"GL tex={tex}", end=" ")
            sys.stdout.flush()

            x0 = -1.0 if eye_name == 'left' else 0.0
            x1 = 0.0 if eye_name == 'left' else 1.0
            print(f"-> VAO...", end=" ")
            sys.stdout.flush()
            vao, vbo, ebo = make_quad_vao(x0, -1.0, x1, 1.0, flip_v=True)
            vaos[eye_name] = vao
            vbos[eye_name] = vbo
            ebos[eye_name] = ebo
            print(f"done")
            sys.stdout.flush()

        print(f"[MODE] Switched to {MODE_NAMES[current_mode]}")

    regenerate_images()
    print("      Images OK", flush=True)

    # Wait for the VR compositor to be ready
    print("      Waiting for VR compositor...", end=" ", flush=True)
    pose_array_len = openvr.k_unMaxTrackedDeviceCount
    render_poses = (openvr.TrackedDevicePose_t * pose_array_len)()
    game_poses = (openvr.TrackedDevicePose_t * pose_array_len)()
    try:
        compositor.waitGetPoses(render_poses, game_poses)
        print("OK", flush=True)
    except Exception as e:
        print(f"warning: {e}", flush=True)

    print("CONTROLS:", flush=True)
    for mode_id, mode_name in MODE_NAMES.items():
        print(f"  {mode_id} : {mode_name}", flush=True)
    print("  D : Export diagnostic JSON", flush=True)
    print("  Q : Quit", flush=True)
    print(flush=True)
    print("Put on the headset and describe what you see.", flush=True)

    frame = 0
    t_last_fps = time.time()

    # Key state tracking to avoid rapid mode switching
    key_states = {
        glfw.KEY_1: False, glfw.KEY_2: False, glfw.KEY_3: False,
        glfw.KEY_4: False, glfw.KEY_5: False, glfw.KEY_6: False,
        glfw.KEY_D: False, glfw.KEY_Q: False,
    }

    def handle_key(key, mode):
        nonlocal current_mode
        pressed = glfw.get_key(window, key) == glfw.PRESS
        if pressed and not key_states[key]:
            if mode is None:  # Export action
                export_diagnostic_data(vr_system, width, height)
            elif current_mode != mode:
                current_mode = mode
                regenerate_images()
        key_states[key] = pressed

    while not glfw.window_should_close(window):
        glfw.poll_events()

        # Handle VR events
        evt = openvr.VREvent_t()
        while vr_system.pollNextEvent(evt):
            if evt.eventType == openvr.VREvent_Quit:
                glfw.set_window_should_close(window, True)

        handle_key(glfw.KEY_1, MODE_EYE_ALIGNMENT)
        handle_key(glfw.KEY_2, MODE_DISTORTION_REF)
        handle_key(glfw.KEY_3, MODE_SCREEN_DOOR)
        handle_key(glfw.KEY_4, MODE_CORNER_ACUITY)
        handle_key(glfw.KEY_5, MODE_FOV_BOUNDARY)
        handle_key(glfw.KEY_6, MODE_PROJECTION_ASYM)
        handle_key(glfw.KEY_D, None)  # Export
        if glfw.get_key(window, glfw.KEY_Q) == glfw.PRESS:
            break

        # Wait for VR poses and submit to compositor
        # Use proper arrays to avoid issues with None
        try:
            compositor.waitGetPoses(render_poses, game_poses)
            submitted = True
        except Exception:
            submitted = False

        if submitted:
            try:
                compositor.submit(openvr.Eye_Left, vr_textures['left'])
                compositor.submit(openvr.Eye_Right, vr_textures['right'])
            except openvr.error_code.CompositorError_DoNotHaveFocus:
                pass  # Dashboard/overlay focus loss is temporary

        # Mirror window rendering
        mw, mh = glfw.get_framebuffer_size(window)
        gl.glViewport(0, 0, mw, mh)
        gl.glClearColor(0.1, 0.1, 0.1, 1.0)
        gl.glClear(gl.GL_COLOR_BUFFER_BIT)
        gl.glUseProgram(prog)
        gl.glActiveTexture(gl.GL_TEXTURE0)
        gl.glUniform1i(u_tex, 0)

        for eye_name in ['left', 'right']:
            gl.glBindTexture(gl.GL_TEXTURE_2D, textures[eye_name])
            gl.glBindVertexArray(vaos[eye_name])
            gl.glDrawElements(gl.GL_TRIANGLES, 6, gl.GL_UNSIGNED_INT, None)

        gl.glBindVertexArray(0)
        glfw.swap_buffers(window)
        frame += 1

        # FPS counter
        now = time.time()
        if now - t_last_fps >= 5.0:
            fps = frame / (now - t_last_fps)
            glfw.set_window_title(window, f"VR Diagnostic - {MODE_NAMES[current_mode]} - {fps:.1f} fps")
            frame = 0
            t_last_fps = now

    # Cleanup - skip explicit GL resource deletion (OS cleans up on exit)
    glfw.terminate()
    openvr.shutdown()
    print("Done.")


if __name__ == '__main__':
    main()
