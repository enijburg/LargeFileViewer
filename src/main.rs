use std::{
    cmp::min,
    fs::File,
    io::{self, Write},
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind},
    execute, queue,
    style::{self, Color, Stylize},
    terminal::{self, ClearType},
};
use memmap2::Mmap;

#[derive(Parser, Debug)]
#[command(version, about = "Memory-mapped large file viewer for Windows x64")]
struct Args {
    /// File path to view.
    file: PathBuf,

    /// Number of spaces a tab represents when rendering.
    #[arg(long, default_value_t = 4)]
    tab_width: usize,

    /// Render comma-separated values in aligned columns.
    #[arg(long)]
    csv: bool,

    /// Enable rudimentary XML syntax highlighting.
    #[arg(long)]
    xml: bool,

    /// In XML mode, indent lines based on tag depth.
    #[arg(long)]
    format: bool,

    /// Enable rudimentary JSON syntax highlighting.
    #[arg(long)]
    json: bool,
}

struct Viewer {
    mmap: Mmap,
    formatted_xml: Option<Vec<u8>>,
    line_offsets: Vec<usize>,
    top_line: usize,
    left_col: usize,
    tab_width: usize,
    csv_column_widths: Option<Vec<usize>>,
    xml_syntax_highlighting: bool,
    json_syntax_highlighting: bool,
    search_query: Option<Vec<u8>>,
    match_range: Option<(usize, usize)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum XmlTokenClass {
    Text,
    TagDelimiter,
    TagName,
    AttributeName,
    AttributeValue,
    Comment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JsonTokenClass {
    Text,
    Delimiter,
    Key,
    String,
    Number,
    Keyword,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenderClass {
    Text,
    TagDelimiter,
    TagName,
    AttributeName,
    AttributeValue,
    Comment,
}

fn centered_top_line(target_line: usize, viewport_rows: usize, line_count: usize) -> usize {
    if line_count == 0 {
        return 0;
    }

    let centered = target_line.saturating_sub(viewport_rows / 2);
    centered.min(line_count - 1)
}

impl Viewer {
    fn open(
        path: PathBuf,
        tab_width: usize,
        csv: bool,
        xml_syntax_highlighting: bool,
        xml_formatting: bool,
        json_syntax_highlighting: bool,
    ) -> Result<Self> {
        let file = File::open(&path)
            .with_context(|| format!("Failed to open file: {}", path.display()))?;

        // SAFETY: File remains alive during mapping creation, and mapping is read-only.
        let mmap = unsafe { Mmap::map(&file) }
            .with_context(|| format!("Failed to memory-map file: {}", path.display()))?;

        let formatted_xml =
            (xml_syntax_highlighting && xml_formatting && !csv && !json_syntax_highlighting)
                .then(|| format_xml_for_display(&mmap));
        let source_bytes = formatted_xml.as_deref().unwrap_or(&mmap);
        let line_offsets = Self::index_lines(source_bytes);
        let csv_column_widths = csv.then(|| Self::index_csv_column_widths(source_bytes, tab_width));

        let top_line = if csv && line_offsets.len() > 1 { 1 } else { 0 };

        Ok(Self {
            mmap,
            formatted_xml,
            line_offsets,
            top_line,
            left_col: 0,
            tab_width,
            csv_column_widths,
            xml_syntax_highlighting,
            json_syntax_highlighting,
            search_query: None,
            match_range: None,
        })
    }

    fn view_bytes(&self) -> &[u8] {
        self.formatted_xml.as_deref().unwrap_or(&self.mmap)
    }

    fn index_lines(bytes: &[u8]) -> Vec<usize> {
        let mut offsets = Vec::with_capacity(bytes.len() / 40 + 1);
        offsets.push(0);

        for (idx, b) in bytes.iter().enumerate() {
            if *b == b'\n' {
                offsets.push(idx + 1);
            }
        }

        offsets
    }

    fn line_count(&self) -> usize {
        self.line_offsets.len()
    }

    fn index_csv_column_widths(bytes: &[u8], tab_width: usize) -> Vec<usize> {
        let mut widths: Vec<usize> = Vec::new();
        let mut column = 0usize;
        let mut current_width = 0usize;

        for &b in bytes {
            match b {
                b'\n' => {
                    if widths.len() <= column {
                        widths.resize(column + 1, 0);
                    }
                    widths[column] = widths[column].max(current_width);
                    column = 0;
                    current_width = 0;
                }
                b'\r' => {}
                b',' => {
                    if widths.len() <= column {
                        widths.resize(column + 1, 0);
                    }
                    widths[column] = widths[column].max(current_width);
                    column += 1;
                    current_width = 0;
                }
                b'\t' => current_width += tab_width,
                _ => current_width += 1,
            }
        }

        if widths.len() <= column {
            widths.resize(column + 1, 0);
        }
        widths[column] = widths[column].max(current_width);
        widths
    }

    fn render(&self, out: &mut impl Write) -> Result<()> {
        let (width, height) = terminal::size().context("Failed to get terminal size")?;
        let body_rows = height.saturating_sub(2) as usize;
        let width = width as usize;

        // Synchronized updates reduce perceived flicker by presenting the frame at once.
        queue!(
            out,
            terminal::BeginSynchronizedUpdate,
            cursor::MoveTo(0, 0),
            terminal::Clear(ClearType::All)
        )?;

        let status = format!(
            "Lines: {} | Top: {} | Left: {} | q: quit | g: goto | f: find | n/p: next/prev | ←/→ ↑/↓ PgUp/PgDn Home/End",
            self.line_count(),
            self.top_line + 1,
            self.left_col + 1
        );
        let clipped_status = clip_to_width(&status, width);
        queue!(
            out,
            style::PrintStyledContent(clipped_status.reverse()),
            cursor::MoveToNextLine(1)
        )?;

        if self.csv_column_widths.is_some() && self.line_count() > 0 {
            self.render_line(out, 0, width)?;
            queue!(out, cursor::MoveToNextLine(1))?;

            let start = self.top_line.max(1);
            for row in 1..body_rows {
                let line_idx = start + (row - 1);
                if line_idx >= self.line_count() {
                    break;
                }

                self.render_line(out, line_idx, width)?;
                queue!(out, cursor::MoveToNextLine(1))?;
            }
        } else {
            for row in 0..body_rows {
                let line_idx = self.top_line + row;
                if line_idx >= self.line_count() {
                    break;
                }

                self.render_line(out, line_idx, width)?;
                queue!(out, cursor::MoveToNextLine(1))?;
            }
        }

        let footer = "Memory-mapped view (renders visible window only)";
        let clipped_footer = clip_to_width(footer, width);
        let y = height.saturating_sub(1);
        queue!(
            out,
            cursor::MoveTo(0, y),
            style::PrintStyledContent(clipped_footer.dark_grey())
        )?;

        queue!(out, terminal::EndSynchronizedUpdate)?;
        out.flush().context("Failed to flush terminal output")?;
        Ok(())
    }

    fn render_line(&self, out: &mut impl Write, line_idx: usize, max_width: usize) -> Result<()> {
        let line_start = self.line_offsets[line_idx];
        let view_bytes = self.view_bytes();
        let line_end = if line_idx + 1 < self.line_offsets.len() {
            self.line_offsets[line_idx + 1]
        } else {
            view_bytes.len()
        };
        let highlight = self.match_range.and_then(|(start, end)| {
            if start < line_end && end > line_start {
                Some((start, end))
            } else {
                None
            }
        });

        let bytes = &view_bytes[line_start..line_end];
        let content_start = skipped_prefix_len(line_idx, bytes);
        let mut segments: Vec<(bool, RenderClass, String)> = Vec::new();
        let mut visible_width = 0usize;
        let mut absolute_col = 0usize;
        let xml_classes = (!self.xml_syntax_highlighting
            || self.csv_column_widths.is_some()
            || self.json_syntax_highlighting)
            .then(Vec::new)
            .unwrap_or_else(|| classify_xml_line(bytes));
        let json_classes = (!self.json_syntax_highlighting
            || self.csv_column_widths.is_some()
            || self.xml_syntax_highlighting)
            .then(Vec::new)
            .unwrap_or_else(|| classify_json_line(bytes));

        let mut push_char = |c: char, is_highlight: bool, render_class: RenderClass| {
            if absolute_col < self.left_col {
                absolute_col += 1;
                return;
            }
            if visible_width >= max_width {
                absolute_col += 1;
                return;
            }
            if segments
                .last()
                .map(|(h, class, _)| *h != is_highlight || *class != render_class)
                .unwrap_or(true)
            {
                segments.push((is_highlight, render_class, String::new()));
            }
            let (_, _, target) = segments.last_mut().expect("segment just pushed");
            target.push(c);
            visible_width += 1;
            absolute_col += 1;
        };

        if let Some(column_widths) = &self.csv_column_widths {
            let mut column_idx = 0usize;
            let mut field_width = 0usize;

            for (idx, &b) in bytes.iter().enumerate().skip(content_start) {
                if b == b'\n' || b == b'\r' {
                    continue;
                }

                let absolute_idx = line_start + idx;
                let is_highlight = highlight
                    .map(|(start, end)| absolute_idx >= start && absolute_idx < end)
                    .unwrap_or(false);

                match b {
                    b',' => {
                        let target_width = column_widths.get(column_idx).copied().unwrap_or(0);
                        for _ in field_width..target_width {
                            push_char(' ', false, RenderClass::Text);
                        }
                        push_char(',', is_highlight, RenderClass::Text);
                        push_char(' ', false, RenderClass::Text);
                        column_idx += 1;
                        field_width = 0;
                    }
                    b'\t' => {
                        for _ in 0..self.tab_width {
                            push_char(' ', is_highlight, RenderClass::Text);
                            field_width += 1;
                        }
                    }
                    0x20..=0x7e => {
                        push_char(b as char, is_highlight, RenderClass::Text);
                        field_width += 1;
                    }
                    _ => {
                        push_char('·', is_highlight, RenderClass::Text);
                        field_width += 1;
                    }
                }
            }
        } else {
            for (idx, &b) in bytes.iter().enumerate().skip(content_start) {
                if b == b'\n' || b == b'\r' {
                    continue;
                }

                let absolute_idx = line_start + idx;
                let is_highlight = highlight
                    .map(|(start, end)| absolute_idx >= start && absolute_idx < end)
                    .unwrap_or(false);
                let render_class = if self.json_syntax_highlighting {
                    match json_classes
                        .get(idx)
                        .copied()
                        .unwrap_or(JsonTokenClass::Text)
                    {
                        JsonTokenClass::Text => RenderClass::Text,
                        JsonTokenClass::Delimiter => RenderClass::TagDelimiter,
                        JsonTokenClass::Key => RenderClass::AttributeName,
                        JsonTokenClass::String => RenderClass::AttributeValue,
                        JsonTokenClass::Number => RenderClass::TagName,
                        JsonTokenClass::Keyword => RenderClass::Comment,
                    }
                } else {
                    match xml_classes.get(idx).copied().unwrap_or(XmlTokenClass::Text) {
                        XmlTokenClass::Text => RenderClass::Text,
                        XmlTokenClass::TagDelimiter => RenderClass::TagDelimiter,
                        XmlTokenClass::TagName => RenderClass::TagName,
                        XmlTokenClass::AttributeName => RenderClass::AttributeName,
                        XmlTokenClass::AttributeValue => RenderClass::AttributeValue,
                        XmlTokenClass::Comment => RenderClass::Comment,
                    }
                };

                match b {
                    b'\t' => {
                        for _ in 0..self.tab_width {
                            push_char(' ', is_highlight, render_class);
                        }
                    }
                    0x20..=0x7e => push_char(b as char, is_highlight, render_class),
                    _ => push_char('·', is_highlight, render_class),
                }
            }
        }

        for (is_highlight, render_class, text) in segments {
            if is_highlight {
                let styled = match render_class {
                    RenderClass::Text => style::style(text).with(Color::White).reverse(),
                    RenderClass::TagDelimiter => style::style(text).with(Color::Cyan).reverse(),
                    RenderClass::TagName => style::style(text).with(Color::DarkYellow).reverse(),
                    RenderClass::AttributeName => style::style(text).with(Color::Green).reverse(),
                    RenderClass::AttributeValue => style::style(text).with(Color::Yellow).reverse(),
                    RenderClass::Comment => style::style(text).with(Color::DarkGrey).reverse(),
                };
                queue!(out, style::PrintStyledContent(styled))?;
            } else {
                let styled = match render_class {
                    RenderClass::Text => style::style(text).with(Color::White),
                    RenderClass::TagDelimiter => style::style(text).with(Color::Cyan),
                    RenderClass::TagName => style::style(text).with(Color::DarkYellow),
                    RenderClass::AttributeName => style::style(text).with(Color::Green),
                    RenderClass::AttributeValue => style::style(text).with(Color::Yellow),
                    RenderClass::Comment => style::style(text).with(Color::DarkGrey),
                };
                queue!(out, style::PrintStyledContent(styled))?;
            }
        }

        Ok(())
    }

    fn scroll_up(&mut self, by: usize) {
        let min_top = usize::from(self.csv_column_widths.is_some() && self.line_count() > 1);
        self.top_line = self.top_line.saturating_sub(by).max(min_top);
    }

    fn scroll_down(&mut self, by: usize) {
        if self.line_count() == 0 {
            self.top_line = 0;
            return;
        }
        self.top_line = min(self.top_line + by, self.line_count() - 1);
    }

    fn scroll_left(&mut self, by: usize) {
        self.left_col = self.left_col.saturating_sub(by);
    }

    fn scroll_right(&mut self, by: usize) {
        self.left_col = self.left_col.saturating_add(by);
    }

    fn line_of_offset(&self, offset: usize) -> usize {
        if self.line_offsets.is_empty() {
            return 0;
        }
        match self.line_offsets.binary_search(&offset) {
            Ok(idx) => idx,
            Err(insert) => insert.saturating_sub(1),
        }
    }

    fn set_match(&mut self, start: usize, end: usize, viewport_rows: usize) {
        self.match_range = Some((start, end));
        let line = self.line_of_offset(start);
        self.top_line = centered_top_line(line, viewport_rows.max(1), self.line_count());
    }

    fn find_forward(&self, query: &[u8], start: usize) -> Option<(usize, usize)> {
        let bytes = self.view_bytes();
        if query.is_empty() || start >= bytes.len() {
            return None;
        }
        bytes[start..]
            .windows(query.len())
            .position(|window| window == query)
            .map(|relative| {
                let found_start = start + relative;
                (found_start, found_start + query.len())
            })
    }

    fn find_backward(&self, query: &[u8], start: usize) -> Option<(usize, usize)> {
        let bytes = self.view_bytes();
        if query.is_empty() || bytes.is_empty() {
            return None;
        }
        let end = min(start.saturating_add(1), bytes.len());
        if end < query.len() {
            return None;
        }
        bytes[..end]
            .windows(query.len())
            .rposition(|window| window == query)
            .map(|found_start| (found_start, found_start + query.len()))
    }
}

fn clip_to_width(s: &str, max_width: usize) -> String {
    s.chars().take(max_width).collect()
}

fn skipped_prefix_len(line_idx: usize, bytes: &[u8]) -> usize {
    if line_idx == 0 && bytes.starts_with(&[0xEF_u8, 0xBB_u8, 0xBF_u8]) {
        3
    } else {
        0
    }
}

fn classify_xml_line(bytes: &[u8]) -> Vec<XmlTokenClass> {
    let mut classes = vec![XmlTokenClass::Text; bytes.len()];
    let mut in_tag = false;
    let mut in_quote: Option<u8> = None;
    let mut in_comment = false;
    let mut saw_tag_name = false;
    let mut in_tag_name = false;
    let mut idx = 0usize;

    while idx < bytes.len() {
        let b = bytes[idx];

        if in_comment {
            classes[idx] = XmlTokenClass::Comment;
            if idx + 2 < bytes.len() && bytes[idx..=idx + 2] == *b"-->" {
                classes[idx + 1] = XmlTokenClass::Comment;
                classes[idx + 2] = XmlTokenClass::Comment;
                idx += 2;
                in_comment = false;
                in_tag = false;
                saw_tag_name = false;
                in_tag_name = false;
            }
        } else if let Some(quote) = in_quote {
            classes[idx] = XmlTokenClass::AttributeValue;
            if b == quote {
                in_quote = None;
            }
        } else if in_tag {
            match b {
                b'>' => {
                    classes[idx] = XmlTokenClass::TagDelimiter;
                    in_tag = false;
                    saw_tag_name = false;
                    in_tag_name = false;
                }
                b'"' | b'\'' => {
                    classes[idx] = XmlTokenClass::AttributeValue;
                    in_quote = Some(b);
                    in_tag_name = false;
                }
                b'=' => {
                    classes[idx] = XmlTokenClass::TagDelimiter;
                    in_tag_name = false;
                }
                b if b.is_ascii_whitespace() => {
                    classes[idx] = XmlTokenClass::TagDelimiter;
                    in_tag_name = false;
                }
                _ if in_tag_name || !saw_tag_name => {
                    classes[idx] = XmlTokenClass::TagName;
                    saw_tag_name = true;
                    in_tag_name = true;
                }
                _ => {
                    classes[idx] = XmlTokenClass::AttributeName;
                }
            }
        } else if b == b'<' {
            if idx + 3 < bytes.len() && bytes[idx..=idx + 3] == *b"<!--" {
                classes[idx] = XmlTokenClass::Comment;
                classes[idx + 1] = XmlTokenClass::Comment;
                classes[idx + 2] = XmlTokenClass::Comment;
                classes[idx + 3] = XmlTokenClass::Comment;
                idx += 3;
                in_comment = true;
                saw_tag_name = false;
                in_tag_name = false;
            } else {
                classes[idx] = XmlTokenClass::TagDelimiter;
                in_tag = true;
                saw_tag_name = false;
                in_tag_name = true;
            }
        } else {
            classes[idx] = XmlTokenClass::Text;
        }

        idx += 1;
    }

    classes
}

fn classify_json_line(bytes: &[u8]) -> Vec<JsonTokenClass> {
    let mut classes = vec![JsonTokenClass::Text; bytes.len()];
    let mut idx = 0usize;
    let mut expecting_key = true;

    while idx < bytes.len() {
        let b = bytes[idx];
        match b {
            b'{' | b'}' | b'[' | b']' | b':' | b',' => {
                classes[idx] = JsonTokenClass::Delimiter;
                if b == b'{' || b == b',' {
                    expecting_key = true;
                } else if b == b':' || b == b'[' {
                    expecting_key = false;
                }
                idx += 1;
            }
            b'"' => {
                let class = if expecting_key {
                    JsonTokenClass::Key
                } else {
                    JsonTokenClass::String
                };
                classes[idx] = class;
                idx += 1;
                let mut escaped = false;
                while idx < bytes.len() {
                    classes[idx] = class;
                    let ch = bytes[idx];
                    if escaped {
                        escaped = false;
                    } else if ch == b'\\' {
                        escaped = true;
                    } else if ch == b'"' {
                        idx += 1;
                        break;
                    }
                    idx += 1;
                }
                expecting_key = false;
            }
            b'-' | b'0'..=b'9' => {
                let start = idx;
                idx += 1;
                while idx < bytes.len()
                    && matches!(bytes[idx], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
                {
                    idx += 1;
                }
                for class in classes.iter_mut().take(idx).skip(start) {
                    *class = JsonTokenClass::Number;
                }
                expecting_key = false;
            }
            b't' if bytes[idx..].starts_with(b"true") => {
                for class in classes.iter_mut().take(idx + 4).skip(idx) {
                    *class = JsonTokenClass::Keyword;
                }
                idx += 4;
                expecting_key = false;
            }
            b'f' if bytes[idx..].starts_with(b"false") => {
                for class in classes.iter_mut().take(idx + 5).skip(idx) {
                    *class = JsonTokenClass::Keyword;
                }
                idx += 5;
                expecting_key = false;
            }
            b'n' if bytes[idx..].starts_with(b"null") => {
                for class in classes.iter_mut().take(idx + 4).skip(idx) {
                    *class = JsonTokenClass::Keyword;
                }
                idx += 4;
                expecting_key = false;
            }
            _ => idx += 1,
        }
    }

    classes
}

#[derive(Clone, Copy)]
struct XmlDisplayToken {
    start: usize,
    end: usize,
    is_tag: bool,
    is_closing: bool,
    is_opening: bool,
    is_self_closing: bool,
}

fn format_xml_for_display(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len().saturating_add(bytes.len() / 4));
    let mut tokens = Vec::new();
    let mut idx = 0usize;

    while idx < bytes.len() {
        if bytes[idx].is_ascii_whitespace() {
            idx += 1;
            continue;
        }

        if bytes[idx] == b'<' {
            let (token_end, is_closing, is_opening, is_self_closing) = xml_tag_bounds(bytes, idx);
            tokens.push(XmlDisplayToken {
                start: idx,
                end: token_end,
                is_tag: true,
                is_closing,
                is_opening,
                is_self_closing,
            });
            idx = token_end;
            continue;
        }

        let text_start = idx;
        while idx < bytes.len() && bytes[idx] != b'<' {
            idx += 1;
        }
        let (trimmed_start, trimmed_end) = trim_ascii_whitespace_range(bytes, text_start, idx);
        if trimmed_start < trimmed_end {
            tokens.push(XmlDisplayToken {
                start: trimmed_start,
                end: trimmed_end,
                is_tag: false,
                is_closing: false,
                is_opening: false,
                is_self_closing: false,
            });
        }
    }

    let mut depth = 0usize;
    let mut i = 0usize;
    while i < tokens.len() {
        let token = tokens[i];

        if token.is_tag
            && token.is_opening
            && !token.is_self_closing
            && i + 2 < tokens.len()
            && !tokens[i + 1].is_tag
            && tokens[i + 2].is_tag
            && tokens[i + 2].is_closing
            && matching_tag_names(bytes, token, tokens[i + 2])
        {
            let mut line = Vec::new();
            line.extend_from_slice(&bytes[token.start..token.end]);
            line.extend_from_slice(&bytes[tokens[i + 1].start..tokens[i + 1].end]);
            line.extend_from_slice(&bytes[tokens[i + 2].start..tokens[i + 2].end]);
            push_indented_xml_line(&mut out, depth, &line);
            i += 3;
            continue;
        }

        let line_depth = if token.is_closing {
            depth.saturating_sub(1)
        } else {
            depth
        };
        push_indented_xml_line(&mut out, line_depth, &bytes[token.start..token.end]);

        if token.is_closing {
            depth = depth.saturating_sub(1);
        } else if token.is_opening && !token.is_self_closing {
            depth = depth.saturating_add(1);
        }
        i += 1;
    }

    if out.last() == Some(&b'\n') {
        out.pop();
    }
    if out.is_empty() {
        out.push(b'\n');
    }
    out
}

fn matching_tag_names(bytes: &[u8], open: XmlDisplayToken, close: XmlDisplayToken) -> bool {
    let open_name = extract_tag_name(bytes, open.start, open.end, false);
    let close_name = extract_tag_name(bytes, close.start, close.end, true);
    !open_name.is_empty() && open_name == close_name
}

fn xml_tag_bounds(bytes: &[u8], start: usize) -> (usize, bool, bool, bool) {
    let mut idx = start + 1;
    let is_closing = idx < bytes.len() && bytes[idx] == b'/';
    let is_special = idx < bytes.len() && matches!(bytes[idx], b'!' | b'?');
    let mut in_quote: Option<u8> = None;

    while idx < bytes.len() {
        let b = bytes[idx];
        if let Some(quote) = in_quote {
            if b == quote {
                in_quote = None;
            }
        } else if b == b'"' || b == b'\'' {
            in_quote = Some(b);
        } else if b == b'>' {
            idx += 1;
            break;
        }
        idx += 1;
    }

    let mut tail = idx.saturating_sub(1);
    if tail > start && bytes[tail] == b'>' {
        tail -= 1;
    }
    while tail > start && bytes[tail].is_ascii_whitespace() {
        tail -= 1;
    }
    let is_self_closing = !is_special && tail > start && bytes[tail] == b'/';
    let is_opening = !is_special && !is_closing;

    (
        idx.min(bytes.len()),
        is_closing,
        is_opening,
        is_self_closing,
    )
}

fn extract_tag_name(bytes: &[u8], start: usize, end: usize, closing: bool) -> &[u8] {
    if end <= start + 2 {
        return &[];
    }
    let mut idx = start + 1;
    if closing && idx < end && bytes[idx] == b'/' {
        idx += 1;
    }
    while idx < end && bytes[idx].is_ascii_whitespace() {
        idx += 1;
    }
    let name_start = idx;
    while idx < end
        && !bytes[idx].is_ascii_whitespace()
        && bytes[idx] != b'/'
        && bytes[idx] != b'>'
        && bytes[idx] != b'?'
    {
        idx += 1;
    }
    &bytes[name_start..idx]
}

fn trim_ascii_whitespace(bytes: &[u8]) -> &[u8] {
    let mut start = 0usize;
    let mut end = bytes.len();
    while start < end && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &bytes[start..end]
}

fn trim_ascii_whitespace_range(bytes: &[u8], start: usize, end: usize) -> (usize, usize) {
    let trimmed = trim_ascii_whitespace(&bytes[start..end]);
    if trimmed.is_empty() {
        return (start, start);
    }
    let leading = bytes[start..end]
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(0);
    (start + leading, start + leading + trimmed.len())
}

fn push_indented_xml_line(out: &mut Vec<u8>, depth: usize, content: &[u8]) {
    out.extend(std::iter::repeat_n(b' ', depth.saturating_mul(2)));
    out.extend_from_slice(content);
    out.push(b'\n');
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut viewer = Viewer::open(
        args.file,
        args.tab_width,
        args.csv,
        args.xml,
        args.format,
        args.json,
    )?;

    terminal::enable_raw_mode().context("Failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;

    let run_result = run_event_loop(&mut viewer, &mut stdout);

    execute!(stdout, cursor::Show, terminal::LeaveAlternateScreen)?;
    terminal::disable_raw_mode().context("Failed to disable raw mode")?;

    run_result
}

fn run_event_loop(viewer: &mut Viewer, out: &mut impl Write) -> Result<()> {
    let mut needs_redraw = true;

    loop {
        if needs_redraw {
            viewer.render(out)?;
            needs_redraw = false;
        }

        if event::poll(Duration::from_millis(250)).context("Failed polling terminal events")? {
            match event::read().context("Failed reading terminal event")? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    let (_, height) = terminal::size().context("Failed to get terminal size")?;
                    let page = height.saturating_sub(2) as usize;
                    match key.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Char('g') => {
                            if let Some(line_number) = prompt_goto_line(viewer, out)? {
                                let target_line = line_number
                                    .saturating_sub(1)
                                    .min(viewer.line_count().saturating_sub(1));
                                viewer.top_line = centered_top_line(
                                    target_line,
                                    page.max(1),
                                    viewer.line_count(),
                                );
                            }
                            needs_redraw = true;
                        }
                        KeyCode::Char('f') => {
                            if let Some(query) = prompt_find(viewer, out)? {
                                let start = viewer.line_offsets[viewer.top_line];
                                if let Some((found_start, found_end)) =
                                    viewer.find_forward(query.as_bytes(), start)
                                {
                                    viewer.search_query = Some(query.into_bytes());
                                    viewer.set_match(found_start, found_end, page.max(1));
                                } else {
                                    viewer.search_query = Some(query.into_bytes());
                                    viewer.match_range = None;
                                }
                            }
                            needs_redraw = true;
                        }
                        KeyCode::Char('n') => {
                            if let Some(query) = &viewer.search_query {
                                let start = viewer
                                    .match_range
                                    .map(|(_, end)| end)
                                    .unwrap_or_else(|| viewer.line_offsets[viewer.top_line]);
                                if let Some((found_start, found_end)) =
                                    viewer.find_forward(query, start)
                                {
                                    viewer.set_match(found_start, found_end, page.max(1));
                                }
                                needs_redraw = true;
                            }
                        }
                        KeyCode::Char('p') => {
                            if let Some(query) = &viewer.search_query {
                                let start = viewer
                                    .match_range
                                    .map(|(start, _)| start.saturating_sub(1))
                                    .unwrap_or_else(|| viewer.line_offsets[viewer.top_line]);
                                if let Some((found_start, found_end)) =
                                    viewer.find_backward(query, start)
                                {
                                    viewer.set_match(found_start, found_end, page.max(1));
                                }
                                needs_redraw = true;
                            }
                        }
                        KeyCode::Left => {
                            viewer.scroll_left(1);
                            needs_redraw = true;
                        }
                        KeyCode::Right => {
                            viewer.scroll_right(1);
                            needs_redraw = true;
                        }
                        KeyCode::Up => {
                            viewer.scroll_up(1);
                            needs_redraw = true;
                        }
                        KeyCode::Down => {
                            viewer.scroll_down(1);
                            needs_redraw = true;
                        }
                        KeyCode::PageUp => {
                            viewer.scroll_up(page.max(1));
                            needs_redraw = true;
                        }
                        KeyCode::PageDown => {
                            viewer.scroll_down(page.max(1));
                            needs_redraw = true;
                        }
                        KeyCode::Home => {
                            viewer.top_line = 0;
                            needs_redraw = true;
                        }
                        KeyCode::End => {
                            if viewer.line_count() > 0 {
                                viewer.top_line = viewer.line_count() - 1;
                            }
                            needs_redraw = true;
                        }
                        _ => {}
                    }
                }
                Event::Resize(_, _) => needs_redraw = true,
                _ => {}
            }
        }
    }

    Ok(())
}

fn prompt_goto_line(viewer: &Viewer, out: &mut impl Write) -> Result<Option<usize>> {
    let mut input = String::new();

    loop {
        let (width, height) = terminal::size().context("Failed to get terminal size")?;
        let prompt = format!(
            "Goto line (1-{}, Enter=go, Esc=cancel): {}",
            viewer.line_count(),
            input
        );
        let clipped_prompt = clip_to_width(&prompt, width as usize);
        let y = height.saturating_sub(1);

        queue!(
            out,
            cursor::MoveTo(0, y),
            terminal::Clear(ClearType::CurrentLine),
            style::PrintStyledContent(clipped_prompt.reverse())
        )?;
        out.flush().context("Failed to flush terminal output")?;

        match event::read().context("Failed reading terminal event")? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc => return Ok(None),
                KeyCode::Enter => {
                    if input.is_empty() {
                        return Ok(None);
                    }

                    if let Ok(line_number) = input.parse::<usize>() {
                        if line_number >= 1 {
                            return Ok(Some(line_number));
                        }
                    }
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    input.push(c);
                }
                _ => {}
            },
            _ => {}
        }
    }
}

