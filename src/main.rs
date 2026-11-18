#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

mod converter;
mod import;
mod output;
mod theme;

use std::collections::{HashMap, HashSet};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use converter::{Category, Options};
use eframe::egui::{self, Color32, RichText};

const APP_NAME: &str = "ConvertCraft";
/// Import errors kept for the tooltip; the total is still counted.
const MAX_REPORTED_ERRORS: usize = 50;
/// How long closing the window waits for running jobs to stop.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

fn main() -> eframe::Result {
    let logo = image::load_from_memory(include_bytes!("../assets/logos/logo.png"))
        .expect("embedded logo must be a valid image")
        .resize(256, 256, image::imageops::FilterType::Lanczos3)
        .into_rgba8();
    let icon = egui::IconData {
        width: logo.width(),
        height: logo.height(),
        rgba: logo.into_raw(),
    };
    let logo = egui::ColorImage::from_rgba_unmultiplied(
        [icon.width as usize, icon.height as usize],
        &icon.rgba,
    );
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_icon(icon)
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([860.0, 520.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |cc| Ok(Box::new(App::new(&cc.egui_ctx, logo)))),
    )
}

enum Status {
    Pending,
    Running,
    Done(PathBuf),
    Failed(String),
}

struct FileItem {
    path: PathBuf,
    category: Category,
    target: &'static str,
    status: Status,
}

enum Msg {
    Started(usize),
    Finished(usize, Result<PathBuf, String>),
}

struct Job {
    idx: usize,
    input: PathBuf,
    output: output::OutputPlan,
    category: Category,
    target: &'static str,
}

/// A queued row as it was when Convert was clicked.
struct Row {
    idx: usize,
    category: Category,
    target: &'static str,
}

/// Everything a batch needs, moved to its own thread so planning (path resolution,
/// directory creation, and output-name probing) never blocks the window.
struct Batch {
    /// Every queued source, including completed rows, by row index.
    inputs: Vec<PathBuf>,
    rows: Vec<Row>,
    out_dir: Option<PathBuf>,
    opts: Options,
    cancel: Arc<AtomicBool>,
    ctx: egui::Context,
}

impl Batch {
    fn run(self, tx: Sender<Msg>) {
        let jobs = self.plan(&tx);
        let workers = thread::available_parallelism()
            .map_or(2, |n| n.get())
            .min(4)
            .min(jobs.len());
        let queue = Mutex::new(jobs);
        thread::scope(|scope| {
            for _ in 0..workers {
                let tx = tx.clone();
                scope.spawn(|| self.work(&queue, tx));
            }
        });
    }

    /// Rows that cannot be planned finish at once with their error.
    fn plan(&self, tx: &Sender<Msg>) -> Vec<Job> {
        // Every queued source is protected, including completed rows. A source that
        // cannot be inspected fails only its own row (and is ignored if already done).
        let (mut planner, errors) =
            output::OutputPlanner::new(self.inputs.iter().map(PathBuf::as_path));
        let mut errors: HashMap<usize, anyhow::Error> = errors.into_iter().collect();
        let mut jobs = Vec::new();
        for row in &self.rows {
            if self.cancel.load(Ordering::Relaxed) {
                break;
            }
            let input = &self.inputs[row.idx];
            let planned = if let Some(error) = errors.remove(&row.idx) {
                Err(format!("{error:#}"))
            } else if row.category.needs_ffmpeg() && converter::ffmpeg_path().is_none() {
                Err("ffmpeg not found on PATH".into())
            } else {
                planner
                    .plan(
                        input,
                        self.out_dir.as_deref(),
                        row.target,
                        self.opts.overwrite,
                    )
                    .map_err(|error| format!("{error:#}"))
            };
            match planned {
                Ok(output) => jobs.push(Job {
                    idx: row.idx,
                    input: input.clone(),
                    output,
                    category: row.category,
                    target: row.target,
                }),
                Err(error) => {
                    let _ = tx.send(Msg::Finished(row.idx, Err(error)));
                    self.ctx.request_repaint();
                }
            }
        }
        jobs.reverse(); // workers pop from the end; keep list order
        jobs
    }

