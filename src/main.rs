use std::io::{self, BufRead, Write};
use std::env;
use serde::Deserialize;
use serde_json::{json, Value};
use std::process::Command;
use regex::Regex;
use semver::Version;

/// Represents a JSON-RPC 2.0 request structure
/// Used for communication between the MCP client and this server
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

/// Parses a GitHub URL to extract owner and repository name
///
/// # Arguments
/// * `url` - A string slice containing the GitHub repository URL
///
/// # Returns
/// * `Result<(String, String), String>` - A tuple containing (owner, repo) or an error message
fn parse_github_url(url: &str) -> Result<(String, String), String> {
    let re = Regex::new(r"github\.com/([^/]+)/([^/]+?)(?:\.git)?$").map_err(|e| e.to_string())?;
    let caps = re.captures(url).ok_or("Invalid GitHub URL")?;
    Ok((caps[1].to_string(), caps[2].to_string()))
}

/// Builds an HTTP client with appropriate headers and authentication
///
/// This function creates a reqwest client with:
/// - Custom User-Agent header
/// - Authorization header if GITHUB_TOKEN environment variable is set
///
/// # Returns
/// * `Result<reqwest::blocking::Client, String>` - An HTTP client instance or an error message
fn build_client() -> Result<reqwest::blocking::Client, String> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("User-Agent", reqwest::header::HeaderValue::from_static("Rust-MCP-Server"));

    // Check for GITHUB_TOKEN environment variable and add authorization header if present
    if let Ok(token) = env::var("GITHUB_TOKEN") {
        eprintln!("[DEBUG] Using GITHUB_TOKEN for authentication.");
        // Clean the token to remove any leading/trailing whitespace or newlines that might cause issues
        let clean_token = token.trim().to_string();
        let auth_value = format!("Bearer {}", clean_token);

        // Safely create the header value, handling any invalid characters
        match reqwest::header::HeaderValue::from_str(&auth_value) {
            Ok(mut auth_header) => {
                auth_header.set_sensitive(true);
                headers.insert("Authorization", auth_header);
            },
            Err(e) => {
                eprintln!("[WARNING] Invalid token format for header: {}", e);
                // Continue without authentication rather than failing completely
            }
        }
    } else {
        eprintln!("[DEBUG] No GITHUB_TOKEN found. Using unauthenticated requests (Rate Limit: 60/hr).");
    }

    reqwest::blocking::Client::builder()
        .default_headers(headers)
        .timeout(std::time::Duration::from_secs(30)) // Add timeout to prevent hanging
        .build()
        .map_err(|e| e.to_string())
}

/// Retrieves Git tags from a repository with semantic version sorting
///
/// This function uses the git command-line tool to fetch remote tags and sorts them
/// using semantic versioning rules, with the newest versions first.
///
/// # Arguments
/// * `link` - A string slice containing the Git repository URL
/// * `limit` - An optional usize specifying the maximum number of tags to return
///
/// # Returns
/// * `Result<Value, String>` - A JSON object containing repository info and tags, or an error message
fn get_tags(link: &str, limit: Option<usize>) -> Result<Value, String> {
    eprintln!("[DEBUG] Fetching tags for: {} (limit: {:?})", link, limit);

    let output = Command::new("git")
        .args(["ls-remote", "--tags", "--refs", link])
        .output()
        .map_err(|e| e.to_string())?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }

    let raw_output = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;

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

    // Sort tags using semantic versioning, with newest versions first
    tags.sort_by(|a, b| {
        let ver_a = Version::parse(a.trim_start_matches('v'));
        let ver_b = Version::parse(b.trim_start_matches('v'));
        match (ver_a, ver_b) {
            (Ok(va), Ok(vb)) => vb.cmp(&va), // Descending order
            (Ok(_), Err(_)) => std::cmp::Ordering::Less,
            (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
            (Err(_), Err(_)) => b.cmp(a),
        }
    });

    if let Some(n) = limit {
        if n < tags.len() { tags.truncate(n); }
    }

    Ok(json!({
        "repository": link,
        "count": tags.len(),
        "limit_applied": limit,
        "tags": tags
    }))
}

