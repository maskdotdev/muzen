use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use clap::{Parser, ValueEnum};
use muzen::agent_runtime::facade::{Agent, Tool};
use muzen::agent_runtime::MuzenError;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Transport {
    Local,
    Http,
}

#[derive(Debug, Parser)]
#[command(name = "muzen-agent-explore-bench")]
struct Args {
    #[arg(long, value_enum)]
    transport: Transport,
    #[arg(long)]
    root: PathBuf,
    #[arg(long)]
    base_url: Option<String>,
    #[arg(long)]
    model_base_url: String,
    #[arg(long, default_value_t = 5)]
    read_files: usize,
}

#[derive(Debug, Deserialize)]
struct PathInput {
    path: String,
}

#[derive(Debug, Deserialize)]
struct GrepInput {
    pattern: String,
    path: String,
}

#[derive(Default)]
struct Counts {
    list: AtomicUsize,
    read: AtomicUsize,
    grep: AtomicUsize,
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--sample-processes") {
        match sample_processes() {
            Ok(rows) => println!("{}", Value::Array(rows)),
            Err(error) => {
                eprintln!("muzen process sampler: {error}");
                std::process::exit(1);
            }
        }
        return;
    }
    // MUZEN_EXPLORE_RT=current_thread isolates scheduler-dependent stalls when
    // diagnosing concurrency incidents; the default matches #[tokio::main].
    let runtime = match std::env::var("MUZEN_EXPLORE_RT").as_deref() {
        Ok("current_thread") => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build(),
        _ => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build(),
    }
    .expect("build tokio runtime");
    if let Err(error) = runtime.block_on(run()) {
        eprintln!("muzen agent explore Rust driver: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.read_files == 0 {
        return Err("--read-files must be positive".into());
    }
    if matches!(args.transport, Transport::Http) && args.base_url.is_none() {
        return Err("--base-url is required for http transport".into());
    }
    let root = Arc::new(fs::canonicalize(&args.root)?);
    let counts = Arc::new(Counts::default());
    let path_schema = json!({
        "type": "object",
        "properties": { "path": { "type": "string" } },
        "required": ["path"],
        "additionalProperties": false
    });

    let list_root = Arc::clone(&root);
    let list_counts = Arc::clone(&counts);
    let fs_list = Tool::typed::<PathInput>(
        "fs_list",
        "Recursively list regular files below a repository path.",
        path_schema.clone(),
        move |input: PathInput| {
            let root = Arc::clone(&list_root);
            let counts = Arc::clone(&list_counts);
            async move {
                counts.list.fetch_add(1, Ordering::SeqCst);
                let value = (|| {
                    let files = files_under(&root, &input.path)?;
                    let entries = files
                        .iter()
                        .map(|path| {
                            Ok(json!({
                                "path": relative_slash(&root, path)?,
                                "bytes": fs::metadata(path).map_err(io_error)?.len()
                            }))
                        })
                        .collect::<Result<Vec<_>, String>>()?;
                    Ok(json!({ "path": input.path, "totalFiles": entries.len(), "files": entries }))
                })();
                Ok::<Value, MuzenError>(tool_value(value))
            }
        },
    )?;

    let read_root = Arc::clone(&root);
    let read_counts = Arc::clone(&counts);
    let fs_read = Tool::typed::<PathInput>(
        "fs_read",
        "Read one UTF-8 repository file.",
        path_schema,
        move |input: PathInput| {
            let root = Arc::clone(&read_root);
            let counts = Arc::clone(&read_counts);
            async move {
                counts.read.fetch_add(1, Ordering::SeqCst);
                let value = (|| {
                    let path = jailed(&root, &input.path)?;
                    let bytes = fs::read(&path).map_err(io_error)?;
                    Ok(json!({
                        "path": input.path,
                        "bytes": bytes.len(),
                        "content": String::from_utf8_lossy(&bytes)
                    }))
                })();
                Ok::<Value, MuzenError>(tool_value(value))
            }
        },
    )?;

    let grep_root = Arc::clone(&root);
    let grep_counts = Arc::clone(&counts);
    let fs_grep = Tool::typed::<GrepInput>(
        "fs_grep",
        "Search repository files for a fixed text pattern.",
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "path": { "type": "string" }
            },
            "required": ["pattern", "path"],
            "additionalProperties": false
        }),
        move |input: GrepInput| {
            let root = Arc::clone(&grep_root);
            let counts = Arc::clone(&grep_counts);
            async move {
                counts.grep.fetch_add(1, Ordering::SeqCst);
                let value = (|| {
                    let mut matches = Vec::new();
                    let mut total = 0usize;
                    for file in files_under(&root, &input.path)? {
                        let bytes = fs::read(&file).map_err(io_error)?;
                        let text = String::from_utf8_lossy(&bytes);
                        for (index, line) in text.lines().enumerate() {
                            if line.contains(&input.pattern) {
                                total += 1;
                                if matches.len() < 100 {
                                    matches.push(json!({
                                        "path": relative_slash(&root, &file)?,
                                        "line": index + 1,
                                        "text": line.chars().take(500).collect::<String>()
                                    }));
                                }
                            }
                        }
                    }
                    Ok(json!({
                        "pattern": input.pattern,
                        "path": input.path,
                        "matches": matches,
                        "totalMatches": total,
                        "truncated": total > matches.len()
                    }))
                })();
                Ok::<Value, MuzenError>(tool_value(value))
            }
        },
    )?;

    let mut builder = Agent::new(
        "Explore the repository with the provided filesystem tools, then summarize what you saw.",
        "openai:muzen-agent-explore",
    )
    .api_key("bench-test-key")
    .model_base_url(&args.model_base_url)
    .tools([fs_list, fs_read, fs_grep]);
    if matches!(args.transport, Transport::Http) {
        builder = builder
            .transport("http")
            .base_url(args.base_url.as_deref().expect("validated HTTP URL"));
    }
    let started = Instant::now();
    let mut agent = builder.build()?;
    let result = agent
        .run("Explore src and report a concise summary.")
        .await?;
    result.raise_for_status()?;
    agent.close().await?;
    let duration_ms = started.elapsed().as_secs_f64() * 1000.0;
    let list_count = counts.list.load(Ordering::SeqCst);
    let read_count = counts.read.load(Ordering::SeqCst);
    let grep_count = counts.grep.load(Ordering::SeqCst);
    if (list_count, read_count, grep_count) != (1, args.read_files, 1) {
        return Err(format!(
            "tool count mismatch: expected list=1 read={} grep=1, got list={list_count} read={read_count} grep={grep_count}",
            args.read_files
        )
        .into());
    }
    if result.text.trim().is_empty() {
        return Err("agent returned an empty summary".into());
    }
    println!(
        "{}",
        serde_json::to_string(&json!({
            "turns": list_count + read_count + grep_count + 1,
            "toolCalls": list_count + read_count + grep_count,
            "durationMs": duration_ms,
            "summaryText": result.text,
        }))?
    );
    Ok(())
}

