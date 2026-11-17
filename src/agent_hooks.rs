//! AI 编程助手的状态提醒：通过官方 hooks 得知「开始处理／等你授权／等你回复」。
//!
//! Claude Code / Codex 的脚本写入各自的会话状态文件，Windows 目录变更通知唤醒
//! 后台读取，再通过通道交给界面。空闲时阻塞等待，没有定时目录扫描。
//! 只传递事件类型、工具名、会话标识和项目文件夹名，不含提示词、命令或回复正文。
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

pub mod activity;
#[cfg(windows)]
mod directory;
#[cfg(windows)]
mod owners;
mod watcher;

const SCRIPT: &str = "octopus-island-hook.ps1";
const CODEX_SCRIPT: &str = "octopus-island-codex-hook.ps1";
const EVENTS: &str = "octopus-island-events";
const MAX_EVENT_BYTES: usize = 16 * 1024;
/// 超过这个时间没有更新的会话状态文件视为过期并清理。
const STALE: Duration = Duration::from_secs(12 * 3600);
/// 监听的 hook 事件及其匹配条件；`Notification` 与 `PreToolUse` 只关心需要用户出面的几种。
const HOOKS: [(&str, Option<&str>); 9] = [
    ("UserPromptSubmit", None),
    ("Stop", None),
    ("SessionEnd", None),
    ("PermissionRequest", None),
    (
        "Notification",
        Some("permission_prompt|idle_prompt|elicitation_dialog"),
    ),
    ("PreToolUse", None),
    ("PostToolUse", None),
    ("PostToolUseFailure", None),
    ("StopFailure", None),
];
/// 与 Codex 0.169.0 官方 hooks 保持一致，不修改 hook 定义以外的用户配置。
const CODEX_HOOKS: [(&str, Option<&str>); 7] = [
    ("UserPromptSubmit", None),
    ("Stop", None),
    ("SessionEnd", None),
    ("PermissionRequest", None),
    ("PreToolUse", None),
    ("PostToolUse", None),
    ("Interrupt", None),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Agent {
    ClaudeCode,
    Codex,
}
impl Agent {
    pub const ALL: [Self; 2] = [Self::ClaudeCode, Self::Codex];
    pub fn label(self) -> &'static str {
        match self {
            Agent::ClaudeCode => "Claude Code",
            Agent::Codex => "Codex",
        }
    }
    fn key(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }
    fn script(self) -> &'static str {
        match self {
            Self::ClaudeCode => SCRIPT,
            Self::Codex => CODEX_SCRIPT,
        }
    }
    fn settings_file(self) -> &'static str {
        match self {
            Self::ClaudeCode => "settings.json",
            Self::Codex => "hooks.json",
        }
    }
    fn events_dir(self) -> &'static str {
        match self {
            Self::ClaudeCode => EVENTS,
            Self::Codex => "octopus-island-codex-events",
        }
    }
    fn hooks(self) -> &'static [(&'static str, Option<&'static str>)] {
        match self {
            Self::ClaudeCode => &HOOKS,
            Self::Codex => &CODEX_HOOKS,
        }
    }
    pub fn config_dir(self) -> PathBuf {
        let (variable, folder) = match self {
            Self::ClaudeCode => ("CLAUDE_CONFIG_DIR", ".claude"),
            Self::Codex => ("CODEX_HOME", ".codex"),
        };
        std::env::var_os(variable)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("USERPROFILE")
                    .or_else(|| std::env::var_os("HOME"))
                    .map(PathBuf::from)
                    .unwrap_or_default()
                    .join(folder)
            })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// 用户刚发出消息，助手开始处理。
    Working,
    /// 工具准备执行；权限检查可能尚未完成。
    ToolStart,
    /// 工具已经返回，整轮工作仍可继续。
    ToolEnd,
    /// 助手要执行某个工具，等用户授权。
    Permission,
    /// 助手弹出了选项，等用户选择。
    Question,
    /// 异步提问不会阻塞整轮工作。
    QuestionAsync,
    /// 计划已写好，等用户确认。
    Plan,
    /// 回合结束。
    Done,
    /// 回合结束后用户一直没有回复。
    Idle,
    /// 用户中断了正在运行的 Codex 回合。
    Interrupted,
    /// 客户端明确报告本轮失败。
    Failed,
    /// 会话已退出。
    End,
}
impl Kind {
    /// 需要用户关注或回复的状态。
    pub fn waiting(self) -> bool {
        matches!(
            self,
            Kind::Permission | Kind::Question | Kind::QuestionAsync | Kind::Plan
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Owner {
    pub pid: u32,
    /// Windows 进程创建时间的 Unix 毫秒值，防止 PID 重用串会话。
    pub started: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Metadata {
    pub turn: String,
    pub call: String,
    pub at: u64,
    pub owner: Option<Owner>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub agent: Agent,
    pub kind: Kind,
    /// 工具名称，不包含参数、命令或返回正文。
    pub tool: String,
    pub session: String,
    /// 工作目录的文件夹名。
    pub project: String,
    pub meta: Metadata,
}

/// 解析 hook 脚本写出的一条状态。
pub fn parse(raw: &[u8]) -> Option<Event> {
    if raw.len() > MAX_EVENT_BYTES {
        return None;
    }
    let value: Value =
        serde_json::from_slice(raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(raw)).ok()?;
    let agent = Agent::ALL
        .into_iter()
        .find(|agent| value["agent"].as_str() == Some(agent.key()))?;
    let text = |key: &str, max: usize| {
        value[key]
            .as_str()
            .unwrap_or("")
            .chars()
            .filter(|c| !c.is_control())
            .take(max)
            .collect::<String>()
    };
    let tool = text("tool", 80);
    let kind = match value["kind"].as_str()? {
        "working" => Kind::Working,
        "tool_start" => Kind::ToolStart,
        "tool_end" => Kind::ToolEnd,
        // 提问和确认计划在 Claude Code 里也是工具调用，这里按工具名区分开。
        "permission" => match (agent, tool.as_str()) {
            (Agent::ClaudeCode, "AskUserQuestion") => Kind::Question,
            (Agent::ClaudeCode, "ExitPlanMode") => Kind::Plan,
            _ => Kind::Permission,
        },
        "question" => Kind::Question,
        "question_async" => Kind::QuestionAsync,
        "plan" => Kind::Plan,
        "done" => Kind::Done,
        "idle" => Kind::Idle,
        "interrupted" => Kind::Interrupted,
        "failed" => Kind::Failed,
        "end" => Kind::End,
        _ => return None,
    };
    let session = text("session", 80);
    if session.is_empty() {
        return None;
    }
    Some(Event {
        agent,
        kind,
        tool: if matches!(
            kind,
            Kind::Permission
                | Kind::ToolStart
                | Kind::ToolEnd
                | Kind::Question
                | Kind::QuestionAsync
                | Kind::Plan
        ) {
            tool
        } else {
            String::new()
        },
        session,
        project: text("project", 60),
        meta: Metadata {
            turn: text("turn", 100),
            call: text("call", 120),
            at: value["at"].as_u64().unwrap_or(0),
            owner: value["owner_pid"]
                .as_u64()
                .and_then(|pid| u32::try_from(pid).ok())
                .filter(|pid| *pid != 0)
                .zip(value["owner_started"].as_u64().filter(|at| *at != 0))
                .map(|(pid, started)| Owner { pid, started }),
        },
    })
}

fn command(agent: Agent, dir: &Path) -> String {
    format!(
        "powershell.exe -NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File \"{}\"",
        dir.join(agent.script()).to_string_lossy().replace('\\', "/")
    )
}

fn is_ours(handler: &Value, agent: Agent) -> bool {
    handler["command"]
        .as_str()
        .is_some_and(|command| command.contains(agent.script()))
}

fn read_settings(path: &Path, agent: Agent) -> Result<Value, String> {
    if !path.is_file() {
        return Ok(json!({}));
    }
    let file = format!("{} 的 {}", agent.label(), agent.settings_file());
    let raw = fs::read(path).map_err(|_| format!("无法读取{file}"))?;
    let value: Value = serde_json::from_slice(raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&raw))
        .map_err(|_| format!("{file} 不是有效的 JSON，已保留原样"))?;
    if !value.is_object() {
        return Err(format!("{file} 顶层必须是 JSON 对象，已保留原样"));
    }
    Ok(value)
}

/// 先移除应用自身的 hook，再按需重新加入；用户已有的 hook 一律保留。
fn apply(settings: &mut Value, agent: Agent, command: Option<&str>) -> Result<(), String> {
    let unknown = format!("{} 的 hooks 配置格式无法识别，已保留原样", agent.label());
    let root = settings.as_object_mut().ok_or_else(|| unknown.clone())?;
    if root.get("hooks").is_some_and(|hooks| hooks.is_null()) {
        root.remove("hooks");
    }
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| unknown.clone())?;
    for groups in hooks.values_mut() {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        for group in groups.iter_mut() {
            if let Some(handlers) = group["hooks"].as_array_mut() {
                let before = handlers.len();
                handlers.retain(|handler| !is_ours(handler, agent));
                // 只剩空壳的分组一并去掉；用户自己写的空分组不动。
                if handlers.is_empty() && before > 0 {
                    *group = Value::Null;
                }
            }
        }
        groups.retain(|group| !group.is_null());
    }
    if let Some(command) = command {
        for &(event, matcher) in agent.hooks() {
            let ending = agent == Agent::Codex && matches!(event, "SessionEnd" | "Interrupt");
            let asynchronous = !(agent == Agent::Codex && event == "SessionEnd");
            let mut group = json!({"hooks": [{"type": "command", "command": command, "async": asynchronous, "timeout": if ending { 3 } else { 10 }}]});
            if let Some(matcher) = matcher {
                group["matcher"] = json!(matcher);
            }
            hooks
                .entry(event)
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .ok_or_else(|| unknown.clone())?
                .push(group);
        }
    } else {
        for &(event, _) in agent.hooks() {
            if hooks
                .get(event)
                .and_then(Value::as_array)
                .is_some_and(|groups| groups.is_empty())
            {
                hooks.remove(event);
            }
        }
        if hooks.is_empty() {
            root.remove("hooks");
        }
    }
    Ok(())
}

fn write_settings(path: &Path, agent: Agent, settings: &Value) -> Result<(), String> {
    let body = serde_json::to_vec_pretty(settings)
        .map_err(|_| format!("无法保存 {} 设置", agent.label()))?;
    crate::model::atomic_write(path, &body)
        .map_err(|_| format!("无法更新 {} 的 {}", agent.label(), agent.settings_file()))
}

/// 安装对应助手的脚本和 hooks；定义未变时不写配置，不改动已有信任记录。
pub fn install(agent: Agent, dir: &Path) -> Result<(), String> {
    if !dir.is_dir() {
        return Err(format!(
            "未找到 {} 配置目录，请先安装并运行一次 {}",
            agent.label(),
            agent.label()
        ));
    }
    let path = dir.join(agent.settings_file());
    let before = read_settings(&path, agent)?;
    let mut settings = before.clone();
    apply(&mut settings, agent, Some(&command(agent, dir)))?;
    let source = match agent {
        Agent::ClaudeCode => include_str!("../scripts/claude-hook.ps1"),
        Agent::Codex => include_str!("../scripts/codex-hook.ps1"),
    };
    let script = format!(
        "\u{feff}{}\n{}",
        include_str!("../scripts/agent-hook-common.ps1").trim_start_matches('\u{feff}'),
        source.trim_start_matches('\u{feff}')
    );
    if fs::read(dir.join(agent.script())).ok().as_deref() != Some(script.as_bytes()) {
        crate::model::atomic_write(&dir.join(agent.script()), script.as_bytes())
            .map_err(|_| format!("无法保存 {} hook 脚本", agent.label()))?;
    }
    if settings != before {
        write_settings(&path, agent, &settings)?;
    }
    Ok(())
}

/// 仅移除对应助手中本应用登记的 hook、脚本和状态目录。
pub fn uninstall(agent: Agent, dir: &Path) -> Result<(), String> {
    let path = dir.join(agent.settings_file());
    if path.is_file() {
        let before = read_settings(&path, agent)?;
        let mut settings = before.clone();
        apply(&mut settings, agent, None)?;
        if settings != before {
            write_settings(&path, agent, &settings)?;
        }
    }
    let _ = fs::remove_file(dir.join(agent.script()));
    let _ = fs::remove_dir_all(dir.join(agent.events_dir()));
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchStatus {
    Listening,
    WaitingForDirectory,
    Failed(String),
}

/// 释放时发送 Windows 停止事件；I/O 取消和句柄释放由工作线程完成，不阻塞界面。
pub struct Service {
    pub rx: mpsc::Receiver<Event>,
    pub status: mpsc::Receiver<WatchStatus>,
    #[cfg(windows)]
    stop: std::sync::Arc<directory::StopEvent>,
    #[cfg(all(test, windows))]
    finished: mpsc::Receiver<()>,
    #[cfg(all(test, windows))]
    stats: std::sync::Arc<watcher::Stats>,
}
impl Drop for Service {
    fn drop(&mut self) {
        #[cfg(windows)]
        self.stop.signal();
    }
}

/// 监听对应助手的事件目录。初始扫描后，仅处理系统报告的变化及有限重试。
pub fn spawn(
    agent: Agent,
    dir: PathBuf,
    wake: impl Fn() + Send + 'static,
) -> Result<Service, String> {
    #[cfg(windows)]
    {
        let (tx, rx) = mpsc::channel();
        let (status_tx, status) = mpsc::channel();
        let stop = std::sync::Arc::new(directory::StopEvent::new()?);
        let worker_stop = stop.clone();
        #[cfg(test)]
        let (finished_tx, finished) = mpsc::channel();
        #[cfg(test)]
        let stats = std::sync::Arc::new(watcher::Stats::default());
        #[cfg(test)]
        let worker_stats = stats.clone();
        std::thread::Builder::new()
            .name(format!("island-{}-events", agent.key()))
            .spawn(move || {
                let result = watcher::run(
                    agent,
                    dir,
                    &worker_stop,
                    &tx,
                    &status_tx,
                    &wake,
                    #[cfg(test)]
                    worker_stats,
                );
                if let Err(error) = result {
                    if !worker_stop.is_set() {
                        let _ = status_tx.send(WatchStatus::Failed(error));
                        wake();
                    }
                }
                #[cfg(test)]
                let _ = finished_tx.send(());
            })
            .map_err(|error| format!("无法启动目录监听：{error}"))?;
        Ok(Service {
            rx,
            status,
            stop,
            #[cfg(test)]
            finished,
            #[cfg(test)]
            stats,
        })
    }
    #[cfg(not(windows))]
    {
        let _ = (agent, dir, wake);
        Err("助手状态监听目前仅支持 Windows".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "octopus-island-agent-hooks-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
    fn record(kind: &str, tool: &str, session: &str) -> String {
        json!({"version": 1, "agent": "claude-code", "kind": kind, "tool": tool, "session": session, "project": "Octopus Island", "at": 1}).to_string()
    }

    #[test]
    fn records_map_to_the_states_shown_on_the_island() {
        let event = parse(record("permission", "Bash", "s1").as_bytes()).unwrap();
        assert_eq!(
            (event.kind, event.tool.as_str(), event.project.as_str()),
            (Kind::Permission, "Bash", "Octopus Island")
        );
        // 提问与确认计划也是工具调用，但对用户来说不是「授权」。
        assert_eq!(
            parse(record("permission", "AskUserQuestion", "s1").as_bytes())
                .unwrap()
                .kind,
            Kind::Question
        );
        assert_eq!(
            parse(record("permission", "ExitPlanMode", "s1").as_bytes())
                .unwrap()
                .kind,
            Kind::Plan
        );
        assert_eq!(
            parse(record("permission", "AskUserQuestion", "s1").as_bytes())
                .unwrap()
                .tool,
            "AskUserQuestion"
        );
        for (raw, kind) in [
            ("working", Kind::Working),
            ("done", Kind::Done),
            ("idle", Kind::Idle),
            ("end", Kind::End),
            ("question", Kind::Question),
        ] {
            assert_eq!(parse(record(raw, "", "s1").as_bytes()).unwrap().kind, kind);
        }
        assert!(parse(record("unknown", "", "s1").as_bytes()).is_none());
        assert!(parse(record("done", "", "").as_bytes()).is_none());
        assert!(parse(br#"{"agent":"other","kind":"done","session":"s1"}"#).is_none());
        assert!(parse(b"not json").is_none());
    }

    #[test]
    fn install_keeps_existing_settings_and_uninstall_restores_them() {
        let dir = temp("install");
        let original = json!({
            "model": "opus",
            "statusLine": {"type": "command", "command": "my-statusline"},
            "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "my-stop-hook"}]}],
                "PostToolUse": [{"matcher": "Edit", "hooks": [{"type": "command", "command": "my-format-hook"}]}]
            }
        });
        let path = dir.join("settings.json");
        fs::write(&path, original.to_string()).unwrap();

        install(Agent::ClaudeCode, &dir).unwrap();
        let installed: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(installed["model"], original["model"]);
        assert_eq!(installed["statusLine"], original["statusLine"]);
        assert_eq!(
            installed["hooks"]["PostToolUse"][0],
            original["hooks"]["PostToolUse"][0]
        );
        assert_eq!(
            installed["hooks"]["Stop"][0], original["hooks"]["Stop"][0],
            "用户已有的 hook 排在前面且保持原样"
        );
        for (event, matcher) in HOOKS {
            let ours: Vec<&Value> = installed["hooks"][event]
                .as_array()
                .unwrap()
                .iter()
                .filter(|group| {
                    group["hooks"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|handler| is_ours(handler, Agent::ClaudeCode))
                })
                .collect();
            assert_eq!(ours.len(), 1, "{event} 只登记一次");
            assert_eq!(ours[0]["matcher"].as_str(), matcher);
            assert_eq!(
                ours[0]["hooks"][0]["async"],
                json!(true),
                "后台运行，不拖慢也不干预 Claude Code"
            );
        }
        assert!(
            fs::read(dir.join(SCRIPT))
                .unwrap()
                .starts_with(b"\xef\xbb\xbf"),
            "带 BOM，Windows PowerShell 才能正确读取中文注释"
        );

        // 重复安装不产生重复条目，也不改写文件。
        let stamp = fs::metadata(&path).unwrap().modified().unwrap();
        install(Agent::ClaudeCode, &dir).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap(),
            installed
        );
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), stamp);

        fs::create_dir_all(dir.join(EVENTS)).unwrap();
        uninstall(Agent::ClaudeCode, &dir).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap(),
            original
        );
        assert!(!dir.join(SCRIPT).exists() && !dir.join(EVENTS).exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn install_starts_from_nothing_and_refuses_unreadable_settings() {
        let dir = temp("fresh");
        install(Agent::ClaudeCode, &dir).unwrap();
        let installed: Value =
            serde_json::from_slice(&fs::read(dir.join("settings.json")).unwrap()).unwrap();
        assert_eq!(installed["hooks"].as_object().unwrap().len(), HOOKS.len());
        uninstall(Agent::ClaudeCode, &dir).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(dir.join("settings.json")).unwrap()).unwrap(),
            json!({})
        );

        for broken in [
            "{ // 注释\n}",
            "[]",
            r#"{"hooks":[]}"#,
            r#"{"hooks":{"Stop":{}}}"#,
        ] {
            fs::write(dir.join("settings.json"), broken).unwrap();
            assert!(install(Agent::ClaudeCode, &dir).is_err(), "{broken}");
            assert_eq!(
                fs::read_to_string(dir.join("settings.json")).unwrap(),
                broken,
                "失败时不改动原文件"
            );
        }
        assert!(install(Agent::ClaudeCode, &dir.join("missing")).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn codex_install_keeps_existing_configuration_and_does_not_rewrite_unchanged_hooks() {
        let dir = temp("codex-install");
        let original = json!({"description":"用户已有配置","hooks":{"Stop":[{"hooks":[{"type":"command","command":"my-stop-hook"}]}]}});
        let path = dir.join("hooks.json");
        fs::write(&path, original.to_string()).unwrap();
        let config = "# 保留信任和其他配置\n[hooks.state.example]\ntrusted_hash = 'existing'\n";
        fs::write(dir.join("config.toml"), config).unwrap();
        install(Agent::Codex, &dir).unwrap();
        let installed: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(installed["hooks"]["Stop"][0], original["hooks"]["Stop"][0]);
        assert_eq!(installed["description"], original["description"]);
        for &(event, matcher) in &CODEX_HOOKS {
            let ours: Vec<_> = installed["hooks"][event]
                .as_array()
                .unwrap()
                .iter()
                .filter(|group| {
                    group["hooks"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|handler| is_ours(handler, Agent::Codex))
                })
                .collect();
            assert_eq!(ours.len(), 1);
            assert_eq!(ours[0]["matcher"].as_str(), matcher);
            assert_eq!(ours[0]["hooks"][0]["async"], json!(event != "SessionEnd"));
            assert_eq!(
                ours[0]["hooks"][0]["timeout"],
                json!(if matches!(event, "SessionEnd" | "Interrupt") {
                    3
                } else {
                    10
                })
            );
        }
        let stamp = fs::metadata(&path).unwrap().modified().unwrap();
        install(Agent::Codex, &dir).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), stamp);
        assert_eq!(fs::read_to_string(dir.join("config.toml")).unwrap(), config);
        let script = fs::read(dir.join(CODEX_SCRIPT)).unwrap();
        assert!(script.starts_with(b"\xef\xbb\xbf") && !script[3..].starts_with(b"\xef\xbb\xbf"));
        fs::create_dir_all(dir.join(EVENTS)).unwrap();
        fs::create_dir_all(dir.join(Agent::Codex.events_dir())).unwrap();
        uninstall(Agent::Codex, &dir).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap(),
            original
        );
        assert!(
            dir.join(EVENTS).exists(),
            "Codex 卸载不清理 Claude 的状态目录"
        );
        assert!(!dir.join(Agent::Codex.events_dir()).exists());
        let _ = fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod live {
    /// 联调用：把 hooks 装进指定目录（例如某个临时项目的 `.claude`），不触碰用户的全局配置。
    #[test]
    #[ignore = "按需把 hooks 装进 OCTOPUS_ISLAND_AGENT_HOOKS_DIR 指定的目录"]
    fn install_into_directory() {
        let dir = std::path::PathBuf::from(
            std::env::var_os("OCTOPUS_ISLAND_AGENT_HOOKS_DIR").expect("需指定目录"),
        );
        let agent = if std::env::var("OCTOPUS_ISLAND_AGENT_KIND").as_deref() == Ok("codex") {
            super::Agent::Codex
        } else {
            super::Agent::ClaudeCode
        };
        super::install(agent, &dir).unwrap();
    }
}
