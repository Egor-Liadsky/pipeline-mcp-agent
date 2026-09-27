//! `pipeline-mcp` — MCP-сервер трёх инструментов, которые складываются в
//! пайплайн «получить → обработать → сохранить»:
//!
//! 1. `search` — строки с запросом в файлах каталога `--root`;
//! 2. `summarize` — сводка по этим строкам (MCP sampling у клиента или
//!    экстрактивно);
//! 3. `save_to_file` — запись сводки в каталог `--output`.
//!
//! Контракт между шагами — JSON: каждый инструмент отдаёт
//! `structuredContent` (и тот же JSON текстом), а входы следующего шага
//! названы так же, как поля выхода предыдущего (`matches` из `search` —
//! аргумент `summarize`, `summary` из `summarize` — `content` для
//! `save_to_file`). Цепочку ведёт клиент: сервер шаги сам не связывает,
//! чтобы каждый оставался отдельным инструментом, доступным модели.
//!
//! Запуск: `pipeline-mcp --root <каталог поиска> --output <каталог записи>`;
//! протокол — JSON-RPC через stdin/stdout.

// Sampling помечен в rmcp устаревшим (SEP-2577), но это единственный
// способ дать серверу без ключей модель клиента; замены в протоколе пока нет.
#![allow(deprecated)]

mod save;
mod search;
mod summarize;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, CreateMessageRequestParams, Implementation, SamplingMessage,
    ServerCapabilities, ServerConfig,
};
use rmcp::{Peer, RoleServer, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use search::Match;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use summarize::{Method, SummaryOutput};

const DEFAULT_MAX_RESULTS: usize = 20;
/// Больше этого `search` не отдаёт: сводке нужны примеры, а не весь проект.
const MAX_RESULTS_LIMIT: usize = 200;
const DEFAULT_MAX_WORDS: usize = 150;
/// Токенов на ответ sampling: с запасом над `max_words` для любого языка.
const SAMPLING_MAX_TOKENS: u32 = 1024;

fn default_max_results() -> usize {
    DEFAULT_MAX_RESULTS
}

fn default_max_words() -> usize {
    DEFAULT_MAX_WORDS
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchArgs {
    /// Text to look for (case-insensitive substring).
    query: String,
    /// Maximum number of matching lines to return.
    #[serde(default = "default_max_results")]
    max_results: usize,
    /// Only files with this extension, e.g. "rs" or "md".
    #[serde(default)]
    extension: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SummarizeArgs {
    /// The query that produced the matches.
    query: String,
    /// The `matches` array returned by `search`, passed unchanged.
    matches: Vec<Match>,
    /// Upper bound on summary length in words.
    #[serde(default = "default_max_words")]
    max_words: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SaveArgs {
    /// File name inside the output directory, e.g. "summary.md". No directories.
    file_name: String,
    /// Text to write, usually the `summary` returned by `summarize`.
    content: String,
    /// Replace the file if it already exists.
    #[serde(default)]
    overwrite: bool,
}

#[derive(Debug, Clone)]
struct PipelineServer {
    root: PathBuf,
    output: PathBuf,
    tool_router: ToolRouter<Self>,
}

fn ok_json(value: &impl Serialize) -> CallToolResult {
    match serde_json::to_value(value) {
        Ok(value) => CallToolResult::structured(value),
        Err(err) => fail(format!("не удалось сериализовать результат: {err}")),
    }
}

fn fail(message: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message)])
}

#[tool_router]
impl PipelineServer {
    fn new(root: PathBuf, output: PathBuf) -> Self {
        Self {
            root,
            output,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Pipeline step 1: finds lines containing the query in the project's text files. \
Returns {query, matches:[{path,line,text}], files_scanned, truncated}; pass `matches` to `summarize`."
    )]
    async fn search(&self, Parameters(args): Parameters<SearchArgs>) -> CallToolResult {
        let root = self.root.clone();
        let max_results = args.max_results.clamp(1, MAX_RESULTS_LIMIT);
        // Обход каталога блокирующий: не держать им поток рантайма, где
        // крутится транспорт.
        let result = tokio::task::spawn_blocking(move || {
            search::search(&root, &args.query, max_results, args.extension.as_deref())
        })
        .await;
        match result {
            Ok(Ok(output)) => ok_json(&output),
            Ok(Err(err)) => fail(err),
            Err(err) => fail(format!("поиск прерван: {err}")),
        }
    }

    #[tool(
        description = "Pipeline step 2: summarizes the `matches` returned by `search` (asks the client's model \
via MCP sampling, falls back to an extractive summary). Returns {summary, method, sources, input_matches}; \
pass `summary` to `save_to_file` as `content`."
    )]
    async fn summarize(
        &self,
        Parameters(args): Parameters<SummarizeArgs>,
        peer: Peer<RoleServer>,
    ) -> CallToolResult {
        if args.matches.is_empty() {
            return fail("нечего обобщать: matches пуст — сначала вызови search".to_string());
        }
        let max_words = args.max_words.max(10);
        let (summary, method, fallback_reason) =
            match sample(&peer, &args.query, &args.matches, max_words).await {
                Ok(summary) => (summary, Method::Sampling, None),
                Err(reason) => (
                    summarize::extractive(&args.query, &args.matches, max_words),
                    Method::Extractive,
                    Some(reason),
                ),
            };
        ok_json(&SummaryOutput {
            summary,
            method,
            sources: summarize::sources(&args.matches),
            input_matches: args.matches.len(),
            fallback_reason,
        })
    }

    #[tool(
        description = "Pipeline step 3: writes `content` to `file_name` in the output directory. \
Returns {path, bytes, sha256}."
    )]
    async fn save_to_file(&self, Parameters(args): Parameters<SaveArgs>) -> CallToolResult {
        match save::save(&self.output, &args.file_name, &args.content, args.overwrite) {
            Ok(output) => ok_json(&output),
            Err(err) => fail(err),
        }
    }
}

