# Rust Reimplementation Plan

## Non-negotiable constraints

- Keep the Python implementation unchanged and usable throughout the rewrite.
- Put all Rust implementation and Rust-specific documentation in this folder.
- Bind the FFmpeg libraries directly; never invoke `ffmpeg` or `ffprobe` as a
  subprocess from the production Rust application.
- Keep unsafe code inside `videoferry-ffmpeg`. All other crates forbid unsafe
  code.
- Support Windows 10/11 x64 and macOS Apple Silicon. Add Intel macOS artifacts
  after Apple Silicon parity unless release requirements change.
- Pin Rust crates and native FFmpeg builds. Upgrade them only through tested
  application releases.
- Keep the Rust crates unpublished until the FFmpeg/x264/x265 distribution
  configuration and repository license have been explicitly finalized.

## Definition of full parity

The Rust release is complete when it provides every user-visible converter
workflow and safety behavior currently present in the Python desktop app:

- Workflows: TV, Animation, Camera videos, Stabilize, Trim, Photo slideshow.
- Encoders: x264, x265, SVT-AV1, H.264/HEVC/AV1 NVENC, and
  H.264/HEVC/AV1 VideoToolbox where supported.
- MKV and MP4 output behavior, including `hvc1` tagging where required.
- Shared-lowest, source, and explicit FPS modes.
- Valid audio stream copying, unknown-stream skipping, and MKV DTS-to-AC3
  compatibility handling.
- Subtitle selection, sparse/bad subtitle rejection, compatible copying, and
  text subtitle conversion where required by the output container.
- Metadata preservation/removal rules and data-stream exclusion.
- Duration and output validation before originals are moved.
- `original/` backup, skip, retry, file-stability, and completed-folder rules.
- DJI camera/profile detection and bundled Action/Pocket LUT behavior.
- Two-pass stabilization with `vidstab` and documented `deshake` fallback.
- Inclusive trim behavior and output naming.
- Slideshow ordering, transitions, audio looping/fade, 1080p/4K output,
  chunking, collage layouts, review UI with drag reordering and a zoom viewer,
  lazy background thumbnails, and temporary-file cleanup.
- Persistent queue, recovery after restart, reordering, retry, folder watching,
  completed history, pause-after-current, stop-current, stop-all, and sleep
  prevention.
- Live progress, ETA, FPS/speed, input/output sizes, resolution, preview frames,
  diagnostics, taskbar state, and macOS Dock badge.

## Architecture

```text
videoferry-app
    |-- queue/settings/presentation state
    |-- native file dialogs and platform integration
    `-- worker command/event channels
             |
             v
videoferry-core <----- videoferry-presets
    |-- safe domain model
    |-- conversion control and queue state machine
    `-- MediaEngine trait
             ^
             |
videoferry-ffmpeg
    |-- RAII wrappers around AV* contexts
    |-- demux/decode/filter/encode/mux pipeline
    `-- runtime capabilities and version guard
