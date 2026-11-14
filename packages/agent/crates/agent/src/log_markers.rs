//! Container output split into lines, and steamcmd's progress read from them.

use std::iter::Peekable;
use std::str::Chars;
use std::sync::LazyLock;

use regex::Regex;

#[allow(clippy::expect_used, reason = "a fixed pattern, which the tests compile")]
static SGR: LazyLock<Regex> = LazyLock::new(|| Regex::new("\x1b\\[[0-9;]*m").expect("valid pattern"));
#[allow(clippy::expect_used, reason = "a fixed pattern, which the tests compile")]
static PROGRESS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)progress:\s*([0-9]+(?:\.[0-9]+)?)").expect("valid pattern"));
#[allow(clippy::expect_used, reason = "a fixed pattern, which the tests compile")]
static STATE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)Update state \(0x[0-9a-f]+\)\s*([a-z ]+)").expect("valid pattern"));
#[allow(clippy::expect_used, reason = "a fixed pattern, which the tests compile")]
static DONE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)already up to date|fully installed").expect("valid pattern"));
#[allow(clippy::expect_used, reason = "a fixed pattern, which the tests compile")]
static STEP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(Logging in|Connecting|Update state|Error!|ERROR!|Verifying)").expect("valid pattern"));

/// Lines of a chunk that print something, without trailing space. A line that carriage returns
/// redrew keeps its last part that prints something, so a reset or a cleared tail after the last
/// return does not hide the text before it. Colour escapes (SGR, `ESC[...m`) stay for the log viewer;
/// see [`printable`] for what else goes.
#[must_use]
pub(crate) fn split_log_chunk(chunk: &str) -> Vec<String> {
    chunk.split('\n').filter_map(|line| line.split('\r').rev().map(printable).find(|part| prints(part))).map(|line| line.trim_end().to_owned()).collect()
}

/// Whether a line shows anything once its colour escapes are gone.
fn prints(line: &str) -> bool {
    !strip_colours(line).trim().is_empty()
}

/// A line without its colour escapes, as it reads.
#[must_use]
pub(crate) fn strip_colours(line: &str) -> std::borrow::Cow<'_, str> {
    SGR.replace_all(line, "")
}

/// The Unicode format characters that change how a line lays out: the bidirectional marks,
/// embeddings, overrides and isolates, which can make a line display in another order than it reads,
/// and the line and paragraph separators, which some renderers break on, so that one line shows as
/// several.
const LAYOUT: [char; 14] = [
    '\u{061c}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}', '\u{2028}',
    '\u{2029}',
];

/// What a log viewer may render of a line: text, tabs and complete SGR sequences of digits and `;`.
/// Every other escape goes whole, by the ECMA-48 grammar: a control sequence (CSI), a control
/// string (OSC, DCS, SOS, PM, APC, as hyperlinks and clipboard writes are), and every other escape,
/// in their 7-bit and 8-bit forms. So do the remaining controls, carriage returns and backspaces
/// among them, and the layout characters in [`LAYOUT`]. A string never closed ends at the end of the
/// line, so it cannot swallow the next.
#[must_use]
pub(crate) fn printable(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    control_sequence(&mut chars, &mut out);
                }
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    chars.next();
                    skip_control_string(&mut chars);
                }
                // nF: intermediate bytes, then one final byte.
                Some('\x20'..='\x2f') => {
                    while chars.next_if(|c| matches!(c, '\x20'..='\x2f')).is_some() {}
                    chars.next_if(|c| matches!(c, '\x30'..='\x7e'));
                }
                // Fp, Fe and Fs: one byte.
                Some('\x30'..='\x7e') => {
                    chars.next();
                }
                // A lone ESC.
                _ => {}
            },
            // The 8-bit CSI. Colours are kept only in the 7-bit form the viewer reads.
            '\u{9b}' => {
                control_sequence(&mut chars, &mut String::new());
            }
            '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => skip_control_string(&mut chars),
            '\t' => out.push(c),
            c if c.is_control() || LAYOUT.contains(&c) => {}
            c => out.push(c),
        }
    }

    out
}

