use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use app_test_support::MockResponsesConfig;
use codex_features::Feature;
use core_test_support::responses;
use serde_json::json;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 20);
const WORKFLOW_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 20);
const WORKFLOW_CALL_ID: &str = "workflow-terminal-call";
const WORKFLOW_PROMPT: &str = "Run the Terminal E2E dynamic workflow now.";
const WORKFLOW_AGENT_PROMPT: &str = "Return terminal workflow compatibility after retry";
const WORKFLOW_SKIP_PROMPT: &str = "Wait until this terminal workflow agent is skipped";
const WORKFLOW_ROLE_INSTRUCTIONS: &str =
    "Apply the terminal workflow role configuration before answering.";
const WORKFLOW_OPTION_MODEL: &str = "workflow-option-model";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_launch_and_control_run_end_to_end_in_the_terminal() -> Result<()> {
    let script = r#"export const meta = {
  name: "terminal-e2e",
  title: "Terminal E2E",
  description: "Exercise terminal workflow launch and controls",
  phases: [{ title: "Inspect" }, { title: "Hold" }],
};
phase("Inspect");
const result = await agent("Return terminal workflow compatibility after retry", {
  label: "retry-agent",
  agentType: "workflow_e2e",
  model: "workflow-option-model",
});
const skipped = await agent("Wait until this terminal workflow agent is skipped", {
  label: "skip-agent",
  agentType: "workflow_e2e",
  model: "workflow-option-model",
});
phase("Hold");
log(`agents returned: ${result}, ${skipped}`);
return await new Promise(() => {});
"#;
    let server = responses::start_mock_server().await;
    let parent_turn = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, WORKFLOW_PROMPT)
                && !body_contains(request, WORKFLOW_CALL_ID)
                && !body_contains(request, "You are a workflow subagent.")
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-terminal-parent-1"),
            responses::ev_function_call(
                WORKFLOW_CALL_ID,
                "Workflow",
                &json!({ "script": script }).to_string(),
            ),
            responses::ev_completed("workflow-terminal-parent-1"),
        ]),
    )
    .await;
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .and(|request: &wiremock::Request| {
            body_contains(request, "You are a workflow subagent.")
                && body_contains(request, WORKFLOW_AGENT_PROMPT)
        })
        .respond_with(RetryingAgentResponder::new())
        .up_to_n_times(2)
        .mount(&server)
        .await;
    responses::mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "You are a workflow subagent.")
                && body_contains(request, WORKFLOW_SKIP_PROMPT)
        },
        responses::sse_response(responses::sse(vec![
            responses::ev_response_created("workflow-terminal-skip-child"),
            responses::ev_assistant_message(
                "workflow-terminal-skip-child-message",
                "should be cancelled",
            ),
            responses::ev_completed_with_tokens("workflow-terminal-skip-child", 11),
        ]))
        .set_delay(Duration::from_secs(/*secs*/ 30)),
    )
    .await;
    let parent_follow_up = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, WORKFLOW_CALL_ID)
                && !body_contains(request, "You are a workflow subagent.")
        },
        responses::sse(vec![
            responses::ev_response_created("workflow-terminal-parent-2"),
            responses::ev_assistant_message(
                "workflow-terminal-parent-message",
                "Workflow launched",
            ),
            responses::ev_completed("workflow-terminal-parent-2"),
        ]),
    )
    .await;

    let repo_root = codex_utils_cargo_bin::repo_root()?;
    let codex_home = tempfile::tempdir()?;
    let log_dir = tempfile::tempdir()?;
    let role_config_path = codex_home.path().join("workflow-e2e-role.toml");
    std::fs::write(
        &role_config_path,
        format!(
            "developer_instructions = {WORKFLOW_ROLE_INSTRUCTIONS:?}\nmodel = \"workflow-role-model\"\n"
        ),
    )?;
    let project_config = format!(
        "[tui]\nanimations = true\n\n[agents.workflow_e2e]\ndescription = \"Terminal workflow test role\"\nconfig_file = {role_config_path:?}\n\n[projects.{repo_root:?}]\ntrust_level = \"trusted\"",
        role_config_path = role_config_path.display().to_string(),
        repo_root = repo_root.display().to_string(),
    );
    MockResponsesConfig::new(&server.uri())
        .with_approval_policy("on-request")
        .with_root_config("suppress_unstable_features_warning = true")
        .enable_feature(Feature::Workflows)
        .enable_feature(Feature::Collab)
        .disable_feature(Feature::MultiAgentV2)
        .with_extra_config(&project_config)
        .write(codex_home.path())?;

    let mut terminal = PtyCodex::start(
        &repo_root,
        codex_home,
        log_dir.path(),
        /*rows*/ 48,
        /*cols*/ 140,
    )?;
    terminal.wait_for_startup()?;
    terminal.send_line(WORKFLOW_PROMPT)?;
    terminal.wait_for_text(
        "workflow approval",
        "Review dynamic workflow before running",
        WORKFLOW_TIMEOUT,
    )?;
    terminal.write_input(b"\r")?;

    terminal.wait_for(
        "live workflow with a retryable child agent",
        WORKFLOW_TIMEOUT,
        |terminal| {
            let screen = terminal.screen_contents();
            screen.contains("Workflow Terminal E2E")
                && screen.contains("retry-agent")
                && screen.contains("running")
                && screen.contains("1 agents")
        },
    )?;
    let (has_rgb, row_colors) = terminal
        .screen_row_color_state("Workflow Terminal E2E")
        .context("running workflow row was not visible")?;
    ensure!(
        has_rgb,
        "running workflow row did not retain its RGB shimmer; raw RGB SGR: {}; colors: \
         {row_colors:?}",
        contains_bytes(&terminal.output, b"\x1b[38;2;")
            || contains_bytes(&terminal.output, b"\x1b[38:2:")
    );
    terminal.wait_for_text("parent completion", "Workflow launched", WORKFLOW_TIMEOUT)?;

    terminal.open_workflows("workflow list")?;
    ensure!(terminal.screen_contains("Terminal E2E"));
    terminal.write_input(b"\r")?;
    terminal.wait_for("workflow detail", WORKFLOW_TIMEOUT, |terminal| {
        let screen = terminal.bottom_contents();
        screen.contains("Stop workflow") && screen.contains("retry-agent")
    })?;
    terminal.write_input(b"2")?;
    terminal.wait_for_bottom_text("workflow agent actions", "Retry attempt", WORKFLOW_TIMEOUT)?;
    terminal.write_input(b"\r")?;

    terminal.wait_for(
        "retried agent advanced to skippable agent",
        WORKFLOW_TIMEOUT,
        |terminal| {
            let screen = terminal.bottom_contents();
            screen.contains("skip-agent")
                && screen.contains("2 agents")
                && screen.contains("running")
        },
    )?;
    terminal.write_input(b"3")?;
    terminal.wait_for_bottom_text("workflow skip action", "Skip agent", WORKFLOW_TIMEOUT)?;
    terminal.write_input(b"\x1b[B")?;
    terminal.write_input(b"\r")?;

    terminal.wait_for("workflow agent was skipped", WORKFLOW_TIMEOUT, |terminal| {
        terminal.bottom_contents().lines().any(|line| {
            line.contains("skip-agent")
                && (line.contains("skipped") || line.contains("− skip-agent"))
        })
    })?;
    terminal.write_input(b"1")?;
    terminal.wait_for_text("stopped workflow", "stopped", WORKFLOW_TIMEOUT)?;

    let parent_requests = parent_turn
        .requests()
        .into_iter()
        .filter(|request| {
            let body = request.body_json().to_string();
            body.contains(WORKFLOW_PROMPT)
                && !body.contains(WORKFLOW_CALL_ID)
                && !body.contains("You are a workflow subagent.")
        })
        .collect::<Vec<_>>();
    ensure!(
        parent_requests.len() == 1,
        "expected one initial parent request, got {}",
        parent_requests.len()
    );
    let parent_tools = response_tool_names(&parent_requests[0].body_json());
    ensure!(
        parent_tools.iter().any(|name| name == "Workflow"),
        "parent did not expose Workflow: {parent_tools:?}"
    );
    ensure!(
        parent_tools.iter().any(|name| name == "multi_agent_v1"),
        "Agent v1 parent namespace was not preserved: {parent_tools:?}"
    );
    ensure!(
        !parent_tools.iter().any(|name| name == "collaboration"),
        "Agent v1 parent unexpectedly exposed Agent v2: {parent_tools:?}"
    );

    let child_request_bodies = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|request| request.body_json::<serde_json::Value>().ok())
        .filter(|body| {
            let body = body.to_string();
            body.contains("You are a workflow subagent.")
                && (body.contains(WORKFLOW_AGENT_PROMPT) || body.contains(WORKFLOW_SKIP_PROMPT))
        })
        .collect::<Vec<_>>();
    ensure!(
        child_request_bodies.len() == 3,
        "expected two retry attempts and one skipped child request, got {}",
        child_request_bodies.len()
    );
    let retry_count = child_request_bodies
        .iter()
        .filter(|body| body.to_string().contains(WORKFLOW_AGENT_PROMPT))
        .count();
    let skip_count = child_request_bodies
        .iter()
        .filter(|body| body.to_string().contains(WORKFLOW_SKIP_PROMPT))
        .count();
    ensure!(
        retry_count == 2,
        "expected two retry attempts, got {retry_count}"
    );
    ensure!(
        skip_count == 1,
        "expected one skipped attempt, got {skip_count}"
    );
    for body in &child_request_bodies {
        ensure!(
            body.to_string().contains(WORKFLOW_ROLE_INSTRUCTIONS),
            "workflow agent did not apply its configured role: {body}"
        );
        let child_tools = response_tool_names(body);
        for forbidden in ["Workflow", "RunWorkflow", "multi_agent_v1", "collaboration"] {
            ensure!(
                !child_tools.iter().any(|name| name == forbidden),
                "workflow child exposed forbidden tool {forbidden}: {child_tools:?}"
            );
        }
        ensure!(
            body["model"] == WORKFLOW_OPTION_MODEL,
            "workflow agent model option did not override its role model: {}",
            body["model"]
        );
    }
    let parent_follow_up_requests = parent_follow_up
        .requests()
        .into_iter()
        .filter(|request| {
            let body = request.body_json().to_string();
            body.contains(WORKFLOW_CALL_ID) && !body.contains("You are a workflow subagent.")
        })
        .count();
    ensure!(
        parent_follow_up_requests == 1,
        "parent did not continue exactly once after the asynchronous Workflow result"
    );

    Ok(())
}

