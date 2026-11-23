# Changelog

All notable changes to Nodal are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and Nodal uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.3.0] - 2026-11-23

The stitching engine, rebuilt. RAW in, 16-bit out, every photo placed, and alignment measured against
ground truth instead of trusted.

### Added

- **RAW, TIFF and PSD input.** RAW from most cameras through FlashRaw (CR3, CR2, NEF, ARW, RAF, DNG,
  ORF, RW2…), 8/16/32-bit TIFF including BigTIFF, Photoshop PSD/PSB. Embedded ICC profiles are
  honoured; RAW frames of a set share one white balance
- **Scene-linear float pipeline** end to end, blending in a perceptual space, and **16-bit output**:
  TIFF (BigTIFF when large), PNG and PSD/PSB, with an sRGB ICC profile and GPano metadata
- **Fisheye projection calibration.** The projection factor (equidistant, equisolid, stereographic or
  in between) is measured from the photos, so the lens no longer has to be named
- **Learned rescue matching** (desktop app and CLI): RaCo-ALIKED keypoints with LightGlue on ONNX
  Runtime for photos SIFT cannot connect (low texture, cloudy sky, repeated patterns). Models (65 MB)
  download once from pinned revisions, checked by SHA-256. `--no-learned` and a workspace switch turn
  it off
- **Global placement** by robust rotation averaging with cycle checks, and a focal-length search that
  closes 360° rings
- **Dense overlap registration.** Every overlap is measured densely and sub-pixel after the global
  solve; it feeds a second adjustment, local warps for hand-held parallax, and the overlap check in the
  workspace
- **Photometric calibration:** exposure, white balance and vignetting solved jointly from the overlaps
- **Ghost masks.** Moving people and shadows are found by consensus and kept out of seams and blending
- **Void filling** for any gap between photos, not just the poles, with a *Fill voids* switch to leave
  them transparent instead
- **Featureless photos** (clear sky, blank walls) are placed from the shooting pattern where they fill
  a gap, and flagged
- **Workspace:** residual heat map on the preview, per-photo agreement dots, the weakest links by
  name, a mask painter, and read-outs for the overlap check, learned matches, pattern-placed photos and
  exposure match
- **`nodal bench --suite`:** an accuracy suite scored by true misregistration against ground truth
  (fisheye families the solver is not told, a low-texture sphere, a partial panorama), with `--json`

### Changed

