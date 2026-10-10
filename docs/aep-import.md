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
Character runs preserve their fonts, sizes, fill/stroke, faux bold/italic, tracking, baseline
shift, horizontal/vertical scale, leading and All Caps/Small Caps without rewriting the source
string. Paragraph alignment and spacing are retained per paragraph. Font warnings refer to
the imported character styles, excluding unused editing defaults in the file's font tables.
Range selectors preserve percent/index units, Based On, Mode, Amount, shape, easing,
smoothness, enabled state and randomization controls. Animator keys remain in layer time,
including layers with negative start times and reversed playback. Randomize Order uses
EffectCraft's deterministic permutation; it does not reproduce After Effects' private RNG,
so individual letters can enter and leave in a different order.
Selector easing profiles and character-blur kernels are approximations; preserving their
controls does not imply pixel parity with After Effects. Missing fonts also change glyph
shapes and advances even when the imported sizes, tracking and positions are correct.
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