fn jailed(root: &Path, requested: &str) -> Result<PathBuf, String> {
    let candidate = fs::canonicalize(root.join(requested)).map_err(io_error)?;
    if candidate != root && !candidate.starts_with(root) {
        return Err(format!("path escapes --root: {requested}"));
    }
    Ok(candidate)
}

fn files_under(root: &Path, requested: &str) -> Result<Vec<PathBuf>, String> {
    let base = jailed(root, requested)?;
    let mut files = Vec::new();
    visit(root, &base, &mut files)?;
    files.sort();
    Ok(files)
}

fn visit(root: &Path, directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(directory).map_err(io_error)?;
    for entry in entries {
        let entry = entry.map_err(io_error)?;
        let kind = entry.file_type().map_err(io_error)?;
        if kind.is_dir() {
            visit(root, &entry.path(), files)?;
        } else if kind.is_file() {
            let path = fs::canonicalize(entry.path()).map_err(io_error)?;
            if path == root || path.starts_with(root) {
                files.push(path);
            }
        }
    }
    Ok(())
}

fn relative_slash(root: &Path, path: &Path) -> Result<String, String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| "path escapes --root".to_owned())?;
    Ok(relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/"))
}

fn io_error(error: std::io::Error) -> String {
    format!("filesystem tool failed: {error}")
}

fn tool_value(result: Result<Value, String>) -> Value {
    result.unwrap_or_else(|error| json!({ "error": error }))
}

#[cfg(target_os = "macos")]
fn sample_processes() -> Result<Vec<Value>, String> {
    use std::mem::{size_of, zeroed};

    let mut pids = vec![0i32; 16_384];
    let count = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            i32::try_from(pids.len() * size_of::<i32>()).map_err(|error| error.to_string())?,
        )
    };
    if count < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let mut rows = Vec::new();
    for pid in pids.into_iter().take(count as usize).filter(|pid| *pid > 0) {
        let mut task = unsafe { zeroed::<libc::proc_taskinfo>() };
        let task_size =
            i32::try_from(size_of::<libc::proc_taskinfo>()).map_err(|error| error.to_string())?;
        let task_result = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTASKINFO,
                0,
                (&mut task as *mut libc::proc_taskinfo).cast(),
                task_size,
            )
        };
        if task_result != task_size {
            continue;
        }
        let mut bsd = unsafe { zeroed::<libc::proc_bsdinfo>() };
        let bsd_size =
            i32::try_from(size_of::<libc::proc_bsdinfo>()).map_err(|error| error.to_string())?;
        let bsd_result = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut bsd as *mut libc::proc_bsdinfo).cast(),
                bsd_size,
            )
        };
        if bsd_result != bsd_size {
            continue;
        }
        let name_bytes = bsd
            .pbi_name
            .iter()
            .map(|value| *value as u8)
            .take_while(|value| *value != 0)
            .collect::<Vec<_>>();
        rows.push(json!({
            "pid": pid,
            "ppid": bsd.pbi_ppid,
            "rssBytes": task.pti_resident_size,
            "command": String::from_utf8_lossy(&name_bytes)
        }));
    }
    Ok(rows)
}

#[cfg(target_os = "linux")]
fn sample_processes() -> Result<Vec<Value>, String> {
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return Err("sysconf(_SC_PAGESIZE) failed".to_owned());
    }
    let mut rows = Vec::new();
    for entry in fs::read_dir("/proc").map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let stat_text = match fs::read_to_string(entry.path().join("stat")) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let Some(after_name) = stat_text.rsplit_once(") ").map(|(_, value)| value) else {
            continue;
        };
        let Some(ppid) = after_name
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        let rss_pages = match fs::read_to_string(entry.path().join("statm")) {
            Ok(value) => value
                .split_whitespace()
                .nth(1)
                .and_then(|item| item.parse::<u64>().ok())
                .unwrap_or(0),
            Err(_) => 0,
        };
        let command = fs::read(entry.path().join("cmdline"))
            .ok()
            .map(|value| {
                String::from_utf8_lossy(&value)
                    .split('\0')
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .unwrap_or_default();
        rows.push(json!({
            "pid": pid,
            "ppid": ppid,
            "rssBytes": rss_pages * page_size as u64,
            "command": command
        }));
    }
    Ok(rows)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn sample_processes() -> Result<Vec<Value>, String> {
    Err("RSS sampling is supported on macOS and Linux".to_owned())
}