    fn work(&self, queue: &Mutex<Vec<Job>>, tx: Sender<Msg>) {
        while !self.cancel.load(Ordering::Relaxed) {
            let job = queue.lock().unwrap_or_else(PoisonError::into_inner).pop();
            let Some(job) = job else {
                break;
            };
            let _ = tx.send(Msg::Started(job.idx));
            self.ctx.request_repaint();
            let result = catch_panic(|| {
                converter::convert(
                    &job.input,
                    job.output,
                    job.category,
                    job.target,
                    &self.opts,
                    &self.cancel,
                )
            })
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Finished(job.idx, result));
            self.ctx.request_repaint();
        }
    }
}

/// A folder/file import running on a background thread.
struct Import {
    rx: Receiver<import::Event>,
    cancel: Arc<AtomicBool>,
    // Paths already queued, so duplicates are dropped in O(1).
    queued: HashSet<PathBuf>,
    found: usize,
}

#[derive(Default)]
struct ImportReport {
    unsupported: usize,
    links: usize,
    too_deep: usize,
    errors: Vec<String>,
    error_count: usize,
    stopped: Option<String>,
}

impl ImportReport {
    fn is_empty(&self) -> bool {
        self.unsupported == 0
            && self.links == 0
            && self.too_deep == 0
            && self.error_count == 0
            && self.stopped.is_none()
    }
}

struct App {
    logo: egui::TextureHandle,
    files: Vec<FileItem>,
    out_dir: Option<PathBuf>,
    opts: Options,
    rx: Option<Receiver<Msg>>,
    cancel: Arc<AtomicBool>,
    /// Plans the batch, then runs and joins its workers.
    batch: Option<JoinHandle<()>>,
    total: usize,
    finished: usize,
    import: Option<Import>,
    report: ImportReport,
}

impl App {
    fn new(ctx: &egui::Context, logo: egui::ColorImage) -> Self {
        // Probing FFmpeg runs a process; keep it off the UI thread.
        let repaint = ctx.clone();
        thread::spawn(move || {
            converter::ffmpeg_path();
            repaint.request_repaint();
        });
        theme::apply(ctx);
        Self {
            logo: ctx.load_texture("convertcraft-logo", logo, egui::TextureOptions::LINEAR),
            files: Vec::new(),
            out_dir: None,
            opts: Options::default(),
            rx: None,
            cancel: Arc::new(AtomicBool::new(false)),
            batch: None,
            total: 0,
            finished: 0,
            import: None,
            report: ImportReport::default(),
        }
    }

    fn running(&self) -> bool {
        self.rx.is_some()
    }

    /// Converting or importing; the queue must not change.
    fn busy(&self) -> bool {
        self.running() || self.import.is_some()
    }

    /// The footer shows the last batch's progress until the queue changes, then the
    /// queue size again.
    fn forget_last_batch(&mut self) {
        self.total = 0;
        self.finished = 0;
    }

