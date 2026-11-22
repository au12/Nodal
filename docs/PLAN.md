# Nodal 0.2 build plan (working notes)

Goal: a complete panorama suite. Stitch overlapping photos into flawless 360° panoramas (PTGui-class
engine), patch pole gaps, then view/edit/publish them as tours (existing engine).

Status legend: [x] done · [~] in progress · [ ] todo. Update this file with every milestone commit so
work can resume from a cold start.

## Milestones
1. [x] `nodal-stitch` alignment: EXIF, lens models, SIFT features, matching, RANSAC, bundle adjustment,
   levelling, synthetic ground-truth tests
2. [x] Render: tiled warp, exposure/WB compensation, graph-cut seams, multi-band blending, output + XMP,
   `nodal stitch` CLI. Possible speed-up left: tight full-res ROI + wide low-res context per layer
3. [x] Local alignment: guided matching, sub-pixel refinement (`refine.rs`), warp fields (`local.rs`)
4. [x] Pole patching (`fill.rs`): thin-plate base + coherent multi-scale PatchMatch in a pole view,
   full-res re-sampling. Known weak case: complex man-made structure overhead (station roofs)
5. [x] GPU: live stitch preview compositor (`nodal-render/src/compose.rs`). GPU warp/blend for the
   final render is still open (CPU path is multi-threaded)
6. [~] App: stitch workspace (`nodal-app/src/stitcher.rs`) with live preview, lens/alignment/output
   panels, Save panorama and Create tour. Still to do: project save/open, mask painting, circular
   fisheye crop detection, server-side stitching for the Docker image
7. [x] Docs, screenshots, changelog, version 0.2.0 (tag `v0.2.0` must be pushed by the owner)

## Conventions
- Camera frame: +X right, +Y up, +Z forward; image x right, y down. `rotation` maps camera → world.
- Pixel coordinates in projects are always full-resolution, oriented (EXIF applied).
- Lens: PanoTools-style radial polynomial `r_src = r(a r³ + b r² + c r + 1-a-b-c)`, r normalised by
  half the shorter side; `d`,`e` principal-point shift in pixels.

## Test data
- Synthetic: `nodal_stitch::synth` renders camera shots from Poly Haven equirects with known lenses.
- Real: OpenCV `opencv_extra/testdata/stitching` sets (boat, budapest, newspaper, a/s series).
