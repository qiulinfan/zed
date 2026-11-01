#[cfg(test)]
use std::sync::LazyLock;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, WPARAM},
        UI::WindowsAndMessaging::PostMessageW,
    },
    core::{PCWSTR, PWSTR},
};

use crate::windows_impl::WM_JOB_UPDATED;

pub(crate) struct Job {
    pub apply: Box<dyn Fn(&Path) -> Result<()> + Send + Sync>,
    pub rollback: Box<dyn Fn(&Path) -> Result<()> + Send + Sync>,
}

impl Job {
    pub fn mkdir(name: &'static Path) -> Self {
        Job {
            apply: Box::new(move |app_dir| {
                let dir = app_dir.join(name);
                std::fs::create_dir_all(&dir)
                    .context(format!("Failed to create directory {}", dir.display()))
            }),
            rollback: Box::new(move |app_dir| {
                let dir = app_dir.join(name);
                std::fs::remove_dir_all(&dir)
                    .context(format!("Failed to remove directory {}", dir.display()))
            }),
        }
    }

    #[cfg(test)]
    pub fn mkdir_if_exists(name: &'static Path, check: &'static Path) -> Self {
        Job {
            apply: Box::new(move |app_dir| {
                let dir = app_dir.join(name);
                let check = app_dir.join(check);

                if check.exists() {
                    std::fs::create_dir_all(&dir)
                        .context(format!("Failed to create directory {}", dir.display()))?
                }
                Ok(())
            }),
            rollback: Box::new(move |app_dir| {
                let dir = app_dir.join(name);

                if dir.exists() {
                    std::fs::remove_dir_all(&dir)
                        .context(format!("Failed to remove directory {}", dir.display()))?
                }

                Ok(())
            }),
        }
    }

    pub fn move_file(filename: &Path, new_filename: &Path) -> Self {
        let source = filename.to_path_buf();
        let destination = new_filename.to_path_buf();
        let rollback_source = source.clone();
        let rollback_destination = destination.clone();
        Job {
            apply: Box::new(move |app_dir| {
                let old_file = app_dir.join(&source);
                let new_file = app_dir.join(&destination);
                log::info!(
                    "Moving file: {}->{}",
                    old_file.display(),
                    new_file.display()
                );

                std::fs::create_dir_all(new_file.parent().context("No destination parent")?)?;
                std::fs::rename(&old_file, new_file)
                    .context(format!("Failed to move file {}", old_file.display()))
            }),
            rollback: Box::new(move |app_dir| {
                let old_file = app_dir.join(&rollback_source);
                let new_file = app_dir.join(&rollback_destination);
                log::info!(
                    "Rolling back file move: {}->{}",
                    old_file.display(),
                    new_file.display()
                );

                std::fs::rename(&new_file, &old_file).context(format!(
                    "Failed to rollback file move {}->{}",
                    new_file.display(),
                    old_file.display()
                ))
            }),
        }
    }

    #[cfg(test)]
    pub fn move_if_exists(filename: &'static Path, new_filename: &'static Path) -> Self {
        Job {
            apply: Box::new(move |app_dir| {
                let old_file = app_dir.join(filename);
                let new_file = app_dir.join(new_filename);

                if old_file.exists() {
                    log::info!(
                        "Moving file: {}->{}",
                        old_file.display(),
                        new_file.display()
                    );

                    std::fs::rename(&old_file, new_file)
                        .context(format!("Failed to move file {}", old_file.display()))?;
                }

                Ok(())
            }),
            rollback: Box::new(move |app_dir| {
                let old_file = app_dir.join(filename);
                let new_file = app_dir.join(new_filename);

                if new_file.exists() {
                    log::info!(
                        "Rolling back file move: {}->{}",
                        old_file.display(),
                        new_file.display()
                    );

                    std::fs::rename(&new_file, &old_file).context(format!(
                        "Failed to rollback file move {}->{}",
                        new_file.display(),
                        old_file.display()
                    ))?
                }

                Ok(())
            }),
        }
    }

    pub fn rmdir_nofail(filename: &'static Path) -> Self {
        Job {
            apply: Box::new(move |app_dir| {
                let filename = app_dir.join(filename);
                log::info!("Removing file: {}", filename.display());
                if let Err(e) = std::fs::remove_dir_all(&filename) {
                    log::warn!("Failed to remove directory: {}", e);
                }

                Ok(())
            }),
            rollback: Box::new(move |app_dir| {
                let filename = app_dir.join(filename);
                anyhow::bail!(
                    "Delete operations cannot be rolled back, file: {}",
                    filename.display()
                )
            }),
        }
    }
}

