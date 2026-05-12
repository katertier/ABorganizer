//! Manual probe for the Swift FFI transcribe path.
//!
//! Calls `ab_transcript::transcribe_window` directly against a
//! user-provided audio file + window, then prints the resulting
//! `[TranscriptSegment]` as pretty JSON. Used to validate the
//! end-to-end Speech FFI stack (build script + Swift bridge +
//! Rust safe wrapper + `SpeechAnalyzer`) before the
//! `transcribe-head-tail` pipeline stage gets built on top.
//!
//! Not part of any production code path — one-shot diagnostic.

// xtask: allow_macros — manual-probe binary prints JSON to stdout
// for human inspection + status messages to stderr; tracing would
// over-engineer a one-shot diagnostic.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "transcribe-probe",
    about = "Call ab_transcript::transcribe_window once and print the JSON result."
)]
struct Args {
    /// Audio file to transcribe (m4a, m4b, mp3, wav, aiff — anything
    /// `AVAudioFile` can read). Optional when `--install-model`
    /// is passed (install-only mode).
    path: Option<PathBuf>,
    /// Start of the window in seconds (0-based, inclusive).
    #[arg(long, default_value_t = 0.0)]
    start_secs: f64,
    /// End of the window in seconds (exclusive). Default reflects
    /// the spec'd head-window length for downstream extractors.
    #[arg(long, default_value_t = 60.0)]
    end_secs: f64,
    /// BCP-47 locale hint passed to `SpeechTranscriber`. The
    /// transcriber maps "en" → "en-US" etc.; identifiers it
    /// doesn't recognise fail fast.
    #[arg(long, default_value = "en-US")]
    locale: String,
    /// Install the on-device Speech model for `--locale` before
    /// transcribing (or instead of, when `path` is omitted).
    /// First install can take multiple minutes.
    #[arg(long)]
    install_model: bool,
    /// Run `NLLanguageRecognizer` on the given text and print the
    /// hypothesis. Standalone — does not require `path`. Useful
    /// for the pre-transcribe language gate (feed it concatenated
    /// tag text).
    #[arg(long, value_name = "TEXT")]
    detect_text: Option<String>,
    /// After transcribing, also run post-transcribe language
    /// detection. Drops the first `--lang-skip-ms` of segments
    /// (publisher-jingle window) before feeding the text to
    /// `NLLanguageRecognizer`.
    #[arg(long)]
    detect_language: bool,
    /// Skip this many milliseconds from the transcript head when
    /// running `--detect-language`. Default matches
    /// `LanguageTunables::post_transcribe_skip_ms` (30 s).
    #[arg(long, default_value_t = 30_000)]
    lang_skip_ms: u64,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();

    if args.install_model {
        eprintln!(
            "probe: install_speech_model(locale={}) — may take minutes on first call",
            args.locale,
        );
        if let Err(e) = ab_transcript::install_speech_model(&args.locale).await {
            eprintln!("probe: install_speech_model failed: {e}");
            return ExitCode::from(1);
        }
        eprintln!("probe: install_speech_model ok");
    }

    // Standalone language-detect mode (no audio needed).
    if let Some(text) = args.detect_text.as_deref() {
        eprintln!(
            "probe: detect_language(text=<{} chars>, max_alternatives=3)",
            text.chars().count(),
        );
        match ab_transcript::detect_language(text, 3).await {
            Ok(Some(d)) => match serde_json::to_string_pretty(&d) {
                Ok(s) => {
                    println!("{s}");
                    if args.path.is_none() {
                        return ExitCode::SUCCESS;
                    }
                }
                Err(e) => {
                    eprintln!("probe: JSON encode failed: {e}");
                    return ExitCode::from(2);
                }
            },
            Ok(None) => {
                eprintln!("probe: detect_language returned None (empty / inconclusive)");
                if args.path.is_none() {
                    return ExitCode::SUCCESS;
                }
            }
            Err(e) => {
                eprintln!("probe: detect_language failed: {e}");
                return ExitCode::from(1);
            }
        }
    }

    let Some(path) = args.path else {
        if args.install_model || args.detect_text.is_some() {
            return ExitCode::SUCCESS;
        }
        eprintln!("probe: no path given and no standalone mode requested; nothing to do");
        return ExitCode::from(2);
    };

    eprintln!(
        "probe: path={} window={:.3}..{:.3}s locale={}",
        path.display(),
        args.start_secs,
        args.end_secs,
        args.locale,
    );
    let segments =
        match ab_transcript::transcribe_window(&path, args.start_secs, args.end_secs, &args.locale)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                eprintln!("probe: transcribe_window failed: {e}");
                return ExitCode::from(1);
            }
        };
    eprintln!("probe: {} segment(s) returned", segments.len());
    match serde_json::to_string_pretty(&segments) {
        Ok(s) => println!("{s}"),
        Err(e) => {
            eprintln!("probe: JSON encode failed: {e}");
            return ExitCode::from(2);
        }
    }

    if args.detect_language {
        eprintln!(
            "probe: detect_from_transcript(skip_ms={}, max_alternatives=3)",
            args.lang_skip_ms,
        );
        match ab_transcript::detect_from_transcript(&segments, args.lang_skip_ms, 3).await {
            Ok(Some(d)) => match serde_json::to_string_pretty(&d) {
                Ok(s) => println!("---\n{s}"),
                Err(e) => {
                    eprintln!("probe: JSON encode failed: {e}");
                    return ExitCode::from(2);
                }
            },
            Ok(None) => {
                eprintln!(
                    "probe: post-transcribe detection returned None (all segments skipped or inconclusive)"
                );
            }
            Err(e) => {
                eprintln!("probe: post-transcribe detection failed: {e}");
                return ExitCode::from(1);
            }
        }
    }
    ExitCode::SUCCESS
}
