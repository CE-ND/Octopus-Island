//! 与目录事件一起阻塞等待助手进程退出；不轮询 CPU，也不把脚本进程当作助手。
use super::{Agent, Event, Kind, Metadata, Owner};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use windows::Win32::Foundation::{ERROR_INVALID_PARAMETER, FILETIME, HANDLE};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};

struct Process {
    owner: Owner,
    handle: OwnedHandle,
    sessions: Vec<Event>,
}
#[derive(Default)]
pub(super) struct Owners {
    processes: Vec<Process>,
}

impl Owners {
    pub(super) fn handles(&self) -> Vec<HANDLE> {
        self.processes
            .iter()
            .map(|process| HANDLE(process.handle.as_raw_handle()))
            .collect()
    }
    pub(super) fn observe(&mut self, event: &Event) -> Option<Event> {
        let terminal = matches!(
            event.kind,
            Kind::Done | Kind::Idle | Kind::Interrupted | Kind::Failed | Kind::End
        );
        if !terminal && event.meta.owner.is_none() {
            return None;
        }
        if let Some(previous) = self
            .processes
            .iter()
            .flat_map(|process| &process.sessions)
            .find(|previous| previous.session == event.session)
        {
            if event.meta.at != 0 && event.meta.at < previous.meta.at {
                return None;
            }
            if terminal
                && !event.meta.turn.is_empty()
                && !previous.meta.turn.is_empty()
                && event.meta.turn != previous.meta.turn
            {
                return None;
            }
        }
        let mut gone = false;
        for process in &mut self.processes {
            process
                .sessions
                .retain(|previous| previous.session != event.session);
        }
        if !terminal {
            if let Some(owner) = event.meta.owner {
                let index = self
                    .processes
                    .iter()
                    .position(|process| process.owner == owner)
                    .or_else(|| {
                        if self.processes.len() >= 60 {
                            return None;
                        }
                        let handle = match unsafe {
                            OpenProcess(
                                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                                false,
                                owner.pid,
                            )
                        } {
                            Ok(handle) => handle,
                            Err(error) => {
                                gone = error.code()
                                    == windows::core::HRESULT::from_win32(
                                        ERROR_INVALID_PARAMETER.0,
                                    );
                                return None;
                            }
                        };
                        let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
                        let (mut creation, mut exit, mut kernel, mut user) = (
                            FILETIME::default(),
                            FILETIME::default(),
                            FILETIME::default(),
                            FILETIME::default(),
                        );
                        unsafe {
                            GetProcessTimes(
                                HANDLE(handle.as_raw_handle()),
                                &mut creation,
                                &mut exit,
                                &mut kernel,
                                &mut user,
                            )
                        }
                        .ok()?;
                        let ticks = (u64::from(creation.dwHighDateTime) << 32)
                            | u64::from(creation.dwLowDateTime);
                        let started = (ticks / 10_000).checked_sub(11_644_473_600_000)?;
                        if started != owner.started {
                            gone = true;
                            return None;
                        }
                        self.processes.push(Process {
                            owner,
                            handle,
                            sessions: Vec::new(),
                        });
                        Some(self.processes.len() - 1)
                    });
                if let Some(index) = index {
                    self.processes[index].sessions.push(event.clone());
                }
            }
        }
        self.processes
            .retain(|process| !process.sessions.is_empty());
        gone.then(|| Event {
            kind: Kind::End,
            tool: String::new(),
            ..event.clone()
        })
    }
    pub(super) fn exited(&mut self, index: usize, agent: Agent) -> Vec<Event> {
        if index >= self.processes.len() {
            return Vec::new();
        }
        self.processes
            .remove(index)
            .sessions
            .into_iter()
            .map(|previous| Event {
                agent,
                kind: Kind::End,
                tool: String::new(),
                session: previous.session,
                project: previous.project,
                meta: Metadata {
                    turn: previous.meta.turn,
                    ..Default::default()
                },
            })
            .collect()
    }
    pub(super) fn clear(&mut self) {
        self.processes.clear();
    }
}
