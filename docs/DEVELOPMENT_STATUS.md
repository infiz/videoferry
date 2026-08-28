# VideoFerry development status

VideoFerry began as a Rust reimplementation of the HomeLab Video Converter.
It remains intentionally isolated from the existing Python implementation. The
Python source remains the behavioral reference until the Rust application has
passed the parity checks documented in `IMPLEMENTATION_PLAN.md`.

## Workspace

- `crates/converter-core`: safe domain types, queue, control state, events, and
  the media-engine interface.
- `crates/ffmpeg-bridge`: the only crate allowed to contain unsafe FFmpeg FFI.
- `crates/presets`: cross-platform preset catalog and validation.
- `crates/app`: the shared Windows/macOS Slint desktop application. A temporary
  `videoferry-egui` fallback binary remains available while the last
  diagnostics and photo-review surfaces are migrated.

## Current status

The workspace establishes the ownership boundaries, queue/control model,
preset catalog, and a consumer-oriented Slint GUI. The main window uses a
cinematic dark surface, simple queue/history navigation, media-style queue
cards, a docked compact/expandable active-conversion panel, and a settings drawer
instead of a dashboard sidebar. Finished conversions can be searched by name or
path and opened, revealed in the file manager, or copied directly. New work
follows the Python sequence: create a task, choose its type
and conversion settings, then add and reorder files or folders before creating
one aggregate queue item. Files and folders can be selected with the picker
buttons or dropped directly onto the task builder's media box on Windows and
macOS. Before creation, the builder shows the exact output plan, source size when
available, and whether the original will move only after validation. With the
`native-ffmpeg` feature enabled,
the bridge now initializes the bundled FFmpeg libraries, reports exact library
versions and licenses, tests hardware-device availability, and probes container
and stream information directly through `libavformat`. Hardware encoders are
advertised only after a direct one-frame encoder open/send/flush probe succeeds;
a generic CUDA or VideoToolbox device alone is not treated as codec support.

The direct packet pipeline supports Trim with selected-stream remuxing,
timestamp repair, cancellation, progress, temporary output, validation, and
atomic publication. Existing named Trim clips are replaced like Python using
a rollback-safe same-folder publish; Stabilize and Slideshow preserve an output
that appears after scheduling. Ordinary converter targets also replace an
existing different-extension destination like Python, while preserving it for
rollback until the new output is published. DTS audio is decoded and re-encoded as 640 kb/s AC-3 for
Matroska. Text subtitles are converted between SRT and `mov_text` when the
destination container requires it. Unknown audio and sparse/bad subtitle
streams are excluded by the shared stream policy. Matroska attachments are
copied, data streams are excluded, and chapters are preserved or clipped and
shifted for trimmed output. Container and stream metadata follow the selected
preserve/remove policy.

The direct video pipeline supports x264, x265, and SVT-AV1 for TV and Animation
workflows. It uses a native filter graph for pixel conversion and true even-size
padding, preserves color characteristics, tags HEVC as `hvc1` in MP4, flushes
both codec directions, and implements source, explicit, and non-recursive
shared-lowest FPS policies. The GUI can queue individual files or recursively
add a folder for these workflows and exposes shared-lowest/source/explicit FPS,
software CRF/speed, SVT-AV1 preset, and NVENC preset controls. It runs FFmpeg
off the UI thread and moves a validated source into `original/` before
publishing the converted file with rollback protection.

Available NVENC encoders are exposed only after CUDA device initialization and
a real per-codec encode probe, then run through the same direct
frame/filter/mux pipeline. H.264, HEVC, and AV1 NVENC have passed Windows decode
fixtures. VideoToolbox uses the same per-codec probe and still needs native
macOS validation.

Camera Videos can detect the supported DJI Action 6 and Pocket 3 metadata and
apply the matching bundled LUT through `libavfilter`. Stabilize implements the
Python-compatible Gentle through Maximum strengths, native two-pass `vidstab`
analysis/transform processing, a `deshake` fallback, 50/50 phase progress,
source-extension output naming, metadata preservation, and temporary transform
cleanup. Stabilized sources are not moved into `original/`.