/// Сводка моделью клиента. `Err` — причина, по которой шаг перешёл на
/// экстрактивную сводку.
async fn sample(
    peer: &Peer<RoleServer>,
    query: &str,
    matches: &[Match],
    max_words: usize,
) -> Result<String, String> {
    let supports = peer
        .peer_info()
        .is_some_and(|info| info.capabilities.sampling.is_some());
    if !supports {
        return Err("клиент не поддерживает sampling".to_string());
    }
    let prompt = summarize::prompt(query, matches, max_words);
    let mut params = CreateMessageRequestParams::new(
        vec![SamplingMessage::user_text(prompt)],
        SAMPLING_MAX_TOKENS,
    );
    params.system_prompt = Some(summarize::SYSTEM_PROMPT.to_string());
    let result = peer
        .create_message(params)
        .await
        .map_err(|err| format!("sampling не удался: {err}"))?;
    let text = result
        .message
        .content
        .into_vec()
        .iter()
        .filter_map(|block| block.as_text().map(|text| text.text.clone()))
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        return Err("модель клиента вернула пустую сводку".to_string());
    }
    Ok(text.trim().to_string())
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PipelineServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(format!(
                "Pipeline tools: search in {} -> summarize the matches -> save_to_file into {}. \
Call them in this order, passing each step's JSON output to the next.",
                self.root.display(),
                self.output.display()
            ))
    }
}

#[derive(Debug, PartialEq)]
struct Args {
    root: PathBuf,
    output: PathBuf,
}

/// `--root <путь> --output <путь>` (или `--ключ=путь`).
fn parse_args(args: impl Iterator<Item = String>) -> Result<Args, String> {
    const USAGE: &str =
        "использование: pipeline-mcp --root <каталог поиска> --output <каталог записи>";
    let mut root = None;
    let mut output = None;
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let (key, value) = match arg.split_once('=') {
            Some((key, value)) => (key.to_string(), Some(value.to_string())),
            None => (arg, None),
        };
        let slot = match key.as_str() {
            "--root" => &mut root,
            "--output" => &mut output,
            _ => return Err(format!("неизвестный аргумент {key}; {USAGE}")),
        };
        let value = value
            .or_else(|| args.next())
            .ok_or_else(|| format!("после {key} нужен путь"))?;
        *slot = Some(PathBuf::from(value));
    }
    match (root, output) {
        (Some(root), Some(output)) => Ok(Args { root, output }),
        _ => Err(USAGE.to_string()),
    }
}

fn existing_dir(path: &Path, what: &str) -> Result<PathBuf, String> {
    if !path.is_dir() {
        return Err(format!(
            "{what} {} не существует или не каталог",
            path.display()
        ));
    }
    path.canonicalize()
        .map_err(|err| format!("не удалось разрешить путь {}: {err}", path.display()))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // Проверка до рукопожатия: с неверным путём клиент получает понятный
    // отказ процесса, а не сервер, у которого падает каждый вызов. Каталог
    // записи создаётся — его, в отличие от корня поиска, может ещё не быть.
    let args = parse_args(std::env::args().skip(1)).map_err(anyhow::Error::msg)?;
    let root = existing_dir(&args.root, "каталог поиска").map_err(anyhow::Error::msg)?;
    std::fs::create_dir_all(&args.output)?;
    let output = existing_dir(&args.output, "каталог записи").map_err(anyhow::Error::msg)?;
    let service = PipelineServer::new(root, output)
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Args, String> {
        parse_args(list.iter().map(|s| s.to_string()))
    }

    #[test]
    fn both_dirs_are_required() {
        let expected = Args {
            root: "/r".into(),
            output: "/o".into(),
        };
        assert_eq!(args(&["--root", "/r", "--output", "/o"]), Ok(expected));
        assert_eq!(
            args(&["--output=/o", "--root=/r"]),
            Ok(Args {
                root: "/r".into(),
                output: "/o".into()
            })
        );
        assert!(args(&["--root", "/r"]).is_err());
        assert!(args(&["--root"]).is_err());
        assert!(args(&["--repository", "/r"]).is_err());
    }
}