/// Plan the complete zip layout, preserving the installation directory and unmanaged files.
pub(crate) fn layout_jobs(app_dir: &Path) -> Result<Vec<Job>> {
    let app_dir = app_dir.canonicalize()?;
    for name in ["install", "old", "updates"] {
        anyhow::ensure!(!app_dir.join(name).is_symlink(), "Linked update directory");
    }
    let install = app_dir.join("install");
    anyhow::ensure!(
        install.canonicalize()?.starts_with(&app_dir),
        "Update stage is outside the app"
    );
    for required in ["Zed.exe", "bin/zed.exe", "tools/auto_update_helper.exe"] {
        anyhow::ensure!(
            install.join(required).is_file(),
            "Update is missing {required}"
        );
    }
    fn files(root: &Path, relative: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for entry in std::fs::read_dir(root.join(relative))? {
            let entry = entry?;
            let kind = entry.file_type()?;
            anyhow::ensure!(!kind.is_symlink(), "Update contains a link");
            let child = relative.join(entry.file_name());
            anyhow::ensure!(
                !["old", "install", "updates"]
                    .iter()
                    .any(|name| child.starts_with(name)),
                "Reserved update path"
            );
            if kind.is_dir() {
                files(root, &child, out)?;
            } else if kind.is_file() {
                out.push(child);
            } else {
                anyhow::bail!("Unsupported update entry");
            }
        }
        Ok(())
    }
    let mut entries = Vec::new();
    files(&install, Path::new(""), &mut entries)?;
    entries.sort();
    let old = app_dir.join("old");
    if old.exists() {
        anyhow::ensure!(
            std::fs::read_dir(&old)?.next().is_none(),
            "Previous update backup requires recovery"
        );
        std::fs::remove_dir(&old)?;
    }
    let mut jobs = vec![Job::mkdir(Path::new("old"))];
    for relative in &entries {
        let target = app_dir.join(relative);
        let ancestor = target
            .ancestors()
            .find(|path| path.exists())
            .context("No target ancestor")?;
        anyhow::ensure!(
            ancestor.canonicalize()?.starts_with(&app_dir),
            "Target is outside the installation"
        );
        anyhow::ensure!(!target.is_symlink(), "Linked update target");
        if target.exists() {
            anyhow::ensure!(
                target.is_file() && !target.is_symlink(),
                "Update target is not a regular file"
            );
            jobs.push(Job::move_file(relative, &Path::new("old").join(relative)));
        }
    }
    for relative in &entries {
        jobs.push(Job::move_file(
            &Path::new("install").join(relative),
            relative,
        ));
    }
    jobs.extend([
        Job::rmdir_nofail(Path::new("updates")),
        Job::rmdir_nofail(Path::new("install")),
        Job::rmdir_nofail(Path::new("old")),
    ]);
    Ok(jobs)
}

#[cfg(test)]
pub(crate) static JOBS: LazyLock<[Job; 9]> = LazyLock::new(|| {
    fn p(value: &str) -> &Path {
        Path::new(value)
    }
    [
        Job {
            apply: Box::new(|_| {
                std::thread::sleep(Duration::from_millis(1000));
                if let Ok(config) = std::env::var("ZED_AUTO_UPDATE") {
                    match config.as_str() {
                        "err1" => Err(std::io::Error::other("Simulated error")).context("Anyhow!"),
                        "err2" => Ok(()),
                        _ => panic!("Unknown ZED_AUTO_UPDATE value: {}", config),
                    }
                } else {
                    Ok(())
                }
            }),
            rollback: Box::new(|_| {
                unsafe { std::env::set_var("ZED_AUTO_UPDATE_RB", "rollback1") };
                Ok(())
            }),
        },
        Job::mkdir(p("test1")),
        Job::mkdir_if_exists(p("test_exists"), p("test1")),
        Job::mkdir_if_exists(p("test_missing"), p("dont")),
        Job {
            apply: Box::new(|folder| {
                std::fs::write(folder.join("test1/test"), "test")?;
                Ok(())
            }),
            rollback: Box::new(|folder| {
                std::fs::remove_file(folder.join("test1/test"))?;
                Ok(())
            }),
        },
        Job::move_file(p("test1/test"), p("test1/moved")),
        Job::move_if_exists(p("test1/test"), p("test1/noop")),
        Job {
            apply: Box::new(|_| {
                std::thread::sleep(Duration::from_millis(1000));
                if let Ok(config) = std::env::var("ZED_AUTO_UPDATE") {
                    match config.as_str() {
                        "err1" => Ok(()),
                        "err2" => Err(std::io::Error::other("Simulated error")).context("Anyhow!"),
                        _ => panic!("Unknown ZED_AUTO_UPDATE value: {}", config),
                    }
                } else {
                    Ok(())
                }
            }),
            rollback: Box::new(|_| Ok(())),
        },
        Job::rmdir_nofail(p("test1/nofolder")),
    ]
});