Camera queue workers also match Python's already-encoded-source rule. The
direct FFmpeg bridge reads x264, x265, and SVT-AV1 encoder signatures from
container metadata or early video packets, verifies the selected FPS policy,
and records the source as a durable skip instead of transcoding it again.
Skipped paths survive queue restart and are treated as converted by aggregate
folder counters.

Photo Slideshow now has an executable native baseline: the GUI accepts a
recursive photo folder or an explicitly selected image set, applies natural
filename ordering, exposes interval/FPS/1080p/4K controls, decodes every image
through FFmpeg, letterboxes mixed aspect ratios, uses the same string-seeded
Python random sequence for the 20 transition choices, emits CFR frames, and
validates decoded frame count, duration, resolution, frame rate, and codec
before publishing MP4. Multiple selected audio tracks are decoded directly,
normalized to stereo 48 kHz, padded by 1.5 seconds per track, concatenated,
looped to the slideshow duration, faded out, and encoded as 192 kb/s AAC.
Portrait collage mode now mirrors the Python grouping and layout selection,
renders every composite directly in FFmpeg video frames, preserves aspect ratio
and balanced gaps for row layouts, and validates the resulting video without
creating temporary collage images. FFmpeg display matrices are also applied for
all eight EXIF orientation values before grouping, fitting, or compositing.
All 20 Python transition names have native implementations; pixelize and hblur
use the FFmpeg sliding-block and sliding-window formulas instead of fade
approximations. Large non-collage shows retain Python's 40-photo threshold,
30-photo transition seed boundaries, and continuous timing while keeping only
the current and previous decoded slide in memory. The temporary egui fallback's photo-review modal
supports inclusion, exclusion, button or drag-and-drop reordering, and
EXIF-correct previews decoded directly through FFmpeg. A double-click or the
Open Full Size button opens an in-app viewer with fit, actual-size, zoom, and
drag-to-pan controls; the mouse wheel zooms the image like the Python viewer.
Visible photo and generated-slide rows receive lazy
native thumbnails; all review decoding runs on a priority background worker,
and stale results are discarded when selection or review state changes. Its
Slides tab recalculates the converter's actual single-photo or collage groups
and previews the selected composite with the same native layout renderer used
for export. Completed or partially converted slideshows remain reviewable, but
their inclusion and ordering controls become
read-only, matching Python's task-details safety boundary.

FFmpeg capability discovery also runs after the first GUI paint on a one-shot
background worker. Conversion, probing, thumbnails, and review rendering never
block the GUI thread.

The GUI persists per-workflow settings and the ordered queue in Rust-owned
`settings.json` and version-2 `queue.json` files. Writes are flushed to disk and
published with a recoverable backup swap. On restart, an interrupted running or
paused task becomes pending and the remaining queue resumes automatically;
completed tasks retain their state. The Rust state lives under
`%LOCALAPPDATA%/VideoFerry` on Windows and
`~/.local/share/VideoFerry` on macOS. It does not read or
write the Python application's state files.
Python's `x264`/`x265` encoder names remain accepted in the Rust schema
(while `libx264`/`libx265` remain the internal FFmpeg names), and a restored
Python task containing several target paths remains one queue task while its
media files are processed once in natural order. Legacy Camera software
settings with no `apply_lut` field retain Python's enabled default, hardware
and non-Camera tasks force it off, and missing slideshow soundtracks are
dropped during restore like Python. Each launch opens on TV while retaining
the saved per-workflow settings; CRF and preset edits are remembered separately
for each workflow/encoder combination during the session without changing FPS
when the encoder changes.

Queue controls now match the Python lifecycle: active work can be paused or
resumed, the queue can pause after the current item, Stop current cancels only
that item and advances, Stop all restores the interrupted item to Pending,
removing the active item stops it before advancing, and completed/failed work
can be rescanned for retry or rerun. Ordinary video conversion failures receive
Python's second attempt after a cancellation-aware three-second delay, then are
counted as failed files under a completed task so the queue can continue;
slideshow-level and engine-unavailable failures stop the queue. Partial native
outputs are removed on cancellation so retry remains safe. Explicit Windows
sharing-violation errors remain pending and retry at one-second intervals until
the input unlocks or the user stops the worker, matching Python's quiet-folder
behavior. Pending tasks can be
renamed, inspected, loaded into the settings editor, and updated in place.
Their file/folder targets can also be added, removed, and reordered with the
same duplicate and Trim restrictions as Python; slideshow target edits rebuild
the natural photo order and invalidate the previous review selection. Initial
batch file selections stay together as one editable aggregate task, while
adds and edits reject duplicate target ownership, multi-file Trim tasks, and
targets incompatible with the selected workflow. The task table supports
Ctrl/Cmd toggles and Shift ranges; Remove selected and Retry/rerun selected act
on the full selection, while move, edit, review, and run-selected remain
single-task operations.

