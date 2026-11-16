//! 可取消的 Windows 目录变更通知。缓冲区和 OVERLAPPED 在 I/O 完成前始终有效。
use std::cell::UnsafeCell;
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::time::Instant;
use windows::core::{Error, HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, ERROR_NOTIFY_ENUM_DIR, HANDLE, WAIT_FAILED,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED,
    FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE, FILE_NOTIFY_CHANGE_DIR_NAME,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Threading::{
    CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects, WaitForSingleObject, INFINITE,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

const BUFFER_WORDS: usize = 4096;

fn raw(handle: &OwnedHandle) -> HANDLE {
    HANDLE(handle.as_raw_handle())
}

fn event() -> Result<OwnedHandle, String> {
    let handle = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
        .map_err(|error| format!("无法创建目录监听事件：{error}"))?;
    // CreateEventW 成功后将唯一的关闭责任交给 OwnedHandle。
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) })
}

pub(super) struct StopEvent(OwnedHandle);
impl StopEvent {
    pub(super) fn new() -> Result<Self, String> {
        event().map(Self)
    }
    pub(super) fn signal(&self) {
        let _ = unsafe { SetEvent(raw(&self.0)) };
    }
    pub(super) fn is_set(&self) -> bool {
        unsafe { WaitForSingleObject(raw(&self.0), 0) == WAIT_OBJECT_0 }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Batch {
    Names(Vec<OsString>),
    Rescan,
}

/// FILE_NOTIFY_INFORMATION 的名字是 UTF-16，偏移量均按字节计算。
/// 溢出、截断或不合法的偏移都转为一次重同步，不访问缓冲区外的内存。
fn decode(bytes: &[u8]) -> Batch {
    if bytes.is_empty() {
        return Batch::Rescan;
    }
    let mut names = Vec::new();
    let mut offset = 0usize;
    loop {
        let Some(header) = bytes.get(offset..offset.saturating_add(12)) else {
            return Batch::Rescan;
        };
        let next = u32::from_le_bytes(header[0..4].try_into().unwrap()) as usize;
        let action = u32::from_le_bytes(header[4..8].try_into().unwrap());
        let length = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
        if length == 0 || length % 2 != 0 {
            return Batch::Rescan;
        }
        let end = offset.saturating_add(12).saturating_add(length);
        let Some(name) = bytes.get(offset + 12..end) else {
            return Batch::Rescan;
        };
        if !(1..=5).contains(&action) {
            return Batch::Rescan;
        }
        let wide: Vec<u16> = name
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        names.push(OsString::from_wide(&wide));
        if next == 0 {
            break;
        }
        if next % 4 != 0 || next < 12 + length || offset.saturating_add(next) >= bytes.len() {
            return Batch::Rescan;
        }
        offset += next;
    }
    Batch::Names(names)
}

struct ChangeWatch {
    directory: OwnedHandle,
    ready: OwnedHandle,
    // 堆分配保证地址稳定；UnsafeCell 允许内核在异步 I/O 中写入。
    overlapped: Box<UnsafeCell<OVERLAPPED>>,
    // u32 数组保证 ReadDirectoryChangesW 要求的 DWORD 对齐。
    buffer: Box<UnsafeCell<[u32; BUFFER_WORDS]>>,
    filter: FILE_NOTIFY_CHANGE,
    pending: bool,
}
impl ChangeWatch {
    fn open(path: &Path, filter: FILE_NOTIFY_CHANGE) -> Result<Self, String> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                FILE_LIST_DIRECTORY.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                HANDLE::default(),
            )
        }
        .map_err(|error| format!("无法打开状态目录监听：{error}"))?;
        let directory = unsafe { OwnedHandle::from_raw_handle(handle.0) };
        let ready = event()?;
        let overlapped = OVERLAPPED {
            hEvent: raw(&ready),
            ..Default::default()
        };
        let mut watcher = Self {
            directory,
            ready,
            overlapped: Box::new(UnsafeCell::new(overlapped)),
            buffer: Box::new(UnsafeCell::new([0; BUFFER_WORDS])),
            filter,
            pending: false,
        };
        watcher.arm()?;
        Ok(watcher)
    }

    fn arm(&mut self) -> Result<(), String> {
        unsafe { ResetEvent(raw(&self.ready)) }
            .map_err(|error| format!("无法重置目录事件：{error}"))?;
        let result = unsafe {
            ReadDirectoryChangesW(
                raw(&self.directory),
                self.buffer.get().cast(),
                (BUFFER_WORDS * 4) as u32,
                false,
                self.filter,
                None,
                Some(self.overlapped.get()),
                None,
            )
        };
        match result {
            Ok(()) => self.pending = true,
            Err(error) if error.code() == HRESULT::from_win32(ERROR_IO_PENDING.0) => {
                self.pending = true
            }
            Err(error) => return Err(format!("无法提交目录监听：{error}")),
        }
        Ok(())
    }

    fn complete(&mut self) -> Result<Batch, String> {
        let mut count = 0u32;
        let result = unsafe {
            GetOverlappedResult(
                raw(&self.directory),
                self.overlapped.get(),
                &mut count,
                false,
            )
        };
        // 即使收到异常的过早唤醒，也必须在 Drop 中取消尚未完成的 I/O。
        self.pending = result
            .as_ref()
            .err()
            .is_some_and(|error| error.code() == HRESULT::from_win32(ERROR_IO_INCOMPLETE.0));
        let batch = match result {
            Ok(()) if count as usize <= BUFFER_WORDS * 4 => {
                // I/O 已完成，这时才能读取缓冲区。重新 arm 之前复制出所有文件名。
                let bytes = unsafe {
                    std::slice::from_raw_parts(self.buffer.get().cast::<u8>(), count as usize)
                };
                decode(bytes)
            }
            Ok(()) => Batch::Rescan,
            Err(error) if error.code() == HRESULT::from_win32(ERROR_NOTIFY_ENUM_DIR.0) => {
                Batch::Rescan
            }
            Err(error) => return Err(format!("目录监听已中断：{error}")),
        };
        // 先继续接收下一批变化，再让上层读取 JSON，避免处理期间出现监听空档。
        self.arm()?;
        Ok(batch)
    }
}
impl Drop for ChangeWatch {
    fn drop(&mut self) {
        if self.pending {
            unsafe {
                let _ = CancelIoEx(raw(&self.directory), Some(self.overlapped.get()));
                // CancelIoEx 只请求取消；必须等到完成，才能释放内核仍在使用的内存。
                let mut ignored = 0;
                let _ = GetOverlappedResult(
                    raw(&self.directory),
                    self.overlapped.get(),
                    &mut ignored,
                    true,
                );
            }
        }
    }
}

