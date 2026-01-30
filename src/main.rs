use std::io::{self, BufRead, Write};
use serde::Deserialize;
use serde_json::{json, Value};
use std::process::Command;
use regex::Regex;
use semver::Version;

/// Data structure to represent JSON-RPC requests from MCP clients
#[derive(Deserialize, Debug)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    method: String,
    #[serde(default)]
    params: Value,
    #[serde(default)]
    id: Option<Value>,
}

/// Fetches a list of tags from a Git repository with options for limiting the count
/// and sorting by SemVer version
fn get_tags(link: &str, limit: Option<usize>) -> Result<Value, String> {
    eprintln!("[DEBUG] Fetching tags for: {} (limit: {:?})", link, limit);

    // Execute git ls-remote command to get the list of tags
    let output = Command::new("git")
        .args(["ls-remote", "--tags", "--refs", link])
        .output()
        .map_err(|e| e.to_string())?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }

    let raw_output = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;

    // Clean tag names from refs/tags/ references
    let mut tags: Vec<String> = raw_output
        .lines()
        .map(|line| {
            line.split('\t')
                .nth(1)
                .unwrap_or("")
                .trim_start_matches("refs/tags/")
                .to_string()
        })
        .collect();

    // Sort tags using SemVer if possible
    tags.sort_by(|a, b| {
        let ver_a = Version::parse(a.trim_start_matches('v'));
        let ver_b = Version::parse(b.trim_start_matches('v'));

        match (ver_a, ver_b) {
            (Ok(va), Ok(vb)) => vb.cmp(&va), // Sort in descending order (latest first)
            (Ok(_), Err(_)) => std::cmp::Ordering::Less, // SemVer versions have higher priority than plain strings
            (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
            (Err(_), Err(_)) => b.cmp(a), // Sort plain strings in descending order
        }
    });

    // Apply tag count limit if provided
    if let Some(n) = limit {
        if n < tags.len() {
            tags.truncate(n);
        }
    }

    Ok(json!({
        "repository": link,
        "count": tags.len(),
        "limit_applied": limit,
        "tags": tags
    }))
}

/// Gets the changelog between two versions in a GitHub repository
fn get_changelog(link: &str, v1: &str, v2: &str) -> Result<Value, String> {
    eprintln!("[DEBUG] Fetching changelog: {}...{}", v1, v2);
    let (owner, repo) = parse_github_url(link)?;

    // Use GitHub API to compare two versions
    let api_url = format!("https://api.github.com/repos/{}/{}/compare/{}...{}", owner, repo, v1, v2);

    let client = reqwest::blocking::Client::new();
    let resp = client.get(&api_url)
        .header("User-Agent", "Rust-MCP-Server/1.0")
        .send()
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        return Err(format!("GitHub API Error: {}", resp.status()));
    }

    let json: Value = resp.json().map_err(|e| e.to_string())?;
    let commits = json["commits"].as_array().ok_or("No commits found")?;

    // Extract commit message summaries and dates
    let summaries: Vec<String> = commits.iter().map(|c| {
        let msg = c["commit"]["message"].as_str().unwrap_or("").lines().next().unwrap_or("");
        let date = c["commit"]["author"]["date"].as_str().unwrap_or("").split('T').next().unwrap_or("");
        format!("[{}] {}", date, msg)
    }).collect();

    Ok(json!({
        "repository": link,
        "from": v1,
        "to": v2,
        "changes": summaries
    }))
}

/// Fetches the content of a README file from a GitHub repository
fn get_readme(link: &str) -> Result<Value, String> {
    eprintln!("[DEBUG] Fetching README for: {}", link);
    let (owner, repo) = parse_github_url(link)?;

    // Use GitHub API endpoint to fetch the README
    let api_url = format!("https://api.github.com/repos/{}/{}/readme", owner, repo);

    let client = reqwest::blocking::Client::new();
    let resp = client.get(&api_url)
        .header("User-Agent", "Rust-MCP-Server/1.0")
        // Accept header to get raw text instead of Base64 JSON
        .header("Accept", "application/vnd.github.raw")
        .send()
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        return Err(format!("Failed to fetch README (Status: {}). Make sure the repo is public/exist.", resp.status()));
    }

    let content = resp.text().map_err(|e| e.to_string())?;

    // Limit content size if too large
    let truncated_content = if content.len() > 20000 {
        format!("{}... [TRUNCATED]", &content[..20000])
    } else {
        content
    };

    Ok(json!({
        "repository": link,
        "type": "readme",
        "content": truncated_content
    }))
}

