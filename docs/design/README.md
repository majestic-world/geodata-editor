# Native editor redesign

The visual reference was generated in Higgsfield from the supplied screenshot,
using GPT Image 2.5 (job `9382c46c-a6ad-4158-a3c1-cc67983539ad`).
See [the generated concept](higgsfield-reference.png) and
[the implemented empty state](editor-empty-dark.jpg).

The Unreal Engine 5 inspiration is expressed through compact toolbars, graphite
surfaces, restrained blue selection, square-edged controls and a dominant 3D
viewport. The implementation remains native Rust/egui/wgpu.
The dark palette follows the supplied Unreal Engine 5 screenshot: neutral Slate
surfaces, lighter menus and muted blue selections. The light theme and semantic
geodata/NSWE colors remain unchanged.

| Role | Dark theme |
| --- | --- |
| Recessed surface / input | `#141414` |
| Panel | `#1A1A1A` |
| Toolbar | `#242424` |
| Inspector section | `#2B2B2B` |
| Menu / floating window | `#383838` |
| Inactive button | `#262626` |
| Divider | `#101010` |
| Primary text | `#C0C0C0` |
| Selected background | `#3E5F77` |
| Active accent | `#0070E0` |

Typography uses the bundled egui proportional font at 13 px for controls and
body text, 11 px for supporting text, and a 12 px monospace face for telemetry.
No operating-system fonts or additional runtime assets are required.

`src/editor_chrome.rs` owns the theme, 24-unit vector icon paths, vector NSWE
compasses and empty-state artwork. Icons are drawn directly through the egui
painter; the generated bitmap is a design reference, not an application skin.
Toolbar strokes have a 1.5-point minimum to retain coverage at small sizes.
Curved icons use Bézier paths, and closed contours use joined seams rather than
overlapping end caps. These paths scale with egui's DPI, including fractional
desktop scales; no raster icon cache or additional scene multisampling is needed.
Existing in-world NSWE textures retain their original rendering and bit mapping.

`src/editor_view.rs` connects the menus, toolbar, viewport switches and inspector
to the existing project and editing actions. Folder and file paths truncate
inside their fields and expose the full value on hover. The inspector resizes
and scrolls; viewport controls scroll horizontally when space is constrained.
Empty-project editing controls are disabled. The light theme remains available.
Project actions use the regular neutral button style, with hover and keyboard
focus feedback. Selected-button styling is reserved for persistent toggle states.

Project decoding, collision buffers, initial overlays and textured resources are
prepared in `src/editor_view/loading.rs` on a worker thread. The current scene
remains visible until the replacement is ready. `src/editor_view/overlays.rs`
owns the 16×16-block chunk cache: local changes reuse unaffected buffers, global
filters regenerate all chunks, and hidden NSWE glyphs are rebuilt lazily.

Visual batches preserve material blend/depth state, authored alpha cutoffs and
separate opacity inputs. Opaque/masked geometry is drawn before transparent
surfaces, which retain per-surface batches for back-to-front sorting. Encountered
pipeline variants and shared bitmap uploads are prepared on the same worker.
BSP visual visibility is independent of collision passability; invisible faces
and zone/sky portals are not rendered as ordinary textured polygons.

Texture uploads share a 16 MiB staging admission budget, accounting for padded
rows across every mip level. An oversized individual upload runs alone; a final
fence completes all pending uploads before publishing the scene.
Rebased batch bounds support conservative view-frustum rejection. Visible draw
order is cached until the camera/projection changes, preserving opaque-first and
back-to-front transparent order without rebuilding geometry or allocating per
frame. Adjacent draws reuse unchanged pipeline/material bindings.

The camera projection and picking share the same viewport rectangle, excluding
the menus, inspector and status bar. Unit tests cover the coordinate mapping at
100%, 125% and 200% scale and a surface smaller than its panels.

Validation: `cargo check --offline`, `cargo test --offline` (56 passed), and the
existing real-L2J integration test (1 passed, saving to a temporary file and
checking that the source bytes remain unchanged). Manual checks include empty
state, loading region 17_13, inspector scrolling, vector presets and both themes.
Picking on the rendered map selected cell 1087,1085 and exposed its two layers.
