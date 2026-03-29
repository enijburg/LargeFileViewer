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
}

struct Viewer {
    mmap: Mmap,
    line_offsets: Vec<usize>,
    top_line: usize,
    left_col: usize,
    tab_width: usize,
    csv_column_widths: Option<Vec<usize>>,
    xml_syntax_highlighting: bool,
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
    ) -> Result<Self> {
        let file = File::open(&path)
            .with_context(|| format!("Failed to open file: {}", path.display()))?;

        // SAFETY: File remains alive during mapping creation, and mapping is read-only.
        let mmap = unsafe { Mmap::map(&file) }
            .with_context(|| format!("Failed to memory-map file: {}", path.display()))?;

        let line_offsets = Self::index_lines(&mmap);
        let csv_column_widths = csv.then(|| Self::index_csv_column_widths(&mmap, tab_width));

        let top_line = if csv && line_offsets.len() > 1 { 1 } else { 0 };

        Ok(Self {
            mmap,
            line_offsets,
            top_line,
            left_col: 0,
            tab_width,
            csv_column_widths,
            xml_syntax_highlighting,
            search_query: None,
            match_range: None,
        })
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
        let line_end = if line_idx + 1 < self.line_offsets.len() {
            self.line_offsets[line_idx + 1]
        } else {
            self.mmap.len()
        };
        let highlight = self.match_range.and_then(|(start, end)| {
            if start < line_end && end > line_start {
                Some((start, end))
            } else {
                None
            }
        });

        let bytes = &self.mmap[line_start..line_end];
        let content_start = skipped_prefix_len(line_idx, bytes);
        let mut segments: Vec<(bool, RenderClass, String)> = Vec::new();
        let mut visible_width = 0usize;
        let mut absolute_col = 0usize;
        let xml_classes = (!self.xml_syntax_highlighting || self.csv_column_widths.is_some())
            .then(Vec::new)
            .unwrap_or_else(|| classify_xml_line(bytes));

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
                let render_class =
                    match xml_classes.get(idx).copied().unwrap_or(XmlTokenClass::Text) {
                        XmlTokenClass::Text => RenderClass::Text,
                        XmlTokenClass::TagDelimiter => RenderClass::TagDelimiter,
                        XmlTokenClass::TagName => RenderClass::TagName,
                        XmlTokenClass::AttributeName => RenderClass::AttributeName,
                        XmlTokenClass::AttributeValue => RenderClass::AttributeValue,
                        XmlTokenClass::Comment => RenderClass::Comment,
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
        if query.is_empty() || start >= self.mmap.len() {
            return None;
        }
        self.mmap[start..]
            .windows(query.len())
            .position(|window| window == query)
            .map(|relative| {
                let found_start = start + relative;
                (found_start, found_start + query.len())
            })
    }

    fn find_backward(&self, query: &[u8], start: usize) -> Option<(usize, usize)> {
        if query.is_empty() || self.mmap.is_empty() {
            return None;
        }
        let end = min(start.saturating_add(1), self.mmap.len());
        if end < query.len() {
            return None;
        }
        self.mmap[..end]
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

fn main() -> Result<()> {
    let args = Args::parse();
    let mut viewer = Viewer::open(args.file, args.tab_width, args.csv, args.xml)?;

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
    use super::{centered_top_line, classify_xml_line, skipped_prefix_len, Viewer, XmlTokenClass};
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

    fn test_viewer_from_bytes(bytes: &[u8]) -> Viewer {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        let path: PathBuf =
            std::env::temp_dir().join(format!("large-file-viewer-test-{nonce}.txt"));
        fs::write(&path, bytes).expect("failed to write temp file");
        let viewer = Viewer::open(path.clone(), 4, false, false).expect("failed to open viewer");
        fs::remove_file(path).expect("failed to remove temp file");
        viewer
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
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        let path: PathBuf =
            std::env::temp_dir().join(format!("large-file-viewer-test-{nonce}.txt"));
        fs::write(&path, bytes).expect("failed to write temp file");
        let viewer =
            Viewer::open(path.clone(), tab_width, csv, false).expect("failed to open viewer");
        fs::remove_file(path).expect("failed to remove temp file");
        viewer
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
}
