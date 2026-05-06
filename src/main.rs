//! agent-pay CLI — pay.sh client for AI agents
//!
//! Usage:
//!   agent-pay <fqn> [body]              Call a provider by FQN (e.g. quicknode/rpc)
//!   agent-pay <url>                      Call a URL directly
//!   agent-pay search <query>             Search the pay.sh catalog
//!   agent-pay list                       List all providers
//!   agent-pay info <fqn>                 Show provider details
//!
//! Environment:
//!   OUTLAYER_API_KEY  Required for signing payments

use agent_pay::{CustodyClient, PayClient};
use std::io::{self, Read};

// ─── Catalog types ───────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct Catalog {
    providers: Vec<Provider>,
}

#[derive(serde::Deserialize, Clone)]
struct Provider {
    fqn: String,
    title: String,
    description: String,
    #[serde(default)]
    category: String,
    service_url: String,
    endpoint_count: u32,
    has_free_tier: bool,
    min_price_usd: f64,
    max_price_usd: f64,
}

// ─── Main ────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.is_empty() {
        print_usage();
        std::process::exit(1);
    }

    let cmd = &args[0];

    match cmd.as_str() {
        "search" => {
            let query = args.get(1).expect("Usage: agent-pay search <query>");
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(cmd_search(query));
        }
        "list" => {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(cmd_list());
        }
        "info" => {
            let fqn = args.get(1).expect("Usage: agent-pay info <fqn>");
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(cmd_info(fqn));
        }
        "help" | "--help" | "-h" => {
            print_usage();
        }
        _ => {
            // It's either a FQN (contains '/') or a URL
            let target = &args[0];
            let body = args.get(1).cloned().or_else(|| read_stdin());
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(cmd_call(target, body.as_deref()));
        }
    }
}

fn print_usage() {
    eprintln!("agent-pay — pay.sh client for AI agents");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  agent-pay <fqn> [body]         Call a provider (e.g. quicknode/rpc)");
    eprintln!("  agent-pay <url>                 Call a URL directly");
    eprintln!("  agent-pay search <query>        Search the pay.sh catalog");
    eprintln!("  agent-pay list                  List all providers");
    eprintln!("  agent-pay info <fqn>            Show provider details");
    eprintln!();
    eprintln!("Environment:");
    eprintln!("  OUTLAYER_API_KEY   Required for signing payments");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  agent-pay search \"stock price\"");
    eprintln!("  agent-pay info quicknode/rpc");
    eprintln!("  agent-pay quicknode/rpc '{{\"method\":\"getHealth\"}}'");
    eprintln!("  echo '{{\"image\":\"...\"}}' | agent-pay solana-foundation/alibaba/ocr-api");
    eprintln!("  agent-pay https://payment-debugger.vercel.app/mpp/quote/AAPL");
}

fn read_stdin() -> Option<String> {
    if atty::is(atty::Stream::Stdin) {
        return None;
    }
    let mut buf = String::new();
    io::stdin().read_to_string(&mut buf).ok()?;
    if buf.is_empty() { None } else { Some(buf) }
}

// ─── Commands ────────────────────────────────────────────────────────

async fn fetch_catalog() -> Vec<Provider> {
    let resp = reqwest::get("https://pay.sh/api/catalog")
        .await
        .expect("Failed to fetch catalog");

    let catalog: Catalog = resp.json().await.expect("Failed to parse catalog");
    catalog.providers
}

async fn cmd_search(query: &str) {
    let providers = fetch_catalog().await;
    let query_lower = query.to_lowercase();

    let matches: Vec<&Provider> = providers
        .iter()
        .filter(|p| {
            let haystack = format!("{} {} {} {}",
                p.fqn, p.title, p.description, p.category);
            haystack.to_lowercase().contains(&query_lower)
        })
        .collect();

    if matches.is_empty() {
        eprintln!("No providers found for '{}'", query);
        return;
    }

    for p in &matches {
        let price = format_price(p.min_price_usd, p.max_price_usd);
        eprintln!("{:<40} {:<12} {:>8}  {}",
            p.fqn, p.category, price, p.title
        );
    }
    eprintln!();
    eprintln!("{} provider(s) found", matches.len());
}

async fn cmd_list() {
    let providers = fetch_catalog().await;

    eprintln!("{:<40} {:<12} {:>8}  {}",
        "FQN", "CATEGORY", "PRICE", "TITLE");
    eprintln!("{}", "─".repeat(100));

    for p in &providers {
        let price = format_price(p.min_price_usd, p.max_price_usd);
        eprintln!("{:<40} {:<12} {:>8}  {}",
            p.fqn, p.category, price, p.title
        );
    }
    eprintln!();
    eprintln!("{} providers", providers.len());
}

