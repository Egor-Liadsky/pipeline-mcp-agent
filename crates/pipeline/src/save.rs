//! Шаг 3 пайплайна: запись результата в каталог `--output`.
//!
//! Модель задаёт только имя файла, каталог — ключ запуска сервера: так
//! пишущий инструмент не может выйти за пределы выделенного каталога,
//! даже если модель или пользователь попросят.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveOutput {
    pub path: String,
    pub bytes: usize,
    /// SHA-256 записанного содержимого: вызывающий сверяет его со своим
    /// `content` и убеждается, что на диск попало ровно переданное.
    pub sha256: String,
}

pub fn sha256_hex(content: &[u8]) -> String {
    hex::encode(Sha256::digest(content))
}

/// Имя — один компонент пути: без разделителей, `..`, ведущей точки.
fn validate_name(name: &str) -> Result<(), String> {
    let invalid = name.is_empty()
        || name.starts_with('.')
        || name.contains(['/', '\\', '\0'])
        || Path::new(name).is_absolute();
    if invalid {
        return Err(format!(
            "недопустимое имя файла {name:?}: нужно одно имя без каталогов, например summary.md"
        ));
    }
    Ok(())
}

pub fn save(
    output_dir: &Path,
    name: &str,
    content: &str,
    overwrite: bool,
) -> Result<SaveOutput, String> {
    validate_name(name)?;
    let path = output_dir.join(name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if overwrite {
        options.create(true).truncate(true);
    } else {
        // `create_new` атомарно отказывает, если файл есть: проверка
        // существования и запись не разделены гонкой.
        options.create_new(true);
    }
    let mut file = options.open(&path).map_err(|err| match err.kind() {
        std::io::ErrorKind::AlreadyExists => {
            format!("файл {name} уже существует; передай overwrite=true, чтобы перезаписать")
        }
        _ => format!("не удалось открыть {}: {err}", path.display()),
    })?;
    file.write_all(content.as_bytes())
        .map_err(|err| format!("не удалось записать {}: {err}", path.display()))?;
    Ok(SaveOutput {
        path: path.display().to_string(),
        bytes: content.len(),
        sha256: sha256_hex(content.as_bytes()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("pipeline-save-{name}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_content_and_reports_hash() {
        let dir = temp_dir("ok");
        let out = save(&dir, "summary.md", "привет", false).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("summary.md")).unwrap(),
            "привет"
        );
        assert_eq!(out.bytes, "привет".len());
        assert_eq!(out.sha256, sha256_hex("привет".as_bytes()));
    }

    #[test]
    fn rejects_paths_outside_output() {
        let dir = temp_dir("escape");
        for name in ["../x.md", "/etc/x", "a/b.md", "..", ".hidden", ""] {
            assert!(save(&dir, name, "x", false).is_err(), "{name}");
        }
    }

    #[test]
    fn does_not_overwrite_unless_asked() {
        let dir = temp_dir("overwrite");
        save(&dir, "a.md", "one", false).unwrap();
        assert!(
            save(&dir, "a.md", "two", false)
                .unwrap_err()
                .contains("overwrite")
        );
        save(&dir, "a.md", "two", true).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("a.md")).unwrap(), "two");
    }
}