#[allow(clippy::disallowed_methods, reason = "doesn't run in the main binary")]
fn zed_launch_command(app_dir: &Path, launch_arguments: &[OsString]) -> std::process::Command {
    let mut command = std::process::Command::new(app_dir.join("Zed.exe"));
    command.args(launch_arguments);
    command
}

pub(crate) fn perform_jobs(
    app_dir: &Path,
    hwnd: Option<isize>,
    launch: bool,
    launch_arguments: &[OsString],
    jobs: &[Job],
) -> Result<()> {
    let hwnd = hwnd.map(|ptr| HWND(ptr as _));

    let mut last_successful_job = None;
    'outer: for (i, job) in jobs.iter().enumerate() {
        let start = Instant::now();
        loop {
            if start.elapsed().as_secs() > 2 {
                log::error!("Timed out, rolling back");
                break 'outer;
            }
            match (job.apply)(app_dir) {
                Ok(_) => {
                    last_successful_job = Some(i);
                    if let Some(hwnd) = hwnd {
                        unsafe {
                            PostMessageW(
                                Some(hwnd),
                                WM_JOB_UPDATED,
                                WPARAM((i + 1) * 100 / jobs.len()),
                                LPARAM(0),
                            )?
                        };
                    }
                    break;
                }
                Err(err) => match err.downcast_ref::<std::io::Error>() {
                    Some(io_err) => match io_err.kind() {
                        std::io::ErrorKind::NotFound => {
                            log::error!("Operation failed with file not found, aborting: {}", err);
                            break 'outer;
                        }
                        _ => {
                            log::error!("Operation failed (retrying): {}", err);
                            std::thread::sleep(Duration::from_millis(50));
                        }
                    },
                    None => {
                        log::error!("Operation failed with unexpected error, aborting: {}", err);
                        break 'outer;
                    }
                },
            }
        }
    }

    if last_successful_job
        .map(|job| job != jobs.len() - 1)
        .unwrap_or(true)
    {
        let Some(last_successful_job) = last_successful_job else {
            anyhow::bail!("Autoupdate failed, nothing to rollback");
        };

        for job in (0..=last_successful_job).rev() {
            let job = &jobs[job];
            if let Err(e) = (job.rollback)(app_dir) {
                anyhow::bail!(
                    "Job rollback failed, the app might be left in an inconsistent state: ({:?})",
                    e
                );
            }
        }

        anyhow::bail!("Autoupdate failed, rollback successful");
    }

    if launch {
        #[allow(clippy::disallowed_methods, reason = "doesn't run in the main binary")]
        let _child = zed_launch_command(app_dir, launch_arguments)
            .spawn()
            .context("Failed to launch Zed after update")?;
    }
    log::info!("Update completed successfully");
    Ok(())
}

#[cfg(test)]
mod test {
    use std::{ffi::OsString, path::Path};

    use super::{JOBS, perform_jobs, zed_launch_command};

    #[test]
    fn test_zed_launch_command_preserves_arguments() {
        let arguments = vec![
            OsString::from("--user-data-dir"),
            OsString::from(r"C:\Zed Data"),
        ];
        let command = zed_launch_command(Path::new(r"C:\Program Files\Zed"), &arguments);

        assert_eq!(
            command.get_program(),
            Path::new(r"C:\Program Files\Zed\Zed.exe").as_os_str()
        );
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            arguments
                .iter()
                .map(OsString::as_os_str)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_perform_update() {
        let app_dir = tempfile::tempdir().unwrap();
        let app_dir = app_dir.path();
        assert!(perform_jobs(app_dir, None, false, &[], &*JOBS).is_ok());

        let app_dir = tempfile::tempdir().unwrap();
        let app_dir = app_dir.path();
        // Simulate a timeout
        unsafe { std::env::set_var("ZED_AUTO_UPDATE", "err1") };
        let ret = perform_jobs(app_dir, None, false, &[], &*JOBS);
        assert!(
            ret.is_err_and(|e| e.to_string().as_str() == "Autoupdate failed, nothing to rollback")
        );

        let app_dir = tempfile::tempdir().unwrap();
        let app_dir = app_dir.path();
        // Simulate a timeout
        unsafe { std::env::set_var("ZED_AUTO_UPDATE", "err2") };
        let ret = perform_jobs(app_dir, None, false, &[], &*JOBS);
        assert!(
            ret.is_err_and(|e| e.to_string().as_str() == "Autoupdate failed, rollback successful")
        );
        assert!(std::env::var("ZED_AUTO_UPDATE_RB").is_ok_and(|e| e == "rollback1"));
    }
    fn payload(app: &Path) {
        for file in [
            "Zed.exe",
            "bin/zed.exe",
            "tools/auto_update_helper.exe",
            "amd_ags_x64.dll",
        ] {
            let target = app.join("install").join(file);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, format!("new:{file}")).unwrap();
        }
    }

