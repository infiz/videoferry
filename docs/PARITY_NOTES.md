# Python/Rust parity notes

The Python application remains the behavioral reference and is not modified by
the Rust rewrite. The repeatable Windows gate is:

```powershell
.\testing\windows-parity.ps1
```

It creates temporary chaptered media with source metadata, AAC and DTS tracks,
a forced timed subtitle, naturally ordered mixed-orientation photo sets
(including a 41-photo chunking fixture), and two audio clips. It also creates
variable-frame-rate and 10-bit BT.2020/PQ fixtures. It runs both implementations
and compares normalized `ffprobe` results for:

- TV x265 MKV
- TV x264 and SVT-AV1 MKV
- Animation x265 MKV
- Camera x265 MP4
- Camera x264 and SVT-AV1 MP4 as Rust repair gates (the current Python
  commands fail before encoding)
- TV x265 MKV from a variable-frame-rate source
- Camera x265 MP4 from a 10-bit BT.2020/PQ source
- Photo slideshow x265 MP4, with base, multi-audio, collage, and 41-photo
  chunked variants
- Balanced stabilization x265 MKV
- Inclusive trim copy
- H.264, HEVC, and AV1 NVENC for TV MKV, Camera MP4, Photo slideshow, and
  Balanced stabilization when each encoder passes a real two-frame hardware
  probe

Every successful Rust workflow also has typed progress assertions for the
engine-resolved numeric Target FPS and total frame count. A separate folder with
24 FPS and 12 FPS sources verifies that shared-lowest conversion reports 12 FPS
and the corresponding 12-frame total instead of merely echoing the queue
policy. Stabilization additionally asserts that overall progress is split
across its two phases while the media `Time` restarts locally for the transform
pass, matching Python's separate percentage callback scaling.
Every software and NVENC stabilization gate additionally verifies that the
first, direct `vidstab` analysis event reports measured Current FPS and Speed;
second-pass metrics cannot satisfy that assertion.

The comparison covers stream order/types/codecs, dimensions, pixel format,
color range/space/transfer/primaries, frame rate, frame count, duration, codec
tags, language/default/forced state, title metadata, and chapter timing/titles.
The source contains both a valid timed subtitle and a one-frame sparse subtitle;
the gate verifies that only the valid stream survives. It also verifies exact
attachment payload hashes and metadata for text, TrueType, OpenType, and
arbitrary binary attachments; malformed/locked-input failure; existing-output
protection; cancellation; and partial-output cleanup. Unit gates additionally
verify source rollback after publication failure and protection from a
late-created output.
The parity runner is compiled with a separate, non-default test feature that
can return a storage-full error after a configured number of real mux packet
writes. Its mid-conversion gate verifies a clear error, no published target,
and no surviving partial file. Production application and package builds do
not compile this fault injector.
Desktop unit gates verify that restored slideshow folders use photo-aware
snapshots and remain one dynamically rescanned queue job.
Video folders and restored multi-target tasks likewise remain one queue row;
their naturally ordered files, per-run failures, and watched-folder additions
are scheduled beneath the aggregate task instead of expanding the visible
queue.
The production Rust application never
uses `ffmpeg` or `ffprobe`; these executables are reference tools used only by
this cross-implementation test.

## Intentional safety differences

- The Python Windows `vidstab` analysis command currently passes an unescaped
  drive-letter path to the filter graph. The gate therefore exercises Python's
  documented `deshake` fallback, while Rust exercises native two-pass `vidstab`
  with an escaped transform path.
- Python stabilization preserves the input suffix but internally declares its
  target as MP4. With an MKV target this causes Python to copy DTS. Rust applies
  the compatibility rule to the actual output container and converts that DTS
  track to AC-3. This deliberately keeps the safer global MKV rule.
- Python's sanitized explicit stream map selects video, valid audio, and valid
  subtitles but not attachments. Rust preserves Matroska attachments (including
  filename and MIME metadata) and verifies that behavior end to end because it
  is part of the declared replacement contract.
