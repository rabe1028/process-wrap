//! A Windows process owner for callers that implement finite polling budgets.

use std::{
	io::{self, Read},
	os::windows::{io::AsRawHandle, process::CommandExt},
	process::{Child, ChildStderr, ChildStdout, Command, ExitStatus},
};

use windows::{
	Win32::{
		Foundation::{ERROR_BROKEN_PIPE, HANDLE},
		System::{
			JobObjects::{
				JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
				QueryInformationJobObject,
			},
			Pipes::PeekNamedPipe,
			Threading::CREATE_SUSPENDED,
		},
	},
	core::HRESULT,
};

use crate::windows::{JobPort, make_job_object, resume_threads, terminate_job};

/// Owns the leader and a kill-on-close Job until confirmed completion.
///
/// There are no blocking wait/read tasks. Failures after process creation retain
/// the suspended child here; callers must inspect `take_initialization_error`
/// and perform their own finite cleanup. Never discard an unconfirmed cleanup
/// as a successful operation. Dropping requests termination without waiting.
#[derive(Debug)]
pub struct OwnedJobChild {
	child: Child,
	job: Option<JobPort>,
	initialization_error: Option<io::Error>,
	completed: bool,
}

impl OwnedJobChild {
	/// Spawn with no extra Windows creation flags. Use `spawn_with_flags` if the
	/// command needs flags; native Command flags are replaced by this method.
	pub fn spawn(command: Command) -> io::Result<Self> {
		Self::spawn_with_flags(command, 0)
	}

	/// Atomically keep a new child suspended until it is assigned to its Job.
	/// `CREATE_SUSPENDED` in `flags` retains explicit caller-requested suspension.
	pub fn spawn_with_flags(mut command: Command, flags: u32) -> io::Result<Self> {
		command.creation_flags(flags | CREATE_SUSPENDED.0);
		let child = command.spawn()?;
		let handle = HANDLE(child.as_raw_handle());
		let mut owner = Self {
			child,
			job: None,
			initialization_error: None,
			completed: false,
		};
		match make_job_object(handle, true) {
			Ok(job) => owner.job = Some(job),
			Err(error) => {
				owner.initialization_error = Some(error);
				return Ok(owner);
			}
		}
		if flags & CREATE_SUSPENDED.0 == 0 {
			if let Err(error) = resume_threads(handle) {
				owner.initialization_error = Some(error);
			}
		}
		Ok(owner)
	}

	/// Take a post-spawn Job assignment/resume failure without losing ownership.
	pub fn take_initialization_error(&mut self) -> Option<io::Error> {
		self.initialization_error.take()
	}

	/// Move the exclusive stdout and stderr readers into nonblocking pollers.
	pub fn take_pipes(&mut self) -> io::Result<(NonblockingPipe, NonblockingPipe)> {
		let stdout = self
			.child
			.stdout
			.take()
			.ok_or_else(|| io::Error::other("missing stdout"))?;
		let stderr = self
			.child
			.stderr
			.take()
			.ok_or_else(|| io::Error::other("missing stderr"))?;
		Ok((
			NonblockingPipe(Pipe::Stdout(stdout)),
			NonblockingPipe(Pipe::Stderr(stderr)),
		))
	}

	/// Request termination; this does not establish that the Job is empty.
	pub fn start_kill(&mut self) -> io::Result<()> {
		if self.completed {
			return Ok(());
		}
		match &self.job {
			Some(job) => terminate_job(job.job, 1),
			// Failed assignment left the leader suspended, so it cannot have
			// created descendants outside the Job.
			None => self.child.kill(),
		}
	}

	/// Report completion only after leader exit AND zero active Job processes.
	pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
		let Some(status) = self.child.try_wait()? else {
			return Ok(None);
		};
		if let Some(job) = &self.job {
			let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
			// SAFETY: the Job handle is owned here; accounting points to a valid
			// writable buffer of exactly the size supplied for this query class.
			unsafe {
				QueryInformationJobObject(
					Some(job.job.0),
					JobObjectBasicAccountingInformation,
					(&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
					std::mem::size_of_val(&accounting)
						.try_into()
						.map_err(io::Error::other)?,
					None,
				)
			}
			.map_err(io::Error::other)?;
			if accounting.ActiveProcesses != 0 {
				return Ok(None);
			}
		}
		self.completed = true;
		Ok(Some(status))
	}
}

impl Drop for OwnedJobChild {
	fn drop(&mut self) {
		let _ = self.start_kill();
		// JobPort closes the kill-on-close Job. No wait is hidden in Drop.
	}
}

#[derive(Debug)]
enum Pipe {
	Stdout(ChildStdout),
	Stderr(ChildStderr),
}

/// Exclusive child-pipe reader. Reads only bytes reported currently available.
/// No raw handle or inner reader is exposed, so another reader cannot consume
/// those bytes between PeekNamedPipe and Read. Empty open pipes return WouldBlock.
#[derive(Debug)]
pub struct NonblockingPipe(Pipe);

impl Read for NonblockingPipe {
	fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
		if buffer.is_empty() {
			return Ok(0);
		}
		let handle = HANDLE(match &self.0 {
			Pipe::Stdout(pipe) => pipe.as_raw_handle(),
			Pipe::Stderr(pipe) => pipe.as_raw_handle(),
		});
		let mut available = 0_u32;
		// SAFETY: the exclusive pipe handle remains owned throughout both calls;
		// available is writable and no optional output buffer is supplied.
		match unsafe { PeekNamedPipe(handle, None, 0, None, Some(&mut available), None) } {
			Ok(()) => {}
			Err(error) if error.code() == HRESULT::from_win32(ERROR_BROKEN_PIPE.0) => return Ok(0),
			Err(error) => return Err(io::Error::other(error)),
		}
		if available == 0 {
			return Err(io::ErrorKind::WouldBlock.into());
		}
		let count = buffer
			.len()
			.min(usize::try_from(available).map_err(io::Error::other)?);
		let buffer = &mut buffer[..count];
		match &mut self.0 {
			Pipe::Stdout(pipe) => pipe.read(buffer),
			Pipe::Stderr(pipe) => pipe.read(buffer),
		}
	}
}