- The Mac app needs macOS 13.4 or later (the learned matcher's ONNX Runtime is built for it)
- Lens calibration stages are kept only when they lower the error on held-out control points
- Full-resolution photos are streamed from disk during rendering; RAWs are decoded at half size when
  the output cannot use more
- Pruning outliers never splits the panorama into disconnected groups
- Photos are ordered by capture time, then by file name with numbers compared as numbers

### Fixed

- Dense measurements taken against a rough model were biased towards it; feature points are now
  refined to sub-pixel first (fisheye test set: true misregistration 2.1 px → 0.03 px median)
- The ring check behind the focal-length search failed when the reference photo pointed steeply up or
  down, and its clarity test was swamped by photos outside the group
- Output ICC profiles were rejected by some readers (libpng warnings); they are now hand-built ICC v2
- The Mac app failed to link ONNX Runtime's Core ML code (missing clang builtins)

## [0.2.0] - 2026-11-22

Nodal now stitches. Drop in overlapping photos and get a finished panorama or tour: alignment, lens calibration, blending and pole patching are automatic.

### Added

- **Stitching engine** (`nodal-stitch`). Works with rectilinear and fisheye lenses, single-row, multi-row and full-sphere sets, in any order
- **Automatic lens detection.** Lens type and field of view from EXIF, or estimated from the photos when EXIF is missing
- **Sub-pixel alignment.** Feature matching, geometric verification, full-resolution refinement and a bundle adjustment that calibrates field of view, distortion (a, b, c) and lens shift
- **Local alignment.** Smooth per-photo corrections for parallax and lens quirks, so matched points coincide
- **Exposure and white-balance compensation,** minimum-cut seam placement and multi-band blending
- **Pole patching.** Zenith and nadir gaps are filled automatically, and a nadir patch can paint out a tripod
- **Stitching workspace** in the native app: photo list, lens and alignment read-outs, control point table, output settings, *Save panorama* and *Create tour*
- **GPU live preview.** The panorama is re-projected on the GPU while you drag to re-centre or level it
- **Project files** (`.nodal`). Save a stitch from the app or with `--project`, and reopen it later with its alignment intact
- **`nodal stitch`** command, with `--tour` to go from photos to a tour in one step
- Panoramas are saved with Photo Sphere (GPano) metadata, and the tiler reads it, so partial panoramas keep their true field of view in tours
- Partial panoramas are trimmed to a clean rectangle
- **`nodal bench`** stitches a built-in synthetic photo set and prints timings and accuracy for the machine it runs on
- The stats overlay (**I**) shows how many frames were drawn in the last second

### Changed

- Dropping several photos that are not 2:1 panoramas opens the stitching workspace instead of creating a tour
- The start screen offers *Stitch photos…* first
- Idle auto-rotation draws at 30 fps, pauses while the window is in the background, and stops after three minutes without input. `autorotate.max_seconds` in a tour sets the limit; `0` rotates forever, for kiosks

### Fixed

- **Battery drain.** The viewer redrew at the display's full refresh rate for as long as a tour auto-rotated, even in the background. It now draws only when something on screen changes

### Known limitations

- Stitching runs in the native app and CLI only, not yet in the web or Docker UI.
- Photos are processed as 8-bit RGB; RAW files and exposure brackets are not merged yet.
- Pole patches are synthesised. Sky, pavement, grass and floors look natural, but complex structure directly overhead can look invented.
- The whole photo set is held in memory while stitching.

## [0.1.0] - 2026-11-21

The first release of **Nodal**: a fast, GPU-accelerated 360° panorama viewer, tiler and virtual-tour editor. It ships as a native app, a WebGPU web viewer and a self-hosted Docker server, all from one Rust codebase.

### Added

- **Renderer.** One draw call per frame with exact per-pixel projections (rectilinear, fisheye, little planet, Pannini, architectural, flat) that morph smoothly into each other
- **Virtual texturing.** Multi-resolution cube tiles stream into a GPU atlas with seamless trilinear level of detail
- **Tours.** Point and polygon hotspots, zoom-through transitions, info cards, links, look-at, auto-rotation, a compass and deep links
- **Editor** in both the native app and the browser, with undo/redo
- **Tiler.** Bordered cube pyramids, linear-light downsampling and partial panoramas. An 8K panorama tiles in about 3 s on two cores
- **Command line.** `nodal tile`, `nodal render` (headless GPU stills) and `nodal info`
- **Web viewer.** WebGPU with a WebGL 2 fallback, touch and pinch, 2.2 MB with brotli
- **Server.** Tour library, streamed uploads, background tiling, token auth, read-only mode and a health check
- **Docker image** for linux/amd64 and linux/arm64 (76 MB) with an Unraid template
- **Apps.** `Nodal.app` for Apple Silicon in a DMG, plus Linux and Windows binaries

### Known limitations

- The Mac build is ad-hoc signed rather than notarized and targets Apple Silicon only. It doesn't yet accept files dropped on the Dock icon or opened with Finder *Open With*; dropping onto the window works.
- On Windows the `.exe` is also the command-line tool, so a console window opens alongside the app.

[Unreleased]: https://github.com/au12/Nodal/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/au12/Nodal/releases/tag/v0.3.0
[0.2.0]: https://github.com/au12/Nodal/releases/tag/v0.2.0
[0.1.0]: https://github.com/au12/Nodal/releases/tag/v0.1.0
