# Experimental AEP import

File ▸ Open, File ▸ Import, dropped project files and desktop launch arguments accept binary
RIFX `Egg!` `.aep` files. The decoder is native Rust. Imported projects must be saved as `.ecproj`;
opening an AEP leaves the save destination unset, and saving to `.aep` or `.aepx` is rejected.

The importer reconstructs folders, compositions, dimensions, frame rates, rational durations,
layer start/in/out times, nested composition and footage references, solid dimensions/colors,
parent relationships, layer switches, blend modes, track mattes and stored transforms. Names need not be unique. Importing into an existing project
shifts item, layer and property IDs together, preserves references, and supports undo/redo.
Standard/custom editor views and the composition's marker stream are not imported as layers.
Windows binary aliases and JSON footage paths are recognized; local relative paths and a bounded
search near the project can relink available files. Missing media stays identifiable as footage.

Numeric animation preserves keyframe times, temporal interpolation/easing, spatial tangents and
expressions, including disabled expressions and separated position dimensions. The importer also
reconstructs vector groups, paths, masks, text documents/styles and supported text animators.
Layered PSD footage retains its source-layer selection. Supported effects are instantiated with
stored parameters and animation; unsupported effects and parameters are reported explicitly.

Rendering fidelity remains experimental: unsupported stream types, layer styles, markers and
parts of camera/particle behavior may differ. Particle World supports animated textured particles,
but its trajectories, sprite sizing and some 3D controls are still approximations. Check the rendered
result against the original.
The import report lists imported effect instances, missing media (grouped by file), missing fonts
and unsupported content. XML `.aepx` and little-endian RIFF project files return explicit
unsupported errors rather than constructing an empty placeholder composition.

## Input safety

The reader checks chunk bounds/padding, limits files to 64 MiB and trees to 1,000,000 chunks and
64 levels, and rejects invalid time denominators, non-finite transforms, duplicate source IDs,
cyclic precompositions and excessive composition nesting. Project reconstruction is limited to
8,192 input items and 4,096 layers. Decode failures leave the current document unchanged.
Text document `LIST/btdk` payloads are opaque data, not nested RIFF chunks.

## Verification and provenance

`crates/engine/tests/aep_import.rs` and `crates/ui-egui/tests/ui_aep_open.rs` use original synthetic
RIFX data built in Rust. They check source references, timing, transforms, path resolution,
malformed inputs, undo, source-file preservation, save/reopen and the import report UI.

For a local real project (never committed), run:

```sh
EFFECTCRAFT_AEP_TEST_FILE=/absolute/path/project.aep \
  cargo test -p effectcraft-engine --test aep_import external_aep_round_trip -- --ignored --nocapture
```

The implementation extends PR #251. Integration fixes were derived from the project's data model,
synthetic inputs and inspection of a user-provided project file, without consulting another
AEP decoder's source or Adobe application code. This is an experimental, observation-based
reader; it does not claim conformance to a published AEP format specification.
