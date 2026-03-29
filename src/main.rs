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
}

struct Viewer {
    mmap: Mmap,
    line_offsets: Vec<usize>,
    top_line: usize,
    tab_width: usize,
}

fn centered_top_line(target_line: usize, viewport_rows: usize, line_count: usize) -> usize {
    if line_count == 0 {
        return 0;
    }

    let centered = target_line.saturating_sub(viewport_rows / 2);
    centered.min(line_count - 1)
}

impl Viewer {
    fn open(path: PathBuf, tab_width: usize) -> Result<Self> {
        let file = File::open(&path)
            .with_context(|| format!("Failed to open file: {}", path.display()))?;

        // SAFETY: File remains alive during mapping creation, and mapping is read-only.
        let mmap = unsafe { Mmap::map(&file) }
            .with_context(|| format!("Failed to memory-map file: {}", path.display()))?;

        let line_offsets = Self::index_lines(&mmap);

        Ok(Self {
            mmap,
            line_offsets,
            top_line: 0,
            tab_width,
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
            "Lines: {} | Top: {} | q: quit | g: goto | ↑/↓ PgUp/PgDn Home/End",
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

            let rendered = self.line_text(line_idx, width);
            queue!(out, style::Print(rendered), cursor::MoveToNextLine(1))?;
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

    fn line_text(&self, line_idx: usize, max_width: usize) -> String {
        let start = self.line_offsets[line_idx];
        let end = if line_idx + 1 < self.line_offsets.len() {
            self.line_offsets[line_idx + 1]
        } else {
            self.mmap.len()
        };

        let bytes = &self.mmap[start..end];
        let mut out = String::with_capacity(min(max_width + 1, bytes.len()));
        let mut visible_width = 0usize;

        for &b in bytes {
            if b == b'\n' || b == b'\r' {
                continue;
            }
            if visible_width >= max_width {
                break;
            }

            match b {
                b'\t' => {
                    for _ in 0..self.tab_width {
                        if visible_width >= max_width {
                            break;
                        }
                        out.push(' ');
                        visible_width += 1;
                    }
                }
                0x20..=0x7e => {
                    out.push(b as char);
                    visible_width += 1;
                }
                _ => {
                    out.push('·');
                    visible_width += 1;
                }
            }
        }

        out
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
}

fn clip_to_width(s: &str, max_width: usize) -> String {
    s.chars().take(max_width).collect()
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut viewer = Viewer::open(args.file, args.tab_width)?;

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

#[cfg(test)]
mod tests {
    use super::{centered_top_line, Viewer};

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
}
