//! 用户数据目录与旧版数据迁移；必须在启动后台服务、写日志和加载设置之前初始化。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const DIRECTORY_NAME: &str = ".octopus_island";

fn environment_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).filter(|value| !value.is_empty()).map(PathBuf::from)
}

pub fn config_dir() -> PathBuf {
    environment_path("OCTOPUS_ISLAND_DATA_DIR").unwrap_or_else(|| {
        environment_path("USERPROFILE").or_else(|| environment_path("HOME"))
            .unwrap_or_else(|| PathBuf::from(".")).join(DIRECTORY_NAME)
    })
}

/// 新目录已存在时以其为准；迁移失败返回错误，不能继续启动并保存一份空白设置。
pub fn initialize_data_dir() -> io::Result<()> {
    let destination = config_dir();
    if environment_path("OCTOPUS_ISLAND_DATA_DIR").is_some() || destination.try_exists()? {
        return fs::create_dir_all(destination);
    }
    let Some(source) = environment_path("APPDATA").map(|root| root.join("OctopusIsland")) else {
        return fs::create_dir_all(destination);
    };
    if !source.try_exists()? {
        return fs::create_dir_all(destination);
    }
    migrate(&source, &destination).map_err(|error| io::Error::new(error.kind(), format!(
        "无法将本地数据从 {} 迁移到 {}：{error}", source.display(), destination.display(),
    )))
}

fn migrate(source: &Path, destination: &Path) -> io::Result<()> {
    let parent = destination.parent().ok_or_else(|| io::Error::other("数据目录缺少父目录"))?;
    fs::create_dir_all(parent)?;
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let staging = loop {
        let path = parent.join(format!("{DIRECTORY_NAME}.migrating-{}-{}",
            std::process::id(), SERIAL.fetch_add(1, Ordering::Relaxed)));
        match fs::create_dir(&path) {
            Ok(()) => break path,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let result = copy_directory(source, &staging).and_then(|()| {
        if destination.try_exists()? {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "新数据目录已存在，未覆盖其中的数据"));
        }
        // 临时目录与目标在同一卷；完整复制后才发布，崩溃或失败不会留下半份正式数据。
        fs::rename(&staging, destination)
    });
    if result.is_err() {
        // 清理前核对解析后的父目录和名称，只删除本次创建的临时目录。
        if let (Ok(root), Ok(path)) = (parent.canonicalize(), staging.canonicalize()) {
            if path.parent() == Some(root.as_path()) && path.file_name() == staging.file_name() {
                let _ = fs::remove_dir_all(path);
            }
        }
    }
    result
}

fn copy_directory(source: &Path, destination: &Path) -> io::Result<()> {
    if !fs::symlink_metadata(source)?.is_dir() {
        return Err(io::Error::other("旧数据目录不是普通文件夹"));
    }
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = destination.join(entry.file_name());
        if kind.is_dir() {
            fs::create_dir(&target)?;
            copy_directory(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), target)?;
        } else {
            return Err(io::Error::other("旧数据目录包含链接或特殊文件，请手动迁移后重试"));
        }
    }
    Ok(())
}
