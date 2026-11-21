# Changelog

All notable changes to Nodal are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and Nodal uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

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

[Unreleased]: https://github.com/au12/Nodal/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/au12/Nodal/releases/tag/v0.1.0