/// The rest of a control sequence after its `CSI`: parameter bytes, intermediate bytes, then one
/// final byte. Only a complete SGR of digits and `;` is written to `out`. A C0 control inside it is
/// skipped and the sequence goes on, as a terminal reads it, except CAN and SUB, which cancel it. A
/// sequence that breaks off drops what it read and leaves the next character to the caller.
fn control_sequence(chars: &mut Peekable<Chars<'_>>, out: &mut String) {
    let mut parameters = String::new();
    let mut intermediates = false;

    loop {
        while chars.next_if(|c| matches!(c, '\x00'..='\x17' | '\x19' | '\x1c'..='\x1f')).is_some() {}

        if let Some(c) = chars.next_if(|c| matches!(c, '\x30'..='\x3f')) {
            if intermediates {
                // A parameter byte after an intermediate makes the sequence malformed.
                return;
            }
            parameters.push(c);
        } else if chars.next_if(|c| matches!(c, '\x20'..='\x2f')).is_some() {
            intermediates = true;
        } else {
            break;
        }
    }

    if chars.next_if(|c| matches!(c, '\x18' | '\x1a')).is_some() {
        return;
    }

    if chars.next_if_eq(&'m').is_some() {
        if !intermediates && parameters.chars().all(|c| c.is_ascii_digit() || c == ';') {
            out.push_str("\x1b[");
            out.push_str(&parameters);
            out.push('m');
        }
    } else {
        chars.next_if(|c| matches!(c, '\x40'..='\x7e'));
    }
}

