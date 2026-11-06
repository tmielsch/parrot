use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

pub fn normalize_text_for_tts(text: &str) -> String {
    if text.trim().is_empty() {
        return String::new();
    }
    // Selected text from web pages and PDFs is not Markdown: leading
    // spaces or tabs would turn whole paragraphs into indented code blocks
    // ("Code example omitted." and the rest was dropped). Only fenced
    // blocks (```) still count as code.
    let text = dedent_lines(text);
    let text = text.as_str();

    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_SMART_PUNCTUATION);

    let parser = Parser::new_ext(text, options);
    let mut renderer = SpeechTextRenderer::default();

    for event in parser {
        renderer.handle_event(event);
    }

    clarify_pauses(&renderer.finish())
}

/// Lines at least this long that end without punctuation are taken as
/// wrapped prose (PDF, narrow columns) and joined with the next line.
/// Shorter ones are headings, labels or list entries and stay separate.
const WRAPPED_LINE_MIN_CHARS: usize = 60;

/// Bullet glyphs from web pages and PDFs; rewritten to Markdown "- ".
const BULLET_GLYPHS: &[char] = &['•', '·', '◦', '▪', '▫', '‣', '●', '○', '■', '□', '➤', '►', '▶', '✓', '✔', '→'];

/// Prepares selected text for the Markdown pass:
///
/// - removes leading indentation outside fenced code blocks (indented
///   paragraphs would otherwise become code blocks and be skipped);
/// - turns bullet glyphs into Markdown list items;
/// - keeps single line breaks: each short line becomes its own block so
///   it gets a pause, while long unpunctuated lines are joined (wrapped
///   prose, no period in the middle of a sentence).
fn dedent_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 64);
    let mut in_fence = false;
    let mut prev: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            if !in_fence {
                out.push('\n');
            }
            in_fence = !in_fence;
            out.push_str(trimmed);
            out.push('\n');
            prev = None;
            continue;
        }
        if in_fence {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if trimmed.is_empty() {
            out.push('\n');
            prev = None;
            continue;
        }
        let item = as_list_item(trimmed);
        let is_item = item.is_some();
        let current = item.unwrap_or_else(|| trimmed.to_string());
        if let Some(previous) = prev.as_deref() {
            let previous_is_item = is_list_line(previous);
            let wrapped = !is_item
                && !previous_is_item
                && previous.chars().count() >= WRAPPED_LINE_MIN_CHARS
                && !previous.ends_with(|c: char| matches!(c, '.' | '!' | '?' | ':' | ';' | '…'));
            if !wrapped {
                // Separate block: Markdown needs a blank line.
                out.push('\n');
            }
        }
        out.push_str(&current);
        out.push('\n');
        prev = Some(current);
    }
    out
}

fn as_list_item(line: &str) -> Option<String> {
    let first = line.chars().next()?;
    if BULLET_GLYPHS.contains(&first) {
        let rest = line[first.len_utf8()..].trim_start();
        return Some(format!("- {rest}"));
    }
    if is_list_line(line) {
        return Some(line.to_string());
    }
    None
}

fn is_list_line(line: &str) -> bool {
    if line.starts_with("- ") || line.starts_with("* ") || line.starts_with("+ ") {
        return true;
    }
    let digits: String = line.chars().take_while(|c| c.is_ascii_digit()).collect();
    !digits.is_empty()
        && digits.len() <= 3
        && line[digits.len()..].starts_with(|c: char| c == '.' || c == ')')
        && line[digits.len() + 1..].starts_with(' ')
}

