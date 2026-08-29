// SPDX-License-Identifier: GPL-3.0-only
//! Splitting normalized text into synthesis chunks.
//!
//! # Why chunk at all
//!
//! Two reasons, and they pull in opposite directions.
//!
//! A model may declare `max_input_chars`, and text over it is simply refused —
//! most local models have a hard input length. That is a correctness bound.
//!
//! The other reason is latency: nothing is heard until the first chunk comes
//! back, so a short first chunk gets speech started while the rest is still
//! synthesizing. But every extra split costs a round trip and resets the
//! model's prosodic context, which is audible as a flatter reading. So the
//! policy here is deliberately asymmetric — one small chunk to kill dead air,
//! then as few splits as the model's limit allows.
//!
//! # Sentence boundaries
//!
//! Splitting mid-sentence sounds wrong in a way splitting between sentences
//! does not, so the chunker only ever cuts at a boundary — except when a single
//! "sentence" exceeds the model's limit, where it falls back to a word break
//! rather than refusing to speak.
//!
//! Finding those boundaries is most of the work, and getting it wrong is
//! audible: "Dr. Smith arrived" split after `Dr.` produces two utterances with
//! a pause and a dropped intonation. See [`is_boundary`].

/// Sentence-ending punctuation.
const TERMINATORS: [char; 6] = ['.', '!', '?', '。', '！', '？'];

/// Words that end in `.` without ending a sentence.
///
/// Lower-cased for comparison. This list is short on purpose: every entry is a
/// judgement that the word is more often an abbreviation than a sentence end,
/// and a wrong entry silently merges two sentences, which is a milder failure
/// than splitting one.
const ABBREVIATIONS: &[&str] = &[
    "mr", "mrs", "ms", "dr", "prof", "sr", "jr", "st", "mt", "rev", "hon", "vs", "etc", "eg", "ie",
    "al", "fig", "no", "vol", "pp", "approx", "inc", "ltd", "co", "corp", "dept", "est", "min",
    "max", "cf", "ca", "circa", "jan", "feb", "mar", "apr", "jun", "jul", "aug", "sep", "sept",
    "oct", "nov", "dec", "mon", "tue", "wed", "thu", "fri", "sat", "sun",
];

/// How chunking behaves for one model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkPolicy {
    /// Hard limit from the model's manifest. `None` means unbounded, and the
    /// chunker then emits one chunk per utterance apart from the opening one.
    pub max_chars: Option<usize>,
    /// Target size for the first chunk of an utterance. Kept small so speech
    /// starts quickly; `0` disables the short opener.
    pub first_chunk_chars: usize,
}

impl Default for ChunkPolicy {
    fn default() -> Self {
        Self {
            max_chars: None,
            // Roughly one short sentence — enough to carry a clause, small
            // enough that the first synthesis returns fast.
            first_chunk_chars: 120,
        }
    }
}

impl ChunkPolicy {
    /// A policy for a model declaring `max_input_chars`.
    #[must_use]
    pub fn for_model(max_chars: Option<u32>) -> Self {
        Self {
            max_chars: max_chars.map(|c| c as usize),
            ..Self::default()
        }
    }

    /// The size this chunk is aiming for, given how many came before.
    fn target(self, emitted: usize) -> usize {
        let cap = self.max_chars.unwrap_or(usize::MAX);
        if emitted == 0 && self.first_chunk_chars > 0 {
            self.first_chunk_chars.min(cap)
        } else {
            cap
        }
    }
}

/// Streaming sentence chunker.
#[derive(Debug)]
pub struct Chunker {
    policy: ChunkPolicy,
    buf: String,
    emitted: usize,
}

impl Chunker {
    /// A chunker following `policy`.
    #[must_use]
    pub fn new(policy: ChunkPolicy) -> Self {
        Self {
            policy,
            buf: String::new(),
            emitted: 0,
        }
    }

    /// Feed normalized text; returns every chunk that is now complete.
    pub fn push(&mut self, text: &str) -> Vec<String> {
        self.buf.push_str(text);
        let mut out = Vec::new();
        while let Some(chunk) = self.take_ready() {
            out.push(chunk);
        }
        out
    }

    /// Flush whatever remains as a final chunk.
    pub fn finish(&mut self) -> Vec<String> {
        let mut out = self.push("");
        let rest = self.buf.trim().to_string();
        self.buf.clear();
        self.emitted = 0;
        if !rest.is_empty() {
            out.push(rest);
        }
        out
    }

    /// Pull one chunk if the buffer holds a complete one.
    fn take_ready(&mut self) -> Option<String> {
        let target = self.policy.target(self.emitted);
        let len = self.buf.chars().count();

        // Nothing is emitted until the buffer has actually outgrown its
        // target. Splitting at every boundary the moment one appears would
        // turn "Hello there. How are you?" into two requests for no reason —
        // and each extra split costs a round trip and resets the model's
        // prosodic context. It is also what makes streaming and one-shot agree:
        // both fire on the same content-based condition rather than on when
        // the text happened to arrive.
        if len <= target {
            return None;
        }

        // Prefer the last sentence boundary at or before the target: it packs
        // the chunk as full as the policy allows without overshooting.
        let split = last_boundary_within(&self.buf, target).or_else(|| {
            // No boundary fits. Only force a split when the text has actually
            // outgrown the hard limit — otherwise wait for more input rather
            // than cutting mid-sentence.
            match self.policy.max_chars {
                Some(max) if len > max => Some(word_break_before(&self.buf, max)),
                _ => None,
            }
        })?;

        let head: String = self.buf.chars().take(split).collect();
        let rest: String = self.buf.chars().skip(split).collect();
        let chunk = head.trim().to_string();
        self.buf = rest.trim_start().to_string();
        if chunk.is_empty() {
            return None;
        }
        self.emitted += 1;
        Some(chunk)
    }
}

