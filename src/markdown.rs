//! Rendering a markdown answer that is still arriving.
//!
//! The REPL used to hold a whole answer so termimad could render it in one
//! pass, which meant the surface built for conversation was the one that sat
//! silent longest: the tokens were already streaming, and every one of them was
//! buffered out of sight. This renders as the answer lands instead, without
//! giving up the formatting.
//!
//! Markdown is only meaningful in whole lines -- `- ` is a bullet at the start
//! of one and a hyphen anywhere else -- so a line is held until its newline
//! arrives and then rendered. Fenced code blocks are the exception: their lines
//! mean nothing apart, so an open fence buffers until it closes and then goes
//! out as one block. Prose is the bulk of an answer and streams line by line;
//! a code block lands whole, exactly as the whole answer used to.
//!
//! Deciding *what* to render is separated from rendering it: `Segments` is a
//! pure state machine returning the pieces that have become renderable, which
//! is the part with the edge cases and the part the tests drive.

use termimad::MadSkin;

/// True for a line that opens or closes a fenced code block.
///
/// Indentation is allowed before the fence because a fence nested in a list
/// item carries it, and the info string after the backticks (```rust) is part
/// of the opening fence rather than something to match on.
fn is_fence(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

/// Cuts a stream of tokens into the largest pieces that can be rendered on
/// their own: one line of prose, or one complete fenced block.
#[derive(Default)]
struct Segments {
    /// The line being assembled, without its terminating newline.
    pending: String,
    /// Lines held while a fence is open, the opening fence line included.
    fenced: Vec<String>,
    /// Whether a ``` fence is currently open.
    in_fence: bool,
}

impl Segments {
    /// Take the next fragment of the answer, returning whatever became
    /// renderable. Fragments are whatever the model emitted, so a token may
    /// hold no newline, several, or start mid-word.
    fn push(&mut self, token: &str) -> Vec<String> {
        let mut ready = Vec::new();
        for ch in token.chars() {
            if ch != '\n' {
                self.pending.push(ch);
                continue;
            }
            let line = std::mem::take(&mut self.pending);
            if let Some(segment) = self.line(line) {
                ready.push(segment);
            }
        }
        ready
    }

    /// One completed line, newline already stripped.
    fn line(&mut self, line: String) -> Option<String> {
        if self.in_fence {
            let closing = is_fence(&line);
            self.fenced.push(line);
            if !closing {
                return None;
            }
            // The fence is complete, so the block finally means something as a
            // whole; render it and go back to line-at-a-time prose.
            self.in_fence = false;
            return Some(std::mem::take(&mut self.fenced).join("\n"));
        }
        if is_fence(&line) {
            self.in_fence = true;
            self.fenced.push(line);
            return None;
        }
        Some(line)
    }

    /// Whatever the stream ended on.
    ///
    /// A model rarely ends its answer with a newline, so the last line is
    /// normally still pending here. An unclosed fence is returned as the plain
    /// text it is rather than held back: a truncated answer that reaches the
    /// reader beats a correctly formatted one that does not.
    fn finish(&mut self) -> Vec<String> {
        let mut ready = Vec::new();
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            if let Some(segment) = self.line(line) {
                ready.push(segment);
            }
        }
        if !self.fenced.is_empty() {
            self.in_fence = false;
            ready.push(std::mem::take(&mut self.fenced).join("\n"));
        }
        ready
    }
}

/// Accumulates streamed tokens and prints them as markdown, a piece at a time.
pub struct MarkdownStream {
    skin: MadSkin,
    segments: Segments,
}

impl MarkdownStream {
    pub fn new() -> Self {
        Self {
            skin: MadSkin::default_dark(),
            segments: Segments::default(),
        }
    }

    /// Take the next fragment of the answer and print whatever it completed.
    pub fn push(&mut self, token: &str) {
        for segment in self.segments.push(token) {
            self.skin.print_text(&segment);
        }
    }