/// Makes the text structure audible for speech models, which pause
/// reliably only at sentence ends:
///
/// - every non-empty line ends with terminal punctuation, so list items,
///   headings, table rows and single-line paragraphs get their pause even
///   when the source had no period (web pages rarely do);
/// Dashes and commas inside a line are left exactly as written: the user
/// asked for the text to reach the model unchanged (2026-11-06). Only a
/// comma or dash at the very END of a line is replaced by the period,
/// because that is a cut-off sentence or list item.
fn clarify_pauses(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    for (index, line) in text.lines().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let mut line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        // "Punkt,." / "Punkt –." from a cut-off sentence: keep the period.
        while line.ends_with('.') {
            let body = line[..line.len() - 1].trim_end();
            if body.ends_with(|c: char| matches!(c, ',' | '-' | '–' | '—')) {
                line = format!("{}.", body[..body.len() - body.chars().last().unwrap().len_utf8()].trim_end());
            } else {
                break;
            }
        }
        if !line.ends_with(|c: char| matches!(c, '.' | '!' | '?' | '…' | ':' | ';')) {
            // A trailing comma or dash is a cut-off sentence: replace it.
            while line.ends_with(|c: char| matches!(c, ',' | '-' | '–' | '—')) {
                line.pop();
                line.truncate(line.trim_end().len());
            }
            if !line.is_empty() {
                line.push('.');
            }
        }
        out.push_str(&line);
    }
    out
}

#[derive(Default)]
struct SpeechTextRenderer {
    output: String,
    pending_breaks: usize,
    list_stack: Vec<ListContext>,
    link_stack: Vec<LinkContext>,
    image_stack: Vec<ImageContext>,
    quote_depth: usize,
    skip_code_block_depth: usize,
    table_cell_index: usize,
    /// Set when the previous event was raw text: whether it ended in whitespace.
    prev_text_trailing_ws: Option<bool>,
}

#[derive(Clone, Debug)]
struct ListContext {
    next_index: Option<u64>,
}

#[derive(Clone, Debug)]
struct LinkContext {
    destination: String,
    text: String,
}

#[derive(Clone, Debug, Default)]
struct ImageContext {
    alt_text: String,
}