/// Helper function to parse a GitHub URL into owner and repository name
fn parse_github_url(url: &str) -> Result<(String, String), String> {
    let re = Regex::new(r"github\.com/([^/]+)/([^/]+?)(?:\.git)?$").map_err(|e| e.to_string())?;
    let caps = re.captures(url).ok_or("Invalid GitHub URL")?;
    Ok((caps[1].to_string(), caps[2].to_string()))
}

/// Gets the file tree structure from a GitHub repository
fn get_file_tree(link: &str, branch: Option<&str>) -> Result<Value, String> {
    eprintln!("[DEBUG] Fetching Tree for: {} (ref: {:?})", link, branch);
    let (owner, repo) = parse_github_url(link)?;

    // Use the specified branch or HEAD if not provided
    let target_ref = branch.unwrap_or("HEAD");

    // Use GitHub API to get the tree structure recursively
    let api_url = format!("https://api.github.com/repos/{}/{}/git/trees/{}?recursive=1", owner, repo, target_ref);

    let client = reqwest::blocking::Client::new();
    let resp = client.get(&api_url)
        .header("User-Agent", "Rust-MCP")
        .send()
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        return Err(format!("Failed to fetch Tree (Status: {}). Check if repo/branch is valid.", resp.status()));
    }

    let json: Value = resp.json().map_err(|e| e.to_string())?;

    let tree_items = json["tree"].as_array().ok_or("Invalid tree response")?;

    let mut file_list: Vec<String> = Vec::new();

    // Format output similar to 'ls -F' command
    for item in tree_items {
        let path = item["path"].as_str().unwrap_or("");
        let type_ = item["type"].as_str().unwrap_or("");

        if type_ == "tree" {
            file_list.push(format!("{}/", path));
        } else {
            file_list.push(path.to_string());
        }
    }

    // Limit the number of files returned to prevent overload
    let total_files = file_list.len();
    if total_files > 1000 {
        file_list.truncate(1000);
        file_list.push(format!("... (remaining {} files hidden)", total_files - 1000));
    }

    Ok(json!({
        "repository": link,
        "ref": target_ref,
        "total_count": total_files,
        "files": file_list
    }))
}

/// Gets the content of a file from a GitHub repository
fn get_file_content(link: &str, file_path: &str, branch: Option<&str>) -> Result<Value, String> {
    eprintln!("[DEBUG] Reading file: {} @ {}", file_path, link);
    let (owner, repo) = parse_github_url(link)?;

    // Use the specified branch or HEAD if not provided
    let target_ref = branch.unwrap_or("HEAD");

    // Remove leading '/' from file path
    let clean_path = file_path.trim_start_matches('/');

    // Use GitHub API to fetch the file content
    let api_url = format!("https://api.github.com/repos/{}/{}/contents/{}?ref={}", owner, repo, clean_path, target_ref);

    let client = reqwest::blocking::Client::new();
    let resp = client.get(&api_url)
        .header("User-Agent", "Rust-MCP")
        // Accept header to get raw text
        .header("Accept", "application/vnd.github.raw")
        .send()
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        return Err(format!("Failed to read file '{}'. Status: {}. Make sure the path is correct and not a folder.", clean_path, resp.status()));
    }

    let content = resp.text().map_err(|e| e.to_string())?;

    // Limit content size if too large
    let max_chars = 30_000;
    let (truncated_content, is_truncated) = if content.len() > max_chars {
        (format!("{}... \n\n[WARNING: File content truncated by MCP because it exceeds 30KB]", &content[..max_chars]), true)
    } else {
        (content, false)
    };

    Ok(json!({
        "repository": link,
        "path": clean_path,
        "ref": target_ref,
        "is_truncated": is_truncated,
        "content": truncated_content
    }))
}