Video and photo-folder tasks are watched for newly arriving media until the
queue reaches a quiet boundary. Each watched folder remains one selectable
queue task; per-file conversions, history rows, failures, retries, and counters
stay aggregated beneath it. A watched photo folder remains one slideshow job,
while individually selected photos remain an explicit set. Sources must
remain unchanged in size and modification time for one second before conversion
starts. Source history metrics and processing time begin after that stability
boundary. Restored folder tasks discover files that arrived while the application
was closed, completed encoder-suffixed folders
are excluded from recursive scans, and `original/` backups or existing
Stabilize targets prevent duplicate conversion. Folder task rows expose the Python
Targets/Folders/Files/Remaining/Converted/Failed counters and deduplicate a
completed file from its `original/` backup.
The Python `chs` directory exclusion is retained. After eligible TV or
Animation work completes, directories whose direct media count matches their
`original/` backups are renamed with the converter-specific suffix, deepest
first; `0_` directories and HEVC NVENC retain their original names exactly as
in the Python converter classes.
Closing the window during active work first cancels the native worker, waits for
partial-output cleanup, persists the interrupted item as pending, and only then
closes the application.

Completed-history rows use the Rust-owned
`completed_history.json` 14-column format and lock. Each
row includes original/source FPS, actual target/output FPS, encoder, CRF quality,
and preset in addition to the
conversion timing and media details. Older Rust 8/9/10/11-column rows are
normalized, writes are merged and deduplicated, and external changes refresh
in the GUI every second. Size and FPS values retain Python's
rounding/general-number display, and Camera rows name only the LUT actually
applied to that file. System-sleep prevention is enabled by default while unpaused work is
active, released immediately when conversion is paused or idle, and reacquired on resume.
It uses native Windows power requests and native macOS IOKit assertions,
allows display sleep, and never launches a helper process.

