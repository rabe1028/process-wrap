#![cfg(all(windows, feature = "owned-job"))]

use process_wrap::std::OwnedJobChild;
use std::{
	io::{self, Read},
	process::{Command, Stdio},
	time::{Duration, Instant},
};
use windows::Win32::{
	Foundation::{CloseHandle, WAIT_OBJECT_0},
	System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
};

const MODE: &str = "PROCESS_WRAP_OWNED_JOB_TEST_MODE";
const MARKER: &str = "PROCESS_WRAP_OWNED_JOB_TEST_MARKER";

fn command(mode: &str, marker: &std::path::Path) -> io::Result<Command> {
	let mut command = Command::new(std::env::current_exe()?);
	command
		.args(["--exact", "fixture", "--ignored", "--nocapture"])
		.env(MODE, mode)
		.env(MARKER, marker)
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped());
	Ok(command)
}

#[test]
#[ignore = "controlled process fixture"]
fn fixture() -> io::Result<()> {
	let mode = std::env::var(MODE).map_err(io::Error::other)?;
	let marker = std::path::PathBuf::from(
		std::env::var_os(MARKER).ok_or_else(|| io::Error::other("marker"))?,
	);
	match mode.as_str() {
		"short" | "long" => {
			let deadline = Instant::now() + Duration::from_secs(10);
			while !marker.with_extension("release").exists() {
				assert!(Instant::now() < deadline);
				std::thread::sleep(Duration::from_millis(5));
			}
			std::thread::sleep(if mode == "short" {
				Duration::from_millis(300)
			} else {
				Duration::from_secs(30)
			});
			std::fs::write(marker, b"complete")?;
		}
		"parent-short" | "parent-long" => {
			let mut child = command(
				if mode == "parent-short" {
					"short"
				} else {
					"long"
				},
				&marker,
			)?;
			child.stdout(Stdio::null()).stderr(Stdio::null());
			let descendant = child.spawn()?;
			std::fs::write(marker.with_extension("pid"), descendant.id().to_string())?;
		}
		_ => return Err(io::Error::other("fixture mode")),
	}
	Ok(())
}

fn drain(reader: &mut impl Read) -> io::Result<bool> {
	let mut buffer = [0; 1024];
	match reader.read(&mut buffer) {
		Ok(0) => Ok(true),
		Ok(_) => Ok(false),
		Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(false),
		Err(error) => Err(error),
	}
}

#[test]
fn completion_requires_signaled_descendant_and_preserves_short_work() -> io::Result<()> {
	for should_kill in [false, true] {
		let dir = tempfile::tempdir()?;
		let marker = dir.path().join("result");
		let mut child = OwnedJobChild::spawn(command(
			if should_kill {
				"parent-long"
			} else {
				"parent-short"
			},
			&marker,
		)?)?;
		assert!(child.take_initialization_error().is_none());
		let (mut stdout, mut stderr) = child.take_pipes()?;
		let deadline = Instant::now() + Duration::from_secs(10);
		while !marker.with_extension("pid").exists() {
			assert!(Instant::now() < deadline);
			let _ = drain(&mut stdout)?;
			let _ = drain(&mut stderr)?;
			std::thread::sleep(Duration::from_millis(5));
		}
		let pid: u32 = std::fs::read_to_string(marker.with_extension("pid"))?
			.parse()
			.map_err(io::Error::other)?;
		// SAFETY: the returned handle is owned here, held through the completion
		// assertion, and closed once below. This observes the same incarnation.
		let handle =
			unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }.map_err(io::Error::other)?;
		std::fs::write(marker.with_extension("release"), b"ready")?;
		if should_kill {
			child.start_kill()?;
		}
		loop {
			assert!(Instant::now() < deadline);
			let _ = drain(&mut stdout)?;
			let _ = drain(&mut stderr)?;
			if child.try_wait()?.is_some() {
				break;
			}
			std::thread::sleep(Duration::from_millis(5));
		}
		let signaled = unsafe { WaitForSingleObject(handle, 0) };
		unsafe { CloseHandle(handle) }.map_err(io::Error::other)?;
		assert_eq!(
			signaled, WAIT_OBJECT_0,
			"Job completion must mean descendant exit"
		);
		assert_eq!(marker.exists(), !should_kill);
	}
	Ok(())
}

#[test]
fn open_empty_pipe_never_starts_a_blocking_reader() -> io::Result<()> {
	let dir = tempfile::tempdir()?;
	let mut child = OwnedJobChild::spawn(command("long", &dir.path().join("result"))?)?;
	assert!(child.take_initialization_error().is_none());
	let (mut stdout, mut stderr) = child.take_pipes()?;
	let start = Instant::now();
	for _ in 0..100 {
		let _ = drain(&mut stdout)?;
		let _ = drain(&mut stderr)?;
	}
	assert!(start.elapsed() < Duration::from_secs(2));
	child.start_kill()?;
	let deadline = Instant::now() + Duration::from_secs(5);
	while child.try_wait()?.is_none() {
		assert!(Instant::now() < deadline);
		std::thread::sleep(Duration::from_millis(5));
	}
	Ok(())
}
