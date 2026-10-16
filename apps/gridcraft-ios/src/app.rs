//! The iPadOS app shell: start-up, preferences, files and the eframe loop.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use gridcraft_engine::Session;
use gridcraft_ui_egui::{Services, SheetApp};
use serde_json::json;

use crate::{control_server, files};

/// The sandbox folders the app uses; `None` when `HOME` is unset (never inside an iOS app).
struct Folders {
    documents: PathBuf,
    config: PathBuf,
}

impl Folders {
    fn get() -> Option<Folders> {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        Some(Folders { documents: files::documents_dir(&home), config: files::config_dir(&home) })
    }
}

struct App {
    sheet: SheetApp,
    folders: Option<Folders>,
    /// Whether the app was in front last frame: coming back rescans `Documents` (files may have
    /// been added in the Files app meanwhile); going to the background saves the preferences,
    /// because iPadOS ends background apps without warning.
    active: bool,
}

impl App {
    fn scan_documents(&mut self) {
        if let Some(f) = &self.folders {
            let found = files::workbooks_in(&f.documents);
            self.sheet.ui.recent = files::merge_recent(&self.sheet.ui.recent, &found);
        }
    }

    fn save_prefs(&self) {
        let Some(f) = &self.folders else { return };
        let _ = std::fs::create_dir_all(&f.config);
        if let Ok(b) = serde_json::to_vec_pretty(&self.sheet.ui) {
            let _ = std::fs::write(f.config.join("ui.json"), b);
        }
        if let Ok(b) = serde_json::to_vec_pretty(&self.sheet.session.prefs) {
            let _ = std::fs::write(f.config.join("prefs.json"), b);
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let active = ctx.input(|i| i.focused);
        if active != self.active {
            self.active = active;
            if active {
                self.scan_documents();
            } else {
                self.save_prefs();
            }
        }
        self.sheet.logic(ctx);
    }
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.sheet.raw_input_hook(raw);
    }
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.sheet.ui(ui);
    }
    fn on_exit(&mut self) {
        self.save_prefs();
    }
}

fn load_prefs(app: &mut SheetApp, config: &Path) {
    // First run (or a ui.json without a usable language): follow the iPad's language and
    // appearance, as the desktop app does.
    app.ui.language = gridcraft_ui_egui::i18n::Language::system();
    app.ui.system_theme = true;
    if let Ok(b) = std::fs::read(config.join("ui.json"))
        && let Ok(ui) = serde_json::from_slice::<gridcraft_ui_egui::UiState>(&b)
    {
        let saved = serde_json::from_slice(&b).ok().and_then(|v| gridcraft_ui_egui::i18n::saved_language(&v));
        let language = saved.unwrap_or(app.ui.language);
        app.ui = ui;
        app.ui.language = language;
    }
    if let Ok(b) = std::fs::read(config.join("prefs.json"))
        && let Ok(p) = serde_json::from_slice(&b)
    {
        app.session.prefs = p;
    }
}

fn services(documents: Option<PathBuf>) -> Services {
    Services {
        // Save As and exports write into `Documents`, under a name that never replaces a file.
        pick_save: documents.map(|dir| -> Box<dyn Fn(&str) -> Option<String>> {
            Box::new(move |suggested: &str| {
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    log::warn!("{}: {e}", dir.display());
                    return None;
                }
                files::save_path(&dir, suggested).to_str().map(str::to_string)
            })
        }),
        ..Default::default()
    }
}

/// Sends `log` records to standard error, which Xcode's console and Console.app show.
struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Warn || (m.level() <= log::Level::Info && m.target().starts_with("gridcraft"))
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            let _ = writeln!(std::io::stderr(), "gridcraft [{} {}] {}", r.level(), r.target(), r.args());
        }
    }
    fn flush(&self) {}
}

static LOGGER: StderrLogger = StderrLogger;

pub fn run() -> eframe::Result<()> {
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
    std::panic::set_hook(Box::new(|info| {
        let trace = std::backtrace::Backtrace::capture();
        log::error!("internal error: {info}\n{trace}");
    }));
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut control_port: Option<u16> = std::env::var("GRIDCRAFT_CONTROL_PORT").ok().and_then(|p| p.parse().ok());
    let mut sample: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args.get(i).map(String::as_str) {
            Some("--control") => {
                i += 1;
                control_port = args.get(i).and_then(|p| p.parse().ok());
            }
            Some("--sample") => {
                i += 1;
                sample = Some(args.get(i).cloned().unwrap_or_else(|| "sales".into()));
            }
            _ => {}
        }
        i += 1;
    }
    let folders = Folders::get();
    log::info!("GridCraft {} for iPadOS, documents in {:?}", env!("CARGO_PKG_VERSION"), folders.as_ref().map(|f| &f.documents));
    let mut session = Session::new();
    if let Some(s) = &sample
        && let Err(e) = session.execute("file.new", json!({"sample": s}))
    {
        log::warn!("sample {s}: {e}");
    }
    if session.documents().is_empty() {
        session.new_workbook();
    }
    let mut sheet = SheetApp::new(session, services(folders.as_ref().map(|f| f.documents.clone())));
    sheet.after_engine();
    if let Some(f) = &folders {
        load_prefs(&mut sheet, &f.config);
    }
    let mut app = App { sheet, folders, active: true };
    app.scan_documents();
    let viewport = egui::ViewportBuilder::default().with_title("GridCraft");
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "GridCraft",
        options,
        Box::new(move |cc| {
            let mut app = app;
            SheetApp::setup_context_for_language(&cc.egui_ctx, app.sheet.ui.dark, app.sheet.ui.language);
            if let Some(port) = control_port {
                match control_server::start(port, cc.egui_ctx.clone()) {
                    Ok(rx) => app.sheet.control_rx = Some(rx),
                    Err(e) => log::error!("control channel on port {port}: {e}"),
                }
            }
            Ok(Box::new(app))
        }),
    )
}
