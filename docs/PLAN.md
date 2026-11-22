# Nodal 0.2 build plan (working notes)

Goal: a complete panorama suite. Stitch overlapping photos into flawless 360° panoramas (PTGui-class
engine), patch pole gaps, then view/edit/publish them as tours (existing engine).

Status legend: [x] done · [~] in progress · [ ] todo. Update this file with every milestone commit so
work can resume from a cold start.

## Milestones
1. [~] `nodal-stitch` alignment: EXIF, lens models, SIFT features, matching, RANSAC, bundle adjustment,
   levelling, synthetic ground-truth tests
2. [ ] Render: tiled warp, exposure/WB compensation, graph-cut seams, multi-band blending, output + XMP,
   `nodal stitch` CLI
3. [ ] Local alignment: guided matching, smooth per-image warp fields
4. [ ] Pole patching: PatchMatch fill in pole-centred view
5. [ ] GPU: live stitch preview shader, GPU warp path
6. [ ] App: unified UI, stitch workspace, project files, send-to-tour
7. [ ] Docs, screenshots, changelog, version 0.2.0

## Conventions
- Camera frame: +X right, +Y up, +Z forward; image x right, y down. `rotation` maps camera → world.
- Pixel coordinates in projects are always full-resolution, oriented (EXIF applied).
- Lens: PanoTools-style radial polynomial `r_src = r(a r³ + b r² + c r + 1-a-b-c)`, r normalised by
  half the shorter side; `d`,`e` principal-point shift in pixels.

## Test data
- Synthetic: `nodal_stitch::synth` renders camera shots from Poly Haven equirects with known lenses.
- Real: OpenCV `opencv_extra/testdata/stitching` sets (boat, budapest, newspaper, a/s series).
