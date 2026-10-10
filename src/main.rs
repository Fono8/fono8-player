//! Fono8 - a compact music player for local files.

// Release builds on Windows start without a console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod artwork;
mod audio;
mod cast;
mod cdp;
#[cfg(target_os = "linux")]
mod desktop;
mod discover;
mod i18n;
mod keystore;
mod library;
mod lock;
mod loopback;
mod meter;
mod net;
mod oauth;
mod playback;
mod services;
mod sleep_timer;
mod spotify;
#[cfg(target_os = "linux")]
mod stream_tap;
mod tidal;
mod transfer;
mod tray;
mod ui;
mod youtube;

use std::path::PathBuf;

use gpui::{App, AppContext, Application, Entity, Global};

use crate::app::Fono8;
use crate::i18n::{Translator, LANGUAGES};
use crate::library::Library;
use crate::lock::{default_data_directory, InstanceLock};

/// Keeps the model alive for the whole application lifetime.
struct Model(#[allow(dead_code)] Entity<Fono8>);

impl Global for Model {}

struct Args {
    folder: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    language: Option<String>,
}

fn usage(translator: &Translator) -> String {
    let t = |key: &str| translator.t(key);
    format!(
        "{}\n\nusage: fono8 [-h] [--version] [--data-dir DIR] [--language {{auto,pl,en}}] [folder]\n\n  folder            {}\n  --data-dir DIR    {}\n  --language CODE   {}\n  --version         {}\n  -h, --help        {}\n",
        t("app_description"),
        t("folder_help"),
        t("data_dir_help"),
        t("language_help"),
        t("version_help"),
        t("help"),
    )
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args { folder: None, data_dir: None, language: None };
    let mut iter = std::env::args().skip(1);
    let mut show_help = false;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => show_help = true,
            "--version" => {
                println!("Fono8 {}", crate::app::version_label());
                std::process::exit(0);
            }
            "--data-dir" => args.data_dir = iter.next().map(PathBuf::from),
            "--language" => args.language = iter.next(),
            other if other.starts_with("--data-dir=") => args.data_dir = Some(PathBuf::from(&other["--data-dir=".len()..])),
            other if other.starts_with("--language=") => args.language = Some(other["--language=".len()..].to_string()),
            other if other.starts_with('-') => return Err(format!("unknown option: {other}")),
            other => args.folder = Some(PathBuf::from(other)),
        }
    }
    if let Some(language) = &args.language {
        if language != "auto" && !LANGUAGES.iter().any(|(code, _)| code == language) {
            return Err(format!("unknown language: {language}"));
        }
    }
    if show_help {
        let translator = Translator::new(args.language.as_deref().unwrap_or("auto"));
        print!("{}", usage(&translator));
        std::process::exit(0);
    }
    Ok(args)
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("fono8: {error}");
            eprint!("{}", usage(&Translator::new("auto")));
            std::process::exit(2);
        }
    };
    let directory = args.data_dir.clone().unwrap_or_else(default_data_directory);
    if let Err(error) = std::fs::create_dir_all(&directory) {
        eprintln!("fono8: cannot create data directory {}: {error}", directory.display());
        std::process::exit(1);
    }
    let Some(lock) = InstanceLock::try_acquire(&directory) else {
        let translator = Translator::new(args.language.as_deref().unwrap_or("auto"));
        eprintln!("{}\n{}", translator.t("already_running"), translator.t("already_running_body"));
        std::process::exit(1);
    };
    // Application menu entry and icons for this user (Linux), refreshed on every start.
    #[cfg(target_os = "linux")]
    std::thread::spawn(desktop::integrate);
    let mut library = match Library::open(&directory.join("library.sqlite3")) {
        Ok(library) => library,
        Err(error) => {
            eprintln!("fono8: cannot open library: {error}");
            std::process::exit(1);
        }
    };
    if let Some(language) = &args.language {
        library.set_setting("language", serde_json::json!(language));
    }
    let folder = args.folder.map(|f| {
        let expanded = if let Ok(stripped) = f.strip_prefix("~") {
            dirs::home_dir().map(|h| h.join(stripped)).unwrap_or(f.clone())
        } else {
            f
        };
        std::fs::canonicalize(&expanded).unwrap_or(expanded)
    });
    let language = args.language.clone();

    Application::new().with_assets(ui::Assets).run(move |cx: &mut App| {
        ui::text_input::bind_keys(cx);
        ui::main_view::bind_keys(cx);
        let model = cx.new(|cx| Fono8::new(library, language, cx));
        cx.set_global(Model(model.clone()));
        ui::open_main_window(model.clone(), cx);
        if let Some(folder) = folder {
            model.update(cx, |m, cx| {
                m.scan(&folder.to_string_lossy());
                cx.notify();
            });
        }
        cx.activate(true);
    });
    drop(lock);
}