Live status includes the source folder/file, Python-compatible File # progress
for aggregate video tasks and slideshow photos, original size/resolution/FPS,
detected Camera Model and Applying LUT fields, target-FPS
policy before work starts and the engine-resolved numeric Target FPS afterward,
Python-compatible frame-based percentage (phase-aware for stabilization while
its media Time remains local to the active pass), pause-adjusted Spent and
Remaining time, media Time
(`HH:MM:SS.hh/total` for ordinary video and Python's compact clock for slideshows),
Frame (`current/total` or `current/?`), Current FPS,
speed, Current File Size,
estimated output size, and approximate
size per minute. Source probing happens on the worker thread. The window title
marks an active conversion. The temporary egui fallback retains the copyable
activity log and detailed diagnostic views while those surfaces are redesigned
for Slint.
The Slint converting surface shows file progress and essential controls in a
compact docked panel. Its Details control expands media position and total
duration, processing FPS and speed, original and target FPS, encoder, quality,
preset, carried audio/subtitle streams, pause-adjusted spent time, estimated total
time, and remaining time without blocking queue management. Pause-after-this-file
has a visible armed/cancel state, Stop all requires confirmation, and queue
completion produces both an in-app summary and a best-effort native notification.
Failed cards expose their error and a copy action instead of relying on a transient
status line.
Each task card keeps a persistent three-part file-count bar: work completed before
the current run, work completed during the current run, and unfinished work. The
bottom conversion bar is deliberately separate and reports only the active file's
in-file progress.
Pending task cards can be dragged to a new queue position, including while another
task is converting. Dragged order is persisted immediately; Move up and Move down
remain available as keyboard-accessible alternatives.
Its optional live frame preview samples the in-flight native decode pipeline once
per media second and transfers a small RGBA image directly to the GUI. Slideshow
audio is an ordered multi-track list with add, remove, move, and clear controls.
The fallback Processes view reports the in-process native worker, application PID,
pause-aware runtime, and target. Queue rows preserve and display Python-compatible
queued and completion timestamps.

The Slint GUI uses a cinematic dark theme and builds a native AccessKit
accessibility tree. Buttons and navigation use keyboard focus with visible
focus treatment,
editable numeric/text controls are explicitly associated with their visible
labels, and live/review images provide screen-reader alternative text. Workflow
specific controls mirror the Python dialog: Trim hides encoder/quality/FPS
controls, accepts validated `MM:SS` or `HH:MM:SS` inclusive ranges, and
Stabilize hides FPS while retaining encoder quality controls. Native
screen-reader walkthroughs on Windows and macOS remain part of the release gate.

Portable Windows packaging is smoke-verified, its SDK archive has a
manifest-driven checksum installer/verifier, and the per-user Inno installer
has passed an isolated install/upgrade/runtime/uninstall lifecycle under a test
identity. Optional Authenticode hooks sign and verify both the application and
installer when a certificate thumbprint, timestamp URL, and `signtool.exe` are
provided; executing those hooks with the release certificate and a genuinely
clean-machine lifecycle remain release gates. Windows uses a statically linked
Rust CRT, and every package build audits the PE dependency closure to require
exactly the six in-process FFmpeg DLLs and no external `VCRUNTIME`/`MSVCP`
dependency. The macOS bundle scripts and Apple Silicon dependency
graph are compile-verified and await a native Mac signing/runtime pass. The
generated red play-and-convert mark is embedded in the Windows executable and
installer, assigned to the Slint window, and packaged as the macOS bundle icon.
About view reports the application version, pinned Rust toolchain, exact
runtime FFmpeg library versions/license, and embedded engine manifest. Active
conversions mirror the Python title/taskbar indicator on
Windows and show the same triangle badge on the macOS Dock. Application-license
finalization and native screen-reader walkthroughs remain release work. The
application never falls back to an FFmpeg subprocess.

## Development

```text
cargo test --workspace
cargo run -p videoferry-app
```

Native FFmpeg development additionally requires `FFMPEG_DIR` to point to a
compatible shared-library SDK and `LIBCLANG_PATH` to point to `libclang`. Enable
the bridge with `--features native-ffmpeg`. Packaged applications will set up
their bundled libraries automatically and will not require these variables.

No crate in this workspace may launch `ffmpeg` or `ffprobe` as a subprocess.

On Windows, run `testing/windows-parity.ps1` to generate a temporary reference
corpus, execute the Python and direct-library Rust implementations, compare
normalized media structure across fifteen CPU gates plus twelve optional NVENC
gates (including a 41-photo chunked slideshow), verify attachment payload
bytes, assert resolved FPS/total-frame progress for every workflow (including a
real 24/12-FPS shared-lowest folder), verify that stabilization crosses 50%
without offsetting its second-pass media clock, and exercise failure cleanup.
NVENC gates
also require the first stabilization-analysis event to carry measured Current
FPS and Speed. Hardware gates are enabled only for codecs that pass a real
hardware probe. The parity runner alone enables a non-default
test feature that simulates storage exhaustion after real mux writes;
production builds do not contain that fault injector. See
`PARITY_NOTES.md` for the current matrix and intentional safety differences.

## Release bundles

Build the self-contained Windows x64 release from PowerShell:

```powershell
.\packaging\windows\install-ffmpeg-sdk.ps1
.\packaging\windows\build.ps1
.\testing\windows-installer-lifecycle.ps1
```

For a signed release, set `VIDEOFERRY_WINDOWS_SIGNING_CERT_THUMBPRINT`,
`VIDEOFERRY_WINDOWS_SIGNING_TIMESTAMP_URL`, and (unless it is on `PATH`)
`VIDEOFERRY_WINDOWS_SIGNTOOL`. The package builder signs the application before
the isolated runtime/smoke gates, signs the compiled installer, and requires
PowerShell to report both signatures as valid. With no thumbprint it produces
the same explicitly unsigned development artifacts used by local tests.

The SDK installer reads the version, archive URL, runtime directory, and
SHA-256 directly from `engine-manifest.toml`; it refuses a checksum mismatch,
an unexpected archive layout, or any install destination outside the Rust
workspace's `.local/ffmpeg` directory. Pass `-VerifyOnly` (and optionally
`-ArchivePath`) to audit an existing SDK/archive without modifying them. The
package script then builds with that pinned shared FFmpeg SDK, copies only the
required DLLs (not `ffmpeg.exe` or `ffprobe.exe`), bundles both DJI LUTs and the engine
manifest, and runs the packaged GUI binary's `--verify-runtime` self-check with
development SDK paths removed. That check requires the exact pinned FFmpeg and
libav versions, GPL runtime, software/audio/subtitle encoders, filters,
stabilization backend, and muxers. A PE import audit also rejects unused or
missing FFmpeg DLLs and any dynamic Visual C++ runtime dependency before the
script produces a portable ZIP and
launches it in an isolated clean-machine simulation with no development SDK
paths or real application data. The Slint smoke launch audits the Windows UI
Automation tree, keyboard focus, empty state, and every workflow in the settings
drawer. It also runs a Unicode-named fixture through the packaged direct engine,
holds the input briefly to inspect the live progress surface and queue controls,
confirms that no child process exists, then verifies publication, the byte-identical
`original/` backup, cleanup of staging files, and completed-history persistence and
clearing. It creates a per-user Inno Setup installer when `ISCC.exe` is available.
A separate screenshot regression command captures Queue, conversion, settings,
Finished, attention, and confirmation states at 900x620, 1180x760, 1440x900, and
minimum-window 125%/150% scale factors.
A non-PATH compiler can be supplied with
`-InnoCompiler C:\path\to\ISCC.exe` or `VIDEOFERRY_INNO_COMPILER`; the build also
recognizes `.local\inno-setup\ISCC.exe`.
`FFMPEG_DIR` and `LIBCLANG_PATH` may override the development SDK locations;
`-SkipSmoke` is available for a deliberately build-only run.