impl SpeechTextRenderer {
    fn handle_event(&mut self, event: Event<'_>) {
        let prev_text_trailing_ws = self.prev_text_trailing_ws.take();
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            // pulldown-cmark splits text at smart quotes and entities, so keep the
            // source whitespace between adjacent text events instead of guessing.
            Event::Text(text) => {
                if self.skip_code_block_depth == 0 {
                    let join =
                        prev_text_trailing_ws.map(|ws| ws || text.starts_with(char::is_whitespace));
                    self.push_text_with_join(&text, join);
                }
                self.prev_text_trailing_ws = Some(if text.is_empty() {
                    prev_text_trailing_ws.unwrap_or(false)
                } else {
                    text.ends_with(char::is_whitespace)
                });
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                if self.skip_code_block_depth == 0 {
                    self.push_text(&text);
                }
            }
            Event::Code(code) => self.push_text(&simplify_inline_code(&code)),
            Event::Html(html) | Event::InlineHtml(html) => {
                if self.skip_code_block_depth == 0 {
                    self.push_text(&strip_html_tags(&html));
                }
            }
            Event::SoftBreak => self.push_space(),
            Event::HardBreak => self.push_block_break(),
            Event::Rule => {
                self.ensure_terminal_punctuation();
                self.push_block_break();
            }
            Event::TaskListMarker(checked) => {
                self.push_text(if checked {
                    "Completed."
                } else {
                    "Not completed."
                });
            }
            Event::FootnoteReference(name) => self.push_text(&normalize_inline_whitespace(&name)),
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { .. } => self.push_block_break(),
            Tag::BlockQuote(_) => {
                self.push_block_break();
                if self.quote_depth == 0 {
                    self.push_text("Quote:");
                }
                self.quote_depth += 1;
            }
            Tag::CodeBlock(_) => {
                self.push_block_break();
                self.push_text("Code example omitted.");
                self.push_block_break();
                self.skip_code_block_depth += 1;
            }
            Tag::List(start) => {
                self.push_block_break();
                self.list_stack.push(ListContext { next_index: start });
            }
            Tag::Item => {
                self.push_block_break();
                if let Some(list) = self.list_stack.last_mut() {
                    if let Some(index) = list.next_index.as_mut() {
                        let current = *index;
                        *index += 1;
                        self.push_text(&format!("{}.", current));
                    }
                }
            }
            Tag::Link { dest_url, .. } => {
                self.link_stack.push(LinkContext {
                    destination: dest_url.into_string(),
                    text: String::new(),
                });
            }
            Tag::Image { .. } => {
                self.image_stack.push(ImageContext::default());
            }
            Tag::Table(_) => self.push_block_break(),
            Tag::TableHead => self.push_block_break(),
            Tag::TableRow => {
                self.push_block_break();
                self.table_cell_index = 0;
            }
            Tag::TableCell => {
                if self.table_cell_index > 0 {
                    self.push_text(",");
                }
                self.table_cell_index += 1;
            }
            Tag::Emphasis
            | Tag::Strong
            | Tag::Strikethrough
            | Tag::FootnoteDefinition(_)
            | Tag::MetadataBlock(_) => {}
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.ensure_terminal_punctuation();
                self.push_block_break();
            }
            TagEnd::Heading(_) => {
                self.ensure_terminal_punctuation();
                self.push_block_break();
            }
            TagEnd::BlockQuote(_) => {
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.ensure_terminal_punctuation();
                self.push_block_break();
            }
            TagEnd::CodeBlock => {
                self.skip_code_block_depth = self.skip_code_block_depth.saturating_sub(1);
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
                self.push_block_break();
            }
            TagEnd::Item => {
                self.ensure_terminal_punctuation();
                self.push_block_break();
            }
            TagEnd::Link => {
                if let Some(link) = self.link_stack.pop() {
                    let label = normalize_inline_whitespace(&link.text);
                    let spoken = if label.is_empty()
                        || normalize_inline_whitespace(&link.destination)
                            .eq_ignore_ascii_case(&label)
                    {
                        simplify_url_for_speech(&link.destination)
                    } else {
                        label
                    };
                    self.push_text(&spoken);
                }
            }
            TagEnd::Image => {
                if let Some(image) = self.image_stack.pop() {
                    let alt_text = normalize_inline_whitespace(&image.alt_text);
                    if !alt_text.is_empty() {
                        self.push_text(&alt_text);
                    }
                }
            }
            TagEnd::Table => self.push_block_break(),
            TagEnd::TableHead => self.push_block_break(),
            TagEnd::TableRow => {
                self.ensure_terminal_punctuation();
                self.push_block_break();
                self.table_cell_index = 0;
            }
            TagEnd::TableCell
            | TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::FootnoteDefinition
            | TagEnd::MetadataBlock(_) => {}
            _ => {}
        }
    }

    fn push_text(&mut self, text: &str) {
        self.push_text_with_join(text, None);
    }

    fn push_text_with_join(&mut self, text: &str, join: Option<bool>) {
        let normalized = normalize_inline_whitespace(text);
        if normalized.is_empty() {
            return;
        }

        if let Some(image) = self.image_stack.last_mut() {
            append_segment(&mut image.alt_text, &normalized, join);
            return;
        }

        if let Some(link) = self.link_stack.last_mut() {
            append_segment(&mut link.text, &normalized, join);
            return;
        }

        self.flush_breaks();

        if !self.output.is_empty()
            && join.unwrap_or_else(|| {
                needs_space_between(
                    self.output.chars().rev().nth(1),
                    self.output.chars().next_back(),
                    normalized.chars().next(),
                )
            })
        {
            self.output.push(' ');
        }
        self.output.push_str(&normalized);
    }

    fn push_space(&mut self) {
        if self.pending_breaks > 0
            || self.output.is_empty()
            || self.output.ends_with(char::is_whitespace)
        {
            return;
        }
        self.output.push(' ');
    }

    fn push_block_break(&mut self) {
        if self.output.trim().is_empty() {
            return;
        }
        self.pending_breaks = self.pending_breaks.max(2);
    }

    fn flush_breaks(&mut self) {
        if self.pending_breaks == 0 {
            return;
        }

        self.output.truncate(self.output.trim_end().len());
        if self.output.is_empty() {
            self.pending_breaks = 0;
            return;
        }

        for _ in 0..self.pending_breaks {
            self.output.push('\n');
        }
        self.pending_breaks = 0;
    }

    fn ensure_terminal_punctuation(&mut self) {
        if self.output.trim().is_empty() {
            return;
        }

        self.output.truncate(self.output.trim_end().len());

        let Some(last) = self.output.chars().next_back() else {
            return;
        };

        if matches!(last, '.' | '!' | '?' | ':' | ';' | '…') {
            return;
        }

        self.output.push('.');
    }

    fn finish(mut self) -> String {
        self.ensure_terminal_punctuation();

        let mut result = String::new();
        let mut blank_line_count = 0usize;

        for line in self.output.lines().map(str::trim) {
            if line.is_empty() {
                blank_line_count += 1;
                if blank_line_count > 1 {
                    continue;
                }
            } else {
                blank_line_count = 0;
            }

            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(line);
        }

        result.trim().to_string()
    }
}

