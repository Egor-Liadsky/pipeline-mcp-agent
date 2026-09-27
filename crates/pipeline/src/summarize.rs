//! Шаг 2 пайплайна: сводка по найденным строкам.
//!
//! LLM и ключей у сервера нет. Основной путь — MCP sampling: сервер
//! просит модель у клиента (`sampling/createMessage`), и сводку пишет та
//! же модель, что ведёт чат. Клиент без sampling получает экстрактивную
//! сводку, собранную здесь без модели, — шаг не падает, а поле `method`
//! говорит, какой путь сработал.

use crate::search::Match;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Sampling,
    Extractive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryOutput {
    pub summary: String,
    pub method: Method,
    /// Файлы, из которых взяты строки, — в порядке первого появления.
    pub sources: Vec<String>,
    /// Сколько совпадений получил шаг: проверка, что поиск передал всё.
    pub input_matches: usize,
    /// Почему не сработал sampling, если сводка экстрактивная.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
}

pub const SYSTEM_PROMPT: &str = "You summarize search results from a local project. \
Answer in the language of the query. Use only the given lines, cite file paths, no preamble.";

/// Промпт для sampling: запрос и каждая строка с её адресом, чтобы модель
/// могла сослаться на файл.
pub fn prompt(query: &str, matches: &[Match], max_words: usize) -> String {
    let mut prompt = format!(
        "Query: {query}\nSummarize the {} matching lines below in at most {max_words} words.\n\n",
        matches.len()
    );
    for m in matches {
        prompt.push_str(&format!("{}:{}: {}\n", m.path, m.line, m.text));
    }
    prompt
}

pub fn sources(matches: &[Match]) -> Vec<String> {
    let mut sources: Vec<String> = Vec::new();
    for m in matches {
        if !sources.contains(&m.path) {
            sources.push(m.path.clone());
        }
    }
    sources
}

fn terms(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|term| !term.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Сводка без модели: строки, в которых больше всего терминов запроса,
/// в исходном порядке, пока не набрано `max_words` слов. Заголовок
/// фиксирует масштаб: сколько строк и файлов стоит за сводкой.
pub fn extractive(query: &str, matches: &[Match], max_words: usize) -> String {
    let files = sources(matches);
    let mut summary = format!(
        "«{query}»: {} совпадений в {} файлах ({}).\n",
        matches.len(),
        files.len(),
        files.join(", ")
    );
    let query_terms = terms(query);
    let mut ranked: Vec<(usize, usize)> = matches
        .iter()
        .enumerate()
        .map(|(index, m)| {
            let line_terms = terms(&m.text);
            let score = query_terms
                .iter()
                .filter(|term| line_terms.iter().any(|t| t.contains(term.as_str())))
                .count();
            (index, score)
        })
        .collect();
    // Стабильная сортировка: при равном счёте сохраняется порядок поиска.
    ranked.sort_by_key(|&(_, score)| std::cmp::Reverse(score));

    let mut words = 0;
    let mut chosen = Vec::new();
    for (index, _) in ranked {
        let count = matches[index].text.split_whitespace().count();
        if words > 0 && words + count > max_words {
            break;
        }
        words += count;
        chosen.push(index);
    }
    chosen.sort_unstable();
    for index in chosen {
        let m = &matches[index];
        summary.push_str(&format!("- {}:{} — {}\n", m.path, m.line, m.text));
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(path: &str, line: usize, text: &str) -> Match {
        Match {
            path: path.into(),
            line,
            text: text.into(),
        }
    }

    #[test]
    fn prompt_carries_every_line() {
        let matches = [m("a.rs", 1, "first"), m("b.rs", 7, "second")];
        let prompt = prompt("q", &matches, 50);
        assert!(prompt.contains("a.rs:1: first"));
        assert!(prompt.contains("b.rs:7: second"));
    }

    #[test]
    fn sources_are_unique_in_order() {
        let matches = [m("b", 1, "x"), m("a", 1, "x"), m("b", 2, "x")];
        assert_eq!(sources(&matches), vec!["b", "a"]);
    }

    #[test]
    fn extractive_prefers_lines_with_query_terms_and_keeps_order() {
        let matches = [
            m(
                "a.rs",
                1,
                "tool loop runs calls one by one sequentially here",
            ),
            m("a.rs", 2, "ToolSet merges executors"),
            m("b.rs", 3, "ToolSet routes call by name"),
        ];
        let summary = extractive("ToolSet", &matches, 10);
        assert!(summary.starts_with("«ToolSet»: 3 совпадений в 2 файлах (a.rs, b.rs)."));
        let a2 = summary.find("a.rs:2").unwrap();
        let b3 = summary.find("b.rs:3").unwrap();
        assert!(a2 < b3);
        assert!(!summary.contains("a.rs:1"), "{summary}");
    }
}
