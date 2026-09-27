//! Сервер проверяется так, как его видит MCP-клиент: настоящий процесс,
//! клиент `rmcp` через stdin/stdout, временные каталоги поиска и записи.
//!
//! Главный тест — цепочка `search → summarize → save_to_file`, где
//! аргументы каждого шага собираются из JSON-выхода предыдущего, а
//! sampling-заглушка клиента запоминает, что сервер передал модели.

#![allow(deprecated)]

use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientConfig, CreateMessageRequestParams,
    CreateMessageResult, ErrorData, Implementation, SamplingMessage,
};
use rmcp::service::{RequestContext, RunningService};
use rmcp::transport::TokioChildProcess;
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

const BINARY: &str = env!("CARGO_BIN_EXE_pipeline-mcp");
const SAMPLED_SUMMARY: &str = "ToolSet объединяет исполнители git-mcp и activity-mcp.";

fn temp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("pipeline-mcp-{name}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Корень поиска: три совпадения в двух файлах и шум.
fn project(name: &str) -> PathBuf {
    let root = temp_dir(name);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/tool_loop.rs"),
        "// ToolSet merges executors\nfn x() {}\npub struct ToolSet;\n",
    )
    .unwrap();
    std::fs::write(
        root.join("README.md"),
        "# Demo\nToolSet routes calls by tool name.\n",
    )
    .unwrap();
    std::fs::write(root.join("other.txt"), "unrelated\n").unwrap();
    root
}

/// Клиент с sampling: вместо модели — фиксированный ответ, а промпт
/// запоминается для проверки.
#[derive(Clone, Default)]
struct Sampler {
    prompts: Arc<Mutex<Vec<String>>>,
}

impl ClientHandler for Sampler {
    async fn create_message(
        &self,
        params: CreateMessageRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<CreateMessageResult, ErrorData> {
        let prompt = params
            .messages
            .into_iter()
            .flat_map(|message| message.content.into_vec())
            .filter_map(|block| block.as_text().map(|text| text.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        self.prompts.lock().unwrap().push(prompt);
        Ok(CreateMessageResult::new(
            SamplingMessage::assistant_text(SAMPLED_SUMMARY),
            "stub-model".to_string(),
        ))
    }

    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::builder().enable_sampling().build(),
            Implementation::new("sampler-test", "0"),
        )
    }
}

async fn connect<H: ClientHandler>(
    handler: H,
    root: &Path,
    output: &Path,
) -> RunningService<RoleClient, H> {
    let mut command = tokio::process::Command::new(BINARY);
    command.arg("--root").arg(root).arg("--output").arg(output);
    let (transport, _stderr) = TokioChildProcess::builder(command)
        .stderr(Stdio::null())
        .spawn()
        .expect("запуск сервера");
    handler.serve(transport).await.expect("рукопожатие MCP")
}

/// Структурированный результат или текст ошибки.
async fn call<H: ClientHandler>(
    client: &RunningService<RoleClient, H>,
    name: &str,
    arguments: Value,
) -> Result<Value, String> {
    let params = CallToolRequestParams::new(name.to_string())
        .with_arguments(arguments.as_object().cloned().unwrap_or_default());
    let result = client.peer().call_tool(params).await.expect("tools/call");
    if result.is_error == Some(true) {
        let content = serde_json::to_value(&result.content).unwrap();
        return Err(content[0]["text"].as_str().unwrap_or_default().to_string());
    }
    // Текстовый блок дублирует structuredContent — клиент без поддержки
    // структурированных ответов получает те же данные.
    let structured = result.structured_content.expect("structuredContent");
    let text = serde_json::to_value(&result.content).unwrap()[0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), structured);
    Ok(structured)
}

#[tokio::test]
async fn lists_three_pipeline_tools() {
    let client = connect((), &project("list"), &temp_dir("list-out")).await;
    let mut names: Vec<String> = client
        .peer()
        .list_all_tools()
        .await
        .expect("tools/list")
        .iter()
        .map(|tool| tool.name.to_string())
        .collect();
    names.sort();
    assert_eq!(names, vec!["save_to_file", "search", "summarize"]);
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn chain_passes_each_output_to_the_next_step() {
    let root = project("chain");
    let output = temp_dir("chain-out");
    let sampler = Sampler::default();
    let client = connect(sampler.clone(), &root, &output).await;

    // Шаг 1.
    let found = call(&client, "search", json!({ "query": "toolset" }))
        .await
        .unwrap();
    let matches = found["matches"].as_array().unwrap().clone();
    assert_eq!(matches.len(), 3);
    assert_eq!(found["files_scanned"], 3);

    // Шаг 2: `matches` передаётся как есть.
    let summary = call(
        &client,
        "summarize",
        json!({ "query": found["query"], "matches": found["matches"] }),
    )
    .await
    .unwrap();
    assert_eq!(summary["method"], "sampling");
    assert_eq!(summary["summary"], SAMPLED_SUMMARY);
    assert_eq!(summary["input_matches"], 3);
    assert_eq!(summary["sources"], json!(["README.md", "src/tool_loop.rs"]));
    let prompts = sampler.prompts.lock().unwrap().clone();
    assert_eq!(prompts.len(), 1, "sampling вызван ровно один раз");
    for m in &matches {
        let line = format!(
            "{}:{}: {}",
            m["path"].as_str().unwrap(),
            m["line"],
            m["text"].as_str().unwrap()
        );
        assert!(
            prompts[0].contains(&line),
            "в промпте нет строки {line}:\n{}",
            prompts[0]
        );
    }

    // Шаг 3: `summary` становится `content`.
    let saved = call(
        &client,
        "save_to_file",
        json!({ "file_name": "summary.md", "content": summary["summary"] }),
    )
    .await
    .unwrap();
    let on_disk = std::fs::read_to_string(output.join("summary.md")).unwrap();
    assert_eq!(on_disk, SAMPLED_SUMMARY);
    assert_eq!(
        saved["sha256"],
        hex::encode(Sha256::digest(on_disk.as_bytes()))
    );
    assert_eq!(saved["bytes"], on_disk.len());
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn client_without_sampling_gets_extractive_summary() {
    let root = project("extractive");
    let client = connect((), &root, &temp_dir("extractive-out")).await;
    let found = call(&client, "search", json!({ "query": "ToolSet" }))
        .await
        .unwrap();
    let summary = call(
        &client,
        "summarize",
        json!({ "query": "ToolSet", "matches": found["matches"] }),
    )
    .await
    .unwrap();
    assert_eq!(summary["method"], "extractive");
    assert!(
        summary["fallback_reason"]
            .as_str()
            .unwrap()
            .contains("sampling")
    );
    let text = summary["summary"].as_str().unwrap();
    for m in found["matches"].as_array().unwrap() {
        assert!(text.contains(m["text"].as_str().unwrap()), "{text}");
    }
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn broken_links_in_the_chain_are_reported() {
    let client = connect((), &project("errors"), &temp_dir("errors-out")).await;
    let err = call(&client, "summarize", json!({ "query": "x", "matches": [] }))
        .await
        .unwrap_err();
    assert!(err.contains("search"), "{err}");
    let err = call(
        &client,
        "save_to_file",
        json!({ "file_name": "../escape.md", "content": "x" }),
    )
    .await
    .unwrap_err();
    assert!(err.contains("недопустимое имя"), "{err}");
    client.cancel().await.unwrap();
}

#[test]
fn refuses_to_start_without_root() {
    let status = std::process::Command::new(BINARY)
        .args(["--root", "/definitely/missing", "--output", "/tmp"])
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success());
}
