//! `gbandit deploy` run as the real binary against a fake platform-api and
//! auth-service, in a git repo with a local remote. The fake answers in ways
//! the real platform rarely does on cue: refusals worth retrying, CRLF line
//! endings, characters split between network chunks, and event streams that
//! drop mid-run.

use std::collections::VecDeque;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Multipart, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use futures_util::StreamExt;
use serde_json::{Value, json};

const PROJECT: &str = "space-shooter";
const DEV_URL: &str = "https://dev-space-shooter.gbandit.test";

#[tokio::test]
async fn a_deploy_survives_a_refused_upload_and_a_dropped_stream_then_pushes_its_commit() {
    let repo = Repo::new();
    repo.write("game.js", "let score = 1;");
    let platform = FakePlatform::start(Script {
        title: "Old Title",
        refusals_before_accepting: 1,
        streams: vec![
            vec![
                sse(None, "snapshot", &snapshot("running", None, json!({}))).into_bytes(),
                sse(Some(1), "pipeline_event", &event("backend_build", "started", "debug", None, T1)).into_bytes(),
                // The ä is split between two chunks, and so is the CRLF after it.
                b"event: pipeline_event\r\ndata: {\"created_at\":\"2026-10-25T10:00:02Z\",\"event\":{\"stage\":\"backend_build\",\"kind\":\"log\",\"level\":\"info\",\"detail\":\"Built r\xC3".to_vec(),
                b"\xA4ksm\xC3\xB6rg\xC3\xA5s.wasm\"}}\r".to_vec(),
                b"\n\r\n".to_vec(),
            ],
            // The reconnect replays the log line from the last id it saw.
            vec![
                sse(None, "snapshot", &snapshot("running", None, json!({}))).into_bytes(),
                sse(None, "pipeline_event", &event("backend_build", "log", "info", Some("Built räksmörgås.wasm"), T2)).into_bytes(),
                sse(Some(2), "pipeline_event", &event("pipeline", "succeeded", "info", None, T3)).into_bytes(),
            ],
        ],
    })
    .await;

    let output = repo.gbandit(&platform.origin, &["deploy"]).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{}", describe(&output));

    assert_eq!(
        stdout.matches("Built räksmörgås.wasm").count(),
        1,
        "{stdout}"
    );
    for line in [
        "Updated project title to \"Space Shooter\".",
        "Auto-committing working tree...",
        "Deploy succeeded.",
        "Push succeeded.",
        &format!("Live at {DEV_URL}"),
    ] {
        assert!(stdout.contains(line), "missing {line:?} in\n{stdout}");
    }

    let seen = platform.seen();
    assert_eq!(seen.titles, ["Space Shooter"]);
    assert_eq!(seen.stream_since, [None, Some(1)]);
    let [refused, accepted] = &seen.uploads[..] else {
        panic!(
            "expected one refused and one accepted upload: {:?}",
            seen.uploads
        );
    };
    assert_eq!(refused.submission_id, accepted.submission_id);
    let head = repo.git(&["rev-parse", "HEAD"]);
    assert_eq!(accepted.commit_sha.as_deref(), Some(head.as_str()));
    assert_eq!(repo.remote_head(), head);
    assert_eq!(accepted.bundle[0], "gbandit.jsonc");
    assert!(
        accepted.bundle.contains(&"game.js".to_string()),
        "{:?}",
        accepted.bundle
    );
    assert_eq!(repo.git(&["status", "--porcelain"]), "");
}

#[tokio::test]
async fn a_failed_deploy_shows_the_failed_stage_log_and_takes_back_its_commit() {
    let repo = Repo::new();
    let before = repo.git(&["rev-parse", "HEAD"]);
    repo.write("game.js", "let score = ;");
    let platform = FakePlatform::start(Script {
        title: "Space Shooter",
        refusals_before_accepting: 0,
        streams: vec![
            vec![
                sse(None, "snapshot", &snapshot("running", None, json!({}))).into_bytes(),
                sse(
                    Some(1),
                    "pipeline_event",
                    &event("backend_build", "started", "debug", None, T1),
                )
                .into_bytes(),
                sse(
                    None,
                    "error",
                    &json!({
                        "type": "about:blank",
                        "title": "Service Unavailable",
                        "status": 503,
                        "detail": "The event journal is restarting.",
                        "retryable": true,
                    }),
                )
                .into_bytes(),
            ],
            // The failure happened while the CLI was away, so only the
            // snapshot tells it which stage failed and what it logged.
            vec![
                sse(
                    None,
                    "snapshot",
                    &snapshot(
                        "failed",
                        Some("The backend build failed."),
                        json!({ "backend_build": {
                            "status": "failed",
                            "log_output": "error: expected expression\n  --> game.js:1:13",
                        }}),
                    ),
                )
                .into_bytes(),
            ],
        ],
    })
    .await;

    let output = repo.gbandit(&platform.origin, &["deploy"]).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{}", describe(&output));

    assert!(
        stdout.contains(
            "--- backend_build log ---\nerror: expected expression\n  --> game.js:1:13\n--- end backend_build log ---"
        ),
        "{stdout}"
    );
    assert!(stdout.contains("Undid the auto-commit"), "{stdout}");
    assert!(stderr.contains("The backend build failed."), "{stderr}");

    let seen = platform.seen();
    assert!(seen.titles.is_empty());
    assert_eq!(seen.stream_since, [None, Some(1)]);
    assert_eq!(repo.git(&["rev-parse", "HEAD"]), before);
    assert_eq!(repo.remote_head(), before);
    assert_eq!(repo.git(&["status", "--porcelain"]), "M game.js");
}