    #[test]
    fn zip_layout_replaces_every_payload_file_and_preserves_unmanaged_files() {
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("Zed with spaces");
        payload(&app);
        std::fs::write(app.join("Zed.exe"), "old").unwrap();
        std::fs::write(app.join("unins000.exe"), "uninstaller").unwrap();
        std::fs::create_dir_all(app.join("updates")).unwrap();
        std::fs::write(app.join("updates/versions.txt"), "ready").unwrap();
        let jobs = super::layout_jobs(&app).unwrap();
        super::perform_jobs(&app, None, false, &[], &jobs).unwrap();
        for file in [
            "Zed.exe",
            "bin/zed.exe",
            "tools/auto_update_helper.exe",
            "amd_ags_x64.dll",
        ] {
            assert_eq!(
                std::fs::read_to_string(app.join(file)).unwrap(),
                format!("new:{file}")
            );
        }
        assert_eq!(
            std::fs::read_to_string(app.join("unins000.exe")).unwrap(),
            "uninstaller"
        );
        assert!(!app.join("updates/versions.txt").exists());
    }

    #[test]
    fn interrupted_layout_swap_rolls_back_the_original_files() {
        let root = tempfile::tempdir().unwrap();
        let app = root.path();
        payload(app);
        std::fs::write(app.join("Zed.exe"), "old").unwrap();
        let jobs = super::layout_jobs(app).unwrap();
        std::fs::remove_file(app.join("install/Zed.exe")).unwrap();
        assert!(super::perform_jobs(app, None, false, &[], &jobs).is_err());
        assert_eq!(std::fs::read_to_string(app.join("Zed.exe")).unwrap(), "old");
        assert!(app.join("install/bin/zed.exe").is_file());
    }

    #[test]
    fn incomplete_layout_never_moves_the_installed_executable() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("install")).unwrap();
        std::fs::write(root.path().join("Zed.exe"), "old").unwrap();
        assert!(super::layout_jobs(root.path()).is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join("Zed.exe")).unwrap(),
            "old"
        );
    }

    #[test]
    fn zip_swap_keeps_a_mapped_old_executable_running() {
        use std::io::{BufRead, BufReader};
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};

        let root = tempfile::tempdir().unwrap();
        let app = root.path();
        payload(app);
        let test_binary = std::env::current_exe().unwrap();
        std::fs::copy(&test_binary, app.join("Zed.exe")).unwrap();
        std::fs::copy(&test_binary, app.join("install/Zed.exe")).unwrap();
        let child = Command::new(app.join("Zed.exe"))
            .args([
                "--exact",
                "updater::test::test_perform_update",
                "--nocapture",
            ])
            .env_remove("ZED_AUTO_UPDATE")
            .current_dir(std::env::temp_dir())
            .creation_flags(0x08000000) // CREATE_NO_WINDOW for the test fixture.
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut child = scopeguard::guard(child, |mut child| {
            child.kill().ok();
            child.wait().ok();
        });
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        while !line.contains("running 1 test") {
            line.clear();
            assert_ne!(output.read_line(&mut line).unwrap(), 0);
        }
        let jobs = super::layout_jobs(app).unwrap();
        super::perform_jobs(app, None, false, &[], &jobs).unwrap();
        assert!(child.try_wait().unwrap().is_none());
        let installed = Command::new(app.join("Zed.exe"))
            .arg("--list")
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(installed.status.success());
        assert!(
            String::from_utf8(installed.stdout)
                .unwrap()
                .contains("zip_swap_keeps_a_mapped_old_executable_running")
        );
    }
}
