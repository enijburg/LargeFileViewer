use std::{
    cmp::min,
    fs::File,
    io::{self, Write},
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result};
use arboard::Clipboard;
use clap::Parser;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind},
    execute, queue,
    style::{self, Color, Stylize},
    terminal::{self, ClearType},
};
use memmap2::Mmap;
use ui::prompt::{prompt_find, prompt_goto_line};

#[derive(Parser, Debug)]
#[command(version, about = "Memory-mapped large file viewer for Windows x64")]
struct Args {
    /// File path to view.
    file: PathBuf,

    /// Number of spaces a tab represents when rendering.
    #[arg(long, default_value_t = 4)]
    tab_width: usize,

    /// Render delimited values in aligned columns.
    #[arg(long)]
    csv: bool,

    /// Field separator for --csv mode (single ASCII character, e.g. ",", ";", "\t", "|").
    #[arg(long)]
    csv_separator: Option<String>,

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
    formatted_view: Option<Vec<u8>>,
    line_offsets: Vec<usize>,
    top_line: usize,
    left_col: usize,
    tab_width: usize,
    csv_column_widths: Option<Vec<usize>>,
    csv_separator: Option<u8>,
    xml_syntax_highlighting: bool,
    json_syntax_highlighting: bool,
    search_query: Option<Vec<u8>>,
    match_range: Option<(usize, usize)>,
    cursor_offset: usize,
    selection_anchor: Option<usize>,
    selection_focus: Option<usize>,
    block_selection_anchor: Option<(usize, usize)>,
    block_selection_focus: Option<(usize, usize)>,
    status_message: Option<String>,
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

fn resolve_csv_separator(user_separator: Option<&str>, bytes: &[u8]) -> Result<u8> {
    if let Some(separator) = user_separator {
        return parse_csv_separator(separator);
    }
    Ok(detect_csv_separator(bytes))
}

fn parse_csv_separator(raw: &str) -> Result<u8> {
    let separator = match raw {
        r"\t" => b'\t',
        _ => {
            let bytes = raw.as_bytes();
            if bytes.len() != 1 || !bytes[0].is_ascii() {
                anyhow::bail!("--csv-separator must be a single ASCII character or \\t");
            }
            bytes[0]
        }
    };
    Ok(separator)
}

fn detect_csv_separator(bytes: &[u8]) -> u8 {
    const CANDIDATES: [u8; 5] = [b',', b';', b'\t', b'|', b':'];
    let sample_lines = bytes
        .split(|&b| b == b'\n')
        .take(50)
        .filter(|line| !line.is_empty());
    let mut best = (b',', 0usize);
    for candidate in CANDIDATES {
        let count = sample_lines
            .clone()
            .map(|line| line.iter().filter(|&&b| b == candidate).count())
            .sum();
        if count > best.1 {
            best = (candidate, count);
        }
    }
    best.0
}

impl Viewer {
    fn open(
        path: PathBuf,
        tab_width: usize,
        csv: bool,
        csv_separator: Option<&str>,
        xml_syntax_highlighting: bool,
        xml_formatting: bool,
        json_syntax_highlighting: bool,
    ) -> Result<Self> {
        let file = File::open(&path)
            .with_context(|| format!("Failed to open file: {}", path.display()))?;

        // SAFETY: File remains alive during mapping creation, and mapping is read-only.
        let mmap = unsafe { Mmap::map(&file) }
            .with_context(|| format!("Failed to memory-map file: {}", path.display()))?;

        let formatted_view = if xml_formatting && !csv {
            if xml_syntax_highlighting && !json_syntax_highlighting {
                Some(format_xml_for_display(&mmap))
            } else if json_syntax_highlighting && !xml_syntax_highlighting {
                format_json_for_display(&mmap)
            } else {
                None
            }
        } else {
            None
        };
        let source_bytes = formatted_view.as_deref().unwrap_or(&mmap);
        let line_offsets = Self::index_lines(source_bytes);
        let csv_separator = if csv {
            Some(resolve_csv_separator(csv_separator, source_bytes)?)
        } else {
            None
        };
        let csv_column_widths = csv_separator
            .map(|separator| Self::index_csv_column_widths(source_bytes, tab_width, separator));

        let top_line = if csv && line_offsets.len() > 1 { 1 } else { 0 };

        Ok(Self {
            mmap,
            formatted_view,
            line_offsets,
            top_line,
            left_col: 0,
            tab_width,
            csv_column_widths,
            csv_separator,
            xml_syntax_highlighting,
            json_syntax_highlighting,
            search_query: None,
            match_range: None,
            cursor_offset: 0,
            selection_anchor: None,
            selection_focus: None,
            block_selection_anchor: None,
            block_selection_focus: None,
            status_message: None,
        })
    }

