# pipeline-mcp-agent

MCP-сервер на Rust с тремя инструментами, которые складываются в пайплайн
«получить → обработать → сохранить»:

1. `search` — находит строки с запросом в текстовых файлах каталога;
2. `summarize` — делает сводку по найденным строкам;
3. `save_to_file` — сохраняет сводку в файл.

Протокол — JSON-RPC через stdin/stdout (транспорт stdio). Сети, ключей и
собственной LLM у сервера нет: сводку пишет модель клиента через MCP
sampling, а клиент без sampling получает экстрактивную сводку.

Консольный клиент [`agentcli`](https://github.com/Egor-Liadsky/agent-cli)
запускает цепочку командой `agentcli pipeline run` и отдаёт те же инструменты
модели в чате. С сервером он связан только процессом — cargo-зависимости
между проектами нет.

## Устройство

Cargo workspace (Rust edition 2024) из одного крейта:

| Крейт          | Каталог           | Что это                                   |
|----------------|-------------------|-------------------------------------------|
| `pipeline-mcp` | `crates/pipeline` | бинарник `pipeline-mcp` — MCP-сервер пайплайна |

- `src/main.rs` — инструменты, их аргументы, sampling и разбор ключей;
- `src/search.rs` — обход каталога и поиск подстроки;
- `src/summarize.rs` — промпт для sampling и экстрактивная сводка;
- `src/save.rs` — запись в каталог `--output` с проверкой имени;
- `tests/protocol.rs` — настоящий процесс против MCP-клиента `rmcp` с
  sampling-заглушкой: вся цепочка и передача данных между шагами.

| Назначение              | Крейты                                        |
|-------------------------|-----------------------------------------------|
| MCP-сервер              | `rmcp` (server, macros, transport-io)         |
| JSON-схемы аргументов   | `schemars`                                    |
| Асинхронный рантайм     | `tokio`                                       |
| Сериализация            | `serde`, `serde_json`                         |
| Контрольная сумма файла | `sha2`, `hex`                                 |
| Тесты (MCP-клиент)      | `rmcp` (client, transport-child-process)      |

## Установка и запуск

```bash
cargo install --git https://github.com/Egor-Liadsky/pipeline-mcp-agent pipeline-mcp
cargo install --path crates/pipeline        # из локальной копии
cargo build --release                       # или target/release/pipeline-mcp

pipeline-mcp --root <каталог поиска> --output <каталог записи>
```

`--root` должен существовать, `--output` создаётся при запуске. Оба пути
задаются только ключами: модель не может искать или писать за их пределами.

## Инструменты и контракт данных

Каждый инструмент отдаёт JSON в `structuredContent` и тот же JSON текстом.
Входы следующего шага названы так же, как поля выхода предыдущего, поэтому
данные передаются без преобразований:

```
search(query)                     → {query, matches, files_scanned, truncated}
summarize(query, matches)         → {summary, method, sources, input_matches, fallback_reason?}
save_to_file(file_name, content)  → {path, bytes, sha256}
          content = summary ─┘
```

| Инструмент     | Аргументы | Что делает |
|----------------|-----------|------------|
| `search`       | `query`, `max_results` (20, не больше 200), `extension` | регистронезависимый поиск подстроки; пропускает `.git`, `target`, `node_modules`, `.idea`, двоичные файлы и файлы больше 1 МБ; пути относительные, порядок детерминирован |
| `summarize`    | `query`, `matches`, `max_words` (150) | если клиент объявил capability `sampling` — `sampling/createMessage` с промптом из всех строк; иначе или при ошибке — лучшие по терминам запроса строки, `method: "extractive"` и причина в `fallback_reason`; пустой `matches` — ошибка |
| `save_to_file` | `file_name`, `content`, `overwrite` (false) | пишет в `--output`; имя — один компонент без `/`, `\`, `..` и ведущей точки; существующий файл без `overwrite` не трогается; `sha256` — сумма записанного |

`search` и `summarize` только читают, `save_to_file` пишет — клиенту стоит
спрашивать подтверждение на него.

Sampling в `rmcp` помечен устаревшим (SEP-2577), но это единственный способ
дать серверу без ключей модель клиента, поэтому он используется, а
экстрактивный путь страхует клиентов, которые его не поддерживают.

## Проверка

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

`tests/protocol.rs` проверяет:

- список инструментов;
- цепочку `search → summarize → save_to_file`: в промпт sampling попала
  каждая найденная строка, `sources` — ровно файлы из `matches`, файл на
  диске равен `summary`, `sha256` совпадает;
- экстрактивную сводку у клиента без sampling;
- ошибки разорванной цепочки (пустой `matches`, имя файла с `..`).

## Лицензия

MIT, см. [LICENSE](LICENSE).
