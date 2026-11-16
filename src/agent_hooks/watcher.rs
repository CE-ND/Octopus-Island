//! 状态文件读取与去重。全量扫描只用于启动、目录恢复和通知缓冲区溢出。
use super::{parse, Agent, Event, Kind, MAX_EVENT_BYTES, STALE};
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const RETRIES: [Duration; 3] = [
    Duration::from_millis(20),
    Duration::from_millis(50),
    Duration::from_millis(100),
];

#[cfg(test)]
#[derive(Default)]
pub(super) struct Stats {
    pub(super) scans: std::sync::atomic::AtomicUsize,
    pub(super) reads: std::sync::atomic::AtomicUsize,
    pub(super) notifications: std::sync::atomic::AtomicUsize,
}

struct Seen {
    fingerprint: u64,
    modified: SystemTime,
}
struct Retry {
    at: Instant,
    attempt: usize,
}
enum Outcome {
    Complete(Option<Event>),
    Retry,
}

struct Reader {
    dir: PathBuf,
    agent: Agent,
    seen: HashMap<OsString, Seen>,
    retries: HashMap<OsString, Retry>,
    // 仅在已有文件变化时顺带清理过期缓存，不单独启动清理计时器。
    cleanup_after: Instant,
    #[cfg(test)]
    stats: std::sync::Arc<Stats>,
}

fn order_events(events: &mut [Event]) {
    events.sort_by_key(|event| {
        (
            event.meta.at,
            match event.kind {
                Kind::Working => 0,
                Kind::ToolStart => 1,
                Kind::Question | Kind::QuestionAsync | Kind::Plan => 2,
                Kind::Permission => 3,
                Kind::ToolEnd => 4,
                Kind::Done | Kind::Idle => 5,
                Kind::Failed | Kind::Interrupted => 6,
                Kind::End => 7,
            },
        )
    });
}

fn event_name(name: &OsStr) -> bool {
    let path = Path::new(name);
    path.file_name() == Some(name)
        && path.extension().and_then(OsStr::to_str).is_some_and(|ext| {
            ext.eq_ignore_ascii_case("json") || ext.eq_ignore_ascii_case("event")
        })
}

impl Reader {
    fn new(agent: Agent, dir: PathBuf, #[cfg(test)] stats: std::sync::Arc<Stats>) -> Self {
        Self {
            dir,
            agent,
            seen: HashMap::new(),
            retries: HashMap::new(),
            cleanup_after: Instant::now() + Duration::from_secs(3600),
            #[cfg(test)]
            stats,
        }
    }

    fn read(&mut self, name: &OsStr, emit: bool) -> Outcome {
        if !event_name(name) {
            return Outcome::Complete(None);
        }
        #[cfg(test)]
        self.stats
            .reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = self.dir.join(name);
        let queued = path.extension().is_some_and(|ext| ext == "event");
        let Ok(file) = fs::File::open(&path) else {
            // 删除和原子替换都会短暂表现为不存在；只为收到变化的文件有限重试。
            self.seen.remove(name);
            return if queued {
                Outcome::Complete(None)
            } else {
                Outcome::Retry
            };
        };
        let Ok(metadata) = file.metadata() else {
            return Outcome::Retry;
        };
        if !metadata.is_file() || metadata.len() > MAX_EVENT_BYTES as u64 {
            return Outcome::Complete(None);
        }
        let Ok(modified) = metadata.modified() else {
            return Outcome::Retry;
        };
        if SystemTime::now()
            .duration_since(modified)
            .is_ok_and(|age| age > STALE)
        {
            drop(file);
            let _ = fs::remove_file(&path);
            self.seen.remove(name);
            return Outcome::Complete(None);
        }
        let mut raw = Vec::new();
        if file
            .take((MAX_EVENT_BYTES + 1) as u64)
            .read_to_end(&mut raw)
            .is_err()
        {
            return Outcome::Retry;
        }
        if raw.len() > MAX_EVENT_BYTES {
            return Outcome::Complete(None);
        }
        // 非原子写入尚未完成时不永久记为已处理，给它短暂的有限重试机会。
        if serde_json::from_slice::<serde_json::Value>(
            raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&raw),
        )
        .is_err()
        {
            return Outcome::Retry;
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        raw.hash(&mut hasher);
        let fingerprint = hasher.finish();
        let same = self
            .seen
            .get(name)
            .is_some_and(|old| old.fingerprint == fingerprint);
        self.seen.insert(
            name.to_owned(),
            Seen {
                fingerprint,
                modified,
            },
        );
        if same {
            return Outcome::Complete(None);
        }
        let Some(event) = parse(&raw).filter(|event| event.agent == self.agent) else {
            return Outcome::Complete(None);
        };
        if queued || event.kind == Kind::End {
            let _ = fs::remove_file(&path);
            if !path.exists() {
                self.seen.remove(name);
            }
        }
        Outcome::Complete(emit.then_some(event))
    }

