//! `siptest call` / `siptest status` — plain HTTP clients against a running
//! daemon, never a second implementation of the call flow (there is exactly
//! one: the daemon's).

use std::process::ExitCode;

use serde_json::Value;

use crate::cli::{Cli, Commands};

pub fn run(cli: &Cli, command: &Commands) -> ExitCode {
    let base = match api_base(cli) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    // No client-side timeout. `POST /calls?wait=true` blocks for the whole
    // call — ring time plus the call's duration — which is routinely longer
    // than reqwest's 30s default, and that default turned a call that rang,
    // was answered and completed into `error sending request` on the CLI
    // while the daemon carried on regardless. The call is bounded by the
    // daemon's own ring timeout and duration; the client has nothing better
    // to bound it with.
    let client = match reqwest::blocking::Client::builder().timeout(None).build() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("could not build the HTTP client: {e}");
            return ExitCode::FAILURE;
        }
    };

    match command {
        Commands::Call {
            destination,
            duration_secs,
            codec,
            ..
        } => call(
            &client,
            &base,
            destination,
            *duration_secs,
            codec.as_deref(),
        ),
        Commands::Status => status(&client, &base),
    }
}

fn api_base(cli: &Cli) -> Result<String, String> {
    let config = crate::config::load(&cli.config)
        .map_err(|e| format!("failed to load {}: {e}", cli.config.display()))?;
    Ok(format!("http://{}", config.api.bind))
}

fn call(
    client: &reqwest::blocking::Client,
    base: &str,
    destination: &str,
    duration_secs: Option<u64>,
    codec: Option<&str>,
) -> ExitCode {
    let mut body = serde_json::json!({"destination": destination});
    if let Some(d) = duration_secs {
        body["duration_secs"] = serde_json::json!(d);
    }
    if let Some(c) = codec {
        body["codec"] = serde_json::json!(c);
    }
    let resp = match client
        .post(format!("{base}/calls?wait=true"))
        .json(&body)
        .send()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("request to siptest daemon failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let status = resp.status();
    let value: Value = match resp.json() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("failed to parse daemon response: {e}");
            return ExitCode::FAILURE;
        }
    };

    if !status.is_success() {
        eprintln!(
            "call failed: {} ({})",
            value
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown"),
            value.get("detail").and_then(|v| v.as_str()).unwrap_or("")
        );
        return ExitCode::FAILURE;
    }

    if let Some(text) = value.get("report_text").and_then(|v| v.as_str()) {
        println!("{text}");
    }
    let success = value
        .get("report")
        .and_then(|r| r.get("success"))
        .and_then(|s| s.as_bool())
        .unwrap_or(false);

    if success {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn status(client: &reqwest::blocking::Client, base: &str) -> ExitCode {
    match client
        .get(format!("{base}/status"))
        .send()
        .and_then(|r| r.json::<Value>())
    {
        Ok(v) => {
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("request to siptest daemon failed: {e}");
            ExitCode::FAILURE
        }
    }
}