    /// Print whatever the stream ended on.
    pub fn finish(&mut self) {
        for segment in self.segments.finish() {
            self.skin.print_text(&segment);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed an answer one character at a time, the worst case a real stream can
    /// present, and collect every segment in order.
    fn drip(answer: &str) -> Vec<String> {
        let mut segments = Segments::default();
        let mut out = Vec::new();
        for ch in answer.chars() {
            out.extend(segments.push(&ch.to_string()));
        }
        out.extend(segments.finish());
        out
    }

    #[test]
    fn a_line_is_held_until_its_newline_lands() {
        let mut s = Segments::default();
        assert!(s.push("The refund window is 30 days").is_empty());
        assert_eq!(s.push("\n"), vec!["The refund window is 30 days"]);
    }

    /// The whole point of the change: prose does not wait for the answer to end.
    #[test]
    fn prose_streams_line_by_line() {
        let mut s = Segments::default();
        assert_eq!(
            s.push("First line.\nSecond line.\n"),
            vec!["First line.", "Second line."]
        );
    }

    /// A model that does not end on a newline still gets its last line out.
    #[test]
    fn the_trailing_line_is_flushed_on_finish() {
        assert_eq!(drip("one\ntwo"), vec!["one", "two"]);
    }

    /// Rendered per line, the fence markers and the code between them would each
    /// be read as prose. They have to arrive together.
    #[test]
    fn a_fenced_block_is_held_until_it_closes() {
        let mut s = Segments::default();
        assert_eq!(s.push("Here:\n```rust\nfn main() {}\n"), vec!["Here:"]);
        assert_eq!(s.push("```\n"), vec!["```rust\nfn main() {}\n```"]);
    }

    #[test]
    fn prose_after_a_fence_goes_back_to_streaming() {
        assert_eq!(
            drip("```\ncode\n```\nafter\n"),
            vec!["```\ncode\n```", "after"]
        );
    }

    /// A truncated answer must not swallow the code it did manage to send.
    #[test]
    fn an_unclosed_fence_is_flushed_rather_than_lost() {
        assert_eq!(drip("```rust\nfn main() {}"), vec!["```rust\nfn main() {}"]);
    }

    /// Token boundaries are the model's, not ours: the same answer must segment
    /// the same whether it arrives a character, a chunk, or all at once.
    #[test]
    fn chunking_does_not_change_the_segments() {
        let answer = "Intro line.\n\n- one\n- two\n\n```sh\nknaix chat \"hi\"\n```\nDone.";
        let by_char = drip(answer);

        let mut whole = Segments::default();
        let mut at_once = whole.push(answer);
        at_once.extend(whole.finish());

        let mut awkward = Segments::default();
        let mut in_chunks = Vec::new();
        // Splits that fall mid-word and mid-fence, as a real stream does.
        for chunk in [
            "Intro li",
            "ne.\n\n- on",
            "e\n- two\n\n``",
            "`sh\nknaix ch",
            "at \"hi\"\n``",
            "`\nDone.",
        ] {
            in_chunks.extend(awkward.push(chunk));
        }
        in_chunks.extend(awkward.finish());

        assert_eq!(by_char, at_once);
        assert_eq!(by_char, in_chunks);
    }

    /// The blank line between two paragraphs is a segment of its own; dropping
    /// it would run them together.
    #[test]
    fn blank_lines_between_paragraphs_survive() {
        assert_eq!(drip("one\n\ntwo\n"), vec!["one", "", "two"]);
    }

    #[test]
    fn an_indented_fence_still_counts() {
        assert!(is_fence("```"));
        assert!(is_fence("```rust"));
        assert!(is_fence("  ```"));
        assert!(!is_fence("not ``` a fence"));
    }

    #[test]
    fn nothing_is_segmented_from_an_empty_answer() {
        assert!(drip("").is_empty());
    }

    /// Multibyte input must not be split or dropped: the stream is chars, and a
    /// token boundary can fall anywhere.
    #[test]
    fn non_ascii_text_survives_a_character_at_a_time() {
        assert_eq!(drip("café ☕\nnext\n"), vec!["café ☕", "next"]);
    }
}
