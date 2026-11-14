//! Native 2.1.322 TLS and proxy contracts against disposable loopback peers.
//! Each case runs in a process because TLS roots are memoized from environment.

use lingxi_core::host::McpHeaders;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const CASES: &[&str] = &[
    "tls-untrusted",
    "tls-extra-ca",
    "tls-ssl-cert-file",
    "tls-ssl-cert-dir",
    "tls-wrong-host",
    "tls-mtls-missing",
    "tls-mtls",
    "tls-mtls-encrypted",
    "tls-mtls-wrong-passphrase",
    "tls-mtls-wrong-pair",
    "proxy-forward",
    "proxy-basic",
    "proxy-bypass",
    "proxy-port-mismatch",
    "proxy-connect",
    "proxy-connect-basic",
    "sse-tls-extra-ca",
    "sse-proxy-forward",
];

struct PeerProcess(Child);
impl Drop for PeerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn native_322_network_tls_mtls_and_proxy_matrix() {
    for case in CASES {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "native_322_network_case_process", "--nocapture"])
            .env("MCP_NETWORK_CASE_322", case)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{case}:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[tokio::test]
async fn native_322_network_case_process() {
    let Ok(case) = std::env::var("MCP_NETWORK_CASE_322") else {
        return;
    };
    assert!(CASES.contains(&case.as_str()));
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let temp = tempfile::tempdir().unwrap();
    let output = std::env::var_os("MCP_NETWORK_RECEIPTS_322")
        .map(|base| PathBuf::from(base).join(&case))
        .unwrap_or_else(|| temp.path().to_path_buf());
    std::fs::create_dir_all(&output).unwrap();
    let mut peer = PeerProcess(
        Command::new("python3")
            .arg(workspace.join("scripts/tests/mcp_network_gap_322_live.py"))
            .args(["--serve", "--scenario", &case, "--output"])
            .arg(&output)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    BufReader::new(peer.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let config: Value = serde_json::from_str(&line).expect("network peer must start");
    for key in config["clear_env"].as_array().unwrap() {
        std::env::remove_var(key.as_str().unwrap());
    }
    for (key, value) in config["env"].as_object().unwrap() {
        std::env::set_var(key, value.as_str().unwrap());
    }
    let url = config["url"].as_str().unwrap();
    let expected_success = config["expected_success"].as_bool().unwrap();
    let connection = if config["transport"] == "sse" {
        platform_common::connect_sse(
            url,
            None,
            &McpHeaders::new(),
            platform_common::SseEndpointMode::EndpointEvent,
        )
        .await
        .map_err(|error| error.to_string())
    } else {
        platform_common::connect_http(url, None, &McpHeaders::new(), Some(Duration::from_secs(3)))
            .await
            .map_err(|error| error.to_string())
    };
    let connection = match connection {
        Ok(connection) => Some(connection),
        Err(error) => {
            // Native rejects this mismatched certificate/key before sending
            // MCP bytes. Rustls can detect the same mismatch while building
            // the client, before there is a connection on which to call ping.
            assert_eq!(case, "tls-mtls-wrong-pair", "{case}: {error}");
            assert!(!expected_success);
            assert!(error.to_string().contains("builder error"), "{error}");
            std::fs::write(output.join("client-construction-error.txt"), &error).unwrap();
            None
        }
    };
    // Ordinary/provider HTTP retains its original SSL_CERT_* behavior while
    // MCP ignores those paths. Both run concurrently under one environment;
    // no temporary environment changes or shared root cache are involved.
    let ordinary_tls = async {
        if matches!(case.as_str(), "tls-ssl-cert-file" | "tls-ssl-cert-dir") {
            let client = http_client::configure_client_tls(
                lingxi_llm_client::transport::http_backend::Client::builder().no_proxy(),
            )
            .build()
            .unwrap();
            let response = tokio::time::timeout(
                Duration::from_secs(5),
                client
                    .post(url)
                    .json(&json!({"jsonrpc":"2.0","id":"ordinary-http-proof","method":"ping"}))
                    .send(),
            )
            .await
            .expect("ordinary TLS must settle")
            .expect("ordinary HTTP must retain SSL_CERT_* trust");
            let body: Value = response.json().await.unwrap();
            assert_eq!(body["result"]["echo"], "WIRE_NETWORK_322_OK");
            std::fs::write(output.join("ordinary-http-proof.json"), body.to_string()).unwrap();
        }
    };
    let (result, ()) = tokio::join!(
        async {
            if let Some(connection) = &connection {
                Some(
                    tokio::time::timeout(
                        Duration::from_secs(5),
                        connection.call::<_, Value>("ping", json!({})),
                    )
                    .await
                    .expect("the network case must settle promptly"),
                )
            } else {
                None
            }
        },
        ordinary_tls
    );
    if expected_success {
        assert_eq!(
            result.unwrap().unwrap(),
            json!({"echo":"WIRE_NETWORK_322_OK"})
        );
    } else {
        assert!(
            result.is_none_or(|result| result.is_err()),
            "{case}: untrusted TLS must fail"
        );
    }
    if let Some(connection) = connection {
        connection.close();
    }
    peer.0.stdin.take().unwrap().write_all(b"stop\n").unwrap();
    assert!(peer.0.wait().unwrap().success());
    let report: Value =
        serde_json::from_slice(&std::fs::read(output.join("report.json")).unwrap()).unwrap();
    let requests = report["peer_requests"].as_array().unwrap();
    let proxy = report["proxy_requests"].as_array().unwrap();
    if expected_success {
        assert!(requests.iter().any(|row| row["body"]["method"] == "ping"));
    } else {
        assert!(
            requests
                .iter()
                .all(|row| row["body"]["id"] == "ordinary-http-proof"),
            "TLS failure must precede MCP bytes"
        );
    }
    if case == "proxy-bypass" {
        assert!(proxy.is_empty());
    } else if case.contains("proxy") {
        assert!(!proxy.is_empty());
        assert!(proxy.iter().all(|row| {
            if case.contains("connect") {
                row["method"] == "CONNECT"
            } else {
                row["target"].as_str().unwrap().starts_with("http://")
            }
        }));
    }
    if case.ends_with("-basic") {
        assert!(proxy.iter().any(
            |row| row["headers"].as_array().unwrap().iter().any(|header| {
                header[0]
                    .as_str()
                    .unwrap()
                    .eq_ignore_ascii_case("proxy-authorization")
                    && header[1] == "Basic cHJvYmU6cGFzcw=="
            })
        ));
        assert!(requests
            .iter()
            .all(
                |row| !row["headers"].as_array().unwrap().iter().any(|header| {
                    header[0]
                        .as_str()
                        .unwrap()
                        .eq_ignore_ascii_case("proxy-authorization")
                })
            ));
    }
    if case.contains("mtls") && expected_success {
        assert!(requests
            .iter()
            .any(|row| !row["tls_client_certificate"].is_null()));
    }
}