pub(super) enum Notice {
    Files(Vec<OsString>),
    Rescan,
    DirectoryRestored,
    DirectoryMissing,
    RetryDeadline,
    ProcessExited(usize),
    Stop,
}

pub(super) struct Monitor {
    path: PathBuf,
    name: OsString,
    parent: ChangeWatch,
    child: Option<ChangeWatch>,
}
impl Monitor {
    pub(super) fn new(path: PathBuf) -> Result<Self, String> {
        let parent_path = path.parent().ok_or("状态目录缺少父目录")?;
        let name = path.file_name().ok_or("状态目录缺少目录名")?.to_owned();
        // 初次启用可以创建空目录；之后若目录被删除，由父目录通知等待脚本重建。
        std::fs::create_dir_all(&path).map_err(|error| format!("无法创建状态目录：{error}"))?;
        let parent = ChangeWatch::open(parent_path, FILE_NOTIFY_CHANGE_DIR_NAME)?;
        let child = ChangeWatch::open(&path, Self::file_filter())?;
        Ok(Self {
            path,
            name,
            parent,
            child: Some(child),
        })
    }

    fn file_filter() -> FILE_NOTIFY_CHANGE {
        FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_SIZE
    }

    fn reopen(&mut self) -> Result<Notice, String> {
        self.child = None;
        match std::fs::metadata(&self.path) {
            Ok(meta) if meta.is_dir() => {
                self.child = Some(ChangeWatch::open(&self.path, Self::file_filter())?);
                Ok(Notice::DirectoryRestored)
            }
            Ok(_) => Err("状态目录被替换为文件，请恢复目录后重新开启提醒".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(Notice::DirectoryMissing)
            }
            Err(error) => Err(format!("无法重新打开状态目录：{error}")),
        }
    }