/// The rest of a control string: up to BEL or ST (`ESC \\` or U+009C), or to the end of the line.
/// An ESC inside it ends the string and starts the next escape, as a terminal reads it.
fn skip_control_string(chars: &mut Peekable<Chars<'_>>) {
    while let Some(&c) = chars.peek() {
        match c {
            '\x07' | '\u{9c}' => {
                chars.next();
                return;
            }
            '\x1b' => {
                let mut ahead = chars.clone();
                ahead.next();
                if ahead.peek() == Some(&'\\') {
                    chars.next();
                    chars.next();
                }
                return;
            }
            _ => {
                chars.next();
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Cs2UpdateProgress {
    /// 0 to 100, or `None` for a step without a figure.
    pub percent: Option<f64>,
    /// The steamcmd state words, or the whole line when it names no state.
    pub message: String,
}

/// What a steamcmd line says about the update, or `None` for a line that says nothing.
#[must_use]
pub(crate) fn parse_cs2_update_progress(line: &str) -> Option<Cs2UpdateProgress> {
    let printed = printable(line);
    let line = &*strip_colours(&printed);

    if let Some(percent) = PROGRESS.captures(line).and_then(|captures| captures[1].parse::<f64>().ok()) {
        let message = STATE.captures(line).map_or_else(|| line.to_owned(), |captures| captures[1].trim().to_owned());

        return Some(Cs2UpdateProgress { percent: Some(percent.clamp(0.0, 100.0)), message });
    }

    if DONE.is_match(line) {
        return Some(Cs2UpdateProgress { percent: Some(100.0), message: line.to_owned() });
    }

    STEP.is_match(line).then(|| Cs2UpdateProgress { percent: None, message: line.to_owned() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::unnecessary_wraps, reason = "compared with the parser's Option")]
    fn progress(percent: Option<f64>, message: &str) -> Option<Cs2UpdateProgress> {
        Some(Cs2UpdateProgress { percent, message: message.to_owned() })
    }

    #[test]
    fn splits_a_tty_chunk_into_printable_lines() {
        assert_eq!(split_log_chunk("one\r\ntwo\n\nthree   \n"), ["one", "two", "three"]);
    }

    fn one(chunk: &str) -> String {
        let lines = split_log_chunk(chunk);
        assert_eq!(lines.len(), 1, "{chunk:?} gave {lines:?}");
        lines.into_iter().next().unwrap_or_default()
    }

    #[test]
    fn keeps_colour_escapes() {
        assert_eq!(one("\x1b[32mgreen\x1b[0m\n"), "\x1b[32mgreen\x1b[0m");
        assert_eq!(one("\x1b[1;38;5;189mSwiftlyS2\x1b[0m \x1b[38;5;147m|\x1b[0m ok\r\n"), "\x1b[1;38;5;189mSwiftlyS2\x1b[0m \x1b[38;5;147m|\x1b[0m ok");
        assert_eq!(
            one("[Swiftly] [\x1b[32mINFO\x1b[39m] [\x1b[33mGameData\x1b[39m] Loaded 132 offsets."),
            "[Swiftly] [\x1b[32mINFO\x1b[39m] [\x1b[33mGameData\x1b[39m] Loaded 132 offsets."
        );
        assert_eq!(one("\x1b[38;2;255;128;0morange\x1b[m"), "\x1b[38;2;255;128;0morange\x1b[m");
    }

    #[test]
    fn drops_every_other_control_sequence() {
        assert_eq!(one("\x1b[2K\x1b[1Gdone\x1b[?25h\n"), "done");
        // Not an SGR this viewer reads: colon parameters, private parameters, intermediate bytes.
        assert_eq!(one("\x1b[38:5:1mx\x1b[?1;2my\x1b[1 mz"), "xyz");
        // Final bytes outside the letters, and a sequence that breaks off.
        assert_eq!(one("a\x1b[3~b\x1b[@c\x1b[12"), "abc");
        // The 8-bit CSI, colour or not.
        assert_eq!(one("\u{9b}31mred\u{9b}2Kok"), "redok");
    }

    #[test]
    fn drops_control_strings_such_as_links_and_clipboard_writes() {
        assert_eq!(one("\x1b]8;;https://example.com\x1b\\click\x1b]8;;\x1b\\"), "click");
        assert_eq!(one("\x1b]52;c;ZXZpbA==\x07ok"), "ok");
        assert_eq!(one("\x1b]0;title\x07\x1bPq#0;2;0;0;0\x1b\\\x1b_app\x1b\\\x1b^pm\x1b\\\x1bXsos\x1b\\text"), "text");
        assert_eq!(one("\u{9d}0;title\u{9c}\u{90}dcs\u{9c}ok"), "ok");
    }

    // An ESC inside a control string that is not ST ends the string without being consumed, so the
    // escape it starts goes or stays whole. Consuming it left "[31mred" behind as text.
    #[test]
    fn an_esc_that_ends_a_control_string_starts_the_next_escape() {
        assert_eq!(one("\x1b]0;title\x1b[31mred\x1b[0m"), "\x1b[31mred\x1b[0m");
        assert_eq!(one("\x1b]0;title\x1b[2Kok"), "ok");
        assert_eq!(one("\x1b]0;title\x1bcok"), "ok");
        assert_eq!(one("\x1b]0;title\x1b]52;c;ZXZpbA==\x07ok"), "ok");
    }

    #[test]
    fn a_control_string_never_closed_ends_at_its_line() {
        assert_eq!(split_log_chunk("\x1b]0;title\nnext\n"), ["next"]);
    }

    #[test]
    fn drops_other_escapes_and_controls() {
        assert_eq!(one("\x1bcreset\x1b7\x1b8\x1b(0x\x1b"), "resetx");
        assert_eq!(one("a\x08b\x07c\x00d\x0be\x0cf\x7fg\u{85}h"), "abcdefgh");
        assert_eq!(one("a\tb"), "a\tb");
    }

    #[test]
    fn keeps_the_last_part_of_a_redrawn_line_that_prints_something() {
        assert_eq!(one("ok\rFAILED\r\n"), "FAILED");
        assert_eq!(one("progress: 1\rprogress: 2\r"), "progress: 2");
        // The part after the last return replaces the line, even when it is shorter.
        assert_eq!(one("long text here\rshort"), "short");
    }

    #[test]
    fn escapes_after_the_last_carriage_return_do_not_hide_the_line() {
        assert_eq!(one("Done\r\x1b[0m"), "Done");
        assert_eq!(one("text\r \x1b[K"), "text");
    }

    #[test]
    fn a_control_inside_a_control_sequence_is_skipped_unless_it_cancels_it() {
        assert_eq!(one("\x1b[3\x081mX"), "\x1b[31mX");
        // CAN and SUB cancel the sequence; what follows is text again.
        assert_eq!(one("\x1b[31\x18mX"), "mX");
        assert_eq!(one("\x1b[31\x1amX"), "mX");
    }

    #[test]
    fn drops_a_string_terminator_that_ends_nothing() {
        assert_eq!(one("a\u{9c}b\x1b\\c"), "abc");
    }

    #[test]
    fn drops_characters_that_change_the_layout() {
        assert_eq!(one("\u{202e}exe.txt\u{202c} and \u{2066}x\u{2069}"), "exe.txt and x");
        assert_eq!(one("a\u{200e}b\u{200f}c\u{61c}d\u{2028}e\u{2029}f"), "abcdef");
    }

    /// The same xorshift a fuzz run would use, so a failure names a reproducible input.
    fn inputs(count: usize) -> impl Iterator<Item = String> {
        const ALPHABET: [char; 32] = [
            '\x1b', '[', ']', ';', 'm', '0', '1', '9', ':', '?', 'P', '\\', 'X', '^', '_', '\x07', '\u{9b}', '\u{9c}', '\u{9d}', '\u{90}', ' ', '(', '~', '@',
            'a', '\r', '\t', '\x08', '\x18', '\u{202e}', '\u{2028}', '\n',
        ];
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;

        (0..count).map(move |_| {
            (0..16)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    ALPHABET[usize::try_from(seed % 32).unwrap_or(0)]
                })
                .collect()
        })
    }

    // Whatever arrives, a kept line holds text, tabs and complete SGR sequences of digits and `;`
    // alone, and sanitising it again changes nothing.
    #[test]
    fn keeps_nothing_but_text_tabs_and_complete_colours_for_any_input() {
        for input in inputs(100_000) {
            for line in split_log_chunk(&input) {
                let plain = strip_colours(&line);

                assert!(!plain.contains('\x1b'), "{input:?} left an escape that is not a colour: {line:?}");
                assert!(!plain.chars().any(|c| (c.is_control() && c != '\t') || LAYOUT.contains(&c)), "{input:?} left a control: {line:?}");
                assert!(prints(&line), "{input:?} kept a line that prints nothing: {line:?}");
                assert_eq!(printable(&line), line, "{input:?} is not stable under a second pass");
            }
        }
    }

    #[test]
    fn drops_a_line_that_prints_nothing() {
        assert_eq!(split_log_chunk("\x1b[0m  \nnext\n"), ["next"]);
        assert_eq!(split_log_chunk("\x1b[2K\x1b]0;t\x07\u{202e}\n"), Vec::<String>::new());
    }

    #[test]
    fn reads_progress_through_colour_escapes() {
        assert_eq!(parse_cs2_update_progress("\x1b[33mprogress: 12.5 (1 / 8)\x1b[0m"), progress(Some(12.5), "progress: 12.5 (1 / 8)"));
        assert_eq!(
            parse_cs2_update_progress("\x1b[32mUpdate state (0x61) downloading\x1b[0m, progress: 42.35 (1234 / 5678)"),
            progress(Some(42.35), "downloading")
        );
        assert_eq!(parse_cs2_update_progress("\x1b[1mLogging in as anonymous\x1b[0m"), progress(None, "Logging in as anonymous"));
    }

    #[test]
    fn extracts_a_percentage_and_the_state_word() {
        assert_eq!(parse_cs2_update_progress("Update state (0x61) downloading, progress: 42.35 (1234 / 5678)"), progress(Some(42.35), "downloading"));
        assert_eq!(parse_cs2_update_progress("Update state (0x81) verifying update, progress: 7.00 (1 / 2)"), progress(Some(7.0), "verifying update"));
    }

    #[test]
    fn keeps_the_whole_line_when_it_names_no_state() {
        assert_eq!(parse_cs2_update_progress("progress: 50.0 (1 / 2)"), progress(Some(50.0), "progress: 50.0 (1 / 2)"));
    }

    #[test]
    fn reads_a_percentage_and_state_without_a_space_before_them() {
        assert_eq!(parse_cs2_update_progress("Update state (0x61)downloading , progress:42 (1 / 2)"), progress(Some(42.0), "downloading"));
    }

    #[test]
    fn treats_a_success_line_as_complete() {
        assert_eq!(parse_cs2_update_progress("Success! App '730' fully installed."), progress(Some(100.0), "Success! App '730' fully installed."));
    }

    #[test]
    fn passes_interesting_lines_through_without_a_percentage() {
        assert_eq!(parse_cs2_update_progress("Logging in as anonymous"), progress(None, "Logging in as anonymous"));
        assert_eq!(parse_cs2_update_progress("ERROR! Failed to install app 730").map(|p| p.percent), Some(None));
    }

    #[test]
    fn ignores_noise() {
        assert_eq!(parse_cs2_update_progress("random srcds chatter"), None);
        assert_eq!(parse_cs2_update_progress(""), None);
        assert_eq!(parse_cs2_update_progress("srcds: Connecting to the master server"), None);
    }
}