fn append_segment(buffer: &mut String, segment: &str, join: Option<bool>) {
    if segment.is_empty() {
        return;
    }

    if !buffer.is_empty()
        && join.unwrap_or_else(|| {
            needs_space_between(
                buffer.chars().rev().nth(1),
                buffer.chars().next_back(),
                segment.chars().next(),
            )
        })
    {
        buffer.push(' ');
    }
    buffer.push_str(segment);
}

fn normalize_inline_whitespace(text: &str) -> String {
    let mut normalized = String::new();
    let mut last_was_space = false;

    for ch in text.chars() {
        if ch.is_whitespace() {
            if !last_was_space {
                normalized.push(' ');
                last_was_space = true;
            }
        } else {
            normalized.push(ch);
            last_was_space = false;
        }
    }

    normalized.trim().to_string()
}

fn needs_space_between(prev_left: Option<char>, left: Option<char>, right: Option<char>) -> bool {
    match (prev_left, left, right) {
        (_, Some(left), Some(right))
            if left.is_alphanumeric() && matches!(right, '&' | '\'' | '’') =>
        {
            false
        }
        (_, Some('&'), Some(right)) if right.is_alphanumeric() => false,
        (prev_left, Some('\'' | '’' | '‘'), Some(right)) if right.is_alphanumeric() => {
            match prev_left {
                None => false,
                Some(ch)
                    if ch.is_alphanumeric()
                        || ch.is_whitespace()
                        || matches!(ch, '(' | '[' | '{' | '"' | ':' | ';' | '—' | '–') =>
                {
                    false
                }
                Some(_) => true,
            }
        }
        (_, Some(left), Some(right)) => {
            if (left.is_alphanumeric() && matches!(right, '&' | '\'' | '’'))
                || (left == '&' && right.is_alphanumeric())
            {
                false
            } else {
                !left.is_whitespace()
                    && !matches!(right, ',' | '.' | '!' | '?' | ':' | ';' | ')' | ']' | '}')
                    && !matches!(left, '(' | '[' | '{' | '/' | '\n')
            }
        }
        _ => false,
    }
}

fn simplify_inline_code(code: &str) -> String {
    let mut simplified = String::new();
    let chars: Vec<char> = code.chars().collect();

    for (index, ch) in chars.iter().enumerate() {
        let prev = index.checked_sub(1).and_then(|idx| chars.get(idx)).copied();
        let next = chars.get(index + 1).copied();
        let adjacent_word_chars = prev.map(is_code_word_char).unwrap_or(false)
            && next.map(is_code_word_char).unwrap_or(false);

        if adjacent_word_chars && matches!(ch, '_' | '-' | '/' | '\\' | '.' | ':') {
            if !simplified.ends_with(' ') {
                simplified.push(' ');
            }
            continue;
        }

        simplified.push(*ch);
    }

    normalize_inline_whitespace(&simplified)
}

fn is_code_word_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric()
}

fn simplify_url_for_speech(url: &str) -> String {
    let trimmed = url
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("mailto:");

    let mut spoken = String::new();
    let mut previous_was_space = false;

    for ch in trimmed.chars() {
        if ch.is_ascii_alphanumeric() {
            spoken.push(ch);
            previous_was_space = false;
            continue;
        }

        if matches!(
            ch,
            '.' | '/' | '_' | '-' | '?' | '&' | '=' | '#' | ':' | '%'
        ) && !previous_was_space
        {
            spoken.push(' ');
            previous_was_space = true;
        }
    }

    normalize_inline_whitespace(&spoken)
}

