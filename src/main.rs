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
    style::{self, Stylize},
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
}

struct Viewer {
    mmap: Mmap,
    line_offsets: Vec<usize>,
    top_line: usize,
    tab_width: usize,
    csv_column_widths: Option<Vec<usize>>,
    search_query: Option<Vec<u8>>,
    match_range: Option<(usize, usize)>,
}

fn centered_top_line(target_line: usize, viewport_rows: usize, line_count: usize) -> usize {
    if line_count == 0 {
        return 0;
    }

    let centered = target_line.saturating_sub(viewport_rows / 2);
    centered.min(line_count - 1)
}

impl Viewer {
    fn open(path: PathBuf, tab_width: usize, csv: bool) -> Result<Self> {
        let file = File::open(&path)
            .with_context(|| format!("Failed to open file: {}", path.display()))?;

        // SAFETY: File remains alive during mapping creation, and mapping is read-only.
        let mmap = unsafe { Mmap::map(&file) }
            .with_context(|| format!("Failed to memory-map file: {}", path.display()))?;

        let line_offsets = Self::index_lines(&mmap);
        let csv_column_widths = csv.then(|| Self::index_csv_column_widths(&mmap, tab_width));

        Ok(Self {
            mmap,
            line_offsets,
            top_line: 0,
            tab_width,
            csv_column_widths,
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
            "Lines: {} | Top: {} | q: quit | g: goto | f: find | n/p: next/prev | ↑/↓ PgUp/PgDn Home/End",
            self.line_count(),
            self.top_line + 1
        );
        let clipped_status = clip_to_width(&status, width);
        queue!(
            out,
            style::PrintStyledContent(clipped_status.reverse()),
            cursor::MoveToNextLine(1)
        )?;

        for row in 0..body_rows {
            let line_idx = self.top_line + row;
            if line_idx >= self.line_count() {
                break;
            }

            self.render_line(out, line_idx, width)?;
            queue!(out, cursor::MoveToNextLine(1))?;
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
        let mut segments: Vec<(bool, String)> = Vec::new();
        let mut visible_width = 0usize;

        let mut push_char = |c: char, is_highlight: bool| {
            if visible_width >= max_width {
                return;
            }
            if segments
                .last()
                .map(|(h, _)| *h != is_highlight)
                .unwrap_or(true)
            {
                segments.push((is_highlight, String::new()));
            }
            let (_, target) = segments.last_mut().expect("segment just pushed");
            target.push(c);
            visible_width += 1;
        };

        if let Some(column_widths) = &self.csv_column_widths {
            let mut column_idx = 0usize;
            let mut field_width = 0usize;

            for (idx, &b) in bytes.iter().enumerate() {
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
                            push_char(' ', false);
                        }
                        push_char(',', is_highlight);
                        push_char(' ', false);
                        column_idx += 1;
                        field_width = 0;
                    }
                    b'\t' => {
                        for _ in 0..self.tab_width {
                            push_char(' ', is_highlight);
                            field_width += 1;
                        }
                    }
                    0x20..=0x7e => {
                        push_char(b as char, is_highlight);
                        field_width += 1;
                    }
                    _ => {
                        push_char('·', is_highlight);
                        field_width += 1;
                    }
                }
            }
        } else {
            for (idx, &b) in bytes.iter().enumerate() {
                if b == b'\n' || b == b'\r' {
                    continue;
                }

                let absolute_idx = line_start + idx;
                let is_highlight = highlight
                    .map(|(start, end)| absolute_idx >= start && absolute_idx < end)
                    .unwrap_or(false);

                match b {
                    b'\t' => {
                        for _ in 0..self.tab_width {
                            push_char(' ', is_highlight);
                        }
                    }
                    0x20..=0x7e => push_char(b as char, is_highlight),
                    _ => push_char('·', is_highlight),
                }
            }
        }

        for (is_highlight, text) in segments {
            if is_highlight {
                queue!(out, style::PrintStyledContent(text.reverse()))?;
            } else {
                queue!(out, style::Print(text))?;
            }
        }

        Ok(())
    }

    fn scroll_up(&mut self, by: usize) {
        self.top_line = self.top_line.saturating_sub(by);
    }

    fn scroll_down(&mut self, by: usize) {
        if self.line_count() == 0 {
            self.top_line = 0;
            return;
        }
        self.top_line = min(self.top_line + by, self.line_count() - 1);
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

fn main() -> Result<()> {
    let args = Args::parse();
    let mut viewer = Viewer::open(args.file, args.tab_width, args.csv)?;

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
    use super::{centered_top_line, Viewer};
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
        let viewer = Viewer::open(path.clone(), 4, false).expect("failed to open viewer");
        fs::remove_file(path).expect("failed to remove temp file");
        viewer
    }

    #[test]
    fn indexes_csv_column_widths() {
        let widths = Viewer::index_csv_column_widths(b"a,bbb\ncccc,d", 4);
        assert_eq!(widths, vec![4, 3]);
    }
}
