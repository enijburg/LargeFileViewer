use std::{borrow::Cow, collections::BTreeMap, fs::File, io::Write, path::PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use eframe::egui::{self, RichText, TextEdit};
use memmap2::Mmap;

#[derive(Parser, Debug)]
#[command(version, about = "GUI large file editor powered by memory mapping")]
struct Args {
    /// File path to open.
    file: PathBuf,

    /// Number of spaces a tab represents when rendering.
    #[arg(long, default_value_t = 4)]
    tab_width: usize,
}

struct Document {
    path: PathBuf,
    mmap: Mmap,
    line_offsets: Vec<usize>,
    dirty_lines: BTreeMap<usize, String>,
    tab_width: usize,
}

impl Document {
    fn open(path: PathBuf, tab_width: usize) -> Result<Self> {
        let file = File::open(&path)
            .with_context(|| format!("Failed to open file: {}", path.display()))?;
        let mmap = unsafe { Mmap::map(&file) }
            .with_context(|| format!("Failed to memory-map file: {}", path.display()))?;
        let line_offsets = Self::index_lines(&mmap);
        Ok(Self {
            path,
            mmap,
            line_offsets,
            dirty_lines: BTreeMap::new(),
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

    fn line_slice(&self, line_idx: usize) -> &[u8] {
        let start = self.line_offsets[line_idx];
        let end = if line_idx + 1 < self.line_offsets.len() {
            self.line_offsets[line_idx + 1]
        } else {
            self.mmap.len()
        };
        &self.mmap[start..end]
    }

    fn line_text(&self, line_idx: usize) -> Cow<'_, str> {
        if let Some(updated) = self.dirty_lines.get(&line_idx) {
            return Cow::Borrowed(updated);
        }
        let raw = self.line_slice(line_idx);
        let no_newline = raw
            .strip_suffix(b"\n")
            .unwrap_or(raw)
            .strip_suffix(b"\r")
            .unwrap_or(raw);
        let mut out = String::with_capacity(no_newline.len());
        for &b in no_newline {
            match b {
                b'\t' => out.extend(std::iter::repeat_n(' ', self.tab_width)),
                0x20..=0x7e => out.push(b as char),
                _ => out.push('·'),
            }
        }
        Cow::Owned(out)
    }

    fn update_line(&mut self, line_idx: usize, value: String) {
        self.dirty_lines.insert(line_idx, value);
    }

    fn save_as(&self, output: &PathBuf) -> Result<()> {
        let mut out = File::create(output)
            .with_context(|| format!("Failed to create output file: {}", output.display()))?;

        for idx in 0..self.line_count() {
            if let Some(updated) = self.dirty_lines.get(&idx) {
                out.write_all(updated.as_bytes())?;
                out.write_all(b"\n")?;
            } else {
                let slice = self.line_slice(idx);
                out.write_all(slice)?;
                if idx + 1 == self.line_count() && !slice.ends_with(b"\n") {
                    out.write_all(b"\n")?;
                }
            }
        }
        out.flush()?;
        Ok(())
    }
}

struct GuiApp {
    doc: Document,
    edit_line: Option<usize>,
    edit_buffer: String,
    save_path: String,
    status: String,
}

impl GuiApp {
    fn new(doc: Document) -> Self {
        let default_save = doc.path.with_extension("edited.txt").display().to_string();
        Self {
            doc,
            edit_line: None,
            edit_buffer: String::new(),
            save_path: default_save,
            status: String::new(),
        }
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(self.doc.path.display().to_string()).strong());
                ui.separator();
                ui.label(format!("Lines: {}", self.doc.line_count()));
                ui.label(format!("Modified lines: {}", self.doc.dirty_lines.len()));
            });

            ui.horizontal(|ui| {
                ui.label("Save as:");
                ui.text_edit_singleline(&mut self.save_path);
                if ui.button("Save").clicked() {
                    let target = PathBuf::from(self.save_path.trim());
                    match self.doc.save_as(&target) {
                        Ok(()) => self.status = format!("Saved {}", target.display()),
                        Err(err) => self.status = format!("Save failed: {err:#}"),
                    }
                }
                if !self.status.is_empty() {
                    ui.label(&self.status);
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
            egui::ScrollArea::vertical().show_rows(
                ui,
                row_height,
                self.doc.line_count(),
                |ui, row_range| {
                    for line_idx in row_range {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("{:>7}", line_idx + 1))
                                    .monospace()
                                    .weak(),
                            );

                            if self.edit_line == Some(line_idx) {
                                let response = ui.add(
                                    TextEdit::singleline(&mut self.edit_buffer)
                                        .font(egui::TextStyle::Monospace)
                                        .desired_width(f32::INFINITY),
                                );
                                let enter = ui.input(|input| input.key_pressed(egui::Key::Enter));
                                let escape = ui.input(|input| input.key_pressed(egui::Key::Escape));

                                if response.lost_focus() && enter {
                                    self.doc.update_line(line_idx, self.edit_buffer.clone());
                                    self.edit_line = None;
                                } else if escape {
                                    self.edit_line = None;
                                }
                            } else {
                                let line_text = self.doc.line_text(line_idx);
                                let response = ui
                                    .selectable_label(false, RichText::new(line_text).monospace());
                                if response.double_clicked() {
                                    self.edit_line = Some(line_idx);
                                    self.edit_buffer = self.doc.line_text(line_idx).into_owned();
                                }
                            }
                        });
                    }
                },
            );
        });
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let doc = Document::open(args.file, args.tab_width)?;
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };

    eframe::run_native(
        "Large File Viewer GUI",
        options,
        Box::new(move |_cc| Ok(Box::new(GuiApp::new(doc)))),
    )
    .map_err(|err| anyhow::anyhow!("GUI exited with an error: {err}"))
}