/// Fetches the changelog between two Git tags using GitHub's compare API
///
/// This function retrieves commit history between two versions and formats
/// the commit messages into a readable changelog format.
///
/// # Arguments
/// * `link` - A string slice containing the GitHub repository URL
/// * `v1` - A string slice representing the starting version tag
/// * `v2` - A string slice representing the ending version tag
///
/// # Returns
/// * `Result<Value, String>` - A JSON object containing repository info and changelog, or an error message
fn get_changelog(link: &str, v1: &str, v2: &str) -> Result<Value, String> {
    eprintln!("[DEBUG] Fetching changelog: {}...{}", v1, v2);
    let (owner, repo) = parse_github_url(link)?;
    let api_url = format!("https://api.github.com/repos/{}/{}/compare/{}...{}", owner, repo, v1, v2);

    let client = build_client()?;
    let resp = client.get(&api_url).send().map_err(|e| e.to_string())?;

    if !resp.status().is_success() { return Err(format!("API Error: {}", resp.status())); }

    let json: Value = resp.json().map_err(|e| e.to_string())?;
    let commits = json["commits"].as_array().ok_or("No commits found")?;
    let summaries: Vec<String> = commits.iter().map(|c| {
        let msg = c["commit"]["message"].as_str().unwrap_or("").lines().next().unwrap_or("");
        let date = c["commit"]["author"]["date"].as_str().unwrap_or("").split('T').next().unwrap_or("");
        format!("[{}] {}", date, msg)
    }).collect();

    Ok(json!({ "repository": link, "from": v1, "to": v2, "changes": summaries }))
}

/// Fetches the README file content from a GitHub repository
///
/// This function retrieves the README file from the root of the repository
/// using GitHub's raw content API endpoint.
///
/// # Arguments
/// * `link` - A string slice containing the GitHub repository URL
///
/// # Returns
/// * `Result<Value, String>` - A JSON object containing repository info and README content, or an error message
fn get_readme(link: &str) -> Result<Value, String> {
    eprintln!("[DEBUG] Fetching README: {}", link);
    let (owner, repo) = parse_github_url(link)?;
    let api_url = format!("https://api.github.com/repos/{}/{}/readme", owner, repo);

    let client = build_client()?;
    let resp = client.get(&api_url)
        .header("Accept", "application/vnd.github.raw")
        .send()
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() { return Err(format!("Error: {}", resp.status())); }

    let content = resp.text().map_err(|e| e.to_string())?;
    let truncated = if content.len() > 20000 { format!("{}... [TRUNCATED]", &content[..20000]) } else { content };

    Ok(json!({ "repository": link, "type": "readme", "content": truncated }))
}

/// Fetches the file tree structure of a GitHub repository
///
/// This function retrieves the entire file structure of a repository using
/// GitHub's Git trees API endpoint, with an option to specify a branch.
///
/// # Arguments
/// * `link` - A string slice containing the GitHub repository URL
/// * `branch` - An optional string slice specifying the branch name (defaults to HEAD)
///
/// # Returns
/// * `Result<Value, String>` - A JSON object containing repository info and file tree, or an error message
fn get_file_tree(link: &str, branch: Option<&str>) -> Result<Value, String> {
    eprintln!("[DEBUG] Fetching Tree: {}", link);
    let (owner, repo) = parse_github_url(link)?;
    let target_ref = branch.unwrap_or("HEAD");
    let api_url = format!("https://api.github.com/repos/{}/{}/git/trees/{}?recursive=1", owner, repo, target_ref);

    let client = build_client()?;
    let resp = client.get(&api_url).send().map_err(|e| e.to_string())?;

    if !resp.status().is_success() { return Err(format!("Error: {}", resp.status())); }

    let json: Value = resp.json().map_err(|e| e.to_string())?;
    let tree_items = json["tree"].as_array().ok_or("Invalid tree response")?;

    let mut file_list: Vec<String> = Vec::new();
    for item in tree_items {
        let path = item["path"].as_str().unwrap_or("");
        let type_ = item["type"].as_str().unwrap_or("");
        if type_ == "tree" { file_list.push(format!("{}/", path)); } else { file_list.push(path.to_string()); }
    }

    // Limit output to prevent overwhelming the client
    if file_list.len() > 1000 {
        file_list.truncate(1000);
        file_list.push("... [TRUNCATED]".to_string());
    }

    Ok(json!({ "repository": link, "ref": target_ref, "files": file_list }))
}

/// Fetches the content of a specific file from a GitHub repository
///
/// This function retrieves the content of a file at a specific path in the repository
/// using GitHub's contents API endpoint, with an option to specify a branch.
///
/// # Arguments
/// * `link` - A string slice containing the GitHub repository URL
/// * `file_path` - A string slice specifying the path to the file in the repository
/// * `branch` - An optional string slice specifying the branch name (defaults to HEAD)
///
/// # Returns
/// * `Result<Value, String>` - A JSON object containing repository info and file content, or an error message
fn get_file_content(link: &str, file_path: &str, branch: Option<&str>) -> Result<Value, String> {
    eprintln!("[DEBUG] Reading file: {} @ {}", file_path, link);
    let (owner, repo) = parse_github_url(link)?;
    let target_ref = branch.unwrap_or("HEAD");
    let clean_path = file_path.trim_start_matches('/');
    let api_url = format!("https://api.github.com/repos/{}/{}/contents/{}?ref={}", owner, repo, clean_path, target_ref);

    let client = build_client()?;
    let resp = client.get(&api_url)
        .header("Accept", "application/vnd.github.raw")
        .send()
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() { return Err(format!("Gagal membaca file: {}", resp.status())); }

    let content = resp.text().map_err(|e| e.to_string())?;
    let max_chars = 30_000;
    let (truncated_content, is_truncated) = if content.len() > max_chars {
        (format!("{}... \n[TRUNCATED]", &content[..max_chars]), true)
    } else {
        (content, false)
    };

    Ok(json!({ "repository": link, "path": clean_path, "ref": target_ref, "is_truncated": is_truncated, "content": truncated_content }))
}

