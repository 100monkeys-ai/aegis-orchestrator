// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The speaker-turn image's program (ADR-143 Update S8, S8a and S8b).
//!
//! `docker/Dockerfile.model-whisper` writes two files into the image with
//! `printf '%s\n' '<line>' ... > <path>`: `turns.awk`, which renders
//! `whisper-cli -tdrz` standard output as speaker turns, and the `transcribe`
//! program. These tests take both from the Dockerfile as the image writes
//! them and run them here:
//!
//! - `turns.awk` under mawk (the image's awk, Debian's) on a fixture of real
//!   `whisper-cli -tdrz` standard output with timestamps on;
//! - `transcribe` under `sh` with stand-ins for `whisper-cli` and `ffmpeg`
//!   that record their arguments, once for a `-tdrz` model and once for any
//!   other, so the `-tdrz` path runs only for a `-tdrz` model and the other
//!   path's argument list is the one it has always been.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn dockerfile() -> String {
    std::fs::read_to_string(repo_root().join("docker/Dockerfile.model-whisper"))
        .expect("docker/Dockerfile.model-whisper is readable")
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap_or_else(|e| panic!("the fixture {name} is readable: {e}"))
}

/// The files the Dockerfile writes with `printf '%s\n'`, by the path each is
/// redirected to, as `printf` writes them: each single-quoted argument one
/// line.
fn printed_files(dockerfile: &str) -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    let mut collecting: Option<Vec<String>> = None;
    for line in dockerfile.lines() {
        let trimmed = line.trim();
        if trimmed.ends_with(r"printf '%s\n' \") {
            collecting = Some(Vec::new());
            continue;
        }
        let Some(lines) = collecting.as_mut() else {
            continue;
        };
        if let Some(target) = trimmed.strip_prefix("> ") {
            let path = target.split_whitespace().next().unwrap().to_string();
            let mut content = lines.join("\n");
            content.push('\n');
            files.insert(path, content);
            collecting = None;
        } else if let Some(quoted) = trimmed
            .strip_suffix(" \\")
            .and_then(|l| l.strip_prefix('\''))
            .and_then(|l| l.strip_suffix('\''))
        {
            lines.push(quoted.to_string());
        } else {
            panic!("a printf argument the test cannot read: {line}");
        }
    }
    files
}

const TURNS_AWK: &str = "/usr/local/share/transcribe/turns.awk";
const TRANSCRIBE: &str = "/usr/local/bin/transcribe";

fn image_file(path: &str) -> String {
    printed_files(&dockerfile())
        .remove(path)
        .unwrap_or_else(|| panic!("the Dockerfile writes no {path}"))
}

fn mawk() -> PathBuf {
    for dir in ["/usr/bin", "/bin", "/usr/local/bin"] {
        let candidate = Path::new(dir).join("mawk");
        if candidate.exists() {
            return candidate;
        }
    }
    panic!("mawk is not installed here; the image renders turns with mawk, Debian's awk");
}

