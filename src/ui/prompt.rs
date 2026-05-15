use crate::{clip_to_width, Viewer};
use anyhow::{Context, Result};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind},
    queue,
    style::Stylize,
    terminal::{self, ClearType},
};
use std::io::Write;

pub(crate) fn prompt_goto_line(viewer: &Viewer, out: &mut impl Write) -> Result<Option<usize>> {
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

pub(crate) fn prompt_find(viewer: &Viewer, out: &mut impl Write) -> Result<Option<String>> {
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