    pub(super) fn wait(
        &mut self,
        stop: &StopEvent,
        deadline: Option<Instant>,
        processes: &[HANDLE],
    ) -> Result<Notice, String> {
        let mut handles = vec![raw(&stop.0), raw(&self.parent.ready)];
        if let Some(child) = &self.child {
            handles.push(raw(&child.ready));
        }
        let process_start = handles.len();
        handles.extend_from_slice(processes);
        let timeout = deadline.map_or(INFINITE, |at| {
            let remaining = at.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                0
            } else {
                remaining.as_millis().clamp(1, (INFINITE - 1) as u128) as u32
            }
        });
        let result = unsafe { WaitForMultipleObjects(&handles, false, timeout) };
        if result == WAIT_OBJECT_0 {
            return Ok(Notice::Stop);
        }
        if result == WAIT_TIMEOUT {
            return Ok(Notice::RetryDeadline);
        }
        if result == WAIT_FAILED {
            return Err(format!("目录事件等待失败：{}", Error::from_win32()));
        }
        let index = (result.0 - WAIT_OBJECT_0.0) as usize;
        if index >= process_start && index < handles.len() {
            return Ok(Notice::ProcessExited(index - process_start));
        }
        match result.0 - WAIT_OBJECT_0.0 {
            1 => {
                let relevant = match self.parent.complete()? {
                    Batch::Rescan => true,
                    Batch::Names(names) => names.iter().any(|name| {
                        name.to_string_lossy()
                            .eq_ignore_ascii_case(&self.name.to_string_lossy())
                    }),
                };
                if relevant {
                    self.reopen()
                } else {
                    Ok(Notice::Files(Vec::new()))
                }
            }
            2 => match self.child.as_mut().ok_or("状态目录监听已关闭")?.complete() {
                Ok(Batch::Names(names)) => Ok(Notice::Files(names)),
                Ok(Batch::Rescan) => Ok(Notice::Rescan),
                // 删除目录时子监听可能先返回错误；父监听仍能捕获之后的重建。
                Err(_) if !self.path.exists() => self.reopen(),
                Err(error) => Err(error),
            },
            _ => Err("收到未知的目录等待结果".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, next: u32) -> Vec<u8> {
        let wide: Vec<u16> = name.encode_utf16().collect();
        let mut bytes = next.to_le_bytes().to_vec();
        bytes.extend(5u32.to_le_bytes());
        bytes.extend(((wide.len() * 2) as u32).to_le_bytes());
        for ch in wide {
            bytes.extend(ch.to_le_bytes());
        }
        if next > 0 {
            bytes.resize(next as usize, 0);
        }
        bytes
    }

    #[test]
    fn native_batch_decodes_unicode_names_and_rename_entries() {
        let mut bytes = entry("a.json", 24);
        bytes.extend(entry("中文.json", 0));
        assert_eq!(
            decode(&bytes),
            Batch::Names(vec!["a.json".into(), "中文.json".into()])
        );
    }

    #[test]
    fn overflow_and_malformed_batches_request_a_rescan() {
        assert_eq!(decode(&[]), Batch::Rescan);
        assert_eq!(decode(&[0; 8]), Batch::Rescan);
        let mut odd = entry("x.json", 0);
        odd[8..12].copy_from_slice(&3u32.to_le_bytes());
        assert_eq!(decode(&odd), Batch::Rescan);
        let mut truncated = entry("x.json", 0);
        truncated.pop();
        assert_eq!(decode(&truncated), Batch::Rescan);
        let mut backwards = entry("x.json", 0);
        backwards[..4].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(decode(&backwards), Batch::Rescan);
    }
}
