# Nodal roadmap

This is where Nodal is headed. Items are roughly in priority order within each milestone, and the
plan will change as real use shows what matters. Ideas and requests are welcome in
[issues](https://github.com/au12/Nodal/issues).

**Legend:** ✅ shipped · 🚧 in progress · ⏳ planned · 💡 idea

## ✅ 0.1: Foundation (shipped)

- [x] **GPU renderer.** One draw call per frame with per-pixel inverse projections (rectilinear,
      fisheye, stereographic/little planet, Pannini, architectural, flat) and smooth morphing between them
- [x] **Virtual texturing.** Atlas plus per-face page table, derivative-based level of detail with
      trilinear blending, LRU eviction and prioritized streaming
- [x] **Tiler.** Bordered multi-resolution cube pyramids with real neighbour pixels across faces,
      linear-light downsampling, pole-aware filtering and partial panoramas (`--hfov`/`--vfov`/`--voffset`)
- [x] **Tours.** Point and polygon hotspots, zoom-through transitions, info cards, links, look-at,
      a compass, auto-rotation and deep links
- [x] **Editor.** Place, draw and move hotspots, undo/redo, start view and north, scene order. Works
      in both the native app and the browser
- [x] **Native app.** Linux, Windows and macOS. Apple Silicon ships as `Nodal.app` in a DMG
- [x] **Web viewer.** WebGPU with a WebGL 2 fallback, touch and pinch, served precompressed (2.2 MB)
- [x] **Server.** Tour library, streamed uploads, background tiling, token auth, read-only mode and a
      health check
- [x] **Unraid and Docker.** A 76 MB multi-arch image on GHCR plus an Unraid template
- [x] **CLI.** `tile`, `render` (headless GPU stills) and `info`
- [x] **CI.** fmt, clippy, tests including the GPU-vs-CPU PSNR comparison, an end-to-end Docker
      smoke test and the macOS app build

## ⏳ 0.2: Distribution and polish

- [ ] **Notarized Mac builds.** The workflow is ready and needs Developer ID secrets. This removes the
      first-launch Gatekeeper step
- [ ] **macOS integration.** Finder *Open With*, dropping onto the Dock icon, file associations for
      panoramas and tour folders, and a `nodal://` URL scheme
- [ ] **Recent tours** list and a "reveal in Finder/Explorer" action on the native start screen
- [ ] **Windows installer** (MSIX or MSI) with code signing, and a GUI build that opens without a console window
- [ ] **Linux packages:** AppImage and Flatpak
- [ ] **Unraid Community Applications** listing
- [ ] **Faster image builds** with cargo-chef dependency caching
- [ ] **Read GPano XMP metadata** (Photo Sphere) to auto-detect partial panoramas and the initial view
- [ ] **Gyroscope look-around** on phones and tablets, with a permission prompt on iOS
- [ ] **Accessibility:** keyboard focus for hotspots, screen-reader labels and reduced-motion transitions

## ⏳ 0.3: Sharing and embedding

- [ ] **Embed API.** An `<iframe>` snippet generator plus a `postMessage` API (set the view, go to a
      scene, receive events)
- [ ] **Share links** with optional per-tour passwords and expiry
- [ ] **Static export.** A self-contained folder or zip that runs on any static host (GitHub Pages,
      S3, Netlify) with no server
- [ ] **Accounts.** Per-user tokens and roles on the server, or OIDC via Authelia/Authentik
      instead of one shared token
- [ ] **Immich import.** Pull equirectangular photos from an Immich album into a tour
- [ ] **Bulk operations.** Tile a whole folder, re-tile at a new quality, and duplicate or merge tours

## ⏳ 0.4: Richer media

- [ ] **GPU-compressed tiles** (KTX2 + Basis Universal → BC7/ASTC/ETC2). These use less VRAM, upload
      faster and allow larger atlases on phones
- [ ] **Smaller web tiles** with AVIF/WebP and per-browser negotiation
- [ ] **HDR input** (EXR, Radiance HDR, 16-bit TIFF, AVIF) with tone mapping and exposure control
- [ ] **Video panoramas.** 360° MP4/WebM decoded straight into the projection shader
- [ ] **Floor plans and maps** with a "you are here" radar and scene pins
- [ ] **Audio.** Ambient audio per scene and positional audio hotspots
- [ ] **Hotspot styling.** Custom icons, colours and themes per tour

## 💡 Ideas and research

- [ ] **WebXR / VR headsets** with stereo rendering from the cube pyramid
- [ ] **Gigapixel flat images** (deep zoom for non-360 images) reusing the pyramid and streamer
- [ ] **Depth-aware transitions** that use estimated depth maps for parallax between neighbouring scenes
- [ ] **Measurement and annotation tools** for real-estate and inspection tours
- [ ] **Importers for open viewer formats** such as Pannellum and Marzipano configs
- [ ] **Privacy-friendly analytics,** such as per-scene view counts stored on your own server

## Known limitations

- Mac builds from CI are **ad-hoc signed** until Developer ID secrets are added, so the first launch
  needs *Open Anyway* in System Settings → Privacy & Security (or right-click → Open on macOS 14 and earlier).
- The macOS app is built for **Apple Silicon only**. An Intel or universal build can be added if there's demand.
- On macOS you can drop files onto the **window**, but dropping onto the Dock icon and Finder *Open With*
  aren't wired up yet.
- The server has **one shared token**, with no per-user accounts yet.
- On **Windows** the `.exe` is also the command-line tool, so a console window opens alongside the app.
- Panoramas must be **stitched equirectangular** images (full or partial).

## Not planned

- **Stitching** raw photos into panoramas. Hugin, PTGui and camera apps already do this well, and
  Nodal starts from their output.
- **Reading proprietary viewers' project files.** Nodal is a clean-room project.
