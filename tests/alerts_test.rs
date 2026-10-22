//! Alerts are sent to a local HTTP server that records the requests.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;
use zesty_backup::alerts::{Alerter, Event, EventKind};
use zesty_backup::config::AlertsConfig;
use zesty_backup::report::Report;

#[derive(Debug)]
struct Request {
    path: String,
    headers: HashMap<String, String>,
    body: String,
}

/// Start a server answering every request with `status`; returns its base URL and the requests.
fn server(status: u16) -> (String, mpsc::Receiver<Request>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            let mut headers = HashMap::new();
            loop {
                let mut h = String::new();
                reader.read_line(&mut h).unwrap();
                let h = h.trim_end();
                if h.is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                }
            }
            let len: usize = headers
                .get("content-length")
                .and_then(|l| l.parse().ok())
                .unwrap_or(0);
            let mut body = vec![0; len];
            reader.read_exact(&mut body).unwrap();
            let _ = write!(
                stream,
                "HTTP/1.1 {} X\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                status
            );
            let _ = tx.send(Request {
                path,
                headers,
                body: String::from_utf8_lossy(&body).to_string(),
            });
        }
    });
    (url, rx)
}

fn drain(rx: &mpsc::Receiver<Request>) -> Vec<Request> {
    let mut out = Vec::new();
    while let Ok(r) = rx.recv_timeout(Duration::from_millis(300)) {
        out.push(r);
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn all_channels(url: &str) -> AlertsConfig {
    AlertsConfig {
        healthchecks_url: Some(format!("{}/hc", url)),
        ntfy_url: Some(format!("{}/ntfy", url)),
        ntfy_token: Some("tk_secret".into()),
        webhook_url: Some(format!("{}/hook", url)),
        server_name: Some("web-1".into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn failure_goes_everywhere() {
    let (url, rx) = server(200);
    let alerter = Alerter::new(Some(&all_channels(&url)));
    alerter
        .notify(&Event::failure(
            EventKind::Upload,
            &anyhow::anyhow!("NoSuchBucket: gone"),
        ))
        .await;
    let reqs = drain(&rx);
    let paths: Vec<&str> = reqs.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(paths, vec!["/hc/fail", "/hook", "/ntfy"]);

    assert!(reqs[0].body.contains("NoSuchBucket"));
    let hook: serde_json::Value = serde_json::from_str(&reqs[1].body).unwrap();
    assert_eq!(hook["event"], "upload");
    assert_eq!(hook["success"], false);
    assert_eq!(hook["server"], "web-1");
    let ntfy = &reqs[2];
    assert_eq!(
        ntfy.headers["title"],
        "zesty-backup on web-1: upload FAILED"
    );
    assert_eq!(ntfy.headers["priority"], "high");
    assert_eq!(ntfy.headers["authorization"], "Bearer tk_secret");
    assert!(ntfy.body.contains("NoSuchBucket: gone"));
}

#[tokio::test]
async fn success_only_pings_unless_asked() {
    let (url, rx) = server(200);
    let alerter = Alerter::new(Some(&all_channels(&url)));
    alerter
        .notify(&Event::success(EventKind::Backup, "backup-x created"))
        .await;
    let paths: Vec<String> = drain(&rx).into_iter().map(|r| r.path).collect();
    assert_eq!(paths, vec!["/hc"]);

    let mut config = all_channels(&url);
    config.notify_success = Some(true);
    Alerter::new(Some(&config))
        .notify(&Event::success(EventKind::Backup, "backup-x created"))
        .await;
    let paths: Vec<String> = drain(&rx).into_iter().map(|r| r.path).collect();
    assert_eq!(paths, vec!["/hc", "/hook", "/ntfy"]);
}

#[tokio::test]
async fn only_new_backups_reset_the_dead_mans_switch() {
    let (url, rx) = server(200);
    let alerter = Alerter::new(Some(&all_channels(&url)));
    // "0 backups uploaded" or a paused clean must not look like proof of life.
    alerter
        .notify(&Event::success(EventKind::Upload, "0 backup(s) uploaded"))
        .await;
    alerter
        .notify(&Event::success(EventKind::Clean, "nothing deleted"))
        .await;
    assert!(drain(&rx).is_empty());
    // Their failures still raise the alarm.
    alerter
        .notify(&Event::failure(EventKind::Clean, &anyhow::anyhow!("x")))
        .await;
    assert!(drain(&rx).iter().any(|r| r.path == "/hc/fail"));
}

#[tokio::test]
async fn drills_do_not_count_as_backups() {
    let (url, rx) = server(200);
    let config = AlertsConfig {
        healthchecks_url: Some(format!("{}/hc", url)),
        ..Default::default()
    };
    let alerter = Alerter::new(Some(&config));

    let mut ok = Report::new();
    ok.pass("Extract", "fine");
    alerter
        .notify(&Event::from_drill(Some("b.tar.zst"), &ok))
        .await;
    assert!(
        drain(&rx).is_empty(),
        "a passing drill must not reset the backup heartbeat"
    );

    let mut bad = Report::new();
    bad.fail("Database check", "SELECT 1 → 0");
    alerter
        .notify(&Event::from_drill(Some("b.tar.zst"), &bad))
        .await;
    let reqs = drain(&rx);
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, "/hc/fail");
    assert!(reqs[0].body.contains("Database check"));

    // With a dedicated drill check, successes are pinged there.
    let config = AlertsConfig {
        healthchecks_url: Some(format!("{}/hc", url)),
        healthchecks_drill_url: Some(format!("{}/drill", url)),
        ..Default::default()
    };
    Alerter::new(Some(&config))
        .notify(&Event::from_drill(None, &ok))
        .await;
    let paths: Vec<String> = drain(&rx).into_iter().map(|r| r.path).collect();
    assert_eq!(paths, vec!["/drill"]);
}

#[tokio::test]
async fn delivery_errors_are_reported_not_raised() {
    let (url, rx) = server(500);
    let alerter = Alerter::new(Some(&all_channels(&url)));
    let event = Event::failure(EventKind::Clean, &anyhow::anyhow!("x"));
    let deliveries = alerter.send(&event).await;
    assert_eq!(deliveries.len(), 3);
    assert!(deliveries.iter().all(|d| d.result.is_err()));
    // notify() swallows the errors.
    alerter.notify(&event).await;
    drain(&rx);

    // Nothing configured: nothing sent, no error.
    assert!(!Alerter::new(None).is_configured());
    assert!(Alerter::new(None).send(&event).await.is_empty());
}