fn strip_html_tags(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    let mut entity = String::new();
    let mut in_entity = false;

    for ch in html.chars() {
        if in_entity {
            if ch == ';' {
                if let Some(decoded) = decode_html_entity(&entity) {
                    out.push_str(&decoded);
                } else {
                    out.push('&');
                    out.push_str(&entity);
                    out.push(';');
                }
                entity.clear();
                in_entity = false;
                continue;
            }

            if entity.len() < 16 {
                entity.push(ch);
            }
            continue;
        }

        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            '&' if !in_tag => {
                in_entity = true;
                entity.clear();
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }

    if in_entity {
        out.push('&');
        out.push_str(&entity);
    }

    normalize_inline_whitespace(&out)
}

fn decode_html_entity(entity: &str) -> Option<String> {
    match entity {
        "amp" => Some("&".to_string()),
        "lt" => Some("<".to_string()),
        "gt" => Some(">".to_string()),
        "quot" => Some("\"".to_string()),
        "apos" => Some("'".to_string()),
        "nbsp" => Some(" ".to_string()),
        _ => decode_numeric_html_entity(entity),
    }
}

fn decode_numeric_html_entity(entity: &str) -> Option<String> {
    let code = entity.strip_prefix('#')?;

    let (radix, digits) = match code.as_bytes() {
        [b'x', rest @ ..] | [b'X', rest @ ..] => (16, std::str::from_utf8(rest).ok()?),
        _ => (10, code),
    };

    if digits.is_empty() {
        return None;
    }

    let value = u32::from_str_radix(digits, radix).ok()?;
    char::from_u32(value).map(|decoded| decoded.to_string())
}

#[cfg(test)]
mod tests {
    use super::normalize_text_for_tts;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn strips_common_markdown_formatting_without_losing_flow() {
        let markdown = r#"
## Heading

This is **important** and includes a [helpful link](https://example.com/docs/getting-started).

- First bullet
- Second bullet with `snake_case` inline code

> Quoted thought
"#;

        let spoken = normalize_text_for_tts(markdown);
        assert!(spoken.contains("Heading."));
        assert!(spoken.contains("This is important and includes a helpful link."));
        assert!(spoken.contains("First bullet."));
        assert!(spoken.contains("Second bullet with snake case inline code."));
        assert!(spoken.contains("Quote: Quoted thought."));
        assert!(!spoken.contains("##"));
        assert!(!spoken.contains("**"));
        assert!(!spoken.contains('`'));
    }

    #[test]
    fn handles_code_fences_and_fallback_links_intentionally() {
        let markdown = r#"
See <https://example.com/docs/api-reference>.

```rust
fn main() {
    println!("hello");
}
```
"#;

        let spoken = normalize_text_for_tts(markdown);
        assert!(spoken.contains("example com docs api reference"));
        assert!(spoken.contains("Code example omitted."));
        assert!(!spoken.contains("```"));
        assert!(!spoken.contains("println!"));
    }

    #[test]
    fn preserves_ordered_list_and_task_list_structure() {
        let markdown = r#"
1. Install dependencies
2. Run tests

- [x] Ship fix
- [ ] Write follow-up
"#;

        let spoken = normalize_text_for_tts(markdown);
        assert!(spoken.contains("1. Install dependencies."));
        assert!(spoken.contains("2. Run tests."));
        assert!(spoken.contains("Completed. Ship fix."));
        assert!(spoken.contains("Not completed. Write follow-up."));
    }

    #[test]
    fn reads_indented_web_text_instead_of_skipping_it() {
        let text = "Erster Absatz einer Webseite.\n\n    Eingerückter zweiter Absatz, der gelesen werden muss.\n\tDritter Absatz mit Tab.";
        let spoken = normalize_text_for_tts(text);
        assert!(!spoken.contains("Code example omitted"), "{spoken}");
        assert!(spoken.contains("Eingerückter zweiter Absatz, der gelesen werden muss."));
        assert!(spoken.contains("Dritter Absatz mit Tab."));
    }

    #[test]
    fn fenced_code_is_still_skipped() {
        let spoken = normalize_text_for_tts("Davor.\n\n```\n    let x = 1;\n```\n\nDanach.");
        assert!(spoken.contains("Code example omitted."));
        assert!(spoken.contains("Danach."));
        assert!(!spoken.contains("let x"));
    }

    #[test]
    fn every_line_and_bullet_ends_with_a_pause() {
        let text = "Überschrift ohne Punkt\nZeile zwei ohne Punkt\n\n• Erster Punkt\n• Zweiter Punkt,\n- Dritter Punkt";
        let spoken = normalize_text_for_tts(text);
        assert!(spoken.contains("Überschrift ohne Punkt."), "{spoken}");
        assert!(spoken.contains("Zeile zwei ohne Punkt."), "{spoken}");
        assert!(spoken.contains("Erster Punkt."), "{spoken}");
        assert!(spoken.contains("Zweiter Punkt."), "{spoken}");
        assert!(spoken.contains("Dritter Punkt."), "{spoken}");
    }

    #[test]
    fn wrapped_pdf_lines_are_joined_without_a_period() {
        let text = "Während in den ersten Tagen nach einem Schlaganfall vor allem die\nneurologischen Ausfälle im Vordergrund stehen, gewinnen andere an Bedeutung.";
        let spoken = normalize_text_for_tts(text);
        assert!(spoken.contains("vor allem die neurologischen Ausfälle"), "{spoken}");
        assert!(!spoken.contains("die."), "{spoken}");
    }

    #[test]
    fn dashes_and_commas_inside_a_line_stay_as_written() {
        let spoken = normalize_text_for_tts(
            "Das Ergebnis – überraschend – war klar, ja. Herz-Kreislauf bleibt. Zahlen 1990–2000 auch. Ende -- wirklich",
        );
        assert!(spoken.contains("Das Ergebnis – überraschend – war klar, ja."), "{spoken}");
        assert!(spoken.contains("Herz-Kreislauf bleibt."), "{spoken}");
        assert!(spoken.contains("1990–2000"), "{spoken}");
        // The Markdown reader itself typesets "--" as an en dash.
        assert!(spoken.contains("Ende – wirklich."), "{spoken}");
    }

    #[test]
    fn a_comma_or_dash_at_the_end_of_a_line_becomes_a_period() {
        let spoken = normalize_text_for_tts("Erster Teil,\n\nZweiter Teil –\n\nDritter Teil");
        assert!(spoken.contains("Erster Teil."), "{spoken}");
        assert!(spoken.contains("Zweiter Teil."), "{spoken}");
        assert!(spoken.contains("Dritter Teil."), "{spoken}");
    }

    #[test]
    fn keeps_contractions_and_quoted_words_intact() {
        assert_eq!(
            normalize_text_for_tts("I don't know, it's fine. We'll see."),
            "I don’t know, it’s fine. We’ll see."
        );
        assert_eq!(
            normalize_text_for_tts("Parrot's voice isn't 'quoted' here."),
            "Parrot’s voice isn’t ‘quoted’ here."
        );
        assert_eq!(normalize_text_for_tts("rock 'n' roll"), "rock ‘n’ roll.");
    }

    #[test]
    fn normalizes_real_repository_markdown_without_raw_markup_artifacts() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let readme = fs::read_to_string(manifest_dir.join("../README.md")).expect("read README");
        let spoken = normalize_text_for_tts(&readme);

        assert!(spoken.contains("Parrot"));
        assert!(!spoken.contains("```"));
        assert!(!spoken.contains("## "));
        assert!(!spoken.contains("**"));
        assert!(spoken.len() > 1000);
    }

    #[test]
    fn decodes_named_and_numeric_html_entities_in_html_blocks() {
        let markdown = r#"
<span>AT&amp;T uses smart quotes: &#8217;hello&#8217; and &#x2014; dash.</span>
"#;

        let spoken = normalize_text_for_tts(markdown);
        assert!(spoken.contains("AT&T uses smart quotes: ’hello’ and — dash."), "{spoken}");
    }

    #[test]
    fn preserves_unknown_or_incomplete_entities_as_literal_text() {
        let markdown = r#"
Keep &custom; visible and preserve dangling &entity text.
"#;

        let spoken = normalize_text_for_tts(markdown);
        assert!(spoken.contains("Keep &custom; visible and preserve dangling &entity text."));
    }
}