/// `turns.awk` run under mawk on `input`.
fn render(input: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let awk = dir.path().join("turns.awk");
    std::fs::write(&awk, image_file(TURNS_AWK)).unwrap();
    let segments = dir.path().join("segments.txt");
    std::fs::write(&segments, input).unwrap();
    let out = Command::new(mawk())
        .arg("-f")
        .arg(&awk)
        .arg(&segments)
        .output()
        .expect("mawk runs");
    assert!(
        out.status.success(),
        "turns.awk failed under mawk: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// The fixture `transcribe-speakers-tdrz-stdout.txt`, the first nine lines of
/// `whisper-cli -tdrz` standard output (timestamps on) on whisper.cpp's
/// Apollo 13 sample, rendered as S8b says: a segment with no marker inside a
/// turn joins the next with one space, and the last turn, which ends with no
/// marker, closes at the end.
const RENDERED_FIXTURE: &str = "\
Speaker 1: Okay Houston, we've had a problem here.

Speaker 2: This is Houston. Say again, please.

Speaker 1: Uh Houston, we've had a problem. We've had a main beam up on a volt.

Speaker 2: Roger, main beam under volt.

Speaker 1: Uh uh uh

Speaker 2: So okay, stand by thirteen, we're looking at it.

Speaker 1: Okay, uh right now uh Houston, the uh voltage is uh is looking good um.
";

/// S8b: turns become paragraphs labelled Speaker 1 and Speaker 2 in turn,
/// separated by one blank line, with the timestamps and markers removed.
#[test]
fn turns_awk_renders_tdrz_segments_as_speaker_paragraphs() {
    assert_eq!(
        render(&fixture("transcribe-speakers-tdrz-stdout.txt")),
        RENDERED_FIXTURE
    );
}

/// S8b: a recording with no turn detected is one paragraph, `Speaker 1: `.
#[test]
fn turns_awk_renders_a_recording_with_no_turn_as_one_paragraph() {
    let segments = "\n\
[00:00:00.000 --> 00:00:04.000]   One voice reads the first line.\n\
[00:00:04.000 --> 00:00:09.500]   And the same voice reads the second.\n";
    assert_eq!(
        render(segments),
        "Speaker 1: One voice reads the first line. And the same voice reads the second.\n"
    );
}

/// What `transcribe` did in one run: the arguments `whisper-cli` received
/// (its working directory written `<work>`), what it printed and what it
/// wrote to `--out`.
struct Run {
    whisper_args: Vec<String>,
    stdout: String,
    transcript: String,
}

/// Runs the image's `transcribe` on an audio file in an image whose
/// `/opt/whisper/model` names `model`. `whisper-cli` is a stand-in that
/// records its arguments, writes "  the base model text" to its
/// `--output-file` `.txt` file and prints the `-tdrz` fixture on standard
/// output, as the real one prints its segments; `ffmpeg` writes an empty wav;
/// `awk` is mawk.
fn run_transcribe(model: &str) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let bin = root.join("bin");
    let opt = root.join("opt/whisper");
    let share = root.join("share/transcribe");
    for d in [&bin, &opt, &share] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(opt.join("model"), format!("{model}\n")).unwrap();
    std::fs::write(opt.join("ggml-model.bin"), b"").unwrap();
    std::fs::write(share.join("turns.awk"), image_file(TURNS_AWK)).unwrap();
    let program = image_file(TRANSCRIBE)
        .replace("/opt/whisper/", &format!("{}/", opt.display()))
        .replace(
            "/usr/local/share/transcribe/",
            &format!("{}/", share.display()),
        );
    let transcribe = root.join("transcribe");
    std::fs::write(&transcribe, program).unwrap();

    let stdout_fixture = root.join("tdrz-stdout.txt");
    std::fs::write(
        &stdout_fixture,
        fixture("transcribe-speakers-tdrz-stdout.txt"),
    )
    .unwrap();
    let args_file = root.join("whisper-args.txt");
    std::fs::write(
        bin.join("whisper-cli"),
        format!(
            "#!/bin/sh\n\
             for a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{args}'\n\
             prev=''\n\
             for a in \"$@\"; do if [ \"$prev\" = --output-file ]; then out=$a; fi; prev=$a; done\n\
             printf '  the base model text\\n' > \"$out.txt\"\n\
             cat '{stdout}'\n",
            args = args_file.display(),
            stdout = stdout_fixture.display(),
        ),
    )
    .unwrap();
    std::fs::write(
        bin.join("ffmpeg"),
        "#!/bin/sh\nfor a in \"$@\"; do last=$a; done\n: > \"$last\"\n",
    )
    .unwrap();
    for tool in ["whisper-cli", "ffmpeg"] {
        Command::new("chmod")
            .arg("0755")
            .arg(bin.join(tool))
            .status()
            .unwrap();
    }
    std::os::unix::fs::symlink(mawk(), bin.join("awk")).unwrap();

    let audio = root.join("DIALOGUE.ogg");
    std::fs::write(&audio, b"not decoded by the stand-in").unwrap();
    let out_path = root.join("out/transcript.txt");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new("sh")
        .arg(&transcribe)
        .arg(&audio)
        .arg("--out")
        .arg(&out_path)
        .env("PATH", path)
        .output()
        .expect("sh runs transcribe");
    assert!(
        out.status.success(),
        "transcribe failed for model {model}: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let raw: Vec<String> = std::fs::read_to_string(&args_file)
        .expect("transcribe ran whisper-cli")
        .lines()
        .map(str::to_string)
        .collect();
    let file_arg = raw
        .iter()
        .position(|a| a == "--file")
        .and_then(|i| raw.get(i + 1))
        .expect("whisper-cli got --file");
    let work = Path::new(file_arg).parent().unwrap().display().to_string();
    let whisper_args = raw
        .iter()
        .map(|a| {
            a.replace(&work, "<work>")
                .replace(&opt.display().to_string(), "/opt/whisper")
        })
        .collect();
    Run {
        whisper_args,
        stdout: String::from_utf8(out.stdout).unwrap(),
        transcript: std::fs::read_to_string(&out_path).expect("transcribe wrote --out"),
    }
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|a| a.to_string()).collect()
}

/// S8b: for a model whose name ends in `-tdrz`, `whisper-cli` runs in English
/// with timestamps on (no `--no-timestamps`) and `-tdrz`, and the transcript
/// is its standard output rendered as turns, printed and written to `--out`.
#[test]
fn transcribe_runs_the_tdrz_path_for_a_tdrz_model() {
    let run = run_transcribe("small.en-tdrz");
    assert_eq!(
        run.whisper_args,
        args(&[
            "--model",
            "/opt/whisper/ggml-model.bin",
            "--file",
            "<work>/audio.wav",
            "--language",
            "en",
            "--temperature",
            "0",
            "--no-fallback",
            "--no-prints",
            "-tdrz",
            "--output-txt",
            "--output-file",
            "<work>/transcript",
        ])
    );
    assert_eq!(run.transcript, RENDERED_FIXTURE);
    assert_eq!(run.stdout, RENDERED_FIXTURE);
}

/// The argument list every model but a `-tdrz` one has run with, unchanged
/// (`docker/Dockerfile.model-whisper` 86 before the speaker-turn image).
const BASE_ARGS: &[&str] = &[
    "--model",
    "/opt/whisper/ggml-model.bin",
    "--file",
    "<work>/audio.wav",
    "--language",
    "auto",
    "--temperature",
    "0",
    "--no-fallback",
    "--no-timestamps",
    "--no-prints",
    "--output-txt",
    "--output-file",
    "<work>/transcript",
];

/// S8b: every model whose name does not end in `-tdrz` runs as before: the
/// same `whisper-cli` arguments, the transcript read from the `.txt` file
/// with leading blanks removed, no turn rendering.
#[test]
fn transcribe_runs_the_base_path_for_any_other_model() {
    for model in ["base", "small.en", "small.en-tdrz-not"] {
        let run = run_transcribe(model);
        assert_eq!(run.whisper_args, args(BASE_ARGS), "model {model}");
        assert_eq!(run.transcript, "the base model text\n", "model {model}");
        assert_eq!(run.stdout, "the base model text\n", "model {model}");
    }
}

/// S8a: the image records the model it was built with at `/opt/whisper/model`,
/// written at build from `WHISPER_MODEL`, which `transcribe` reads.
#[test]
fn the_image_records_its_model_name() {
    let dockerfile = dockerfile();
    assert!(
        dockerfile.contains(r#"printf '%s\n' "${WHISPER_MODEL}" > /out/model"#),
        "the build stage does not write WHISPER_MODEL to /out/model"
    );
    assert!(
        dockerfile
            .lines()
            .any(|l| l == "COPY --from=build /out/model /opt/whisper/model"),
        "the image does not carry /opt/whisper/model"
    );
    assert!(
        image_file(TRANSCRIBE).contains("model=$(cat /opt/whisper/model)"),
        "transcribe does not read /opt/whisper/model"
    );
}

/// S8a: the weights' location is a build argument, `WHISPER_MODEL_URL`, whose
/// default is the location the base model has always been fetched from; the
/// base defaults are unchanged.
#[test]
fn the_dockerfile_takes_its_model_url_by_build_argument() {
    let dockerfile = dockerfile();
    let lines: Vec<&str> = dockerfile.lines().collect();
    for expected in [
        "ARG WHISPER_MODEL=base",
        "ARG WHISPER_MODEL_SHA256=60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe",
        "ARG WHISPER_MODEL_URL=https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-${WHISPER_MODEL}.bin",
        "ADD ${WHISPER_MODEL_URL} /tmp/ggml-model.bin",
    ] {
        assert!(
            lines.contains(&expected),
            "the Dockerfile has no line {expected:?}"
        );
    }
    assert!(
        dockerfile
            .contains(r#"echo "${WHISPER_MODEL_SHA256}  /tmp/ggml-model.bin" | sha256sum -c -"#),
        "the weights are not checked against WHISPER_MODEL_SHA256"
    );
}