fn prompt_find(viewer: &Viewer, out: &mut impl Write) -> Result<Option<String>> {
    let mut input = viewer
        .search_query
        .as_ref()
        .map(|query| String::from_utf8_lossy(query).to_string())
        .unwrap_or_default();

    loop {
        let (width, height) = terminal::size().context("Failed to get terminal size")?;
        let prompt = format!("Find text (Enter=find, Esc=cancel): {}", input);
        let clipped_prompt = clip_to_width(&prompt, width as usize);
        let y = height.saturating_sub(1);

        queue!(
            out,
            cursor::MoveTo(0, y),
            terminal::Clear(ClearType::CurrentLine),
            style::PrintStyledContent(clipped_prompt.reverse())
        )?;
        out.flush().context("Failed to flush terminal output")?;

        match event::read().context("Failed reading terminal event")? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc => return Ok(None),
                KeyCode::Enter => {
                    if input.is_empty() {
                        return Ok(None);
                    }
                    return Ok(Some(input));
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) => {
                    if !c.is_control() {
                        input.push(c);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        centered_top_line, classify_json_line, classify_xml_line, format_xml_for_display,
        skipped_prefix_len, JsonTokenClass, Viewer, XmlTokenClass,
    };
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn indexes_lines() {
        let offsets = Viewer::index_lines(b"a\nb\nlast");
        assert_eq!(offsets, vec![0, 2, 4]);
    }

    #[test]
    fn indexes_empty() {
        let offsets = Viewer::index_lines(b"");
        assert_eq!(offsets, vec![0]);
    }

    #[test]
    fn centers_target_line_when_possible() {
        assert_eq!(centered_top_line(50, 20, 200), 40);
    }

    #[test]
    fn centers_target_line_with_small_targets() {
        assert_eq!(centered_top_line(3, 20, 200), 0);
    }

    #[test]
    fn centers_target_line_near_end() {
        assert_eq!(centered_top_line(199, 20, 200), 189);
    }

    #[test]
    fn finds_forward_and_backward() {
        let viewer = test_viewer_from_bytes(b"abc\ndef abc xyz abc");
        assert_eq!(viewer.find_forward(b"abc", 0), Some((0, 3)));
        assert_eq!(viewer.find_forward(b"abc", 4), Some((8, 11)));
        assert_eq!(viewer.find_backward(b"abc", 18), Some((16, 19)));
        assert_eq!(viewer.find_backward(b"missing", 18), None);
    }

    #[test]
    fn maps_offsets_to_lines() {
        let viewer = test_viewer_from_bytes(b"line1\nline2\nline3");
        assert_eq!(viewer.line_of_offset(0), 0);
        assert_eq!(viewer.line_of_offset(6), 1);
        assert_eq!(viewer.line_of_offset(12), 2);
    }

    fn with_temp_file(bytes: &[u8], f: impl FnOnce(PathBuf) -> Viewer) -> Viewer {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("large-file-viewer-test-{nonce}.txt"));
        fs::write(&path, bytes).expect("failed to write temp file");
        let viewer = f(path.clone());
        fs::remove_file(path).expect("failed to remove temp file");
        viewer
    }

    fn test_viewer_from_bytes(bytes: &[u8]) -> Viewer {
        with_temp_file(bytes, |path| {
            Viewer::open(path, 4, false, false, false, false).expect("failed to open viewer")
        })
    }

    #[test]
    fn indexes_csv_column_widths() {
        let widths = Viewer::index_csv_column_widths(b"a,bbb\ncccc,d", 4);
        assert_eq!(widths, vec![4, 3]);
    }

    #[test]
    fn csv_mode_starts_below_pinned_header() {
        let viewer = test_viewer_with_options(b"h1,h2\nv1,v2\nv3,v4", 4, true);
        assert_eq!(viewer.top_line, 1);
    }

    #[test]
    fn csv_scroll_up_keeps_header_pinned() {
        let mut viewer = test_viewer_with_options(b"h1,h2\nv1,v2\nv3,v4", 4, true);
        viewer.scroll_up(10);
        assert_eq!(viewer.top_line, 1);
    }

    fn test_viewer_with_options(bytes: &[u8], tab_width: usize, csv: bool) -> Viewer {
        with_temp_file(bytes, |path| {
            Viewer::open(path, tab_width, csv, false, false, false).expect("failed to open viewer")
        })
    }

    #[test]
    fn classifies_xml_tags_and_attributes() {
        let classes = classify_xml_line(br#"<node attr="value">text</node>"#);
        assert_eq!(classes[0], XmlTokenClass::TagDelimiter);
        assert_eq!(classes[1], XmlTokenClass::TagName);
        assert_eq!(classes[6], XmlTokenClass::AttributeName);
        assert_eq!(classes[11], XmlTokenClass::AttributeValue);
        assert_eq!(classes[19], XmlTokenClass::Text);
    }

    #[test]
    fn classifies_xml_comments() {
        let classes = classify_xml_line(br#"<!-- comment --> <node/>"#);
        assert_eq!(classes[0], XmlTokenClass::Comment);
        assert_eq!(classes[10], XmlTokenClass::Comment);
        assert_eq!(classes[16], XmlTokenClass::Text);
        assert_eq!(classes[17], XmlTokenClass::TagDelimiter);
        assert_eq!(classes[18], XmlTokenClass::TagName);
    }

    #[test]
    fn skips_utf8_bom_prefix_on_first_line_only() {
        let bom_prefixed = [0xEF_u8, 0xBB_u8, 0xBF_u8, b'<', b'x', b'>'];
        assert_eq!(skipped_prefix_len(0, &bom_prefixed), 3);
        assert_eq!(skipped_prefix_len(1, &bom_prefixed), 0);
        assert_eq!(skipped_prefix_len(0, b"<x>"), 0);
    }

    #[test]
    fn classifies_json_tokens() {
        let json = br#"{"name":"bob","age":42,"ok":true,"v":null}"#;
        let classes = classify_json_line(json);

        let first_colon = json
            .iter()
            .position(|&b| b == b':')
            .expect("json should contain colon");
        let age_index = json
            .windows(3)
            .position(|w| w == b"age")
            .expect("json should contain age");
        let number_index = json
            .iter()
            .position(|&b| b == b'4')
            .expect("json should contain number");
        let true_index = json
            .windows(4)
            .position(|w| w == b"true")
            .expect("json should contain true");
        let null_index = json
            .windows(4)
            .position(|w| w == b"null")
            .expect("json should contain null");

        assert_eq!(classes[0], JsonTokenClass::Delimiter);
        assert_eq!(classes[1], JsonTokenClass::Key);
        assert_eq!(classes[first_colon], JsonTokenClass::Delimiter);
        assert_eq!(classes[age_index], JsonTokenClass::Key);
        assert_eq!(classes[number_index], JsonTokenClass::Number);
        assert_eq!(classes[true_index], JsonTokenClass::Keyword);
        assert_eq!(classes[null_index], JsonTokenClass::Keyword);
    }

    #[test]
    fn formats_single_line_xml_into_indented_lines() {
        let xml = br#"<root><parent><child/></parent><value>text</value></root>"#;
        let formatted = format_xml_for_display(xml);
        let formatted = String::from_utf8(formatted).expect("formatted xml should be utf8");
        let expected =
            "<root>\n  <parent>\n    <child/>\n  </parent>\n  <value>text</value>\n</root>";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn formats_xml_with_comments_and_header() {
        let xml = br#"<?xml version="1.0"?><root><!-- comment --><node/></root>"#;
        let formatted = format_xml_for_display(xml);
        let formatted = String::from_utf8(formatted).expect("formatted xml should be utf8");
        let expected = "<?xml version=\"1.0\"?>\n<root>\n  <!-- comment -->\n  <node/>\n</root>";
        assert_eq!(formatted, expected);
    }
}
