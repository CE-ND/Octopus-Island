use octopus_island::model::{config_dir, initialize_data_dir, Settings};
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

static ENVIRONMENT: Mutex<()> = Mutex::new(());

struct Fixture {
    root: PathBuf,
    previous: Vec<(&'static str, Option<OsString>)>,
    _guard: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let guard = ENVIRONMENT.lock().unwrap();
        let root = std::env::temp_dir().join(format!("octopus-island-data-directory-{}-{name}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let previous = ["USERPROFILE", "HOME", "APPDATA", "OCTOPUS_ISLAND_DATA_DIR"]
            .into_iter().map(|key| (key, std::env::var_os(key))).collect();
        std::env::set_var("USERPROFILE", root.join("profile"));
        std::env::set_var("HOME", root.join("home"));
        std::env::set_var("APPDATA", root.join("roaming"));
        std::env::remove_var("OCTOPUS_ISLAND_DATA_DIR");
        Self { root, previous, _guard: guard }
    }

    fn legacy(&self) -> PathBuf { self.root.join("roaming/OctopusIsland") }
    fn destination(&self) -> PathBuf { self.root.join("profile/.octopus_island") }

    fn write_legacy(&self) {
        fs::create_dir_all(self.legacy().join("credentials")).unwrap();
        fs::write(self.legacy().join("settings.txt"), "pet=0\nskin=cream\ncodex_notifications=1\n").unwrap();
        fs::write(self.legacy().join("credentials/session.bin"), [0, 1, 128, 255]).unwrap();
        fs::write(self.legacy().join("本地日程.json"), "{\"测试\":true}").unwrap();
        fs::create_dir(self.legacy().join("empty-cache")).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for (key, value) in &self.previous {
            if let Some(value) = value { std::env::set_var(key, value); }
            else { std::env::remove_var(key); }
        }
        // 只清理由本测试在临时目录中创建的树。
        if let (Ok(root), Ok(temporary)) = (self.root.canonicalize(), std::env::temp_dir().canonicalize()) {
            if root.parent() == Some(temporary.as_path()) && root.file_name() == self.root.file_name() {
                let _ = fs::remove_dir_all(root);
            }
        }
    }
}

#[test]
fn fresh_install_uses_the_current_user_profile() {
    let fixture = Fixture::new("fresh");
    assert_eq!(config_dir(), fixture.destination());
    initialize_data_dir().unwrap();
    assert!(fixture.destination().is_dir());
    assert!(!fixture.legacy().exists());
}

#[test]
fn legacy_data_is_copied_completely_and_future_saves_use_the_new_directory() {
    let fixture = Fixture::new("migration");
    fixture.write_legacy();
    let original = fs::read(fixture.legacy().join("settings.txt")).unwrap();
    initialize_data_dir().unwrap();
    for path in ["settings.txt", "credentials/session.bin", "本地日程.json"] {
        assert_eq!(fs::read(fixture.destination().join(path)).unwrap(), fs::read(fixture.legacy().join(path)).unwrap());
    }
    assert!(fixture.destination().join("empty-cache").is_dir());
    let mut settings = Settings::load();
    assert!(!settings.pet);
    assert!(settings.codex_notifications);
    assert_eq!(settings.skin, octopus_island::model::Skin::Cream);
    settings.pet = true;
    settings.save();
    fs::remove_file(fixture.destination().join("本地日程.json")).unwrap();
    initialize_data_dir().unwrap();
    assert!(Settings::load().pet);
    assert!(!fixture.destination().join("本地日程.json").exists(), "重启不能复活已删除的数据");
    assert_eq!(fs::read(fixture.legacy().join("settings.txt")).unwrap(), original);
    assert_eq!(fs::read_dir(fixture.root.join("profile")).unwrap().count(), 1);
}

#[test]
fn an_existing_destination_is_never_overwritten_or_merged_with_old_data() {
    let fixture = Fixture::new("existing");
    fixture.write_legacy();
    fs::create_dir_all(fixture.destination()).unwrap();
    fs::write(fixture.destination().join("settings.txt"), b"pet=1\n").unwrap();
    initialize_data_dir().unwrap();
    assert_eq!(fs::read(fixture.destination().join("settings.txt")).unwrap(), b"pet=1\n");
    assert!(!fixture.destination().join("credentials").exists());
    assert!(!fixture.destination().join("本地日程.json").exists());
}

#[test]
fn explicit_overrides_remain_isolated_from_daily_data() {
    let fixture = Fixture::new("override");
    fixture.write_legacy();
    let custom = fixture.root.join("custom-development");
    std::env::set_var("OCTOPUS_ISLAND_DATA_DIR", &custom);
    assert_eq!(config_dir(), custom);
    initialize_data_dir().unwrap();
    assert!(custom.is_dir());
    assert!(!custom.join("settings.txt").exists());
    assert!(!fixture.destination().exists());
    assert!(fixture.legacy().join("settings.txt").exists());
}

#[test]
fn an_empty_override_uses_the_default_and_missing_userprofile_can_use_home() {
    let fixture = Fixture::new("fallback");
    std::env::set_var("OCTOPUS_ISLAND_DATA_DIR", "");
    assert_eq!(config_dir(), fixture.destination());
    std::env::remove_var("USERPROFILE");
    assert_eq!(config_dir(), fixture.root.join("home/.octopus_island"));
    initialize_data_dir().unwrap();
    assert!(config_dir().is_dir());
}

#[test]
fn a_file_at_the_destination_is_reported_without_changing_either_copy() {
    let fixture = Fixture::new("blocked-destination");
    fixture.write_legacy();
    fs::create_dir_all(fixture.destination().parent().unwrap()).unwrap();
    fs::write(fixture.destination(), b"existing-file").unwrap();
    assert!(initialize_data_dir().is_err());
    assert_eq!(fs::read(fixture.destination()).unwrap(), b"existing-file");
    assert!(fixture.legacy().join("settings.txt").exists());
}

#[test]
fn an_invalid_legacy_directory_does_not_publish_an_empty_destination() {
    let fixture = Fixture::new("invalid-legacy");
    fs::create_dir_all(fixture.legacy().parent().unwrap()).unwrap();
    fs::write(fixture.legacy(), b"existing-file").unwrap();
    assert!(initialize_data_dir().is_err());
    assert!(!fixture.destination().exists());
    assert_eq!(fs::read(fixture.legacy()).unwrap(), b"existing-file");
    assert_eq!(fs::read_dir(fixture.root.join("profile")).unwrap().count(), 0);
}

#[cfg(windows)]
#[test]
fn a_failed_copy_preserves_the_legacy_data_and_can_be_retried() {
    use std::os::windows::fs::OpenOptionsExt;
    let fixture = Fixture::new("retry");
    fixture.write_legacy();
    let locked = fs::OpenOptions::new().read(true).share_mode(0)
        .open(fixture.legacy().join("credentials/session.bin")).unwrap();
    assert!(initialize_data_dir().is_err());
    assert!(!fixture.destination().exists());
    assert_eq!(fs::read_dir(fixture.root.join("profile")).unwrap().count(), 0);
    assert!(fixture.legacy().join("settings.txt").exists());
    drop(locked);
    initialize_data_dir().unwrap();
    assert_eq!(fs::read(fixture.destination().join("credentials/session.bin")).unwrap(), [0, 1, 128, 255]);
}