/// Character index just past the last sentence boundary at or before `limit`,
/// or `None` if there is none.
fn last_boundary_within(s: &str, limit: usize) -> Option<usize> {
    let chars: Vec<char> = s.chars().collect();
    let end = limit.min(chars.len());
    // The split lands past any closing quote, not straight after the
    // terminator — otherwise `He said "go now."` is cut before its own closing
    // quote, which then opens the next chunk.
    (0..end).rev().find_map(|i| boundary_end(&chars, i))
}

/// Whether `chars[i]` ends a sentence.
///
/// The cases that matter, each of which is audible when it goes wrong:
///
/// - **Decimals and versions.** The `.` in `3.14` or `v1.2` has digits on both
///   sides. Splitting there says "three point" and then "one four".
/// - **Abbreviations.** `Dr.`, `etc.`, `Fig.` — see [`ABBREVIATIONS`].
/// - **Initials.** A single capital before the `.`, as in `J. R. R. Tolkien`.
/// - **Ellipses.** `...` is a pause inside a sentence far more often than an
///   end, so only the last dot of a run is even considered.
/// - **What follows.** A real boundary is followed by whitespace and then
///   something that can start a sentence. `example.com` fails this because `c`
///   is neither.
fn boundary_end(chars: &[char], i: usize) -> Option<usize> {
    let c = chars[i];
    if !TERMINATORS.contains(&c) {
        return None;
    }

    // Trailing quotes and brackets belong to the sentence that is ending, so
    // step over them *before* judging what follows — and the split lands past
    // them. Checking `chars[i + 1]` directly would see the closing quote in
    // `"go now." Then` and conclude the dot was mid-token, like `example.com`.
    let mut j = i + 1;
    while chars
        .get(j)
        .is_some_and(|c| matches!(c, '"' | '\'' | '”' | '’' | ')' | ']'))
    {
        j += 1;
    }

    // CJK terminators are unambiguous: never decimal points, never in
    // abbreviations, never in domain names.
    if matches!(c, '。' | '！' | '？') {
        return Some(j);
    }

    if c == '.' {
        let prev = i.checked_sub(1).map(|p| chars[p]);
        let after = chars.get(j).copied();

        // A decimal point, or a dot inside a version number.
        if prev.is_some_and(|p| p.is_ascii_digit()) && after.is_some_and(|n| n.is_ascii_digit()) {
            return None;
        }
        // Mid-ellipsis: only the final dot of a run is even a candidate.
        if after == Some('.') {
            return None;
        }
        // A dot glued to what follows: `example.com`, `a.b`.
        if after.is_some_and(|n| !n.is_whitespace()) {
            return None;
        }
        // A single capital before the dot is an initial: `J. R. R. Tolkien`.
        let word = word_before(chars, i);
        if word.chars().count() == 1 && word.chars().next().is_some_and(char::is_uppercase) {
            return None;
        }
        if ABBREVIATIONS.contains(&word.to_lowercase().as_str()) {
            return None;
        }
    }

    match chars.get(j) {
        // End of input is a boundary only once no more text is coming, which
        // `finish` handles by flushing the remainder wholesale.
        None => None,
        Some(n) if !n.is_whitespace() => None,
        // What follows has to look like the start of a sentence. This is the
        // general form of the abbreviation list: no enumeration can cover
        // `e.g.`, `p.m.`, or a name nobody thought of, but a lowercase word
        // after a period almost never starts one. It also settles the ellipsis
        // case, where "Well... it depends" continues rather than restarts.
        _ => starts_a_sentence(chars, j).then_some(j),
    }
}

/// Whether the first non-space character at or after `j` could open a sentence.
///
/// Uppercase, a digit, an opening quote or bracket — or any character from a
/// script without letter case, where the test does not apply and a terminator
/// is already unambiguous.
fn starts_a_sentence(chars: &[char], j: usize) -> bool {
    let Some(&c) = chars[j..].iter().find(|c| !c.is_whitespace()) else {
        return false;
    };
    if c.is_lowercase() {
        return false;
    }
    c.is_uppercase()
        || c.is_ascii_digit()
        || matches!(c, '"' | '\'' | '“' | '‘' | '(' | '[')
        // Caseless scripts (CJK, Arabic, Hebrew, …): `is_lowercase` and
        // `is_uppercase` are both false, and treating that as "not a sentence"
        // would merge every sentence in those languages into one chunk.
        || !c.is_alphabetic()
        || (!c.is_lowercase() && !c.is_uppercase())
}

/// The word immediately before `chars[i]`, without its punctuation.
fn word_before(chars: &[char], i: usize) -> String {
    let mut start = i;
    while start > 0 {
        let c = chars[start - 1];
        if c.is_alphanumeric() {
            start -= 1;
        } else {
            break;
        }
    }
    chars[start..i].iter().collect()
}

/// Character index of a word break at or before `limit`, for text with no
/// usable sentence boundary. Falls back to a hard cut when a single word is
/// longer than the limit, since refusing to speak is worse.
fn word_break_before(s: &str, limit: usize) -> usize {
    let chars: Vec<char> = s.chars().collect();
    let end = limit.min(chars.len());
    (0..end)
        .rev()
        .find(|&i| chars[i].is_whitespace())
        .map_or(end, |i| i + 1)
}

#[cfg(test)]
mod tests;
