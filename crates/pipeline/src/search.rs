//! Шаг 1 пайплайна: поиск строк по текстовым файлам каталога `--root`.
//!
//! Поиск локальный и детерминированный — без сети, индекса и ключей: при
//! одних и тех же файлах выход один и тот же, поэтому цепочку можно
//! проверять точным сравнением.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Файлы крупнее этого не читаются: это почти всегда сборочные артефакты
/// или данные, а не текст, в котором ищут.
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Совпавшая строка обрезается до стольких символов: минифицированный
/// файл в одну строку не должен заполнить весь ответ.
const MAX_LINE_CHARS: usize = 400;
/// Каталоги, которые не обходятся: служебные данные VCS и сборки.
const SKIPPED_DIRS: [&str; 4] = [".git", "target", "node_modules", ".idea"];

/// Одна найденная строка. Та же структура — вход `summarize`, так что
/// выход шага передаётся следующему без преобразований.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Match {
    /// Path relative to the search root, with `/` separators.
    pub path: String,
    /// 1-based line number.
    pub line: usize,
    /// Matched line, trimmed.
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchOutput {
    pub query: String,
    pub matches: Vec<Match>,
    pub files_scanned: usize,
    /// Совпадений было больше `max_results`, выдана только часть.
    pub truncated: bool,
}

/// Регистронезависимый поиск подстроки. Файлы обходятся в
/// отсортированном порядке — выход не зависит от порядка `read_dir`.
pub fn search(
    root: &Path,
    query: &str,
    max_results: usize,
    extension: Option<&str>,
) -> Result<SearchOutput, String> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Err("запрос поиска пуст".to_string());
    }
    let extension = extension.map(|ext| ext.trim_start_matches('.').to_lowercase());
    let mut files = Vec::new();
    collect_files(root, &mut files);
    files.sort();

    let mut output = SearchOutput {
        query: query.to_string(),
        matches: Vec::new(),
        files_scanned: 0,
        truncated: false,
    };
    for file in files {
        if let Some(ext) = &extension {
            let matches_ext = file
                .extension()
                .is_some_and(|actual| actual.to_string_lossy().to_lowercase() == *ext);
            if !matches_ext {
                continue;
            }
        }
        let Some(text) = read_text(&file) else {
            continue;
        };
        output.files_scanned += 1;
        for (index, line) in text.lines().enumerate() {
            if !line.to_lowercase().contains(&needle) {
                continue;
            }
            if output.matches.len() == max_results {
                output.truncated = true;
                return Ok(output);
            }
            output.matches.push(Match {
                path: relative(root, &file),
                line: index + 1,
                text: line.trim().chars().take(MAX_LINE_CHARS).collect(),
            });
        }
    }
    Ok(output)
}

fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        // Символические ссылки не обходятся: ссылка наружу вывела бы поиск
        // за пределы `--root`, а ссылка на предка зациклила бы обход.
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            let name = entry.file_name();
            if !SKIPPED_DIRS.iter().any(|skipped| name == *skipped) {
                collect_files(&path, files);
            }
        } else if kind.is_file() {
            files.push(path);
        }
    }
}

/// Текст файла или `None` для больших, нечитаемых и двоичных (NUL в
/// содержимом, не UTF-8).
fn read_text(path: &Path) -> Option<String> {
    let size = std::fs::metadata(path).ok()?.len();
    if size > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn relative(root: &Path, file: &Path) -> String {
    let relative = file.strip_prefix(root).unwrap_or(file);
    relative
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("pipeline-search-{name}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finds_lines_in_sorted_order_ignoring_case() {
        let root = temp_dir("order");
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::write(root.join("b/two.md"), "nothing\nToolSet here\n").unwrap();
        std::fs::write(root.join("a.rs"), "  let toolset = 1;  \n").unwrap();
        let out = search(&root, "TOOLSET", 10, None).unwrap();
        assert_eq!(
            out.matches,
            vec![
                Match {
                    path: "a.rs".into(),
                    line: 1,
                    text: "let toolset = 1;".into()
                },
                Match {
                    path: "b/two.md".into(),
                    line: 2,
                    text: "ToolSet here".into()
                },
            ]
        );
        assert_eq!(out.files_scanned, 2);
        assert!(!out.truncated);
    }

    #[test]
    fn skips_service_dirs_and_binary_files() {
        let root = temp_dir("skip");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join(".git/config"), "needle\n").unwrap();
        std::fs::write(root.join("target/out.txt"), "needle\n").unwrap();
        std::fs::write(root.join("blob.bin"), b"needle\0\x01").unwrap();
        std::fs::write(root.join("ok.txt"), "needle\n").unwrap();
        let out = search(&root, "needle", 10, None).unwrap();
        let paths: Vec<_> = out.matches.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, vec!["ok.txt"]);
    }

    #[test]
    fn limit_and_extension_filter() {
        let root = temp_dir("limit");
        std::fs::write(root.join("x.md"), "hit\nhit\nhit\n").unwrap();
        std::fs::write(root.join("y.rs"), "hit\n").unwrap();
        let out = search(&root, "hit", 2, None).unwrap();
        assert_eq!(out.matches.len(), 2);
        assert!(out.truncated);
        let out = search(&root, "hit", 10, Some(".rs")).unwrap();
        assert_eq!(out.matches.len(), 1);
        assert_eq!(out.matches[0].path, "y.rs");
    }

    #[test]
    fn empty_query_is_rejected() {
        let root = temp_dir("empty");
        assert!(search(&root, "  ", 10, None).is_err());
    }
}