    fn add_paths(&mut self, ctx: &egui::Context, roots: Vec<PathBuf>) {
        if roots.is_empty() || self.busy() {
            return;
        }
        self.forget_last_batch();
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let mut sent = 0usize;
            import::scan(roots, &import::Limits::default(), &flag, |event| {
                let _ = tx.send(event);
                sent += 1;
                if sent.is_multiple_of(256) {
                    ctx.request_repaint();
                }
            });
            // Disconnect before the final repaint so the UI sees the scan end.
            drop(tx);
            ctx.request_repaint();
        });
        self.import = Some(Import {
            rx,
            cancel,
            queued: self.files.iter().map(|f| f.path.clone()).collect(),
            found: 0,
        });
    }

    fn poll_import(&mut self) {
        let Some(import) = &mut self.import else {
            return;
        };
        loop {
            match import.rx.try_recv() {
                Ok(import::Event::File(path, category)) => {
                    if import.queued.insert(path.clone()) {
                        import.found += 1;
                        self.files.push(FileItem {
                            target: converter::default_target(&path, category),
                            path,
                            category,
                            status: Status::Pending,
                        });
                    }
                }
                Ok(import::Event::Skipped(import::Skip::Unsupported)) => {
                    self.report.unsupported += 1;
                }
                Ok(import::Event::Skipped(import::Skip::Link)) => self.report.links += 1,
                Ok(import::Event::Skipped(import::Skip::TooDeep)) => {
                    self.report.too_deep += 1;
                }
                Ok(import::Event::Error(path, error)) => {
                    self.report.error_count += 1;
                    if self.report.errors.len() < MAX_REPORTED_ERRORS {
                        self.report
                            .errors
                            .push(format!("{}: {error}", path.display()));
                    }
                }
                Ok(import::Event::Stopped(message)) => self.report.stopped = Some(message),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.import = None;
                    break;
                }
            }
        }
    }

    fn import_status(&self, ui: &mut egui::Ui) {
        if let Some(import) = &self.import {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!("Scanning… {} files found", import.found));
                if ui.button("Cancel").clicked() {
                    import.cancel.store(true, Ordering::Relaxed);
                }
            });
        }
        let report = &self.report;
        if report.is_empty() {
            return;
        }
        ui.horizontal_wrapped(|ui| {
            if report.unsupported > 0 {
                ui.label(
                    RichText::new(format!("{} unsupported files skipped", report.unsupported))
                        .weak(),
                );
            }
            if report.links > 0 {
                ui.label(RichText::new(format!("{} links not followed", report.links)).weak())
                    .on_hover_text("Symbolic links and junctions inside folders are skipped");
            }
            if report.too_deep > 0 {
                ui.label(
                    RichText::new(format!("{} folders nested too deeply", report.too_deep)).weak(),
                );
            }
            if report.error_count > 0 {
                let mut details = report.errors.join("\n");
                let hidden = report.error_count - report.errors.len();
                if hidden > 0 {
                    details.push_str(&format!("\n… and {hidden} more"));
                }
                ui.label(
                    RichText::new(format!("{} import errors", report.error_count))
                        .color(theme::RED),
                )
                .on_hover_text(details);
            }
            if let Some(stopped) = &report.stopped {
                ui.label(RichText::new(stopped).color(theme::AMBER));
            }
        });
    }

    fn start(&mut self, ctx: &egui::Context) {
        let mut rows = Vec::new();
        for (idx, f) in self.files.iter_mut().enumerate() {
            if matches!(f.status, Status::Done(_)) {
                continue;
            }
            f.status = Status::Pending;
            rows.push(Row {
                idx,
                category: f.category,
                target: f.target,
            });
        }
        if rows.is_empty() {
            return;
        }

        self.total = rows.len();
        self.finished = 0;
        self.cancel = Arc::new(AtomicBool::new(false));
        let batch = Batch {
            inputs: self.files.iter().map(|f| f.path.clone()).collect(),
            rows,
            out_dir: self.out_dir.clone(),
            opts: self.opts.clone(),
            cancel: Arc::clone(&self.cancel),
            ctx: ctx.clone(),
        };
        // The queue stays locked until every sender is gone, so row indexes stay valid.
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        self.batch = Some(thread::spawn(move || {
            // A panic outside a conversion still ends the batch; `poll` then fails
            // the unfinished rows.
            let _ = panic::catch_unwind(AssertUnwindSafe(|| batch.run(tx)));
            ctx.request_repaint();
        }));
        self.rx = Some(rx);
    }

    /// Stop taking jobs and stop running FFmpeg processes.
    fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(import) = &self.import {
            import.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn poll(&mut self) {
        let Some(rx) = &self.rx else { return };
        loop {
            match rx.try_recv() {
                Ok(Msg::Started(i)) => self.files[i].status = Status::Running,
                Ok(Msg::Finished(i, result)) => {
                    self.finished += 1;
                    self.files[i].status = match result {
                        Ok(path) => Status::Done(path),
                        Err(e) => Status::Failed(e),
                    };
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // All workers are gone. Rows they never finished must not stay
                    // "Pending" or "Converting" forever.
                    let reason = if self.cancel.load(Ordering::Relaxed) {
                        "cancelled"
                    } else {
                        "conversion did not finish"
                    };
                    for file in &mut self.files {
                        if matches!(file.status, Status::Pending | Status::Running) {
                            file.status = Status::Failed(reason.into());
                        }
                    }
                    self.rx = None;
                    self.batch = None;
                    break;
                }
            }
        }
    }

    fn header(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.image((self.logo.id(), egui::vec2(44.0, 44.0)));
            ui.add_space(4.0);
            ui.label(
                RichText::new(APP_NAME)
                    .size(28.0)
                    .strong()
                    .color(theme::TEXT),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                match converter::ffmpeg_status() {
                    None => {
                        theme::pill(ui, "checking ffmpeg…", theme::MUTED);
                    }
                    Some(Some(path)) => {
                        theme::pill(ui, "ffmpeg ✔", theme::GREEN).on_hover_text(format!(
                            "Audio and video conversion available\n{}",
                            display_path(path).display()
                        ));
                    }
                    Some(None) => {
                        theme::pill(ui, "ffmpeg missing", theme::AMBER).on_hover_text(
                            "Install ffmpeg and add its folder to PATH to convert audio and video",
                        );
                    }
                }
            });
        });
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let height = egui::vec2(0.0, 34.0);
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(!self.busy(), |ui| {
                if ui
                    .add(theme::primary("➕  Add files…").min_size(height))
                    .clicked()
                    && let Some(paths) = rfd::FileDialog::new().pick_files()
                {
                    self.add_paths(ui.ctx(), paths);
                }
                if ui
                    .add(egui::Button::new("📁  Add folder…").min_size(height))
                    .clicked()
                    && let Some(dir) = rfd::FileDialog::new().pick_folder()
                {
                    self.add_paths(ui.ctx(), vec![dir]);
                }
                ui.separator();
                ui.label(RichText::new("Convert all").color(theme::MUTED));
                for cat in Category::ALL {
                    let present = self.files.iter().any(|f| f.category == cat);
                    let color = theme::category_color(cat);
                    let chip = theme::tinted(
                        format!("{}  {}", theme::category_icon(cat), cat.label()),
                        color,
                    )
                    .min_size(height);
                    ui.add_enabled_ui(present, |ui| {
                        egui::containers::menu::MenuButton::from_button(chip)
                            .ui(ui, |ui| {
                                for &t in cat.targets() {
                                    if ui.button(format!("All to {}", t.to_uppercase())).clicked() {
                                        self.files
                                            .iter_mut()
                                            .filter(|f| f.category == cat)
                                            .for_each(|f| f.target = t);
                                    }
                                }
                            })
                            .0
                            .on_hover_text(format!("Set the target of every {} file", cat.label()));
                    });
                }
            });
        });
        self.import_status(ui);
    }

    fn options_panel(&mut self, ui: &mut egui::Ui) {
        theme::card().show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.add_enabled_ui(!self.running(), |ui| {
                ui.label(
                    RichText::new("Output options")
                        .size(18.0)
                        .strong()
                        .color(theme::TEXT),
                );
                ui.add_space(4.0);
                if ui
                    .radio(self.out_dir.is_none(), "Next to source file")
                    .clicked()
                {
                    self.out_dir = None;
                }
                if ui.radio(self.out_dir.is_some(), "Choose folder…").clicked()
                    && let Some(dir) = rfd::FileDialog::new().pick_folder()
                {
                    self.out_dir = Some(dir);
                }
                if let Some(dir) = &self.out_dir {
                    ui.add(
                        egui::Label::new(
                            RichText::new(dir.display().to_string())
                                .small()
                                .color(theme::MUTED),
                        )
                        .truncate(),
                    )
                    .on_hover_text(dir.display().to_string());
                }
                ui.separator();

                ui.horizontal(|ui| {
                    ui.label("JPEG/AVIF quality");
                    ui.label(RichText::new(self.opts.quality.to_string()).strong());
                });
                ui.spacing_mut().slider_width = ui.available_width();
                ui.add(egui::Slider::new(&mut self.opts.quality, 10..=100).show_value(false));
                ui.separator();

                ui.checkbox(&mut self.opts.resize, "Shrink images to fit")
                    .on_hover_text(
                        "Scale images down to fit within the given width and height, \
                         keeping the aspect ratio. Smaller images are not enlarged.",
                    );
                ui.add_enabled_ui(self.opts.resize, |ui| {
                    ui.horizontal(|ui| {
                        ui.add_space(24.0);
                        ui.add(
                            egui::DragValue::new(&mut self.opts.max_width)
                                .range(1..=65_535)
                                .suffix(" px"),
                        );
                        ui.label("×");
                        ui.add(
                            egui::DragValue::new(&mut self.opts.max_height)
                                .range(1..=65_535)
                                .suffix(" px"),
                        );
                    });
                });
                ui.separator();

                ui.checkbox(&mut self.opts.overwrite, "Overwrite existing files");
                ui.checkbox(
                    &mut self.opts.csv_formula_guard,
                    "Safe CSV for spreadsheets",
                )
                .on_hover_text(
                    "Prefix CSV cells starting with = + - @ tab or CR with an apostrophe \
                     so spreadsheets show them as text instead of running them as formulas",
                );
            });
        });
    }

    fn bottom_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if self.total > 0 {
                ui.label(format!("{} of {} files", self.finished, self.total));
                let fraction = self.finished as f32 / self.total as f32;
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .desired_width(260.0)
                        .desired_height(8.0)
                        .fill(theme::BLUE)
                        .corner_radius(egui::CornerRadius::same(4))
                        .animate(self.running()),
                );
            } else {
                ui.label(
                    RichText::new(format!("{} files queued", self.files.len())).color(theme::MUTED),
                );
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let pending = self
                    .files
                    .iter()
                    .filter(|f| !matches!(f.status, Status::Done(_)))
                    .count();
                // Wait for the FFmpeg check so media rows are not failed prematurely.
                let ready = converter::ffmpeg_status().is_some();
                let convert = egui::Button::new(
                    RichText::new("Convert")
                        .size(16.0)
                        .strong()
                        .color(Color32::WHITE),
                )
                .fill(theme::ORANGE)
                .stroke(egui::Stroke::NONE)
                .min_size(egui::vec2(120.0, 38.0));
                if ui
                    .add_enabled(!self.busy() && ready && pending > 0, convert)
                    .clicked()
                {
                    self.start(ui.ctx());
                }
                if ui
                    .add_enabled(
                        self.running() && !self.cancel.load(Ordering::Relaxed),
                        egui::Button::new("Cancel").min_size(egui::vec2(0.0, 38.0)),
                    )
                    .on_hover_text("Stop after the jobs already running; FFmpeg jobs stop at once")
                    .clicked()
                {
                    self.cancel();
                }
                ui.add_enabled_ui(!self.busy(), |ui| {
                    if ui
                        .add(egui::Button::new("Clear finished").min_size(egui::vec2(0.0, 38.0)))
                        .clicked()
                    {
                        self.files.retain(|f| !matches!(f.status, Status::Done(_)));
                        self.forget_last_batch();
                    }
                    if ui
                        .add(egui::Button::new("Clear all").min_size(egui::vec2(0.0, 38.0)))
                        .clicked()
                    {
                        self.files.clear();
                        self.forget_last_batch();
                        self.report = ImportReport::default();
                    }
                });
            });
        });
    }

    fn file_list(&mut self, ui: &mut egui::Ui) {
        theme::card().show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            if self.files.is_empty() {
                self.empty_queue(ui);
            } else {
                self.file_table(ui);
            }
        });
    }

    fn empty_queue(&mut self, ui: &mut egui::Ui) {
        let response = ui.vertical_centered(|ui| {
            ui.add_space(((ui.available_height() - 200.0) / 2.0).max(0.0));
            ui.image((self.logo.id(), egui::vec2(96.0, 96.0)));
            ui.add_space(12.0);
            let title = ui
                .add(
                    egui::Label::new(
                        RichText::new("Drop files or folders here")
                            .size(22.0)
                            .strong()
                            .color(theme::TEXT),
                    )
                    .sense(egui::Sense::click()),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            let browse = ui
                .add(
                    egui::Label::new(RichText::new("or click to browse").color(theme::BLUE))
                        .sense(egui::Sense::click()),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            title.clicked() || browse.clicked()
        });
        if response.inner
            && !self.busy()
            && let Some(paths) = rfd::FileDialog::new().pick_files()
        {
            self.add_paths(ui.ctx(), paths);
        }
    }

    fn file_table(&mut self, ui: &mut egui::Ui) {
        let busy = self.busy();
        let mut remove = None;
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                // The other columns are fixed-size; the file name takes the rest of the card.
                let name_width = (ui.available_width() - 370.0).max(160.0);
                egui::Grid::new("files")
                    .num_columns(5)
                    .striped(true)
                    .spacing([20.0, 12.0])
                    .show(ui, |ui| {
                        for heading in ["File", "Type", "Convert to", "Status", ""] {
                            ui.label(RichText::new(heading).strong().color(theme::MUTED));
                        }
                        ui.end_row();

                        for (i, f) in self.files.iter_mut().enumerate() {
                            let color = theme::category_color(f.category);
                            let icon = theme::category_icon(f.category);
                            let name = f.path.file_name().map_or_else(
                                || f.path.display().to_string(),
                                |n| n.to_string_lossy().into_owned(),
                            );
                            ui.horizontal(|ui| {
                                ui.set_min_width(name_width);
                                ui.label(RichText::new(icon).size(18.0).color(color));
                                ui.label(RichText::new(name).color(theme::TEXT))
                                    .on_hover_text(f.path.display().to_string());
                            });
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(icon).color(color));
                                ui.label(RichText::new(f.category.label()).color(theme::MUTED));
                            });

                            ui.add_enabled_ui(!busy, |ui| {
                                egui::ComboBox::from_id_salt(("target", i))
                                    .selected_text(f.target.to_uppercase())
                                    .width(96.0)
                                    .show_ui(ui, |ui| {
                                        for &t in f.category.targets() {
                                            ui.selectable_value(&mut f.target, t, t.to_uppercase());
                                        }
                                    });
                            });

                            match &f.status {
                                Status::Pending => {
                                    ui.label(RichText::new("🕓  Pending").color(theme::MUTED));
                                }
                                Status::Running => {
                                    ui.horizontal(|ui| {
                                        ui.spinner();
                                        ui.label(RichText::new("Converting…").color(theme::BLUE));
                                    });
                                }
                                Status::Done(out) => {
                                    let done = ui
                                        .add(
                                            theme::tinted("✔  Done", theme::GREEN)
                                                .corner_radius(egui::CornerRadius::same(14)),
                                        )
                                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                                        .on_hover_text(format!(
                                            "{}\nClick to show in folder",
                                            display_path(out).display()
                                        ));
                                    if done.clicked() {
                                        reveal(out);
                                    }
                                }
                                Status::Failed(err) => {
                                    ui.label(RichText::new("⚠  Failed").color(theme::RED))
                                        .on_hover_text(err);
                                }
                            }

                            if ui
                                .add_enabled(
                                    !busy,
                                    egui::Button::new(RichText::new("✖").color(theme::MUTED))
                                        .frame(false),
                                )
                                .on_hover_text("Remove from the queue")
                                .clicked()
                            {
                                remove = Some(i);
                            }
                            ui.end_row();
                        }
                    });
            });
        if let Some(i) = remove {
            self.files.remove(i);
            self.forget_last_batch();
        }
    }

    fn handle_drops(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|path| !path.as_os_str().is_empty())
                .collect()
        });
        if !self.busy() {
            self.add_paths(ctx, dropped);
        }

        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::unique("drop"),
            ));
            let rect = ctx.viewport_rect();
            painter.rect_filled(rect, 0.0, Color32::from_black_alpha(160));
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Drop to add",
                egui::FontId::proportional(28.0),
                Color32::WHITE,
            );
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        self.poll_import();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.handle_drops(ui.ctx());

        let frame = |fill, left, right, top, bottom| {
            egui::Frame::new().fill(fill).inner_margin(egui::Margin {
                left,
                right,
                top,
                bottom,
            })
        };
        egui::Panel::top("header")
            .frame(frame(theme::CARD, 20, 20, 12, 12))
            .show(ui, |ui| self.header(ui));
        egui::Panel::top("toolbar")
            .frame(frame(theme::BG, 20, 20, 16, 4))
            .show_separator_line(false)
            .show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("bottom")
            .frame(frame(theme::CARD, 20, 20, 12, 12))
            .show(ui, |ui| self.bottom_bar(ui));
        egui::Panel::right("options")
            .frame(frame(theme::BG, 0, 20, 12, 16))
            .exact_size(300.0)
            .resizable(false)
            .show_separator_line(false)
            .show(ui, |ui| self.options_panel(ui));
        egui::CentralPanel::default()
            .frame(frame(theme::BG, 20, 16, 12, 16))
            .show(ui, |ui| self.file_list(ui));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Let workers stop FFmpeg and clean up their staging files before the
        // process exits; image/data jobs cannot be interrupted, so wait briefly.
        self.cancel();
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while self.batch.as_ref().is_some_and(|b| !b.is_finished()) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Turn a panic inside a conversion into an ordinary per-file error.
fn catch_panic<T>(f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    panic::catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|payload| {
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".into());
        Err(anyhow::anyhow!("internal error: {message}"))
    })
}