/// Searches for code within a GitHub repository using GitHub's code search API
///
/// This function queries GitHub's code search functionality to find files containing
/// specific text or code patterns within the specified repository.
///
/// # Arguments
/// * `link` - A string slice containing the GitHub repository URL
/// * `query` - A string slice containing the search query
///
/// # Returns
/// * `Result<Value, String>` - A JSON object containing repository info and search results, or an error message
fn search_repository(link: &str, query: &str) -> Result<Value, String> {
    eprintln!("[DEBUG] Searching '{}' in {}", query, link);
    let (owner, repo) = parse_github_url(link)?;

    let q = format!("{} repo:{}/{}", query, owner, repo);
    let api_url = format!("https://api.github.com/search/code?q={}&per_page=10", urlencoding::encode(&q));

    let client = build_client()?;
    let resp = client.get(&api_url)
        .send()
        .map_err(|e: reqwest::Error| e.to_string())?;

    if !resp.status().is_success() {
        return Err(format!("Search API Error: {} (Search requires Auth & Valid Repo)", resp.status()));
    }

    let json: Value = resp.json().map_err(|e: reqwest::Error| e.to_string())?;
    let items = json["items"].as_array().ok_or("No items found in search response")?;

    let mut results: Vec<Value> = Vec::new();
    for item in items {
        let path = item["path"].as_str().unwrap_or("unknown");
        let url = item["html_url"].as_str().unwrap_or("");
        results.push(json!({
            "path": path,
            "url": url
        }));
    }

    Ok(json!({
        "repository": link,
        "query": query,
        "count_found": results.len(),
        "results": results
    }))
}

/// Main entry point for the Rust MCP (Model Context Protocol) server
///
/// This function implements the MCP server protocol by:
/// 1. Reading JSON-RPC requests from stdin
/// 2. Processing requests for initialization, tool listing, and tool execution
/// 3. Sending responses back to stdout
///
/// The server supports various tools for interacting with Git repositories,
/// including getting tags, changelogs, README files, file trees, file content,
/// and searching within repositories.
fn main() {
    // Set up panic hook to capture and log panic information before the program exits
    std::panic::set_hook(Box::new(|info| {
        let msg = match info.payload().downcast_ref::<&'static str>() {
            Some(s) => *s,
            None => match info.payload().downcast_ref::<String>() {
                Some(s) => &s[..],
                None => "Box<Any>",
            },
        };
        eprintln!("[FATAL CRASH] Location: {:?}, Error: {}", info.location(), msg);
    }));

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    // Process incoming JSON-RPC requests from stdin
    for line in stdin.lock().lines() {
        let input = match line {
            Ok(s) => s,
            Err(_) => break,
        };

        if input.trim().is_empty() { continue; }

        // Parse the JSON-RPC request
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

        // Process requests with ID and generate appropriate responses
        let response = match req.method.as_str() {
            // Initialize the MCP connection and return server capabilities
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": req.id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "rust-git-mcp", "version": "0.2.0" }
                }
            }),

            // Return the list of available tools
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
                        },
                        {
                            "name": "search_repository",
                            "description": "Search for code, functions, or text inside the repository using GitHub Search API.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "url": { "type": "string" },
                                    "query": { "type": "string", "description": "Text/Code to search (e.g., 'dependencies', 'fn main', 'struct Config')" }
                                },
                                "required": ["url", "query"]
                            }
                        }
                    ]
                }
            }),

            // Execute specific tools based on the request
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
                    "get_file_content" => get_file_content(args["url"].as_str().unwrap_or(""), args["path"].as_str().unwrap_or(""), args["branch"].as_str()),

                    "search_repository" => search_repository(args["url"].as_str().unwrap_or(""), args["query"].as_str().unwrap_or("")),

                    _ => Err(format!("Tool '{}' not found", name))
                };

                match result_content {
                    Ok(data) => json!({ "jsonrpc": "2.0", "id": req.id, "result": { "content": [{ "type": "text", "text": data.to_string() }] } }),
                    Err(e) => json!({ "jsonrpc": "2.0", "id": req.id, "result": { "isError": true, "content": [{ "type": "text", "text": e }] } })
                }
            },
            // Default response for unrecognized methods
            _ => json!({ "jsonrpc": "2.0", "id": req.id, "result": {} })
        };

        // Send the response back to the MCP client
        let output_str = response.to_string();
        println!("{}", output_str);
        stdout.flush().unwrap();
    }
}