fn main() {
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    // Main loop to read requests from stdin
    for line in stdin.lock().lines() {
        let input = match line {
            Ok(s) => s,
            Err(_) => break,
        };

        if input.trim().is_empty() { continue; }

        // Parse JSON-RPC request
        let req: JsonRpcRequest = match serde_json::from_str(&input) {
            Ok(val) => val,
            Err(e) => {
                eprintln!("[ERROR] Invalid JSON: {} | Input: {}", e, input);
                continue;
            }
        };

        // Handle notifications (requests without ID)
        if req.id.is_none() {
            if req.method == "notifications/initialized" {
                eprintln!("[INFO] Client initialized successfully.");
            }
            continue;
        }

        // Handle requests with ID (must be responded to)
        let response = match req.method.as_str() {
            // Response for client initialization
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": req.id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "rust-git-mcp", "version": "0.1.0" }
                }
            }),

            // List of available tools
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": req.id,
                "result": {
                    "tools": [
                        {
                            "name": "get_tags",
                            "description": "Call this tool BEFORE writing any dependency in Cargo.toml/package.json. Returns the latest versions. Use 'limit: 5' to avoid fetching old tags.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "url": { "type": "string" },
                                    "limit": { "type": "integer", "description": "Number of latest tags to return. Default returns ALL (avoid this for large repos)." }
                                },
                                "required": ["url"]
                            }
                        },
                        {
                            "name": "get_changelog",
                            "description": "Analyze commit messages between versions to identify breaking changes, deprecated features, or migration guides.",
                            "inputSchema": { "type": "object", "properties": { "url": { "type": "string" }, "start_tag": { "type": "string" }, "end_tag": { "type": "string" } }, "required": ["url", "start_tag", "end_tag"] }
                        },
                        {
                            "name": "get_readme",
                            "description": "Read the README to find installation instructions and basic usage examples that are compatible with the fetched version.",
                            "inputSchema": { "type": "object", "properties": { "url": { "type": "string" } }, "required": ["url"] }
                        },
                        {
                            "name": "get_file_tree",
                            "description": "Explore the repository structure. Look for 'examples/' or 'tests/' folders to find up-to-date code patterns.",
                            "inputSchema": { "type": "object", "properties": { "url": { "type": "string" }, "branch": { "type": "string" } }, "required": ["url"] }
                        },
                        {
                            "name": "get_file_content",
                            "description": "Read content of source files (especially in 'examples/'). Use this to verify API syntax and ensure the code you write matches the library version.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "url": { "type": "string", "description": "Repository URL" },
                                    "path": { "type": "string", "description": "Path to the file (e.g., 'src/main.cpp' or 'module.prop')" },
                                    "branch": { "type": "string", "description": "Branch name or Tag (e.g., 'v1.0.0'). Defaults to HEAD/main." }
                                },
                                "required": ["url", "path"]
                            }
                        }
                    ]
                }
            }),

            // Execute tool calls
            "tools/call" => {
                let args = &req.params["arguments"];
                let name = req.params["name"].as_str().unwrap_or("");

                let result_content = match name {
                    "get_tags" => {
                        let url = args["url"].as_str().unwrap_or("");
                        let limit = args["limit"].as_u64().map(|v| v as usize);
                        get_tags(url, limit)
                    },
                    "get_changelog" => get_changelog(args["url"].as_str().unwrap_or(""), args["start_tag"].as_str().unwrap_or(""), args["end_tag"].as_str().unwrap_or("")),
                    "get_readme" => get_readme(args["url"].as_str().unwrap_or("")),
                    "get_file_tree" => get_file_tree(args["url"].as_str().unwrap_or(""), args["branch"].as_str()),
                    "get_file_content" => {
                        let url = args["url"].as_str().unwrap_or("");
                        let path = args["path"].as_str().unwrap_or("");
                        let branch = args["branch"].as_str();
                        get_file_content(url, path, branch)
                    },
                    _ => Err(format!("Tool '{}' not found", name))
                };

                match result_content {
                    Ok(data) => json!({ "jsonrpc": "2.0", "id": req.id, "result": { "content": [{ "type": "text", "text": data.to_string() }] } }),
                    Err(e) => json!({ "jsonrpc": "2.0", "id": req.id, "result": { "isError": true, "content": [{ "type": "text", "text": e }] } })
                }
            },
            // Fallback response for other methods
            _ => json!({ "jsonrpc": "2.0", "id": req.id, "result": {} })
        };

        // Send response to stdout and ensure buffer is flushed
        let output_str = response.to_string();
        println!("{}", output_str);
        stdout.flush().unwrap();
    }
}
