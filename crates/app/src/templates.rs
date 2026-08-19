//! Шаблоны оформления — именованные наборы «гарнитура + кегль + начертание».
//!
//! Смысл в книге с неровным переводом: один раз настроил вид основного текста,
//! подписи к рисунку и заголовка — дальше правка любого абзаца сводится к
//! одному клику вместо повторной установки четырёх параметров.
//!
//! Хранятся рядом с настройками приложения и живут между запусками.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::recents::data_dir;

/// Сколько шаблонов помещается в панель, не превращая её в кашу.
pub const MAX_TEMPLATES: usize = 8;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FormatTemplate {
    pub name: String,
    pub family: String,
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

impl FormatTemplate {
    /// Краткое описание для подсказки на кнопке.
    pub fn summary(&self) -> String {
        let mut parts = vec![format!("{} {:.0} pt", self.family, self.size)];
        if self.bold {
            parts.push("полужирный".to_owned());
        }
        if self.italic {
            parts.push("курсив".to_owned());
        }
        if self.underline {
            parts.push("подчёркнутый".to_owned());
        }
        parts.join(", ")
    }
}

#[derive(Default)]
pub struct Templates {
    items: Vec<FormatTemplate>,
    /// `None` — список только в памяти. Так работают тесты: прогон не должен
    /// трогать настройки пользователя.
    storage: Option<PathBuf>,
}

impl Templates {
    pub fn load() -> Templates {
        let storage = data_dir().map(|dir| dir.join("templates.json"));
        let items = storage
            .as_ref()
            .and_then(|file| std::fs::read_to_string(file).ok())
            .and_then(
                |raw| match serde_json::from_str::<Vec<FormatTemplate>>(&raw) {
                    Ok(items) => Some(items),
                    Err(e) => {
                        tracing::warn!("шаблоны повреждены, начинаю заново: {e}");
                        None
                    }
                },
            )
            .unwrap_or_default();

        Templates { items, storage }
    }

    pub fn items(&self) -> &[FormatTemplate] {
        &self.items
    }

    /// Добавляет шаблон; одноимённый заменяется.
    pub fn save(&mut self, template: FormatTemplate) {
        match self.items.iter_mut().find(|t| t.name == template.name) {
            Some(existing) => *existing = template,
            None => {
                self.items.push(template);
                self.items.truncate(MAX_TEMPLATES);
            }
        }
        self.persist();
    }

    pub fn remove(&mut self, name: &str) {
        self.items.retain(|t| t.name != name);
        self.persist();
    }

    /// Имя для нового шаблона, не совпадающее с существующими.
    pub fn suggest_name(&self) -> String {
        for n in 1..=MAX_TEMPLATES + 1 {
            let name = format!("Стиль {n}");
            if !self.items.iter().any(|t| t.name == name) {
                return name;
            }
        }
        "Стиль".to_owned()
    }

    pub fn is_full(&self) -> bool {
        self.items.len() >= MAX_TEMPLATES
    }

    fn persist(&self) {
        let Some(file) = self.storage.as_ref() else {
            return;
        };
        if let Some(dir) = file.parent()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            tracing::warn!("не удалось создать каталог данных: {e}");
            return;
        }
        match serde_json::to_string_pretty(&self.items) {
            Ok(json) => {
                if let Err(e) = std::fs::write(file, json) {
                    tracing::warn!("не удалось сохранить шаблоны: {e}");
                }
            }
            Err(e) => tracing::warn!("не удалось сериализовать шаблоны: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template(name: &str) -> FormatTemplate {
        FormatTemplate {
            name: name.to_owned(),
            family: "Arial".to_owned(),
            size: 11.0,
            bold: false,
            italic: false,
            underline: false,
        }
    }

    #[test]
    fn detached_list_never_touches_disk() {
        let mut templates = Templates::default();
        assert!(templates.storage.is_none());
        templates.save(template("Стиль 1"));
        assert!(
            templates.storage.is_none(),
            "список без файла обязан остаться в памяти"
        );
    }

    #[test]
    fn saving_same_name_replaces_instead_of_duplicating() {
        let mut templates = Templates::default();
        templates.save(template("Основной"));
        templates.save(FormatTemplate {
            size: 14.0,
            ..template("Основной")
        });

        assert_eq!(templates.items().len(), 1);
        assert_eq!(templates.items()[0].size, 14.0);
    }

    #[test]
    fn list_is_capped() {
        let mut templates = Templates::default();
        for i in 0..(MAX_TEMPLATES + 5) {
            templates.save(template(&format!("Стиль {i}")));
        }
        assert_eq!(templates.items().len(), MAX_TEMPLATES);
        assert!(templates.is_full());
    }

    #[test]
    fn suggested_name_skips_taken_ones() {
        let mut templates = Templates::default();
        assert_eq!(templates.suggest_name(), "Стиль 1");
        templates.save(template("Стиль 1"));
        assert_eq!(templates.suggest_name(), "Стиль 2");
    }

    #[test]
    fn summary_mentions_every_enabled_trait() {
        let t = FormatTemplate {
            bold: true,
            italic: true,
            underline: true,
            ..template("Всё сразу")
        };
        let summary = t.summary();
        assert!(summary.contains("Arial"));
        assert!(summary.contains("полужирный"));
        assert!(summary.contains("курсив"));
        assert!(summary.contains("подчёркнутый"));
    }
}