struct RetryingAgentResponder {
    attempts: AtomicUsize,
    delayed: ResponseTemplate,
    completed: ResponseTemplate,
}

impl RetryingAgentResponder {
    fn new() -> Self {
        let first = responses::sse(vec![
            responses::ev_response_created("workflow-terminal-child-first"),
            responses::ev_assistant_message(
                "workflow-terminal-child-first-message",
                "should be retried",
            ),
            responses::ev_completed_with_tokens("workflow-terminal-child-first", 13),
        ]);
        let second = responses::sse(vec![
            responses::ev_response_created("workflow-terminal-child-second"),
            responses::ev_assistant_message("workflow-terminal-child-second-message", "compatible"),
            responses::ev_completed_with_tokens("workflow-terminal-child-second", 37),
        ]);
        Self {
            attempts: AtomicUsize::new(0),
            delayed: responses::sse_response(first)
                .set_delay(Duration::from_secs(/*secs*/ 30)),
            completed: responses::sse_response(second),
        }
    }
}

impl Respond for RetryingAgentResponder {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            self.delayed.clone()
        } else {
            self.completed.clone()
        }
    }
}

struct PtyCodex {
    master: File,
    child: Child,
    parser: vt100::Parser,
    output: Vec<u8>,
    cursor_answered: bool,
    palette_answered: bool,
    keyboard_answered: bool,
    _codex_home: TempDir,
}