```

The GUI thread never calls FFmpeg and the FFmpeg worker never mutates GUI state.
They exchange typed commands and events.

## Current delivery status

The default executable now uses Slint 1.17.1 for a consumer-style queue,
history, active-conversion bar, and scrollable settings drawer. It exposes all
six workflow setting groups, embeds Noto Sans CJK SC, and provides native
AccessKit names, invoke/toggle/range patterns, keyboard focus, and visible focus
treatment. The proven controller and FFmpeg worker remain in-process behind a
temporary presentation bridge. `videoferry-egui` is retained as a fallback
for photo review, live preview, process details, and diagnostic log surfaces
until those final views are redesigned in Slint. The old packaged UI Automation
smoke suite must be re-baselined to the new accessibility tree before the next
release bundle is promoted.

| Phase | Status | Remaining release work |
| --- | --- | --- |
| 0. Baseline and contracts | Complete | Final corpus results belong to Phase 9. |
| 1. Safe foundation and GUI shell | Complete | None. |
| 2. FFmpeg build and probe layer | Windows complete; macOS compile-verified | Reproduce and run the native macOS FFmpeg SDK on Apple Silicon. |
| 3. Remux and stream mapping | Complete | Repeat the fixture matrix on macOS. |
| 4. Software transcoding | Complete | Repeat the fixture matrix on macOS. |
| 5. Hardware acceleration | Hardware choices now require generic device initialization plus a successful direct one-frame probe for each codec. Windows H.264/HEVC/AV1 NVENC is runtime-verified across TV, Camera, Slideshow, and Stabilize; VideoToolbox is compile-verified with a native Apple Silicon matrix scripted for the same four workflows and all three codecs when individually proven | Execute the H.264/HEVC/AV1 VideoToolbox matrix on supported Mac hardware. |
| 6. Specialized media workflows | Complete on Windows; native Mac CPU/VideoToolbox runtime matrix scripted | Camera encoded-library/FPS skips are direct-bitstream and restart tested. Execute the end-to-end TV, Animation, Camera, Stabilization, Trim, and Slideshow matrix on macOS. |
| 7. Full desktop behavior | In progress | Python encoder-name/settings persistence (including legacy LUT/audio normalization), per-workflow/encoder quality memory, workflow-specific setting visibility and Trim time syntax, extended table selection, editable/validated aggregate folder and multi-target queues (including one-task batch admission from the GUI), duplicate ownership, separate stop-current/stop-all controls, video/photo folder watching, Python-style retry/failure/locked-input continuation, post-stability source metrics, live Python-compatible File #, engine-resolved Target FPS, frame-based/phase-aware percentage, pause-adjusted Spent/Remaining, mode-compatible media Time (including ordinary-video hundredths), Frame current/total (or current/?), Current FPS/File Size, and Camera Model/Applying LUT state, accurate completed-history LUT/numeric fields, durable already-encoded skips, `chs` exclusion and completed-folder renaming, collision-safe specialized publication, filesystem-derived counters, background review thumbnails, mouse-wheel photo zoom, graceful close cleanup, and workflow defaults are parity-tested. The packaged Windows app exposes a native 45-element startup UI Automation tree with required names, roles, keyboard focus, and interaction patterns; the same gate covers all six workflows and verifies dynamic control visibility and state. Its About view reports the exact app/Rust/FFmpeg versions and active license, while a live Processes view exposes the in-process worker/PID/source and an OS query proves no child process exists. The real locked two-file aggregate worker reports `2/2` and proves Pause, Resume, Pause-after-current, and persisted paused recovery state. Isolated Python-schema tasks prove packaged queue reordering/removal/clearing, History clearing, Run selected, completed-with-failure, error persistence, source/partial safety, rerun-to-pending, successful conversion, byte-identical `original/` backup, queue completion, and eleven-column History behavior. A real locked-input worker is force-terminated after its `running`/`was_running` marker reaches disk; an isolated relaunch then proves automatic resume, safe completion, backup/history publication, running-marker cleanup, and zero staging residue. Application-license finalization and human Windows/macOS screen-reader walkthroughs remain; platform indicators are complete. |
| 8. Packaging and upgrades | Windows has a manifest-driven, SHA-256-enforcing SDK installer/verifier plus a static Rust CRT/minimal six-DLL PE dependency gate, portable bundle, exact in-process packaged-runtime gate, isolated no-SDK launch, native UI Automation, packaged failure/success queue lifecycles, forced-interruption/restart recovery, explicit local Inno compiler selection, real per-user installer build, optional app/installer Authenticode signing with post-signature validation, and a separate-GUID/no-shortcut install-upgrade-runtime-uninstall lifecycle. The macOS source recipe and bundle builder are syntax/compile-verified; the latter now requires build/license records and audits the bundle, extracted ZIP, and mounted DMG for arm64 architecture, plist metadata, signatures, dylib closure, forbidden executables, resources, and exact in-process runtime health. | Execute Windows signing with the release certificate and a genuinely clean-machine lifecycle plus native execution of the macOS verifier, VideoToolbox fixtures, signing/notarization, and clean-machine tests. |
| 9. Parity and release gate | In progress | On this RTX 5080, the generated Windows corpus passes twenty-seven workflow/encoder gates: twenty-four Python/Rust output matches plus three verified Rust repairs for Python's broken Camera x264/SVT-AV1 commands and its `hev1` NVENC Camera output. Twelve gates exercise H.264/HEVC/AV1 NVENC across TV, Camera, Slideshow, and Stabilize. Every successful workflow asserts engine-resolved FPS/total-frame progress, with a separate real 24/12-FPS shared-folder gate proving the shared-lowest result; every stabilization gate also proves that phase-aware overall progress crosses 50% while second-pass media time restarts locally. Coverage also includes valid/sparse/malformed subtitles, chapters, VFR/HDR, multi-audio/collage and 41-photo chunked slideshows, exact text/TrueType/OpenType/binary attachment payload preservation, deterministic garbage/truncated/locked inputs, and test-only mid-mux storage exhaustion with clean partial-output handling. Run private/full media, real-world attachment variants, physical disk-full, and macOS matrices. |

Rust is pinned to stable 1.98.0. FFmpeg is pinned to 9.0.1 for packaged native
libraries and the Rust binding is pinned to the matching 9.0 series. These pins
move only through the upgrade and release checks below.

## Delivery phases

### Phase 0: Baseline and contracts

- Inventory presets, queue behaviors, file movement, stream mapping, filters,
  and platform packaging.
- Convert Python regression expectations into platform-neutral Rust contracts.
- Define representative tiny media fixtures without committing large files.

Exit criteria: every current behavior is mapped to a phase and test category.

### Phase 1: Safe foundation and GUI shell

- Create the four-crate workspace and forbid unsafe code outside the bridge.
- Implement queue lifecycle, progress events, pause/resume/stop state, settings,
  and preset identifiers.
- Create a responsive cross-platform GUI shell using Slint. Keep the earlier
  `eframe`/`egui` presentation as a temporary fallback during migration.

Exit criteria: workspace tests pass; the GUI opens on Windows and macOS and can
represent/reorder/remove queued work without performing conversion.

### Phase 2: FFmpeg build and probe layer

- Establish reproducible shared-library builds for Windows x64 and macOS arm64.
- Add exact-version generated bindings and RAII ownership wrappers.
- Implement library version checks and expose build/license configuration.
- Probe containers, streams, dispositions, tags, duration, FPS, dimensions,
  color/HDR fields, and DJI metadata directly through `libavformat`.
- Enumerate encoders, filters, muxers, and hardware configurations at runtime.

Exit criteria: probe results match `ffprobe` reference JSON for the fixture set;
the shipped application itself does not depend on `ffprobe`.

### Phase 3: Remux and stream mapping

- Create output contexts and copy codec parameters safely.
- Rescale packet timestamps and interleave output correctly.
- Copy all valid audio streams and skip unknown/bad streams.
- Implement DTS-to-AC3 handling for MKV.
- Map subtitles, attachments, chapters, and metadata according to each preset.
- Add required bitstream filters for container transitions.

Exit criteria: remux, remove-metadata, and trim-copy fixtures have correct stream
counts, timestamps, duration, language/default/forced flags, and playback.

### Phase 4: Software transcoding

- Implement decoder and encoder send/receive loops with complete flushing.
- Add pixel conversion, padding to even dimensions, and filter graphs.
- Implement x264, x265, and SVT-AV1 options and tune/quality mappings.
- Implement FPS policies, VFR/CFR timestamps, progress, cancellation, and retry.
- Validate output before atomically finalizing or moving the source.

Exit criteria: TV, Animation, and Camera software presets match the reference
stream layout and meet duration/timestamp tolerances.

### Phase 5: Hardware acceleration

- Detect and initialize NVENC on supported Windows/NVIDIA systems.
- Detect and initialize VideoToolbox on macOS.
- Implement hardware pixel-format and frame-transfer paths where needed.
- Report unavailable encoders before queue execution and offer software fallback.

Exit criteria: supported hardware paths pass the same semantic output tests;
unsupported systems fail clearly without losing source files.

### Phase 6: Specialized media workflows

- Port DJI model/D-Log detection and LUT selection.
- Implement trimming and two-phase stabilization.
- Implement slideshow image decode/orientation, layout scoring, collage render,
  transitions, chunking, concatenation, audio sequencing, fades, and review data.
- Generate preview frames directly from decoded frames.

Exit criteria: each specialized workflow has deterministic unit tests plus at
least one end-to-end fixture on both platforms.

### Phase 7: Full desktop behavior

- Complete add-file/folder, drag/drop, task editor, details, queue counters,
  history, process table, preview, log, and settings interfaces.
- Persist queue/settings/history with schema versions and atomic writes.
- Add folder watching with debounce and file-stability checks.
- Add pause, pause-after-current, resume, stop-current, stop-all, and crash
  recovery.
- Add sleep prevention, Python-compatible Windows title/taskbar state, macOS
  Dock badge, dark/light themes, accessibility labels, and keyboard navigation.

Exit criteria: GUI parity checklist passes on Windows and macOS, including
restart recovery during an active queue.

### Phase 8: Packaging and upgrades

- Bundle pinned FFmpeg DLLs in the Windows installer.
- Embed FFmpeg dylibs with correct `@rpath` values in the macOS app bundle.
- Store the FFmpeg source revision, configure flags, checksums, and license
  notices in a machine-readable engine manifest.
- Automate Windows signing and macOS Developer ID signing/notarization.
- Ship native-library upgrades only as complete, signed application releases.

Exit criteria: clean machines can install, convert, upgrade, and uninstall; the
About screen reports exact Rust/app/FFmpeg versions and licenses.

### Phase 9: Parity and release gate

- Run the Python and Rust implementations against the same corpus.
- Compare streams, codecs, durations, frame rates, dispositions, metadata,
  output names, backup behavior, and failure recovery.
- Fuzz/probe malformed media and run cancellation tests at every pipeline stage.
- Document known intentional differences and migration/rollback instructions.

Exit criteria: no unresolved data-loss risk or required parity gap; Python stays
available until a separately approved retirement decision.

## Test strategy

- Unit tests: time/FPS arithmetic, queue state, preset validation, timestamp
  rescaling, stream policies, output naming, collage grouping, and control state.
- Integration tests: generated short media fixtures and malformed-stream cases.
- Golden tests: normalized probe snapshots from Python and Rust outputs.
- Platform tests: CPU-only Windows/macOS on every change; NVENC and VideoToolbox
  on dedicated runners before release.
- Packaging tests: launch from installer/app bundle without PATH dependencies.
- Safety tests: cancellation, disk-full, invalid output, locked files, restart,
  partial temporary outputs, and library-version mismatch.

## Upgrade policy

- Pin the FFmpeg binding crate and every native library major version.
- Permit compatible patch/minor updates only after the full media matrix passes.
- Treat a native library major update as a bridge migration and application
  release, never as a hot-swapped DLL/dylib.
- Keep one previous signed release available for rollback.
- Update corresponding FFmpeg source/build/license artifacts with every engine
  change.