    fn read_or_retry(
        &mut self,
        name: OsString,
        attempt: usize,
        now: Instant,
        out: &mut Vec<Event>,
    ) {
        match self.read(&name, true) {
            Outcome::Complete(Some(event)) => out.push(event),
            Outcome::Complete(None) => {}
            Outcome::Retry => {
                if let Some(delay) = RETRIES.get(attempt) {
                    self.retries.insert(
                        name,
                        Retry {
                            at: now + *delay,
                            attempt: attempt + 1,
                        },
                    );
                }
            }
        }
    }

    fn changed(&mut self, names: Vec<OsString>, now: Instant) -> Vec<Event> {
        let mut out = Vec::new();
        let names: HashSet<_> = names.into_iter().filter(|name| event_name(name)).collect();
        for name in names {
            self.retries.remove(&name);
            self.read_or_retry(name, 0, now, &mut out);
        }
        if now >= self.cleanup_after {
            let stale: Vec<_> = self
                .seen
                .iter()
                .filter(|(_, seen)| {
                    SystemTime::now()
                        .duration_since(seen.modified)
                        .is_ok_and(|age| age > STALE)
                })
                .map(|(name, _)| name.clone())
                .collect();
            for name in stale {
                // read 会重新检查最新修改时间，避免按旧缓存直接删除刚更新的记录。
                self.read_or_retry(name, 0, now, &mut out);
            }
            self.cleanup_after = now + Duration::from_secs(3600);
        }
        order_events(&mut out);
        out
    }

    fn rescan(&mut self, emit: bool) -> Result<Vec<Event>, String> {
        #[cfg(test)]
        self.stats
            .scans
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.clear();
                return Ok(Vec::new());
            }
            Err(error) => return Err(format!("无法读取状态目录：{error}")),
        };
        let names: HashSet<_> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| event_name(name))
            .collect();
        self.seen.retain(|name, _| names.contains(name));
        self.retries.retain(|name, _| names.contains(name));
        let mut out = Vec::new();
        for name in names {
            self.retries.remove(&name);
            if emit {
                self.read_or_retry(name, 0, Instant::now(), &mut out);
            } else {
                let _ = self.read(&name, false);
            }
        }
        order_events(&mut out);
        Ok(out)
    }

    fn retry_due(&mut self, now: Instant) -> Vec<Event> {
        let due: Vec<_> = self
            .retries
            .iter()
            .filter(|(_, retry)| retry.at <= now)
            .map(|(name, retry)| (name.clone(), retry.attempt))
            .collect();
        let mut out = Vec::new();
        for (name, attempt) in due {
            self.retries.remove(&name);
            self.read_or_retry(name, attempt, now, &mut out);
        }
        order_events(&mut out);
        out
    }
    fn deadline(&self) -> Option<Instant> {
        self.retries.values().map(|retry| retry.at).min()
    }
    fn clear(&mut self) {
        self.seen.clear();
        self.retries.clear();
    }
}