impl PtyCodex {
    fn start(
        repo_root: &Path,
        codex_home: TempDir,
        log_dir: &Path,
        rows: u16,
        cols: u16,
    ) -> Result<Self> {
        let mut master_fd = -1;
        let mut slave_fd = -1;
        let mut window_size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        // SAFETY: `openpty` initializes both file descriptors on success, and the supplied window
        // size remains valid for the duration of the call.
        let result = unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                /*name*/ std::ptr::null_mut(),
                /*termp*/ std::ptr::null_mut(),
                &raw mut window_size,
            )
        };
        if result == -1 {
            return Err(std::io::Error::last_os_error()).context("open workflow-test terminal");
        }

        // SAFETY: a successful `openpty` transfers ownership of both unique file descriptors.
        let master = File::from(unsafe { OwnedFd::from_raw_fd(master_fd) });
        // SAFETY: `slave_fd` is the second unique descriptor initialized by `openpty`.
        let slave = File::from(unsafe { OwnedFd::from_raw_fd(slave_fd) });
        let stdin = slave.try_clone().context("clone workflow-test stdin")?;
        let stdout = slave.try_clone().context("clone workflow-test stdout")?;
        let codex = codex_utils_cargo_bin::cargo_bin("codex-tui")
            .or_else(|_| codex_utils_cargo_bin::cargo_bin("codex"))?;
        let log_dir_override = format!(
            "log_dir={}",
            serde_json::to_string(&log_dir.to_string_lossy())?
        );
        let child = Command::new(codex)
            .arg("--no-alt-screen")
            .arg("-C")
            .arg(repo_root)
            .arg("-c")
            .arg("analytics.enabled=false")
            .arg("-c")
            .arg(log_dir_override)
            .env("TERM", "xterm-direct")
            .env("COLORTERM", "truecolor")
            .env("FORCE_COLOR", "3")
            .env("RUST_LOG", "trace")
            .env("CODEX_HOME", codex_home.path())
            .env_remove("NO_COLOR")
            .stdin(stdin)
            .stdout(stdout)
            .stderr(slave)
            .spawn()
            .context("start Codex in workflow-test terminal")?;

        Ok(Self {
            master,
            child,
            parser: vt100::Parser::new(rows, cols, /*scrollback_len*/ 1_000),
            output: Vec::new(),
            cursor_answered: false,
            palette_answered: false,
            keyboard_answered: false,
            _codex_home: codex_home,
        })
    }

    fn wait_for_startup(&mut self) -> Result<()> {
        self.wait_for("Codex startup", STARTUP_TIMEOUT, |terminal| {
            terminal.screen_contains("OpenAI Codex")
        })
    }

    fn wait_for_text(&mut self, description: &str, text: &str, timeout: Duration) -> Result<()> {
        self.wait_for(description, timeout, |terminal| {
            terminal.screen_contains(text)
        })
    }

    fn wait_for_bottom_text(
        &mut self,
        description: &str,
        text: &str,
        timeout: Duration,
    ) -> Result<()> {
        self.wait_for(description, timeout, |terminal| {
            terminal.bottom_contents().contains(text)
        })
    }

    fn wait_for(
        &mut self,
        description: &str,
        timeout: Duration,
        predicate: impl Fn(&Self) -> bool,
    ) -> Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.read_output(Duration::from_millis(/*millis*/ 50))?;
            self.answer_startup_queries()?;
            if predicate(self) {
                return Ok(());
            }
            if let Some(status) = self.child.try_wait()? {
                bail!(
                    "Codex exited while waiting for {description} ({status}); screen:\n{}",
                    self.screen_contents()
                );
            }
        }
        let has_rgb_sgr = contains_bytes(&self.output, b"\x1b[38;2;")
            || contains_bytes(&self.output, b"\x1b[38:2:");
        bail!(
            "timed out waiting for {description} after {timeout:?}; raw RGB SGR: \
             {has_rgb_sgr}; screen:\n{}",
            self.screen_contents()
        )
    }

    fn send_line(&mut self, text: &str) -> Result<()> {
        self.write_input(text.as_bytes())?;
        self.read_output(Duration::from_millis(/*millis*/ 20))?;
        self.write_input(b"\r")
    }

    fn open_workflows(&mut self, description: &str) -> Result<()> {
        const LIST_SUBTITLE: &str = "Live and completed runs for this thread";
        self.send_line("/workflows")?;
        std::thread::sleep(Duration::from_millis(/*millis*/ 150));
        self.read_output(Duration::from_millis(/*millis*/ 50))?;
        if self.bottom_contents().contains(LIST_SUBTITLE) {
            return Ok(());
        }
        if self
            .bottom_contents()
            .lines()
            .any(|line| line.trim() == "› /workflows")
        {
            self.write_input(b"\r")?;
        }
        self.wait_for_bottom_text(description, LIST_SUBTITLE, WORKFLOW_TIMEOUT)
    }

    fn answer_startup_queries(&mut self) -> Result<()> {
        if !self.cursor_answered && contains_bytes(&self.output, b"\x1b[6n") {
            self.write_input(b"\x1b[1;1R")?;
            self.cursor_answered = true;
        }
        if !self.keyboard_answered && contains_bytes(&self.output, b"\x1b[?u") {
            self.write_input(b"\x1b[?0u\x1b[?1;2c")?;
            self.keyboard_answered = true;
        }
        if !self.palette_answered
            && contains_bytes(&self.output, b"\x1b]10;?")
            && contains_bytes(&self.output, b"\x1b]11;?")
        {
            self.write_input(b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1b]11;rgb:0000/0000/0000\x1b\\")?;
            self.palette_answered = true;
        }
        Ok(())
    }

    fn read_output(&mut self, timeout: Duration) -> Result<()> {
        let timeout_ms = timeout.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
        let mut descriptor = libc::pollfd {
            fd: self.master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `descriptor` points to one initialized poll descriptor.
        let ready = unsafe {
            libc::poll(&mut descriptor, /*nfds*/ 1, timeout_ms)
        };
        if ready == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                return Ok(());
            }
            return Err(error).context("poll workflow-test terminal");
        }
        if ready == 0 || descriptor.revents & libc::POLLIN == 0 {
            return Ok(());
        }

        let mut chunk = [0_u8; 8_192];
        let count = self.master.read(&mut chunk)?;
        self.output.extend_from_slice(&chunk[..count]);
        self.parser.process(&chunk[..count]);
        Ok(())
    }

    fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        self.master.write_all(bytes)?;
        self.master.flush()?;
        Ok(())
    }

    fn screen_contains(&self, text: &str) -> bool {
        self.parser.screen().contents().contains(text)
    }

    fn screen_contents(&self) -> String {
        self.parser.screen().contents()
    }

    fn bottom_contents(&self) -> String {
        let screen = self.screen_contents();
        let lines = screen.lines().collect::<Vec<_>>();
        lines[lines.len().saturating_sub(20)..].join("\n")
    }

    fn screen_row_color_state(&self, text: &str) -> Option<(bool, Vec<(String, vt100::Color)>)> {
        let screen = self.parser.screen();
        let (_, cols) = screen.size();
        screen
            .rows(/*start*/ 0, cols)
            .enumerate()
            .find_map(|(row, contents)| {
                if !contents.contains(text) {
                    return None;
                }
                let row = u16::try_from(row).ok()?;
                let mut colors = Vec::new();
                for col in 0..cols {
                    if let Some(cell) = screen.cell(row, col)
                        && !cell.contents().trim().is_empty()
                    {
                        colors.push((cell.contents().to_string(), cell.fgcolor()));
                    }
                }
                let has_rgb = colors
                    .iter()
                    .any(|(_, color)| matches!(color, vt100::Color::Rgb(..)));
                Some((has_rgb, colors))
            })
    }
}

impl Drop for PtyCodex {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn contains_bytes(buffer: &[u8], needle: &[u8]) -> bool {
    buffer.windows(needle.len()).any(|window| window == needle)
}

fn response_tool_names(body: &serde_json::Value) -> Vec<String> {
    body.get("tools")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            tool.get("name")
                .or_else(|| tool.get("type"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

fn body_contains(request: &wiremock::Request, text: &str) -> bool {
    String::from_utf8(request.body.clone())
        .ok()
        .is_some_and(|body| body.contains(text))
}
