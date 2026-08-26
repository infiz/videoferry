param(
    [switch]$KeepArtifacts,
    [switch]$SkipHardware,
    [string]$ReferencePythonProject = $env:VIDEOFERRY_REFERENCE_PYTHON_PROJECT
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$PSNativeCommandUseErrorActionPreference = $false

$scriptDirectory = Split-Path -Parent $MyInvocation.MyCommand.Path
$rustRoot = (Resolve-Path (Join-Path $scriptDirectory '..')).Path
if ([string]::IsNullOrWhiteSpace($ReferencePythonProject)) {
    $ReferencePythonProject = Join-Path $rustRoot '..\homelab\projects\media-toolkits'
}
$referencePythonRoot = (Resolve-Path -LiteralPath $ReferencePythonProject).Path
$ffmpegRoot = (Resolve-Path (Join-Path $rustRoot '.local\ffmpeg\ffmpeg-9.0.1-full_build-shared')).Path
$ffmpeg = Join-Path $ffmpegRoot 'bin\ffmpeg.exe'
$ffprobe = Join-Path $ffmpegRoot 'bin\ffprobe.exe'
$runsRoot = Join-Path $rustRoot '.local\parity-runs'
New-Item -ItemType Directory -Force -Path $runsRoot | Out-Null
$runRoot = Join-Path $runsRoot ([guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $runRoot | Out-Null
$succeeded = $false

function Assert-Condition {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) {
        throw $Message
    }
}

function Assert-Equal {
    param($Actual, $Expected, [string]$Message)
    if ($Actual -ne $Expected) {
        throw "$Message (expected '$Expected', got '$Actual')"
    }
}

function Assert-Near {
    param([double]$Actual, [double]$Expected, [double]$Tolerance, [string]$Message)
    if ([math]::Abs($Actual - $Expected) -gt $Tolerance) {
        throw "$Message (expected $Expected +/- $Tolerance, got $Actual)"
    }
}

function Convert-RustDebugDurationSeconds {
    param([string]$Value)
    if ($Value -notmatch '^([0-9]+(?:\.[0-9]+)?)(ns|us|µs|ms|s)$') {
        throw "Unsupported Rust Duration debug value '$Value'"
    }
    $amount = [double]$Matches[1]
    switch ($Matches[2]) {
        'ns' { return $amount / 1000000000 }
        'us' { return $amount / 1000000 }
        'µs' { return $amount / 1000000 }
        'ms' { return $amount / 1000 }
        's' { return $amount }
    }
}

function Find-ByteSequenceOffset {
    param([byte[]]$Haystack, [byte[]]$Needle)
    if ($Needle.Length -eq 0 -or $Needle.Length -gt $Haystack.Length) {
        return -1
    }
    for ($offset = 0; $offset -le $Haystack.Length - $Needle.Length; $offset++) {
        $matched = $true
        for ($index = 0; $index -lt $Needle.Length; $index++) {
            if ($Haystack[$offset + $index] -ne $Needle[$index]) {
                $matched = $false
                break
            }
        }
        if ($matched) {
            return $offset
        }
    }
    return -1
}

function Get-Tag {
    param($Tags, [string]$Name)
    if ($null -eq $Tags) {
        return $null
    }
    $property = $Tags.PSObject.Properties | Where-Object { $_.Name -ieq $Name } | Select-Object -First 1
    if ($null -eq $property) {
        return $null
    }
    if ($property.Value -is [string] -and [string]::IsNullOrWhiteSpace($property.Value)) {
        return $null
    }
    return $property.Value
}

function Get-PropertyValue {
    param($Object, [string]$Name)
    if ($null -eq $Object) {
        return $null
    }
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) {
        return $null
    }
    return $property.Value
}

function Convert-Rational {
    param([string]$Value)
    if ([string]::IsNullOrWhiteSpace($Value) -or $Value -eq '0/0') {
        return 0.0
    }
    $parts = $Value.Split('/')
    if ($parts.Count -ne 2 -or [double]$parts[1] -eq 0) {
        throw "Invalid rational value: $Value"
    }
    return [double]$parts[0] / [double]$parts[1]
}

function Get-Probe {
    param([string]$Path)
    $json = & $ffprobe -v error -show_entries `
        'format=duration:format_tags:stream=index,codec_type,codec_name,codec_tag_string,width,height,pix_fmt,color_range,color_space,color_transfer,color_primaries,avg_frame_rate,duration,nb_frames:stream_tags=language,title,filename,mimetype:stream_disposition=default,forced:chapter=id,start_time,end_time:chapter_tags=title' `
        -of json $Path
    if ($LASTEXITCODE -ne 0) {
        throw "ffprobe failed for $Path"
    }
    return ($json | Out-String | ConvertFrom-Json)
}

function Assert-AttachmentPayload {
    param(
        [string]$Container,
        [int]$StreamIndex,
        [string]$ExpectedPath,
        [string]$Description
    )
    $extractedPath = Join-Path $runRoot ("extracted-attachment-{0:D2}.bin" -f $StreamIndex)
    & $ffmpeg -hide_banner -loglevel error `
        "-dump_attachment:t:$StreamIndex" $extractedPath `
        -i $Container -map '0:v:0?' -frames:v 1 -f null NUL
    if ($LASTEXITCODE -ne 0) {
        throw "Unable to extract $Description"
    }
    Assert-Condition (Test-Path -LiteralPath $extractedPath -PathType Leaf) "$Description was not extracted"
    $actualHash = (Get-FileHash -LiteralPath $extractedPath -Algorithm SHA256).Hash
    $expectedHash = (Get-FileHash -LiteralPath $ExpectedPath -Algorithm SHA256).Hash
    Assert-Equal $actualHash $expectedHash "$Description payload hash"
}

function Test-NvencEncoder {
    param([string]$Encoder)
    $probeOutput = Join-Path $runRoot "nvenc-probe-$Encoder.mkv"
    $probeLog = Join-Path $runRoot "nvenc-probe-$Encoder.log"
    & $ffmpeg -hide_banner -loglevel error -f lavfi `
        -i 'color=c=black:s=320x180:r=24:d=0.2' -frames:v 2 `
        -c:v $Encoder -preset p4 -y $probeOutput *> $probeLog
    $available = $LASTEXITCODE -eq 0 -and (Test-Path -LiteralPath $probeOutput -PathType Leaf)
    Remove-Item -LiteralPath $probeOutput -Force -ErrorAction SilentlyContinue
    return $available
}

function Compare-Probes {
    param([string]$CaseName, $PythonProbe, $RustProbe)
    $pythonStreams = @($PythonProbe.streams)
    $rustStreams = @($RustProbe.streams)
    Assert-Equal $rustStreams.Count $pythonStreams.Count "$CaseName stream count"
    Assert-Near ([double]$RustProbe.format.duration) ([double]$PythonProbe.format.duration) 0.125 "$CaseName container duration"
    Assert-Equal (Get-Tag (Get-PropertyValue $RustProbe.format 'tags') 'title') (Get-Tag (Get-PropertyValue $PythonProbe.format 'tags') 'title') "$CaseName title metadata"

    $pythonChapterValue = Get-PropertyValue $PythonProbe 'chapters'
    $rustChapterValue = Get-PropertyValue $RustProbe 'chapters'
    $pythonChapters = @()
    $rustChapters = @()
    if ($null -ne $pythonChapterValue) {
        $pythonChapters = @($pythonChapterValue)
    }
    if ($null -ne $rustChapterValue) {
        $rustChapters = @($rustChapterValue)
    }
    Assert-Equal $rustChapters.Count $pythonChapters.Count "$CaseName chapter count"
    for ($chapterIndex = 0; $chapterIndex -lt $pythonChapters.Count; $chapterIndex++) {
        Assert-Near ([double]$rustChapters[$chapterIndex].start_time) ([double]$pythonChapters[$chapterIndex].start_time) 0.002 "$CaseName chapter $chapterIndex start"
        Assert-Near ([double]$rustChapters[$chapterIndex].end_time) ([double]$pythonChapters[$chapterIndex].end_time) 0.002 "$CaseName chapter $chapterIndex end"
        Assert-Equal (Get-Tag (Get-PropertyValue $rustChapters[$chapterIndex] 'tags') 'title') (Get-Tag (Get-PropertyValue $pythonChapters[$chapterIndex] 'tags') 'title') "$CaseName chapter $chapterIndex title"
    }

    for ($index = 0; $index -lt $pythonStreams.Count; $index++) {
        $pythonStream = $pythonStreams[$index]
        $rustStream = $rustStreams[$index]
        Assert-Equal $rustStream.codec_type $pythonStream.codec_type "$CaseName stream $index type"
        $knownStabilizeDtsSafetyDifference = $CaseName -like 'stabilize*' -and $pythonStream.codec_name -eq 'dts' -and $rustStream.codec_name -eq 'ac3'
        if (-not $knownStabilizeDtsSafetyDifference) {
            Assert-Equal $rustStream.codec_name $pythonStream.codec_name "$CaseName stream $index codec"
        }
        Assert-Equal $rustStream.disposition.default $pythonStream.disposition.default "$CaseName stream $index default disposition"
        Assert-Equal $rustStream.disposition.forced $pythonStream.disposition.forced "$CaseName stream $index forced disposition"
        Assert-Equal (Get-Tag (Get-PropertyValue $rustStream 'tags') 'language') (Get-Tag (Get-PropertyValue $pythonStream 'tags') 'language') "$CaseName stream $index language"
        if ($pythonStream.codec_type -eq 'video') {
            Assert-Equal $rustStream.width $pythonStream.width "$CaseName video width"
            Assert-Equal $rustStream.height $pythonStream.height "$CaseName video height"
            $knownCameraHevcTagRepair = $CaseName -eq 'camera-hevc-nvenc' -and $pythonStream.codec_tag_string -eq 'hev1' -and $rustStream.codec_tag_string -eq 'hvc1'
            if (-not $knownCameraHevcTagRepair) {
                Assert-Equal $rustStream.codec_tag_string $pythonStream.codec_tag_string "$CaseName video codec tag"
            }
            foreach ($property in @('pix_fmt', 'color_range', 'color_space', 'color_transfer', 'color_primaries')) {
                Assert-Equal (Get-PropertyValue $rustStream $property) (Get-PropertyValue $pythonStream $property) "$CaseName video $property"
            }
            Assert-Near (Convert-Rational $rustStream.avg_frame_rate) (Convert-Rational $pythonStream.avg_frame_rate) 0.01 "$CaseName video frame rate"
            $pythonDuration = Get-PropertyValue $pythonStream 'duration'
            $rustDuration = Get-PropertyValue $rustStream 'duration'
            if ($null -ne $pythonDuration -and $null -ne $rustDuration) {
                Assert-Near ([double]$rustDuration) ([double]$pythonDuration) 0.125 "$CaseName video duration"
            }
            $pythonFrameCount = Get-PropertyValue $pythonStream 'nb_frames'
            $rustFrameCount = Get-PropertyValue $rustStream 'nb_frames'
            if ($null -ne $pythonFrameCount -and $null -ne $rustFrameCount) {
                Assert-Equal $rustFrameCount $pythonFrameCount "$CaseName video frame count"
            }
        }
    }
}

function Invoke-PythonConversion {
    param(
        [string]$Mode,
        [string]$Encoder,
        [string]$Source,
        [string]$Output,
        [string]$Variant = '',
        [string]$AudioPathList = '',
        [switch]$ExpectFailure
    )
    # Keep this as one physical line so Poetry's Windows launcher preserves the
    # complete `python -c` argument on every supported PowerShell version.
    $pythonCode = 'import os, sys; from media_toolkit.converters.presets import AnimationX265MkvConverter, Av1NvencMkvConverter, Av1NvencMp4Converter, CameraVideosX264Mp4Converter, CameraVideosX265Mp4Converter, CameraVideosSvtAv1Mp4Converter, H264NvencMkvConverter, H264NvencMp4Converter, HevcNvencMkvConverter, HevcNvencMp4Converter, PhotoSlideshowConverter, StabilizeVideoConverter, TrimCopyConverter, TvSvtAv1MkvConverter, TvX264MkvConverter, TvX265MkvConverter; mode, encoder, source, output, variant, audio_paths = sys.argv[1:7]; converter = ({("tv", "x265"): TvX265MkvConverter, ("tv", "x264"): TvX264MkvConverter, ("tv", "svtav1"): TvSvtAv1MkvConverter, ("animation", "x265"): AnimationX265MkvConverter, ("camera", "x265"): CameraVideosX265Mp4Converter, ("camera", "x264"): CameraVideosX264Mp4Converter, ("camera", "svtav1"): CameraVideosSvtAv1Mp4Converter, ("tv", "hevc_nvenc"): HevcNvencMkvConverter, ("tv", "h264_nvenc"): H264NvencMkvConverter, ("tv", "av1_nvenc"): Av1NvencMkvConverter, ("camera", "hevc_nvenc"): HevcNvencMp4Converter, ("camera", "h264_nvenc"): H264NvencMp4Converter, ("camera", "av1_nvenc"): Av1NvencMp4Converter}.get((mode, encoder)) or {"slideshow": PhotoSlideshowConverter, "stabilize": StabilizeVideoConverter, "trim": TrimCopyConverter}[mode])(); mode == "camera" and hasattr(converter, "set_apply_lut") and converter.set_apply_lut(False); mode == "slideshow" and converter.set_encoder(encoder); mode == "slideshow" and converter.set_quality_options({"x265": "28", "x264": "23", "svtav1": "24"}.get(encoder, "N/A"), "p4" if encoder.endswith("_nvenc") else ("6" if encoder == "svtav1" else "medium")); mode == "slideshow" and converter.set_slideshow_resolution("1080p"); mode == "slideshow" and converter.set_slideshow_fps(12); mode == "slideshow" and converter.set_photo_interval_seconds(0.5); variant == "large" and converter.set_slideshow_fps(10); variant == "large" and converter.set_photo_interval_seconds(0.2); variant == "audio" and converter.set_slideshow_audio_paths(tuple(audio_paths.split(os.pathsep))); variant == "collage" and converter.set_slideshow_collage_enabled(True); mode == "stabilize" and StabilizeVideoConverter._filter_cache.update({"vidstabdetect": False, "vidstabtransform": False, "deshake": True}); mode == "stabilize" and converter.set_stabilize_strength("Balanced"); mode == "stabilize" and converter.set_encoder(encoder); encoder.endswith("_nvenc") and mode not in ("slideshow", "trim") and setattr(converter, "cmd", converter.cmd[:converter.cmd.index("-y")] + ["-preset", "p4"] + converter.cmd[converter.cmd.index("-y"):]); mode == "trim" and converter.set_trim_range("00:00:01", "00:00:02"); converter.convert_file(source, output, None)'
    $log = Join-Path $runRoot ("python-" + [IO.Path]::GetFileNameWithoutExtension($Output) + '.log')
    Push-Location $referencePythonRoot
    try {
        & poetry run python -c $pythonCode $Mode $Encoder $Source $Output $Variant $AudioPathList *> $log
        if ($ExpectFailure) {
            Assert-Condition ($LASTEXITCODE -ne 0) "Python $Mode/$Encoder unexpectedly succeeded"
        } elseif ($LASTEXITCODE -ne 0) {
            Get-Content $log
            throw "Python $Mode conversion failed"
        }
    } finally {
        Pop-Location
    }
}

function Invoke-RustConversion {
    param(
        [string]$Mode,
        [string]$Encoder,
        [string]$Source,
        [string]$Output,
        [string[]]$ExtraArguments = @(),
        [string]$Variant = '',
        [string]$AudioPathList = '',
        [string]$Fps = '',
        [Nullable[int]]$CancelAfterMilliseconds = $null,
        [Nullable[int]]$DiskFullAfterMuxWrites = $null,
        [switch]$ExpectFailure
    )
    $env:VIDEOFERRY_MODE = $Mode
    $env:VIDEOFERRY_ENCODER = $Encoder
    if ([string]::IsNullOrWhiteSpace($Fps)) {
        Remove-Item Env:VIDEOFERRY_FPS -ErrorAction SilentlyContinue
    } else {
        $env:VIDEOFERRY_FPS = $Fps
    }
    if ($Mode -eq 'slideshow') {
        $env:VIDEOFERRY_SLIDESHOW = '1920x1080'
        $env:VIDEOFERRY_SLIDESHOW_FPS = if ($Variant -eq 'large') { '10' } else { '12' }
        $env:VIDEOFERRY_SLIDESHOW_INTERVAL = if ($Variant -eq 'large') { '0.2' } else { '0.5' }
        if ($Variant -eq 'audio') {
            $env:VIDEOFERRY_SLIDESHOW_AUDIO = $AudioPathList
        } else {
            Remove-Item Env:VIDEOFERRY_SLIDESHOW_AUDIO -ErrorAction SilentlyContinue
        }
        if ($Variant -eq 'collage') {
            $env:VIDEOFERRY_SLIDESHOW_COLLAGE = '1'
        } else {
            Remove-Item Env:VIDEOFERRY_SLIDESHOW_COLLAGE -ErrorAction SilentlyContinue
        }
    } else {
        Remove-Item Env:VIDEOFERRY_SLIDESHOW -ErrorAction SilentlyContinue
        Remove-Item Env:VIDEOFERRY_SLIDESHOW_FPS -ErrorAction SilentlyContinue
        Remove-Item Env:VIDEOFERRY_SLIDESHOW_INTERVAL -ErrorAction SilentlyContinue
        Remove-Item Env:VIDEOFERRY_SLIDESHOW_AUDIO -ErrorAction SilentlyContinue
        Remove-Item Env:VIDEOFERRY_SLIDESHOW_COLLAGE -ErrorAction SilentlyContinue
    }
    if ($null -ne $CancelAfterMilliseconds) {
        $env:VIDEOFERRY_CANCEL_AFTER_MS = $CancelAfterMilliseconds.ToString()
    } else {
        Remove-Item Env:VIDEOFERRY_CANCEL_AFTER_MS -ErrorAction SilentlyContinue
    }
    if ($null -ne $DiskFullAfterMuxWrites) {
        $env:VIDEOFERRY_TEST_DISK_FULL_AFTER_MUX_WRITES = $DiskFullAfterMuxWrites.ToString()
    } else {
        Remove-Item Env:VIDEOFERRY_TEST_DISK_FULL_AFTER_MUX_WRITES -ErrorAction SilentlyContinue
    }
    $logName = [IO.Path]::GetFileNameWithoutExtension($Output)
    $log = Join-Path $runRoot "$logName.log"
    & $script:rustRunner $Source $Output @ExtraArguments *> $log
    $exitCode = $LASTEXITCODE
    Remove-Item Env:VIDEOFERRY_CANCEL_AFTER_MS -ErrorAction SilentlyContinue
    Remove-Item Env:VIDEOFERRY_TEST_DISK_FULL_AFTER_MUX_WRITES -ErrorAction SilentlyContinue
    Remove-Item Env:VIDEOFERRY_SLIDESHOW -ErrorAction SilentlyContinue
    Remove-Item Env:VIDEOFERRY_SLIDESHOW_FPS -ErrorAction SilentlyContinue
    Remove-Item Env:VIDEOFERRY_SLIDESHOW_INTERVAL -ErrorAction SilentlyContinue
    Remove-Item Env:VIDEOFERRY_SLIDESHOW_AUDIO -ErrorAction SilentlyContinue
    Remove-Item Env:VIDEOFERRY_SLIDESHOW_COLLAGE -ErrorAction SilentlyContinue
    Remove-Item Env:VIDEOFERRY_FPS -ErrorAction SilentlyContinue
    if ($ExpectFailure) {
        Assert-Condition ($exitCode -ne 0) "$Mode conversion unexpectedly succeeded"
    } elseif ($exitCode -ne 0) {
        Get-Content $log
        throw "Rust $Mode conversion failed"
    }
}

try {
    foreach ($required in @($ffmpeg, $ffprobe)) {
        Assert-Condition (Test-Path -LiteralPath $required -PathType Leaf) "Missing test dependency: $required"
    }

    $env:FFMPEG_DIR = $ffmpegRoot
    $env:LIBCLANG_PATH = if ($env:LIBCLANG_PATH) { $env:LIBCLANG_PATH } else { 'C:\Program Files\LLVM\bin' }
    $env:PATH = "$(Join-Path $ffmpegRoot 'bin');$env:PATH"
    Push-Location $rustRoot
    try {
        & cargo build --locked -p videoferry-ffmpeg --example native_convert --features native-ffmpeg,test-fault-injection
        if ($LASTEXITCODE -ne 0) {
            throw 'Unable to build the direct-library parity runner'
        }
    } finally {
        Pop-Location
    }
    $script:rustRunner = Join-Path $rustRoot 'target\debug\examples\native_convert.exe'
    Assert-Condition (Test-Path -LiteralPath $script:rustRunner -PathType Leaf) 'Parity runner executable was not produced'

    $subtitle = Join-Path $runRoot 'captions.srt'
    [IO.File]::WriteAllText($subtitle, "1`r`n00:00:00,000 --> 00:00:01,500`r`nFirst caption`r`n`r`n2`r`n00:00:02,000 --> 00:00:04,000`r`nSecond caption`r`n", [Text.UTF8Encoding]::new($false))
    $sparseSubtitle = Join-Path $runRoot 'sparse-captions.srt'
    [IO.File]::WriteAllText($sparseSubtitle, "1`r`n00:00:00,000 --> 00:00:00,040`r`nBad one-frame caption`r`n", [Text.UTF8Encoding]::new($false))
    $chapterMetadata = Join-Path $runRoot 'chapters.ffmeta'
    [IO.File]::WriteAllText($chapterMetadata, ";FFMETADATA1`r`ntitle=Parity source metadata`r`n[CHAPTER]`r`nTIMEBASE=1/1000`r`nSTART=0`r`nEND=2000`r`ntitle=First half`r`n[CHAPTER]`r`nTIMEBASE=1/1000`r`nSTART=2000`r`nEND=4000`r`ntitle=Second half`r`n", [Text.UTF8Encoding]::new($false))
    $source = Join-Path $runRoot 'source.mp4'
    & $ffmpeg -hide_banner -loglevel error `
        -f lavfi -i 'testsrc2=size=320x180:rate=24:duration=4' `
        -f lavfi -i 'sine=frequency=440:sample_rate=48000:duration=4' `
        -f lavfi -i 'sine=frequency=880:sample_rate=48000:duration=4' `
        -f srt -i $subtitle -f ffmetadata -i $chapterMetadata `
        -map 0:v:0 -map 1:a:0 -map 2:a:0 -map 3:s:0 `
        -map_metadata 4 -map_chapters 4 `
        -c:v libx264 -preset ultrafast -c:a:0 aac -c:a:1 dca -strict -2 -c:s mov_text `
        -metadata:s:a:0 language=eng -metadata:s:a:1 language=jpn `
        -metadata:s:s:0 language=spa -disposition:s:0 forced `
        -disposition:a:0 default -disposition:a:1 0 $source
    if ($LASTEXITCODE -ne 0) {
        throw 'Unable to generate the parity fixture'
    }
    $malformedSubtitleSource = Join-Path $runRoot 'malformed-subtitle-source.mp4'
    $malformedSubtitleBytes = [IO.File]::ReadAllBytes($source)
    $subtitlePayload = [Text.Encoding]::UTF8.GetBytes('First caption')
    $subtitlePayloadOffset = Find-ByteSequenceOffset $malformedSubtitleBytes $subtitlePayload
    Assert-Condition ($subtitlePayloadOffset -ge 0) 'Unable to locate the subtitle packet payload'
    $malformedSubtitleBytes[$subtitlePayloadOffset] = 0xff
    $malformedSubtitleBytes[$subtitlePayloadOffset + 1] = 0xfe
    [IO.File]::WriteAllBytes($malformedSubtitleSource, $malformedSubtitleBytes)
    $malformedSubtitleProbe = Get-Probe $malformedSubtitleSource
    $malformedSubtitleStreams = @($malformedSubtitleProbe.streams | Where-Object { $_.codec_type -eq 'subtitle' })
    Assert-Equal $malformedSubtitleStreams.Count 1 'Malformed subtitle fixture stream count'
    $sparseSubtitleSource = Join-Path $runRoot 'sparse-subtitle-source.mp4'
    & $ffmpeg -hide_banner -loglevel error -i $source -f srt -i $sparseSubtitle `
        -map 0 -map 1:s:0 -c copy -c:s mov_text `
        -metadata:s:s:1 language=und -disposition:s:1 0 $sparseSubtitleSource
    if ($LASTEXITCODE -ne 0) {
        throw 'Unable to generate the sparse subtitle fixture'
    }
    $stabilizeSource = Join-Path $runRoot 'stabilize-source.mkv'
    & $ffmpeg -hide_banner -loglevel error -i $source -map 0:v:0 -map 0:a -c copy -map_metadata 0 -map_chapters 0 $stabilizeSource
    if ($LASTEXITCODE -ne 0) {
        throw 'Unable to generate the stabilization fixture'
    }
    $attachment = Join-Path $runRoot 'parity-note.txt'
    [IO.File]::WriteAllText($attachment, 'VideoFerry attachment parity fixture', [Text.UTF8Encoding]::new($false))
    $fontAttachment = Join-Path $runRoot 'parity-font.ttf'
    [IO.File]::WriteAllBytes($fontAttachment, [byte[]](0x00, 0x01, 0x00, 0x00, 0x48, 0x4C, 0x56, 0x43))
    $openTypeAttachment = Join-Path $runRoot 'parity-font.otf'
    [IO.File]::WriteAllBytes($openTypeAttachment, [byte[]](0x4F, 0x54, 0x54, 0x4F, 0x00, 0x01, 0x48, 0x4C, 0x56, 0x43))
    $binaryAttachment = Join-Path $runRoot 'parity-payload.bin'
    [IO.File]::WriteAllBytes($binaryAttachment, [byte[]](0x00, 0xFF, 0x10, 0x80, 0x48, 0x4C, 0x56, 0x43, 0x00))
    $attachmentSource = Join-Path $runRoot 'attachment-source.mkv'
    & $ffmpeg -hide_banner -loglevel error -i $stabilizeSource -map 0 -c copy `
        -attach $attachment -metadata:s:t:0 mimetype=text/plain `
        -metadata:s:t:0 filename=parity-note.txt `
        -attach $fontAttachment -metadata:s:t:1 mimetype=application/x-truetype-font `
        -metadata:s:t:1 filename=parity-font.ttf `
        -attach $openTypeAttachment -metadata:s:t:2 mimetype=application/vnd.ms-opentype `
        -metadata:s:t:2 filename=parity-font.otf `
        -attach $binaryAttachment -metadata:s:t:3 mimetype=application/octet-stream `
        -metadata:s:t:3 filename=parity-payload.bin $attachmentSource
    if ($LASTEXITCODE -ne 0) {
        throw 'Unable to generate the Matroska attachment fixture'
    }
    $vfrSource = Join-Path $runRoot 'vfr-source.mkv'
    & $ffmpeg -hide_banner -loglevel error `
        -f lavfi -i 'testsrc2=size=320x180:rate=10:duration=2' `
        -f lavfi -i 'testsrc2=size=320x180:rate=30:duration=2' `
        -filter_complex '[0:v][1:v]concat=n=2:v=1:a=0[v]' -map '[v]' `
        -fps_mode vfr -c:v ffv1 $vfrSource
    if ($LASTEXITCODE -ne 0) {
        throw 'Unable to generate the VFR fixture'
    }
    $hdrSource = Join-Path $runRoot 'hdr-source.mkv'
    & $ffmpeg -hide_banner -loglevel error `
        -f lavfi -i 'testsrc2=size=320x180:rate=24:duration=2' `
        -vf format=yuv420p10le -c:v ffv1 `
        -color_range tv -colorspace bt2020nc -color_primaries bt2020 -color_trc smpte2084 `
        $hdrSource
    if ($LASTEXITCODE -ne 0) {
        throw 'Unable to generate the HDR/color fixture'
    }

    $photos = Join-Path $runRoot 'photos'
    New-Item -ItemType Directory -Path $photos | Out-Null
    foreach ($photo in @(
        [pscustomobject]@{ Name = 'photo1.png'; Color = 'red'; Size = '320x180' },
        [pscustomobject]@{ Name = 'photo2.png'; Color = 'green'; Size = '180x320' },
        [pscustomobject]@{ Name = 'photo10.png'; Color = 'blue'; Size = '180x320' }
    )) {
        & $ffmpeg -hide_banner -loglevel error -f lavfi -i "color=c=$($photo.Color):s=$($photo.Size)" -frames:v 1 (Join-Path $photos $photo.Name)
        if ($LASTEXITCODE -ne 0) {
            throw "Unable to generate $($photo.Name)"
        }
    }

    $largePhotos = Join-Path $runRoot 'large-photos'
    New-Item -ItemType Directory -Path $largePhotos | Out-Null
    for ($photoIndex = 1; $photoIndex -le 41; $photoIndex++) {
        $fixtureName = @('photo1.png', 'photo2.png', 'photo10.png')[($photoIndex - 1) % 3]
        $largePhotoName = 'photo{0:D3}.png' -f $photoIndex
        Copy-Item -LiteralPath (Join-Path $photos $fixtureName) -Destination (Join-Path $largePhotos $largePhotoName)
    }
    Assert-Equal @(Get-ChildItem -LiteralPath $largePhotos -File).Count 41 'Large slideshow fixture photo count'

    $audioPaths = @()
    foreach ($audio in @(
        [pscustomobject]@{ Name = 'audio1.wav'; Frequency = 330; Duration = 0.4 },
        [pscustomobject]@{ Name = 'audio2.wav'; Frequency = 660; Duration = 0.6 }
    )) {
        $audioPath = Join-Path $runRoot $audio.Name
        & $ffmpeg -hide_banner -loglevel error -f lavfi -i "sine=frequency=$($audio.Frequency):sample_rate=48000:duration=$($audio.Duration)" -c:a pcm_s16le $audioPath
        if ($LASTEXITCODE -ne 0) {
            throw "Unable to generate $($audio.Name)"
        }
        $audioPaths += $audioPath
    }
    $audioPathList = $audioPaths -join [IO.Path]::PathSeparator

    $cases = @(
        [pscustomobject]@{ Name = 'tv'; Mode = 'tv'; Encoder = 'x265'; Extension = 'mkv'; Extra = @(); Variant = ''; PythonFailure = $false },
        [pscustomobject]@{ Name = 'tv-x264'; Mode = 'tv'; Encoder = 'x264'; Extension = 'mkv'; Extra = @(); Variant = ''; PythonFailure = $false },
        [pscustomobject]@{ Name = 'tv-svtav1'; Mode = 'tv'; Encoder = 'svtav1'; Extension = 'mkv'; Extra = @(); Variant = ''; PythonFailure = $false },
        [pscustomobject]@{ Name = 'tv-vfr'; Mode = 'tv'; Encoder = 'x265'; Extension = 'mkv'; Extra = @(); Variant = ''; PythonFailure = $false },
        [pscustomobject]@{ Name = 'animation'; Mode = 'animation'; Encoder = 'x265'; Extension = 'mkv'; Extra = @(); Variant = ''; PythonFailure = $false },
        [pscustomobject]@{ Name = 'camera'; Mode = 'camera'; Encoder = 'x265'; Extension = 'mp4'; Extra = @(); Variant = ''; PythonFailure = $false },
        [pscustomobject]@{ Name = 'camera-x264'; Mode = 'camera'; Encoder = 'x264'; Extension = 'mp4'; Extra = @(); Variant = ''; PythonFailure = $true },
        [pscustomobject]@{ Name = 'camera-svtav1'; Mode = 'camera'; Encoder = 'svtav1'; Extension = 'mp4'; Extra = @(); Variant = ''; PythonFailure = $true },
        [pscustomobject]@{ Name = 'camera-hdr'; Mode = 'camera'; Encoder = 'x265'; Extension = 'mp4'; Extra = @(); Variant = ''; PythonFailure = $false },
        [pscustomobject]@{ Name = 'slideshow'; Mode = 'slideshow'; Encoder = 'x265'; Extension = 'mp4'; Extra = @(); Variant = ''; PythonFailure = $false },
        [pscustomobject]@{ Name = 'slideshow-audio'; Mode = 'slideshow'; Encoder = 'x265'; Extension = 'mp4'; Extra = @(); Variant = 'audio'; PythonFailure = $false },
        [pscustomobject]@{ Name = 'slideshow-collage'; Mode = 'slideshow'; Encoder = 'x265'; Extension = 'mp4'; Extra = @(); Variant = 'collage'; PythonFailure = $false },
        [pscustomobject]@{ Name = 'slideshow-large'; Mode = 'slideshow'; Encoder = 'x265'; Extension = 'mp4'; Extra = @(); Variant = 'large'; PythonFailure = $false },
        [pscustomobject]@{ Name = 'stabilize'; Mode = 'stabilize'; Encoder = 'x265'; Extension = 'mkv'; Extra = @(); Variant = ''; PythonFailure = $false },
        [pscustomobject]@{ Name = 'trim'; Mode = 'trim'; Encoder = 'x265'; Extension = 'mkv'; Extra = @('1', '2'); Variant = ''; PythonFailure = $false }
    )
    $availableNvencEncoders = @()
    if (-not $SkipHardware) {
        $availableNvencEncoders = @('hevc_nvenc', 'h264_nvenc', 'av1_nvenc') | Where-Object { Test-NvencEncoder $_ }
    }
    foreach ($nvencEncoder in $availableNvencEncoders) {
        $shortEncoder = $nvencEncoder.Replace('_nvenc', '')
        $cases += [pscustomobject]@{ Name = "tv-$shortEncoder-nvenc"; Mode = 'tv'; Encoder = $nvencEncoder; Extension = 'mkv'; Extra = @(); Variant = ''; PythonFailure = $false }
        $cases += [pscustomobject]@{ Name = "camera-$shortEncoder-nvenc"; Mode = 'camera'; Encoder = $nvencEncoder; Extension = 'mp4'; Extra = @(); Variant = ''; PythonFailure = $false; RustRepair = $nvencEncoder -eq 'hevc_nvenc' }
        $cases += [pscustomobject]@{ Name = "slideshow-$shortEncoder-nvenc"; Mode = 'slideshow'; Encoder = $nvencEncoder; Extension = 'mp4'; Extra = @(); Variant = ''; PythonFailure = $false }
        $cases += [pscustomobject]@{ Name = "stabilize-$shortEncoder-nvenc"; Mode = 'stabilize'; Encoder = $nvencEncoder; Extension = 'mkv'; Extra = @(); Variant = ''; PythonFailure = $false }
    }
    $results = @()
    foreach ($case in $cases) {
        $pythonOutput = Join-Path $runRoot "python-$($case.Name).$($case.Extension)"
        $rustOutput = Join-Path $runRoot "rust-$($case.Name).$($case.Extension)"
        $caseSource = if ($case.Name -eq 'slideshow-large') {
            $largePhotos
        } elseif ($case.Mode -eq 'slideshow') {
            $photos
        } elseif ($case.Mode -in @('stabilize', 'trim')) {
            $stabilizeSource
        } elseif ($case.Name -eq 'tv-vfr') {
            $vfrSource
        } elseif ($case.Name -eq 'camera-hdr') {
            $hdrSource
        } else {
            $source
        }
        Invoke-PythonConversion $case.Mode $case.Encoder $caseSource $pythonOutput $case.Variant $audioPathList -ExpectFailure:$case.PythonFailure
        Invoke-RustConversion $case.Mode $case.Encoder $caseSource $rustOutput $case.Extra $case.Variant $audioPathList
        $rustLog = Join-Path $runRoot "$([IO.Path]::GetFileNameWithoutExtension($rustOutput)).log"
        $rustLogText = [IO.File]::ReadAllText($rustLog)
        Assert-Condition ($rustLogText -match 'total_frames: Some\([1-9][0-9]*\)') "$($case.Name) progress omitted its total frame count"
        Assert-Condition ($rustLogText -match 'target_fps: Some\([0-9]') "$($case.Name) progress omitted its resolved target FPS"
        if ($case.Mode -eq 'stabilize') {
            $progressLines = @(Get-Content -LiteralPath $rustLog | Where-Object { $_ -like '*Progress(ConversionProgress*' })
            $firstProgress = $progressLines | Select-Object -First 1
            Assert-Condition (-not [string]::IsNullOrWhiteSpace($firstProgress)) "$($case.Name) emitted no analysis progress"
            Assert-Condition ($firstProgress -match 'frames_per_second: Some\([0-9]') "$($case.Name) analysis progress omitted Current FPS"
            Assert-Condition ($firstProgress -match 'speed: Some\([0-9]') "$($case.Name) analysis progress omitted Speed"
            Assert-Condition ($firstProgress -match 'overall: Some\(ProgressRatio') "$($case.Name) analysis progress omitted its phase-aware overall ratio"
            $secondPassProgress = $null
            foreach ($progressLine in $progressLines) {
                if ($progressLine -notmatch 'overall: Some\(ProgressRatio \{ completed: ([0-9]+), total: ([0-9]+) \}\), completed: ([^,]+), total: Some\(([^)]+)\)') {
                    continue
                }
                $overallCompleted = [decimal]$Matches[1]
                $overallTotal = [decimal]$Matches[2]
                if ($overallCompleted * 2 -gt $overallTotal) {
                    $secondPassProgress = $progressLine
                    $secondPassMediaTime = Convert-RustDebugDurationSeconds $Matches[3]
                    $fullMediaTime = Convert-RustDebugDurationSeconds $Matches[4]
                    break
                }
            }
            Assert-Condition ($null -ne $secondPassProgress) "$($case.Name) emitted no second-pass overall progress"
            Assert-Condition ($secondPassMediaTime -lt $fullMediaTime / 4) "$($case.Name) second-pass media Time did not restart locally while overall progress crossed 50%"
        }
        $rustProbe = Get-Probe $rustOutput
        if ($case.PythonFailure) {
            $expectedCodec = @{ x264 = 'h264'; svtav1 = 'av1' }[$case.Encoder]
            $rustVideo = @($rustProbe.streams) | Where-Object { $_.codec_type -eq 'video' } | Select-Object -First 1
            Assert-Equal $rustVideo.codec_name $expectedCodec "$($case.Name) repaired Rust codec"
        } else {
            $pythonProbe = Get-Probe $pythonOutput
            Compare-Probes $case.Name $pythonProbe $rustProbe
        }

        $video = @($rustProbe.streams) | Where-Object { $_.codec_type -eq 'video' } | Select-Object -First 1
        $audioCodecs = @($rustProbe.streams) | Where-Object { $_.codec_type -eq 'audio' } | ForEach-Object { $_.codec_name }
        $results += [pscustomobject]@{
            Case = $case.Name
            Video = $video.codec_name
            FPS = $video.avg_frame_rate
            Audio = ($audioCodecs -join ',')
            Duration = [math]::Round([double]$rustProbe.format.duration, 3)
            Result = if ($case.PythonFailure -or (Get-PropertyValue $case 'RustRepair')) { 'Rust repair' } else { 'matched' }
        }
    }

    $tvProbe = Get-Probe (Join-Path $runRoot 'rust-tv.mkv')
    $tvFormat = $tvProbe.format
    $cameraFormat = (Get-Probe (Join-Path $runRoot 'rust-camera.mp4')).format
    Assert-Equal (Get-Tag (Get-PropertyValue $tvFormat 'tags') 'title') $null 'TV metadata removal'
    Assert-Equal (Get-Tag (Get-PropertyValue $cameraFormat 'tags') 'title') 'Parity source metadata' 'Camera metadata preservation'

    $sharedLowestRoot = Join-Path $runRoot 'shared-lowest'
    New-Item -ItemType Directory -Path $sharedLowestRoot | Out-Null
    $sharedSource = Join-Path $sharedLowestRoot 'source-24fps.mp4'
    $sharedSibling = Join-Path $sharedLowestRoot 'sibling-12fps.mp4'
    & $ffmpeg -hide_banner -loglevel error -f lavfi -i 'testsrc2=size=96x64:rate=24:duration=1' -c:v libx264 -pix_fmt yuv420p $sharedSource
    Assert-Condition ($LASTEXITCODE -eq 0) 'Unable to generate the shared-lowest source fixture'
    & $ffmpeg -hide_banner -loglevel error -f lavfi -i 'testsrc2=size=96x64:rate=12:duration=1' -c:v libx264 -pix_fmt yuv420p $sharedSibling
    Assert-Condition ($LASTEXITCODE -eq 0) 'Unable to generate the shared-lowest sibling fixture'
    $sharedOutput = Join-Path $sharedLowestRoot 'resolved-output.mkv'
    Invoke-RustConversion 'tv' 'x265' $sharedSource $sharedOutput -Fps 'shared-lowest'
    $sharedLog = [IO.File]::ReadAllText((Join-Path $runRoot 'resolved-output.log'))
    Assert-Condition ($sharedLog -match 'total_frames: Some\(12\)') 'Shared-lowest progress did not report the resolved total frame count'
    Assert-Condition ($sharedLog -match 'target_fps: Some\(12\.0\)') 'Shared-lowest progress did not report the resolved 12 FPS target'

    $sparseSubtitleOutput = Join-Path $runRoot 'rust-sparse-subtitle.mkv'
    Invoke-RustConversion 'tv' 'x265' $sparseSubtitleSource $sparseSubtitleOutput
    $sparseSubtitleProbe = Get-Probe $sparseSubtitleOutput
    $sparseOutputStreams = @($sparseSubtitleProbe.streams | Where-Object { $_.codec_type -eq 'subtitle' })
    Assert-Equal $sparseOutputStreams.Count 1 'Sparse one-frame subtitle rejection'

    $attachmentOutput = Join-Path $runRoot 'rust-attachment.mkv'
    Invoke-RustConversion 'tv' 'x265' $attachmentSource $attachmentOutput
    $attachmentProbe = Get-Probe $attachmentOutput
    $attachmentStreams = @($attachmentProbe.streams | Where-Object { $_.codec_type -eq 'attachment' })
    Assert-Equal $attachmentStreams.Count 4 'Matroska attachment count'
    Assert-Equal (Get-Tag (Get-PropertyValue $attachmentStreams[0] 'tags') 'filename') 'parity-note.txt' 'Matroska attachment filename'
    Assert-Equal (Get-Tag (Get-PropertyValue $attachmentStreams[0] 'tags') 'mimetype') 'text/plain' 'Matroska attachment MIME type'
    Assert-Equal (Get-Tag (Get-PropertyValue $attachmentStreams[1] 'tags') 'filename') 'parity-font.ttf' 'Matroska font attachment filename'
    Assert-Equal (Get-Tag (Get-PropertyValue $attachmentStreams[1] 'tags') 'mimetype') 'application/x-truetype-font' 'Matroska font attachment MIME type'
    Assert-Equal (Get-Tag (Get-PropertyValue $attachmentStreams[2] 'tags') 'filename') 'parity-font.otf' 'Matroska OpenType attachment filename'
    Assert-Equal (Get-Tag (Get-PropertyValue $attachmentStreams[2] 'tags') 'mimetype') 'application/vnd.ms-opentype' 'Matroska OpenType attachment MIME type'
    Assert-Equal (Get-Tag (Get-PropertyValue $attachmentStreams[3] 'tags') 'filename') 'parity-payload.bin' 'Matroska binary attachment filename'
    Assert-Equal (Get-Tag (Get-PropertyValue $attachmentStreams[3] 'tags') 'mimetype') 'application/octet-stream' 'Matroska binary attachment MIME type'
    Assert-AttachmentPayload $attachmentOutput 0 $attachment 'Matroska text attachment'
    Assert-AttachmentPayload $attachmentOutput 1 $fontAttachment 'Matroska TrueType attachment'
    Assert-AttachmentPayload $attachmentOutput 2 $openTypeAttachment 'Matroska OpenType attachment'
    Assert-AttachmentPayload $attachmentOutput 3 $binaryAttachment 'Matroska binary attachment'

    $malformed = Join-Path $runRoot 'malformed.mkv'
    [IO.File]::WriteAllBytes($malformed, [byte[]](0x00, 0x11, 0x22, 0x33, 0x44))
    $malformedOutput = Join-Path $runRoot 'malformed-output.mkv'
    Invoke-RustConversion 'tv' 'x265' $malformed $malformedOutput -ExpectFailure
    Assert-Condition (-not (Test-Path -LiteralPath $malformedOutput)) 'Malformed input published an output'

    $sourceBytes = [IO.File]::ReadAllBytes($source)
    foreach ($truncation in @(
        [pscustomobject]@{ Name = 'header-only'; Length = 32 },
        [pscustomobject]@{ Name = 'quarter'; Length = [math]::Floor($sourceBytes.Length / 4) },
        [pscustomobject]@{ Name = 'half'; Length = [math]::Floor($sourceBytes.Length / 2) }
    )) {
        $truncatedInput = Join-Path $runRoot "truncated-$($truncation.Name).mp4"
        $truncatedLength = [int]$truncation.Length
        Assert-Condition ($truncatedLength -gt 0 -and $truncatedLength -lt $sourceBytes.Length) "Invalid $($truncation.Name) truncation length"
        [byte[]]$truncatedBytes = $sourceBytes[0..($truncatedLength - 1)]
        [IO.File]::WriteAllBytes($truncatedInput, $truncatedBytes)
        $truncatedOutput = Join-Path $runRoot "truncated-$($truncation.Name)-output.mkv"
        Invoke-RustConversion 'tv' 'x265' $truncatedInput $truncatedOutput -ExpectFailure
        Assert-Condition (-not (Test-Path -LiteralPath $truncatedOutput)) "Truncated $($truncation.Name) input published an output"
    }

    $malformedSubtitleOutput = Join-Path $runRoot 'malformed-subtitle-output.mkv'
    Invoke-RustConversion 'tv' 'x265' $malformedSubtitleSource $malformedSubtitleOutput -ExpectFailure
    Assert-Condition (-not (Test-Path -LiteralPath $malformedSubtitleOutput)) 'Malformed subtitle published an output'

    $existingOutput = Join-Path $runRoot 'existing-output.mkv'
    [IO.File]::WriteAllBytes($existingOutput, [Text.Encoding]::UTF8.GetBytes('do-not-overwrite'))
    Invoke-RustConversion 'tv' 'x265' $source $existingOutput -ExpectFailure
    Assert-Equal ([IO.File]::ReadAllText($existingOutput)) 'do-not-overwrite' 'Existing output protection'

    $lockedOutput = Join-Path $runRoot 'locked-input-output.mkv'
    $lock = [IO.File]::Open($source, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::None)
    try {
        Invoke-RustConversion 'tv' 'x265' $source $lockedOutput -ExpectFailure
    } finally {
        $lock.Dispose()
    }
    Assert-Condition (-not (Test-Path -LiteralPath $lockedOutput)) 'Locked input published an output'

    $cancelledOutput = Join-Path $runRoot 'cancelled-output.mkv'
    Invoke-RustConversion 'tv' 'x265' $source $cancelledOutput -CancelAfterMilliseconds 0 -ExpectFailure
    Assert-Condition (-not (Test-Path -LiteralPath $cancelledOutput)) 'Cancelled conversion published an output'

    $diskFullOutput = Join-Path $runRoot 'disk-full-output.mkv'
    Invoke-RustConversion 'tv' 'x265' $source $diskFullOutput -DiskFullAfterMuxWrites 8 -ExpectFailure
    Assert-Condition (-not (Test-Path -LiteralPath $diskFullOutput)) 'Simulated disk-full conversion published an output'
    $diskFullLog = [IO.File]::ReadAllText((Join-Path $runRoot 'disk-full-output.log'))
    Assert-Condition ($diskFullLog.Contains('No space left on device')) 'Simulated disk-full error was not reported clearly'
    $partialFiles = @(Get-ChildItem -LiteralPath $runRoot -File | Where-Object { $_.Name -like '*.videoferry-partial-*' })
    Assert-Equal $partialFiles.Count 0 'Failure cleanup left partial outputs'

    $results | Format-Table -AutoSize
    $matchedCount = @($results | Where-Object { $_.Result -eq 'matched' }).Count
    $repairCount = @($results | Where-Object { $_.Result -eq 'Rust repair' }).Count
    Write-Output "Workflow/encoder gates passed: $($results.Count) ($matchedCount matched, $repairCount Rust repairs)"
    $hardwareCount = @($results | Where-Object { $_.Case -like '*-nvenc' }).Count
    if ($hardwareCount -gt 0) {
        Write-Output "NVENC hardware gates passed: $hardwareCount across $($availableNvencEncoders.Count) codecs"
    } elseif ($SkipHardware) {
        Write-Output 'NVENC hardware gates skipped by request.'
    } else {
        Write-Output 'NVENC hardware gates skipped because no encoder passed the runtime probe.'
    }
    Write-Output 'Safety gates passed: sparse-subtitle rejection, malformed-subtitle rejection, attachment metadata/payload preservation, garbage/truncated/locked input, existing output, cancellation, simulated disk-full, partial cleanup'
    if ($KeepArtifacts) {
        Write-Output "Artifacts retained: $runRoot"
    } else {
        Write-Output 'Temporary artifacts will be cleaned.'
    }
    $succeeded = $true
} finally {
    Remove-Item Env:VIDEOFERRY_MODE -ErrorAction SilentlyContinue
    Remove-Item Env:VIDEOFERRY_ENCODER -ErrorAction SilentlyContinue
    Remove-Item Env:VIDEOFERRY_CANCEL_AFTER_MS -ErrorAction SilentlyContinue
    Remove-Item Env:VIDEOFERRY_TEST_DISK_FULL_AFTER_MUX_WRITES -ErrorAction SilentlyContinue
    Remove-Item Env:VIDEOFERRY_SLIDESHOW -ErrorAction SilentlyContinue
    Remove-Item Env:VIDEOFERRY_SLIDESHOW_FPS -ErrorAction SilentlyContinue
    Remove-Item Env:VIDEOFERRY_SLIDESHOW_INTERVAL -ErrorAction SilentlyContinue
    Remove-Item Env:VIDEOFERRY_SLIDESHOW_AUDIO -ErrorAction SilentlyContinue
    Remove-Item Env:VIDEOFERRY_SLIDESHOW_COLLAGE -ErrorAction SilentlyContinue
    if ($succeeded -and -not $KeepArtifacts) {
        $resolvedRunsRoot = [IO.Path]::GetFullPath($runsRoot).TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
        $resolvedRunRoot = [IO.Path]::GetFullPath($runRoot)
        if (-not $resolvedRunRoot.StartsWith($resolvedRunsRoot, [StringComparison]::OrdinalIgnoreCase)) {
            throw "Refusing to remove unexpected parity directory: $resolvedRunRoot"
        }
        Remove-Item -LiteralPath $resolvedRunRoot -Recurse -Force
    } elseif (-not $succeeded) {
        Write-Warning "Parity artifacts retained after failure: $runRoot"
    }
}
