//! Инициализация pdfium.
//!
//! `Pdfium::new` внутри себя делает `assert!(BINDINGS.get().is_none())` —
//! библиотека допускает ровно один экземпляр на процесс. Поэтому он живёт в
//! глобальном `OnceLock`, а не создаётся по месту: только так `PdfDocument`
//! получает время жизни `'static` и может храниться в структуре потока
//! рендера, не превращаясь в самоссылающийся тип.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, anyhow};
use pdfium_render::prelude::*;

static PDFIUM: OnceLock<Pdfium> = OnceLock::new();
static INIT: Mutex<()> = Mutex::new(());

/// Возвращает глобальный экземпляр pdfium, инициализируя его при первом вызове.
pub fn pdfium() -> Result<&'static Pdfium> {
    if let Some(existing) = PDFIUM.get() {
        return Ok(existing);
    }

    // Без этого замка два потока могли бы одновременно пройти проверку выше и
    // второй словил бы assert внутри Pdfium::new.
    let _guard = INIT
        .lock()
        .map_err(|_| anyhow!("замок инициализации pdfium отравлен"))?;
    if let Some(existing) = PDFIUM.get() {
        return Ok(existing);
    }

    let bindings = bind_library()?;
    let _ = PDFIUM.set(Pdfium::new(bindings));
    PDFIUM.get().context("pdfium не сохранился в OnceLock")
}

fn bind_library() -> Result<Box<dyn PdfiumLibraryBindings>> {
    let mut tried = Vec::new();

    for dir in candidate_dirs() {
        let path = Pdfium::pdfium_platform_library_name_at_path(&dir);
        match Pdfium::bind_to_library(&path) {
            Ok(bindings) => {
                tracing::info!(path = %path.display(), "pdfium загружен");
                return Ok(bindings);
            }
            Err(e) => tried.push(format!("  {} — {e}", path.display())),
        }
    }

    // Последняя попытка — библиотека, установленная в системе.
    match Pdfium::bind_to_system_library() {
        Ok(bindings) => {
            tracing::info!("pdfium загружен из системного пути");
            Ok(bindings)
        }
        Err(e) => {
            tried.push(format!("  системный путь — {e}"));
            Err(anyhow!(
                "не удалось загрузить pdfium. Проверены пути:\n{}",
                tried.join("\n")
            ))
        }
    }
}

/// Каталоги, где ищется `pdfium.dll`, в порядке убывания приоритета:
/// рядом с исполняемым файлом (так выглядит установленное приложение),
/// затем `vendor/pdfium/bin` относительно корня репозитория (так выглядит
/// запуск через `cargo run`).
fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if let Ok(exe) = std::env::current_exe()
        && let Some(exe_dir) = exe.parent()
    {
        dirs.push(exe_dir.to_path_buf());
        // Корень репозитория лежит на два уровня выше для `target/debug/app.exe`
        // и на три — для тестов в `target/debug/deps/`.
        for depth in [2, 3] {
            if let Some(root) = exe_dir.ancestors().nth(depth) {
                dirs.push(root.join("vendor/pdfium/bin"));
            }
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd.join("vendor/pdfium/bin"));
    }

    dirs
}
