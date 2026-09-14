# Native editor redesign

The visual reference was generated in Higgsfield from the supplied screenshot,
using GPT Image 2.5 (job `9382c46c-a6ad-4158-a3c1-cc67983539ad`).
See [the generated concept](higgsfield-reference.png) and
[the implemented empty state](editor-empty-dark.jpg).

The Unreal Engine 5 inspiration is expressed through compact toolbars, graphite
surfaces, restrained blue selection, square-edged controls and a dominant 3D
viewport. The implementation remains native Rust/egui/wgpu.

| Role | Dark theme |
| --- | --- |
| Recessed surface | `#17191D` |
| Panel | `#22252B` |
| Input / inactive button | `#2E323A` |
| Divider | `#363B44` |
| Primary text | `#DEE2E9` |
| Active accent | `#4A9EFF` |

Typography uses the bundled egui proportional font at 13 px for controls and
body text, 11 px for supporting text, and a 12 px monospace face for telemetry.
No operating-system fonts or additional runtime assets are required.

`src/editor_chrome.rs` owns the theme, 24-unit vector icon paths, vector NSWE
compasses and empty-state artwork. Icons are drawn directly through the egui
painter; the generated bitmap is a design reference, not an application skin.
Existing in-world NSWE textures retain their original rendering and bit mapping.

`src/editor_view.rs` connects the menus, toolbar, viewport switches and inspector
to the existing project and editing actions. Folder and file paths truncate
inside their fields and expose the full value on hover. The inspector resizes
and scrolls; viewport controls scroll horizontally when space is constrained.
Empty-project editing controls are disabled. The light theme remains available.

The camera projection and picking share the same viewport rectangle, excluding
the menus, inspector and status bar. Unit tests cover the coordinate mapping at
100%, 125% and 200% scale and a surface smaller than its panels.

Validation: `cargo check --offline`, `cargo test --offline` (56 passed), and the
existing real-L2J integration test (1 passed, saving to a temporary file and
checking that the source bytes remain unchanged). Manual checks include empty
state, loading region 17_13, inspector scrolling, vector presets and both themes.
Picking on the rendered map selected cell 1087,1085 and exposed its two layers.