    fn view_bytes(&self) -> &[u8] {
        self.formatted_view.as_deref().unwrap_or(&self.mmap)
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

    fn index_csv_column_widths(bytes: &[u8], tab_width: usize, separator: u8) -> Vec<usize> {
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
                b if b == separator => {
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
        let content_width = width.saturating_sub(1);

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
            self.render_line(out, 0, content_width)?;
            queue!(out, cursor::MoveToNextLine(1))?;

            let start = self.top_line.max(1);
            for row in 1..body_rows {
                let line_idx = start + (row - 1);
                if line_idx >= self.line_count() {
                    break;
                }

                self.render_line(out, line_idx, content_width)?;
                queue!(out, cursor::MoveToNextLine(1))?;
            }
        } else {
            for row in 0..body_rows {
                let line_idx = self.top_line + row;
                if line_idx >= self.line_count() {
                    break;
                }

                self.render_line(out, line_idx, content_width)?;
                queue!(out, cursor::MoveToNextLine(1))?;
            }
        }

        self.render_scrollbar(out, width, body_rows)?;

        let footer = self
            .status_message
            .as_deref()
            .unwrap_or("Memory-mapped view (renders visible window only)");
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

        let selection = self.selection_range().and_then(|(start, end)| {
            if start < line_end && end > line_start {
                Some((start, end))
            } else {
                None
            }
        });
        let block_selection = self.block_selection_range();
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

        if let Some(column_widths) = &self.csv_column_widths {
            let separator = self.csv_separator.unwrap_or(b',');
            let mut column_idx = 0usize;
            let mut field_width = 0usize;

            for (idx, &b) in bytes.iter().enumerate().skip(content_start) {
                if b == b'\n' || b == b'\r' {
                    continue;
                }

                let absolute_idx = line_start + idx;
                let is_selected = selection
                    .map(|(start, end)| absolute_idx >= start && absolute_idx < end)
                    .unwrap_or(false);
                let current_col = absolute_col;
                let is_block_selected = if let Some((top, bottom, left, right)) = block_selection {
                    line_idx >= top
                        && line_idx <= bottom
                        && current_col >= left
                        && current_col <= right
                } else {
                    false
                };
                let is_highlight = is_selected
                    || is_block_selected
                    || highlight
                        .map(|(start, end)| absolute_idx >= start && absolute_idx < end)
                        .unwrap_or(false);

                match b {
                    b if b == separator => {
                        let target_width = column_widths.get(column_idx).copied().unwrap_or(0);
                        for _ in field_width..target_width {
                            push_char(
                                ' ',
                                false,
                                RenderClass::Text,
                                self.left_col,
                                max_width,
                                &mut absolute_col,
                                &mut visible_width,
                                &mut segments,
                            );
                        }
                        push_char(
                            separator as char,
                            is_highlight,
                            RenderClass::Text,
                            self.left_col,
                            max_width,
                            &mut absolute_col,
                            &mut visible_width,
                            &mut segments,
                        );
                        push_char(
                            ' ',
                            false,
                            RenderClass::Text,
                            self.left_col,
                            max_width,
                            &mut absolute_col,
                            &mut visible_width,
                            &mut segments,
                        );
                        column_idx += 1;
                        field_width = 0;
                    }
                    b'\t' => {
                        for _ in 0..self.tab_width {
                            push_char(
                                ' ',
                                is_highlight,
                                RenderClass::Text,
                                self.left_col,
                                max_width,
                                &mut absolute_col,
                                &mut visible_width,
                                &mut segments,
                            );
                            field_width += 1;
                        }
                    }
                    0x20..=0x7e => {
                        push_char(
                            b as char,
                            is_highlight,
                            RenderClass::Text,
                            self.left_col,
                            max_width,
                            &mut absolute_col,
                            &mut visible_width,
                            &mut segments,
                        );
                        field_width += 1;
                    }
                    _ => {
                        push_char(
                            '·',
                            is_highlight,
                            RenderClass::Text,
                            self.left_col,
                            max_width,
                            &mut absolute_col,
                            &mut visible_width,
                            &mut segments,
                        );
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
                let is_selected = selection
                    .map(|(start, end)| absolute_idx >= start && absolute_idx < end)
                    .unwrap_or(false);
                let current_col = absolute_col;
                let is_block_selected = if let Some((top, bottom, left, right)) = block_selection {
                    line_idx >= top
                        && line_idx <= bottom
                        && current_col >= left
                        && current_col <= right
                } else {
                    false
                };
                let is_highlight = is_selected
                    || is_block_selected
                    || highlight
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
                            push_char(
                                ' ',
                                is_highlight,
                                render_class,
                                self.left_col,
                                max_width,
                                &mut absolute_col,
                                &mut visible_width,
                                &mut segments,
                            );
                        }
                    }
                    0x20..=0x7e => push_char(
                        b as char,
                        is_highlight,
                        render_class,
                        self.left_col,
                        max_width,
                        &mut absolute_col,
                        &mut visible_width,
                        &mut segments,
                    ),
                    _ => push_char(
                        '·',
                        is_highlight,
                        render_class,
                        self.left_col,
                        max_width,
                        &mut absolute_col,
                        &mut visible_width,
                        &mut segments,
                    ),
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

    fn render_scrollbar(&self, out: &mut impl Write, width: usize, body_rows: usize) -> Result<()> {
        if width == 0 || body_rows == 0 {
            return Ok(());
        }

        let scrollbar_col = (width - 1) as u16;
        let line_count = self.line_count();

        let (thumb_top, thumb_bottom) = if line_count <= body_rows {
            (0, body_rows)
        } else {
            let thumb_size = body_rows
                .saturating_mul(body_rows)
                .checked_div(line_count)
                .unwrap_or(body_rows)
                .max(1)
                .min(body_rows);
            let scrollable = line_count - body_rows;
            let thumb_top = self
                .top_line
                .saturating_mul(body_rows - thumb_size)
                .checked_div(scrollable)
                .unwrap_or(0)
                .min(body_rows - thumb_size);
            (thumb_top, (thumb_top + thumb_size).min(body_rows))
        };

        for row in 0..body_rows {
            let screen_row = row as u16 + 1; // +1 because row 0 is the status bar
            let ch = if row >= thumb_top && row < thumb_bottom {
                '█'
            } else {
                '░'
            };
            queue!(
                out,
                cursor::MoveTo(scrollbar_col, screen_row),
                style::PrintStyledContent(style::style(ch).dark_grey())
            )?;
        }

        Ok(())
    }

    fn scroll_up(&mut self, by: usize) {
        let min_top = usize::from(self.csv_column_widths.is_some() && self.line_count() > 1);
        self.top_line = self.top_line.saturating_sub(by).max(min_top);
    }

    fn scroll_down(&mut self, by: usize, viewport_rows: usize) {
        if self.line_count() == 0 {
            self.top_line = 0;
            return;
        }
        // In CSV mode the header occupies one viewport row, leaving one fewer row for data.
        let visible_rows = if self.csv_column_widths.is_some() && viewport_rows > 1 {
            viewport_rows - 1
        } else {
            viewport_rows
        };
        let max_top = self.line_count().saturating_sub(visible_rows);
        // In CSV mode the header row (line 0) is always pinned, so top_line must be at least 1.
        // The line_count() > 1 guard avoids pinning past the only line in a single-line CSV file.
        let min_top = usize::from(self.csv_column_widths.is_some() && self.line_count() > 1);
        self.top_line = min(self.top_line + by, max_top).max(min_top);
    }

    fn scroll_left(&mut self, by: usize) {
        self.left_col = self.left_col.saturating_sub(by);
    }

    /// Maps a scrollbar screen row (0-indexed from top of terminal) to `top_line`,
    /// using the inverse of the thumb-position formula used in `render_scrollbar`.
    fn scroll_to_scrollbar_row(&mut self, screen_row: usize, body_rows: usize) {
        let line_count = self.line_count();
        if body_rows == 0 || line_count <= body_rows {
            return;
        }
        // screen_row 0 is the status bar; body rows start at screen row 1.
        let row = screen_row
            .saturating_sub(1)
            .min(body_rows.saturating_sub(1));

        let thumb_size = body_rows
            .saturating_mul(body_rows)
            .checked_div(line_count)
            .unwrap_or(body_rows)
            .max(1)
            .min(body_rows);

        let scrollable = line_count - body_rows;
        let track_rows = body_rows.saturating_sub(thumb_size);

        let top_line = if track_rows == 0 {
            0
        } else {
            row.saturating_mul(scrollable)
                .checked_div(track_rows)
                .unwrap_or(0)
                .min(scrollable)
        };

        let min_top = usize::from(self.csv_column_widths.is_some() && line_count > 1);
        self.top_line = top_line.max(min_top);
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

    fn selection_range(&self) -> Option<(usize, usize)> {
        let (a, b) = (self.selection_anchor?, self.selection_focus?);
        if a == b {
            None
        } else {
            Some((a.min(b), a.max(b)))
        }
    }

    fn clear_selection(&mut self) {
        self.selection_anchor = None;
        self.selection_focus = None;
        self.block_selection_anchor = None;
        self.block_selection_focus = None;
    }

    fn block_selection_range(&self) -> Option<(usize, usize, usize, usize)> {
        let (a_line, a_col) = self.block_selection_anchor?;
        let (b_line, b_col) = self.block_selection_focus?;
        if a_line == b_line && a_col == b_col {
            None
        } else {
            Some((
                a_line.min(b_line),
                a_line.max(b_line),
                a_col.min(b_col),
                a_col.max(b_col),
            ))
        }
    }

    fn line_display_chars(&self, line_idx: usize) -> Vec<char> {
        let line_start = self.line_offsets[line_idx];
        let view = self.view_bytes();
        let line_end = if line_idx + 1 < self.line_offsets.len() {
            self.line_offsets[line_idx + 1]
        } else {
            view.len()
        };
        let bytes = &view[line_start..line_end];
        let mut out = Vec::new();
        for &b in bytes.iter().skip(skipped_prefix_len(line_idx, bytes)) {
            if b == b'\n' || b == b'\r' {
                continue;
            }
            if b == b'\t' {
                out.extend(std::iter::repeat_n(' ', self.tab_width));
            } else if (0x20..=0x7e).contains(&b) {
                out.push(b as char);
            } else {
                out.push('·');
            }
        }
        out
    }

    fn offset_for_line_col(&self, line_idx: usize, target_col: usize) -> usize {
        let view = self.view_bytes();
        let line_start = *self.line_offsets.get(line_idx).unwrap_or(&0);
        let line_end = if line_idx + 1 < self.line_offsets.len() {
            self.line_offsets[line_idx + 1]
        } else {
            view.len()
        };
        let bytes = &view[line_start..line_end];
        let mut visual_col = 0usize;
        for (idx, &b) in bytes
            .iter()
            .enumerate()
            .skip(skipped_prefix_len(line_idx, bytes))
        {
            if b == b'\n' || b == b'\r' {
                continue;
            }
            if visual_col >= target_col {
                return line_start + idx;
            }
            visual_col += if b == b'\t' { self.tab_width } else { 1 };
        }
        line_end.saturating_sub(1)
    }
}

fn clip_to_width(s: &str, max_width: usize) -> String {
    s.chars().take(max_width).collect()
}

fn push_char(
    c: char,
    is_highlight: bool,
    render_class: RenderClass,
    left_col: usize,
    max_width: usize,
    absolute_col: &mut usize,
    visible_width: &mut usize,
    segments: &mut Vec<(bool, RenderClass, String)>,
) {
    if *absolute_col < left_col {
        *absolute_col += 1;
        return;
    }
    if *visible_width >= max_width {
        *absolute_col += 1;
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
    *visible_width += 1;
    *absolute_col += 1;
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

fn format_json_for_display(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(bytes.len().saturating_add(bytes.len() / 2));
    let mut idx = 0usize;
    let mut indent = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    while idx < bytes.len() {
        let b = bytes[idx];
        if in_string {
            out.push(b);
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            idx += 1;
            continue;
        }

        match b {
            b'"' => {
                in_string = true;
                out.push(b'"');
            }
            b'{' | b'[' => {
                out.push(b);
                out.push(b'\n');
                indent = indent.saturating_add(1);
                out.extend(std::iter::repeat_n(b' ', indent.saturating_mul(2)));
            }
            b'}' | b']' => {
                out.push(b'\n');
                indent = indent.saturating_sub(1);
                out.extend(std::iter::repeat_n(b' ', indent.saturating_mul(2)));
                out.push(b);
            }
            b',' => {
                out.push(b',');
                out.push(b'\n');
                out.extend(std::iter::repeat_n(b' ', indent.saturating_mul(2)));
            }
            b':' => {
                out.push(b':');
                out.push(b' ');
            }
            b if b.is_ascii_whitespace() => {}
            _ => out.push(b),
        }
        idx += 1;
    }

    if in_string || indent != 0 {
        return None;
    }
    Some(out)
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut viewer = Viewer::open(
        args.file,
        args.tab_width,
        args.csv,
        args.csv_separator.as_deref(),
        args.xml,
        args.format,
        args.json,
    )?;

    terminal::enable_raw_mode().context("Failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        terminal::EnterAlternateScreen,
        cursor::Hide,
        event::EnableMouseCapture
    )?;

    let run_result = run_event_loop(&mut viewer, &mut stdout);

    execute!(
        stdout,
        event::DisableMouseCapture,
        cursor::Show,
        terminal::LeaveAlternateScreen
    )?;
    terminal::disable_raw_mode().context("Failed to disable raw mode")?;

    run_result
}

fn run_event_loop(viewer: &mut Viewer, out: &mut impl Write) -> Result<()> {
    let mut needs_redraw = true;
    let mut scrollbar_drag = false;
    let mut selection_drag = false;
    let mut clipboard = Clipboard::new().ok();

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
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            if copy_selection_to_clipboard(viewer, &mut clipboard) {
                                needs_redraw = true;
                            }
                        }
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
                            if key.modifiers.contains(KeyModifiers::SHIFT) {
                                let anchor =
                                    viewer.selection_anchor.unwrap_or(viewer.cursor_offset);
                                viewer.cursor_offset = viewer.cursor_offset.saturating_sub(1);
                                viewer.selection_anchor = Some(anchor);
                                viewer.selection_focus = Some(viewer.cursor_offset);
                            } else {
                                viewer.clear_selection();
                                viewer.cursor_offset = viewer.cursor_offset.saturating_sub(1);
                            }
                            needs_redraw = true;
                        }
                        KeyCode::Right => {
                            viewer.scroll_right(1);
                            if key.modifiers.contains(KeyModifiers::SHIFT) {
                                let anchor =
                                    viewer.selection_anchor.unwrap_or(viewer.cursor_offset);
                                viewer.cursor_offset =
                                    (viewer.cursor_offset + 1).min(viewer.view_bytes().len());
                                viewer.selection_anchor = Some(anchor);
                                viewer.selection_focus = Some(viewer.cursor_offset);
                            } else {
                                viewer.clear_selection();
                                viewer.cursor_offset =
                                    (viewer.cursor_offset + 1).min(viewer.view_bytes().len());
                            }
                            needs_redraw = true;
                        }
                        KeyCode::Up => {
                            viewer.scroll_up(1);
                            needs_redraw = true;
                        }
                        KeyCode::Down => {
                            viewer.scroll_down(1, page);
                            needs_redraw = true;
                        }
                        KeyCode::PageUp => {
                            viewer.scroll_up(page.max(1));
                            needs_redraw = true;
                        }
                        KeyCode::PageDown => {
                            viewer.scroll_down(page.max(1), page);
                            needs_redraw = true;
                        }
                        KeyCode::Home => {
                            viewer.top_line = 0;
                            needs_redraw = true;
                        }
                        KeyCode::End => {
                            viewer.scroll_down(usize::MAX, page);
                            needs_redraw = true;
                        }
                        _ => {}
                    }
                }
                Event::Resize(_, _) => needs_redraw = true,
                Event::Mouse(mouse) => {
                    let (width, height) =
                        terminal::size().context("Failed to get terminal size")?;
                    let body_rows = height.saturating_sub(2) as usize;
                    let scrollbar_col = width.saturating_sub(1);
                    match mouse.kind {
                        MouseEventKind::Down(MouseButton::Left)
                            if mouse.column == scrollbar_col =>
                        {
                            scrollbar_drag = true;
                            viewer.scroll_to_scrollbar_row(mouse.row as usize, body_rows);
                            needs_redraw = true;
                        }
                        MouseEventKind::Down(MouseButton::Left) => {
                            selection_drag = true;
                            let row = mouse.row as usize;
                            if row > 0 && row <= body_rows {
                                let line_idx = viewer.top_line + row - 1;
                                if line_idx < viewer.line_count() {
                                    let col = viewer.left_col + mouse.column as usize;
                                    let offset = viewer.offset_for_line_col(line_idx, col);
                                    viewer.cursor_offset = offset;
                                    if mouse.modifiers.contains(KeyModifiers::CONTROL) {
                                        viewer.selection_anchor = None;
                                        viewer.selection_focus = None;
                                        viewer.block_selection_anchor = Some((line_idx, col));
                                        viewer.block_selection_focus = Some((line_idx, col));
                                        viewer.status_message =
                                            Some("Selecting block…".to_string());
                                    } else {
                                        viewer.block_selection_anchor = None;
                                        viewer.block_selection_focus = None;
                                        viewer.selection_anchor = Some(offset);
                                        viewer.selection_focus = Some(offset);
                                        viewer.status_message = Some("Selecting text…".to_string());
                                    }
                                    needs_redraw = true;
                                }
                            }
                        }
                        MouseEventKind::Drag(MouseButton::Left) if scrollbar_drag => {
                            viewer.scroll_to_scrollbar_row(mouse.row as usize, body_rows);
                            needs_redraw = true;
                        }
                        MouseEventKind::Drag(MouseButton::Left) if selection_drag => {
                            let row = mouse.row as usize;
                            if row > 0 && row <= body_rows {
                                let line_idx = viewer.top_line + row - 1;
                                if line_idx < viewer.line_count() {
                                    let col = viewer.left_col + mouse.column as usize;
                                    let offset = viewer.offset_for_line_col(line_idx, col);
                                    viewer.cursor_offset = offset;
                                    if viewer.block_selection_anchor.is_some() {
                                        viewer.block_selection_focus = Some((line_idx, col));
                                    } else {
                                        viewer.selection_focus = Some(offset);
                                    }
                                    needs_redraw = true;
                                }
                            }
                        }
                        MouseEventKind::Up(MouseButton::Left) => {
                            scrollbar_drag = false;
                            if selection_drag {
                                selection_drag = false;
                                viewer.status_message = if viewer.block_selection_range().is_some()
                                {
                                    Some(
                                        "Block selected. Right-click or Ctrl+C to copy."
                                            .to_string(),
                                    )
                                } else {
                                    viewer.selection_range().map(|(start, end)| {
                                        format!(
                                            "Selected {} bytes. Press Ctrl+C to copy.",
                                            end - start
                                        )
                                    })
                                };
                                needs_redraw = true;
                            }
                        }
                        MouseEventKind::Down(MouseButton::Right) => {
                            if copy_selection_to_clipboard(viewer, &mut clipboard) {
                                needs_redraw = true;
                            }
                        }
                        MouseEventKind::ScrollDown => {
                            viewer.scroll_down(3, body_rows);
                            needs_redraw = true;
                        }
                        MouseEventKind::ScrollUp => {
                            viewer.scroll_up(3);
                            needs_redraw = true;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }

    Ok(())
}

fn copy_selection_to_clipboard(viewer: &mut Viewer, clipboard: &mut Option<Clipboard>) -> bool {
    if let Some((top, bottom, left, right)) = viewer.block_selection_range() {
        let mut text = String::new();
        for line_idx in top..=bottom {
            if line_idx >= viewer.line_count() {
                break;
            }
            let chars = viewer.line_display_chars(line_idx);
            let start = left.min(chars.len());
            let end = (right + 1).min(chars.len());
            if start < end {
                text.extend(chars[start..end].iter());
            }
            if line_idx < bottom {
                text.push('\n');
            }
        }
        if let Some(cb) = clipboard.as_mut() {
            if cb.set_text(text).is_ok() {
                viewer.status_message = Some("Copied block selection to clipboard".to_string());
            } else {
                viewer.status_message = Some("Failed to copy selection to clipboard".to_string());
            }
        } else {
            viewer.status_message = Some("Clipboard unavailable in this environment".to_string());
        }
        return true;
    }

    let Some((start, end)) = viewer.selection_range() else {
        return false;
    };

    let copied = String::from_utf8_lossy(&viewer.view_bytes()[start..end]).to_string();
    if let Some(cb) = clipboard.as_mut() {
        if cb.set_text(copied).is_ok() {
            viewer.status_message = Some(format!("Copied {} bytes to clipboard", end - start));
        } else {
            viewer.status_message = Some("Failed to copy selection to clipboard".to_string());
        }
    } else {
        viewer.status_message = Some("Clipboard unavailable in this environment".to_string());
    }
    true
}

mod ui;

#[cfg(test)]
mod tests;