The installer lifecycle command compiles two fast test installers from the same
definition using a new GUID, no shortcuts, and an install directory under the
Rust workspace. It proves first install, installed direct-runtime health with
development paths removed, in-place package replacement, preservation of an
unrelated user file, updated uninstall registration, and silent uninstall. It
does not touch an existing HomeLab Video Converter installation. Pass `-KeepArtifacts` to retain
its ignored `.local` diagnostics.

Build the Apple Silicon application, ZIP, and DMG on macOS:

```bash
./scripts/build-mac.sh
```

The top-level command builds the pinned FFmpeg SDK when it is not already
present, then creates and verifies all three package formats. To run the
underlying steps separately and include the extended runtime matrix, use:

```bash
bash packaging/macos/build-ffmpeg-sdk.sh
bash packaging/macos/build.sh
bash testing/macos-runtime.sh
```

The first script verifies the pinned FFmpeg source checksum and builds the
shared Apple Silicon SDK with x264, x265, SVT-AV1, vid.stab, and VideoToolbox.
The package build gathers the complete non-system dylib dependency closure,
rewrites install names to `@rpath`, embeds required build/license records and
LUTs under `Contents/Resources`, and verifies the app signature. Its DMG uses a
custom Finder layout with the application on the left, a visual installation
arrow, and the Applications link on the right. It then runs
`packaging/macos/verify-package.sh` against the bundle, an extracted ZIP, and a
mounted DMG. The verifier checks arm64-only binaries, the complete `@rpath`
closure, plist identity/version/deployment target, signatures, required
resources and license notices, absence of `ffmpeg`/`ffprobe` executables, and
the packaged binary's exact direct-library runtime self-check. Set
`VIDEOFERRY_CODESIGN_IDENTITY` for Developer ID signing and
`VIDEOFERRY_NOTARY_PROFILE` for `notarytool` submission; without an identity it
creates an ad-hoc signed development bundle. A native Apple Silicon execution
of these gates remains required before release. The final command runs all six
workflows through the direct engine and, when advertised by the packaged
runtime, runs H.264/HEVC/AV1 VideoToolbox across TV, Camera, Slideshow, and
Stabilize before probing every output directly. Clean-machine testing remains a
separate release gate.