async fn cmd_info(fqn: &str) {
    let providers = fetch_catalog().await;
    let provider = providers.iter().find(|p| p.fqn == fqn);

    match provider {
        Some(p) => {
            eprintln!("Title:    {}", p.title);
            eprintln!("FQN:      {}", p.fqn);
            eprintln!("Category: {}", p.category);
            eprintln!("URL:      {}", p.service_url);
            eprintln!("Endpoints: {}", p.endpoint_count);
            eprintln!("Free tier: {}", if p.has_free_tier { "yes" } else { "no" });
            eprintln!("Price:    {}", format_price(p.min_price_usd, p.max_price_usd));
            eprintln!();
            eprintln!("{}", p.description);
        }
        None => {
            eprintln!("Provider '{}' not found. Use 'agent-pay search' to find providers.", fqn);
            std::process::exit(1);
        }
    }
}

async fn cmd_call(target: &str, body: Option<&str>) {
    let api_key = std::env::var("OUTLAYER_API_KEY")
        .unwrap_or_else(|_| {
            eprintln!("Error: OUTLAYER_API_KEY not set");
            eprintln!("  export OUTLAYER_API_KEY=wk_...");
            std::process::exit(1);
        });

    // Resolve FQN to service_url, or use target as-is (URL)
    let url = if target.contains("://") {
        target.to_string()
    } else if target.contains('/') {
        // Looks like a FQN — resolve via catalog
        resolve_fqn(target).await
    } else {
        // Single word — try to find a matching provider
        resolve_fuzzy(target).await
    };

    eprintln!("[agent-pay] {}", url);

    // Set up PayClient (uses mainnet=false internally)
    let custody = CustodyClient::from_api_key(&api_key);
    let mut client = PayClient::new(custody);

    // Determine method: POST if body, GET otherwise
    let result = if body.is_some() {
        let json_body: Option<serde_json::Value> = body
            .and_then(|b| serde_json::from_str(b).ok());
        client.post(&url, json_body, vec![]).await
    } else {
        client.get(&url).await
    };

    match result {
        Ok(resp) => {
            // Print response body to stdout (pipe-friendly)
            print!("{}", resp.body);
        }
        Err(e) => {
            eprintln!("[agent-pay] Error: {:?}", e);
            std::process::exit(1);
        }
    }
}

// ─── FQN Resolution ──────────────────────────────────────────────────

async fn resolve_fqn(fqn: &str) -> String {
    let providers = fetch_catalog().await;
    let provider = providers.iter().find(|p| p.fqn == fqn);

    match provider {
        Some(p) => {
            eprintln!("[agent-pay] Resolved {} → {}", fqn, p.service_url);
            p.service_url.clone()
        }
        None => {
            eprintln!("[agent-pay] FQN '{}' not found in catalog", fqn);
            eprintln!("[agent-pay] Use 'agent-pay search' to find providers");
            std::process::exit(1);
        }
    }
}

async fn resolve_fuzzy(query: &str) -> String {
    let providers = fetch_catalog().await;
    let query_lower = query.to_lowercase();

    // Try exact match on FQN first
    if let Some(p) = providers.iter().find(|p| p.fqn == query) {
        eprintln!("[agent-pay] Resolved {} → {}", query, p.service_url);
        return p.service_url.clone();
    }

    // Try partial match on title/FQN
    let matches: Vec<&Provider> = providers
        .iter()
        .filter(|p| {
            p.title.to_lowercase().contains(&query_lower)
                || p.fqn.to_lowercase().contains(&query_lower)
        })
        .collect();

    match matches.len() {
        0 => {
            eprintln!("[agent-pay] No provider matching '{}'", query);
            std::process::exit(1);
        }
        1 => {
            let p = matches[0];
            eprintln!("[agent-pay] Resolved '{}' → {} ({})", query, p.fqn, p.service_url);
            p.service_url.clone()
        }
        _ => {
            eprintln!("[agent-pay] Multiple providers match '{}':", query);
            for p in &matches {
                eprintln!("  {} — {}", p.fqn, p.title);
            }
            eprintln!("[agent-pay] Be more specific with the full FQN");
            std::process::exit(1);
        }
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────

fn format_price(min: f64, max: f64) -> String {
    if min == 0.0 && max == 0.0 {
        "free".to_string()
    } else if min == max {
        format!("${:.3}", min)
    } else {
        format!("${:.3}-${:.3}", min, max)
    }
}