#[cfg(windows)]
pub(super) fn run(
    agent: Agent,
    dir: PathBuf,
    stop: &super::directory::StopEvent,
    tx: &std::sync::mpsc::Sender<Event>,
    status: &std::sync::mpsc::Sender<super::WatchStatus>,
    wake: &impl Fn(),
    #[cfg(test)] stats: std::sync::Arc<Stats>,
) -> Result<(), String> {
    use super::{
        directory::{Monitor, Notice},
        WatchStatus,
    };
    if stop.is_set() {
        return Ok(());
    }
    let path = dir.join(agent.events_dir());
    let mut monitor = Monitor::new(path.clone())?;
    let mut reader = Reader::new(
        agent,
        path,
        #[cfg(test)]
        stats.clone(),
    );
    // 监听先注册，之后的处理期间也有内核缓冲；初始旧记录只建立基线。
    reader.rescan(false)?;
    let report = |state| {
        let _ = status.send(state);
        wake();
    };
    let publish = |events: Vec<Event>, owners: &mut super::owners::Owners| {
        let found = !events.is_empty();
        for event in events {
            let closed = owners.observe(&event);
            if stop.is_set() || tx.send(event).is_err() {
                return false;
            }
            if let Some(closed) = closed {
                if tx.send(closed).is_err() {
                    return false;
                }
            }
        }
        if found {
            wake();
        }
        true
    };
    report(WatchStatus::Listening);
    let mut owners = super::owners::Owners::default();
    loop {
        if stop.is_set() {
            return Ok(());
        }
        if !publish(reader.retry_due(Instant::now()), &mut owners) {
            return Ok(());
        }
        let notice = monitor.wait(stop, reader.deadline(), &owners.handles())?;
        let events = match notice {
            Notice::Stop => return Ok(()),
            Notice::RetryDeadline => continue,
            Notice::ProcessExited(index) => owners.exited(index, agent),
            Notice::Files(names) => {
                #[cfg(test)]
                stats
                    .notifications
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                reader.changed(names, Instant::now())
            }
            Notice::Rescan => reader.rescan(true)?,
            Notice::DirectoryRestored => {
                let events = reader.rescan(true)?;
                report(WatchStatus::Listening);
                events
            }
            Notice::DirectoryMissing => {
                reader.clear();
                owners.clear();
                report(WatchStatus::WaitingForDirectory);
                Vec::new()
            }
        };
        if !publish(events, &mut owners) {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn queued_parallel_events_are_ordered_consumed_and_not_replayed() {
        let temp = Temp::new();
        let mut reader = temp.reader();
        let payload = |kind, call, at| {
            serde_json::json!({"version":2,"agent":"codex","kind":kind,"session":"s1","turn":"t1","call":call,"at":at}).to_string()
        };
        fs::write(temp.0.join("b.event"), payload("tool_start", "b", 20)).unwrap();
        fs::write(temp.0.join("a.event"), payload("permission", "a", 10)).unwrap();
        fs::write(temp.0.join("c.event"), payload("tool_end", "a", 30)).unwrap();
        let names = vec!["b.event".into(), "c.event".into(), "a.event".into()];
        let events = reader.changed(names.clone(), Instant::now());
        assert_eq!(
            events.iter().map(|event| event.meta.at).collect::<Vec<_>>(),
            vec![10, 20, 30]
        );
        assert!(reader.changed(names, Instant::now()).is_empty());
        assert!(reader.retries.is_empty() && reader.seen.is_empty());
        assert_eq!(fs::read_dir(&temp.0).unwrap().count(), 0);
    }
    use std::sync::{atomic::Ordering, Arc};

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "island-directory-测试-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn reader(&self) -> Reader {
            Reader::new(Agent::Codex, self.0.clone(), Arc::new(Stats::default()))
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn record(kind: &str, at: u64) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"agent":"codex","kind":kind,"session":"s1","project":"项目","at":at})).unwrap()
    }

    #[test]
    fn baseline_and_duplicate_files_do_not_replay_and_only_changed_files_are_read() {
        let temp = Temp::new();
        fs::write(temp.0.join("old.json"), record("done", 1)).unwrap();
        let mut reader = temp.reader();
        assert!(reader.rescan(false).unwrap().is_empty());
        let reads = reader.stats.reads.load(Ordering::Relaxed);
        fs::write(temp.0.join("s1.json"), record("working", 2)).unwrap();
        let events = reader.changed(
            vec!["s1.json".into(), "s1.json".into(), "s1.tmp".into()],
            Instant::now(),
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, Kind::Working);
        assert_eq!(reader.stats.reads.load(Ordering::Relaxed), reads + 1);
        assert!(reader
            .changed(vec!["s1.json".into()], Instant::now())
            .is_empty());
        assert_eq!(reader.stats.scans.load(Ordering::Relaxed), 1);
        // 相同状态的新回合仍应通知；去重比较完整记录，包括脚本的 at。
        fs::write(temp.0.join("s1.json"), record("working", 3)).unwrap();
        assert_eq!(
            reader.changed(vec!["s1.json".into()], Instant::now()).len(),
            1
        );
    }

    #[test]
    fn partial_records_retry_finitely_and_recover_when_completed() {
        let temp = Temp::new();
        let mut reader = temp.reader();
        let now = Instant::now();
        fs::write(temp.0.join("s1.json"), b"{\"agent\":").unwrap();
        assert!(reader.changed(vec!["s1.json".into()], now).is_empty());
        assert!(reader.deadline().is_some());
        fs::write(temp.0.join("s1.json"), record("done", 1)).unwrap();
        assert_eq!(
            reader.retry_due(now + Duration::from_millis(20))[0].kind,
            Kind::Done
        );
        assert!(reader.deadline().is_none());
        assert!(reader.changed(vec!["gone.json".into()], now).is_empty());
        for step in 1..=3 {
            assert!(reader.retry_due(now + Duration::from_secs(step)).is_empty());
        }
        assert!(
            reader.deadline().is_none(),
            "耗尽重试后恢复无限等待，不能退化成轮询"
        );
    }

    #[test]
    fn overflow_rescan_recovers_unseen_changes_without_replaying_known_records() {
        let temp = Temp::new();
        let mut reader = temp.reader();
        fs::write(temp.0.join("old.json"), record("done", 1)).unwrap();
        reader.rescan(false).unwrap();
        fs::write(temp.0.join("new.json"), record("question", 2)).unwrap();
        assert_eq!(reader.rescan(true).unwrap().len(), 1);
        assert!(reader.rescan(true).unwrap().is_empty());
    }

    #[test]
    fn unrelated_oversized_or_outside_files_are_ignored() {
        let temp = Temp::new();
        let mut reader = temp.reader();
        fs::write(
            temp.0.join("other.json"),
            br#"{"agent":"claude-code","kind":"done","session":"s1"}"#,
        )
        .unwrap();
        fs::write(temp.0.join("large.json"), vec![b' '; MAX_EVENT_BYTES + 1]).unwrap();
        assert!(reader
            .changed(
                vec![
                    "other.json".into(),
                    "large.json".into(),
                    "x.tmp".into(),
                    "../outside.json".into()
                ],
                Instant::now()
            )
            .is_empty());
        assert!(reader.deadline().is_none());
        assert_eq!(reader.stats.reads.load(Ordering::Relaxed), 2);
    }

    #[cfg(windows)]
    mod native {
        use super::*;
        use crate::agent_hooks::{install, spawn, Service, WatchStatus};

        fn payload(agent: Agent, kind: &str, at: u64) -> Vec<u8> {
            serde_json::to_vec(&serde_json::json!({"agent":agent.key(),"kind":kind,"session":"s1","project":"中文项目","at":at})).unwrap()
        }
        fn wait_status(service: &Service, want: WatchStatus) {
            loop {
                let status = service
                    .status
                    .recv_timeout(Duration::from_secs(3))
                    .expect("目录监听应返回状态");
                assert!(!matches!(status, WatchStatus::Failed(_)), "{status:?}");
                if status == want {
                    return;
                }
            }
        }
        fn start(agent: Agent, dir: &Path) -> Service {
            let service = spawn(agent, dir.to_owned(), || {}).unwrap();
            wait_status(&service, WatchStatus::Listening);
            service
        }
        fn stop(mut service: Service) {
            let (_, empty) = std::sync::mpsc::channel();
            let finished = std::mem::replace(&mut service.finished, empty);
            drop(service);
            finished
                .recv_timeout(Duration::from_secs(3))
                .expect("关闭服务必须取消挂起 I/O 并退出");
        }
        fn received(service: &Service, kind: Kind) -> Event {
            let event = service
                .rx
                .recv_timeout(Duration::from_secs(3))
                .expect("系统文件变化应触发提醒");
            assert_eq!(event.kind, kind);
            event
        }

        #[test]
        fn assistant_process_exit_ends_sessions_without_periodic_scanning() {
            use std::os::windows::{io::AsRawHandle, process::CommandExt};
            use windows::Win32::{
                Foundation::{FILETIME, HANDLE},
                System::Threading::GetProcessTimes,
            };
            struct Child(std::process::Child);
            impl Drop for Child {
                fn drop(&mut self) {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
            let mut child = Child(
                std::process::Command::new("powershell.exe")
                    .args([
                        "-NoProfile",
                        "-NonInteractive",
                        "-WindowStyle",
                        "Hidden",
                        "-Command",
                        "[Threading.Thread]::Sleep(30000)",
                    ])
                    .creation_flags(0x08000000)
                    .spawn()
                    .unwrap(),
            );
            let (mut created, mut exit, mut kernel, mut user) = (
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
            );
            unsafe {
                GetProcessTimes(
                    HANDLE(child.0.as_raw_handle()),
                    &mut created,
                    &mut exit,
                    &mut kernel,
                    &mut user,
                )
            }
            .unwrap();
            let started = (((u64::from(created.dwHighDateTime) << 32)
                | u64::from(created.dwLowDateTime))
                / 10_000)
                - 11_644_473_600_000;
            let temp = Temp::new();
            let service = start(Agent::Codex, &temp.0);
            let dir = temp.0.join(Agent::Codex.events_dir());
            for (index, session) in ["one", "two"].into_iter().enumerate() {
                let event = serde_json::json!({"agent":"codex","kind":"working","session":session,"at":index+1,"owner_pid":child.0.id(),"owner_started":started});
                fs::write(dir.join(format!("{session}.event")), event.to_string()).unwrap();
                received(&service, Kind::Working);
            }
            let scans = service.stats.scans.load(Ordering::Relaxed);
            child.0.kill().unwrap();
            child.0.wait().unwrap();
            let mut closed = vec![
                received(&service, Kind::End).session,
                received(&service, Kind::End).session,
            ];
            closed.sort();
            assert_eq!(closed, vec!["one", "two"]);
            assert_eq!(service.stats.scans.load(Ordering::Relaxed), scans);
            stop(service);
        }

        #[test]
        fn idle_listener_does_not_scan_read_or_wake_for_periodic_checks() {
            let temp = Temp::new();
            let dir = temp.0.join(Agent::Codex.events_dir());
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("old.json"), payload(Agent::Codex, "done", 1)).unwrap();
            let service = start(Agent::Codex, &temp.0);
            assert!(
                service.rx.recv_timeout(Duration::from_millis(650)).is_err(),
                "启动前的状态不重放"
            );
            assert_eq!(
                service.stats.scans.load(Ordering::Relaxed),
                1,
                "只有初始扫描"
            );
            assert_eq!(
                service.stats.reads.load(Ordering::Relaxed),
                1,
                "空闲时不再读取旧文件"
            );
            assert_eq!(
                service.stats.notifications.load(Ordering::Relaxed),
                0,
                "没有变化就没有系统事件"
            );
            stop(service);
        }

        #[test]
        fn file_creation_atomic_replacement_and_end_work_for_both_agents() {
            for agent in Agent::ALL {
                let temp = Temp::new();
                let service = start(agent, &temp.0);
                let dir = temp.0.join(agent.events_dir());
                let path = dir.join("中文会话.json");
                fs::write(dir.join("ignored.tmp"), b"partial data").unwrap();
                crate::model::atomic_write(&path, &payload(agent, "working", 1)).unwrap();
                assert_eq!(received(&service, Kind::Working).agent, agent);
                crate::model::atomic_write(&path, &payload(agent, "working", 1)).unwrap();
                assert!(
                    service.rx.recv_timeout(Duration::from_millis(230)).is_err(),
                    "相同记录不重复弹出"
                );
                crate::model::atomic_write(&path, &payload(agent, "question", 2)).unwrap();
                received(&service, Kind::Question);
                crate::model::atomic_write(&path, &payload(agent, "end", 3)).unwrap();
                received(&service, Kind::End);
                assert!(!path.exists());
                assert_eq!(
                    service.stats.scans.load(Ordering::Relaxed),
                    1,
                    "普通事件只读变化文件"
                );
                stop(service);
            }
        }

        #[test]
        fn deleted_state_directory_recovers_from_parent_notification() {
            let temp = Temp::new();
            let service = start(Agent::Codex, &temp.0);
            let dir = temp.0.join(Agent::Codex.events_dir());
            fs::remove_dir_all(&dir).unwrap();
            wait_status(&service, WatchStatus::WaitingForDirectory);
            let scans = service.stats.scans.load(Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(350));
            assert_eq!(
                service.stats.scans.load(Ordering::Relaxed),
                scans,
                "目录缺失时也不轮询"
            );
            fs::create_dir_all(&dir).unwrap();
            // 不等待重新挂接，验证重建到订阅之间产生的状态能由重同步补上。
            fs::write(dir.join("s1.json"), payload(Agent::Codex, "question", 1)).unwrap();
            received(&service, Kind::Question);
            wait_status(&service, WatchStatus::Listening);
            stop(service);
        }

        #[test]
        fn dropping_a_blocked_listener_cancels_io_and_allows_reenable() {
            let temp = Temp::new();
            for _ in 0..4 {
                let service = start(Agent::Codex, &temp.0);
                stop(service);
            }
        }

        #[test]
        fn invalid_directory_reports_failure_and_exits_instead_of_busy_retrying() {
            let temp = Temp::new();
            fs::write(temp.0.join(Agent::Codex.events_dir()), b"not a directory").unwrap();
            let service = spawn(Agent::Codex, temp.0.clone(), || {}).unwrap();
            let status = service.status.recv_timeout(Duration::from_secs(3)).unwrap();
            assert!(matches!(status, WatchStatus::Failed(_)));
            stop(service);
        }

        #[test]
        fn installed_powershell_hooks_reach_the_native_directory_listener() {
            use std::{
                io::Write,
                os::windows::process::CommandExt,
                process::{Command, Stdio},
            };
            for agent in Agent::ALL {
                let temp = Temp::new();
                let dir = temp.0.join("带 空格 ' 的配置");
                fs::create_dir_all(&dir).unwrap();
                install(agent, &dir).unwrap();
                let service = start(agent, &dir);
                let tool = if agent == Agent::Codex {
                    "request_user_input_async"
                } else {
                    "AskUserQuestion"
                };
                let input = serde_json::json!({"hook_event_name":"PreToolUse","session_id":"s1","tool_name":tool,"cwd":"D:\\工作\\中文项目"});
                let mut child = Command::new("powershell.exe")
                    .args([
                        "-NoProfile",
                        "-NonInteractive",
                        "-WindowStyle",
                        "Hidden",
                        "-ExecutionPolicy",
                        "Bypass",
                        "-File",
                    ])
                    .arg(dir.join(agent.script()))
                    .creation_flags(0x08000000)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(input.to_string().as_bytes())
                    .unwrap();
                let output = child.wait_with_output().unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(output.stdout.is_empty() && output.stderr.is_empty());
                let event = received(
                    &service,
                    if agent == Agent::Codex {
                        Kind::QuestionAsync
                    } else {
                        Kind::Question
                    },
                );
                assert_eq!((event.agent, event.project.as_str()), (agent, "中文项目"));
                stop(service);
            }
        }
    }
}