/// Drop the `\\?\` prefix that `canonicalize` adds on Windows when the plain form
/// names the same file, for display and for Explorer (which does not accept it).
fn display_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    if let Some(text) = path.to_str() {
        // Plain paths of MAX_PATH length or more need the prefix.
        if text.len() < 260 {
            if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
                return PathBuf::from(format!(r"\\{rest}"));
            }
            if let Some(rest) = text.strip_prefix(r"\\?\")
                && rest.as_bytes().get(1) == Some(&b':')
            {
                return PathBuf::from(rest);
            }
        }
    }
    path.to_path_buf()
}

/// Open the system file manager with the converted file selected.
fn reveal(path: &Path) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Absolute path: Windows would otherwise look next to this executable first.
        let windows = std::env::var_os("SystemRoot")
            .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        // Explorer splits arguments on commas; quote the path (it cannot contain `"`).
        let _ = std::process::Command::new(windows.join("explorer.exe"))
            .raw_arg(format!("/select,\"{}\"", display_path(path).display()))
            .spawn();
    }
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("/usr/bin/open")
        .arg("-R")
        .arg(path)
        .spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    if let Some(dir) = path.parent() {
        let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panics_become_errors() {
        let error =
            catch_panic(|| -> anyhow::Result<()> { panic!("decoder exploded") }).unwrap_err();
        assert_eq!(error.to_string(), "internal error: decoder exploded");
        assert_eq!(catch_panic(|| Ok(7)).unwrap(), 7);
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_prefixes_are_hidden_when_safe() {
        assert_eq!(
            display_path(Path::new(r"\\?\C:\Users\a.png")),
            Path::new(r"C:\Users\a.png")
        );
        assert_eq!(
            display_path(Path::new(r"\\?\UNC\server\share\a.png")),
            Path::new(r"\\server\share\a.png")
        );
        let long = format!(r"\\?\C:\{}", "a".repeat(300));
        assert_eq!(display_path(Path::new(&long)), Path::new(&long));
        assert_eq!(display_path(Path::new(r"C:\plain")), Path::new(r"C:\plain"));
    }
}