const T1: &str = "2026-10-25T10:00:01Z";
const T2: &str = "2026-10-25T10:00:02Z";
const T3: &str = "2026-10-25T10:00:03Z";

fn snapshot(status: &str, error_summary: Option<&str>, stages: Value) -> Value {
    let mut run = json!({
        "id": 7,
        "status": status,
        "error_summary": error_summary,
        "stages": ["frontend_build", "backend_build", "rollout"],
    });
    run.as_object_mut()
        .unwrap()
        .extend(stages.as_object().unwrap().clone());
    run
}

fn event(stage: &str, kind: &str, level: &str, detail: Option<&str>, at: &str) -> Value {
    json!({
        "created_at": at,
        "event": { "stage": stage, "kind": kind, "level": level, "detail": detail },
    })
}

/// Sent with CRLF line endings, which the SSE spec allows alongside LF.
fn sse(id: Option<i64>, name: &str, data: &Value) -> String {
    let id = id.map(|id| format!("id: {id}\r\n")).unwrap_or_default();
    format!("{id}event: {name}\r\ndata: {data}\r\n\r\n")
}

fn describe(output: &Output) -> String {
    format!(
        "exit: {}\n--- stdout\n{}\n--- stderr\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A project repo with auto-commit on, its first commit pushed to a bare
/// remote, and a home of its own so no real login or git config leaks in.
struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    fn new() -> Self {
        let repo = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir_all(repo.home()).unwrap();
        std::fs::create_dir_all(repo.work()).unwrap();
        git_in(
            repo.dir.path(),
            &repo,
            &["init", "-q", "--bare", "remote.git"],
        );
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.name", "Tester"]);
        repo.git(&["config", "user.email", "tester@gbandit.test"]);
        repo.git(&["remote", "add", "origin", "../remote.git"]);
        repo.write(
            "gbandit.jsonc",
            &format!(
                r#"{{ "project": "{PROJECT}", "title": "Space Shooter", "local_dev": {{ "auto_commit": true }} }}"#
            ),
        );
        repo.write("game.js", "let score = 0;");
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "-qm", "first"]);
        repo.git(&["push", "-q", "origin", "main"]);
        repo
    }

    fn work(&self) -> std::path::PathBuf {
        self.dir.path().join("work")
    }

    fn home(&self) -> std::path::PathBuf {
        self.dir.path().join("home")
    }

    fn write(&self, path: &str, contents: &str) {
        std::fs::write(self.work().join(path), contents).unwrap();
    }

    fn git(&self, args: &[&str]) -> String {
        git_in(&self.work(), self, args)
    }

    fn remote_head(&self) -> String {
        git_in(
            &self.dir.path().join("remote.git"),
            self,
            &["rev-parse", "main"],
        )
    }

    async fn gbandit(&self, origin: &str, args: &[&str]) -> Output {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_gbandit"));
        for (name, _) in std::env::vars().filter(|(name, _)| name.starts_with("GBANDIT_")) {
            command.env_remove(name);
        }
        let run = command
            .args(args)
            .current_dir(self.work())
            .envs(self.isolated_env())
            .env("GBANDIT_AUTH_ORIGIN", origin)
            .env("GBANDIT_PLATFORM_API_ORIGIN", origin)
            .env("GBANDIT_SESSION_TOKEN", "session")
            .env("GBANDIT_USER_ID", "user-1")
            .stdin(std::process::Stdio::null())
            .output();
        tokio::time::timeout(Duration::from_secs(60), run)
            .await
            .expect("gbandit did not finish")
            .unwrap()
    }

    fn isolated_env(&self) -> [(&'static str, std::path::PathBuf); 4] {
        [
            ("HOME", self.home()),
            ("XDG_CONFIG_HOME", self.home().join(".config")),
            ("GIT_CONFIG_GLOBAL", "/dev/null".into()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
        ]
    }
}

fn git_in(dir: &Path, repo: &Repo, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .envs(repo.isolated_env())
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        describe(&output)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// How the fake platform answers one deploy. Each event stream connection
/// gets the next entry of `streams`, sent chunk by chunk, and the connection
/// ends after the last chunk.
struct Script {
    title: &'static str,
    refusals_before_accepting: u32,
    streams: Vec<Vec<Vec<u8>>>,
}

#[derive(Default)]
struct Seen {
    titles: Vec<String>,
    uploads: Vec<Upload>,
    stream_since: Vec<Option<i64>>,
}

#[derive(Debug)]
struct Upload {
    submission_id: String,
    commit_sha: Option<String>,
    bundle: Vec<String>,
}

struct Fake {
    title: &'static str,
    refusals_left: Mutex<u32>,
    streams: Mutex<VecDeque<Vec<Vec<u8>>>>,
    seen: Mutex<Seen>,
}

struct FakePlatform {
    origin: String,
    fake: Arc<Fake>,
}

impl FakePlatform {
    async fn start(script: Script) -> Self {
        let fake = Arc::new(Fake {
            title: script.title,
            refusals_left: Mutex::new(script.refusals_before_accepting),
            streams: Mutex::new(script.streams.into()),
            seen: Mutex::default(),
        });
        let app = Router::new()
            .route(
                "/api/cli/token",
                post(|| async { axum::Json(json!({ "access_token": "access" })) }),
            )
            .route(&format!("/projects/{PROJECT}"), get(project))
            .route(
                &format!("/projects/{PROJECT}/game-profile"),
                patch(update_title),
            )
            .route(&format!("/projects/{PROJECT}/deploys"), post(start_deploy))
            .route("/pipelines/7/stream", get(stream))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { origin, fake }
    }

    fn seen(&self) -> std::sync::MutexGuard<'_, Seen> {
        self.fake.seen.lock().unwrap()
    }
}

async fn project(State(fake): State<Arc<Fake>>) -> impl IntoResponse {
    axum::Json(json!({
        "slug": PROJECT,
        "title": fake.title,
        "cover_image_url": null,
        "published_at": null,
        "play_url": "https://space-shooter.gbandit.test",
        "dev_url": DEV_URL,
        "deletes_at": null,
    }))
}

async fn update_title(
    State(fake): State<Arc<Fake>>,
    axum::Json(body): axum::Json<Value>,
) -> StatusCode {
    let title = body["title"].as_str().unwrap().to_string();
    fake.seen.lock().unwrap().titles.push(title);
    StatusCode::NO_CONTENT
}

async fn start_deploy(State(fake): State<Arc<Fake>>, mut form: Multipart) -> Response {
    let mut upload = Upload {
        submission_id: String::new(),
        commit_sha: None,
        bundle: Vec::new(),
    };
    while let Some(field) = form.next_field().await.unwrap() {
        match field.name().unwrap() {
            "submission_id" => upload.submission_id = field.text().await.unwrap(),
            "commit_sha" => upload.commit_sha = Some(field.text().await.unwrap()),
            "bundle" => upload.bundle = bundle_entries(&field.bytes().await.unwrap()),
            _ => {}
        }
    }
    fake.seen.lock().unwrap().uploads.push(upload);

    let mut refusals_left = fake.refusals_left.lock().unwrap();
    if *refusals_left > 0 {
        *refusals_left -= 1;
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "application/problem+json")],
            json!({
                "type": "about:blank",
                "title": "Service Unavailable",
                "status": 503,
                "detail": "The build queue is full.",
                "retryable": true,
            })
            .to_string(),
        )
            .into_response();
    }
    (
        StatusCode::ACCEPTED,
        axum::Json(json!({ "pipeline_run_id": 7 })),
    )
        .into_response()
}

async fn stream(
    State(fake): State<Arc<Fake>>,
    Query(query): Query<std::collections::HashMap<String, i64>>,
) -> Response {
    fake.seen
        .lock()
        .unwrap()
        .stream_since
        .push(query.get("since").copied());
    let chunks = fake
        .streams
        .lock()
        .unwrap()
        .pop_front()
        .expect("the CLI connected more often than the script expects");
    // A pause between chunks so each reaches the CLI in a read of its own.
    let body = futures_util::stream::iter(chunks).then(|chunk| async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        Ok::<_, std::convert::Infallible>(Bytes::from(chunk))
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(body))
        .unwrap()
}

fn bundle_entries(archive: &[u8]) -> Vec<String> {
    let decoder = zstd::stream::Decoder::new(archive).unwrap();
    tar::Archive::new(decoder)
        .entries()
        .unwrap()
        .map(|entry| {
            entry
                .unwrap()
                .path()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}