- Python's Camera x264 and Camera SVT-AV1 subclasses inherit the x265
  `cmd_threads_index`, overwrite their `-c:v` value with `log-level=error`, and
  fail. Rust verifies successful H.264/AV1 MP4 output for both advertised modes
  instead of reproducing that command-construction defect.
- Python's HEVC NVENC Camera MP4 uses the default `hev1` sample entry. Rust
  emits `hvc1`, matching the replacement contract and Apple-compatible behavior
  used by every other HEVC MP4 path.
- MP4 `mov_text` represents each visible cue plus a clearing sample in the
  container frame count. Rust normalizes that direct-library count and rejects
  a one-cue stream that ends far before the source. Python currently applies
  its sparse-subtitle check only when Matroska-style `NUMBER_OF_FRAMES` and
  `DURATION` tags are available, so the generated MP4 case is a Rust safety
  repair rather than an exact-output comparison.

These differences do not change output naming or queue behavior. Python stays
available for rollback until retirement is separately approved.

## Gates still requiring external coverage

- The representative private/user media corpus, including malformed subtitle
  tracks and real-world font/binary attachment variants. Generated sparse
  one-frame subtitle rejection and exact text/TrueType/OpenType/binary
  attachment preservation are covered by the Windows runtime gate.
- Execute `testing/macos-runtime.sh` on Apple Silicon hardware. It requires
  H.264/HEVC VideoToolbox, exercises optional AV1 when advertised, and runs each
  available hardware codec across TV, Camera, Slideshow, and Stabilize after
  all six CPU workflows. Windows H.264, HEVC, and AV1 NVENC are runtime-covered
  on an RTX 5080; machines without working NVENC skip those optional gates after
  real encoder probes rather than relying on encoder names alone.
- Physical full-volume failures and clean-machine installer/app-bundle tests.
  Deterministic mid-mux storage exhaustion is covered through the test-only
  fault above. The Windows portable ZIP is automatically smoke-launched with
  development paths removed and isolated application data; locked-input,
  publication rollback, and late-output protection are also covered locally.
  The macOS build now scripts equivalent bundle/ZIP/DMG architecture,
  dependency, signature, license/resource, forbidden-executable, and exact
  in-process runtime checks, but they still require native execution.
  The Windows Inno definition has separately passed two revisions under an
  isolated GUID and workspace install directory, including installed runtime
  verification, unrelated-file preservation across upgrade, updated uninstall
  registration, shortcut suppression, and silent uninstall; a genuinely clean
  Windows machine is still required for release.
- Human Windows/macOS screen-reader walkthroughs. The packaged Windows app's
  native UI Automation tree is checked automatically for required accessible
  names, roles, keyboard focus, and interaction patterns across all six
  workflows. The About view must expose the exact app/Rust/FFmpeg versions and
  active license. During a locked-input run, the Processes view must expose the
  in-process worker/PID/source while an operating-system query proves that the
  packaged app has no child process. That real two-file aggregate worker must
  report the current item as `2/2` and also exercises Pause, Resume,
  Pause-after-current, paused-state persistence, and the live Python-compatible
  Camera Model/Applying LUT progress fields.
  The same isolated package run drives a Python-schema malformed-media task
  through Run selected, completed-with-failure persistence, source/partial
  safety, and rerun-to-pending behavior. A checksum-pinned synthetic source
  then verifies successful conversion, byte-identical `original/` backup,
  queue completion, and the Python-compatible eleven-column History storage
  and accessible view. That UI-driven gate also persists queue reordering,
  task removal, queue clearing, and History clearing. It then holds the source
  under an exclusive lock, starts a real packaged queue worker, waits for the
  `running` task and `was_running` marker to reach disk, force-terminates the
  process, releases the lock, and relaunches the package. The new process must
  automatically resume and complete with a validated output, byte-identical
  backup, one History row, a cleared running marker, and no partial files.
