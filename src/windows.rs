//! Windows asynchronous process handling.
//!
//! Like with Unix we don't actually have a way of registering a process with an
//! IOCP object. As a result we similarly need another mechanism for getting a
//! signal when a process has exited. For now this is implemented with the
//! `RegisterWaitForSingleObject` function in the kernel32.dll.
//!
//! This strategy is the same that libuv takes and essentially just queues up a
//! wait for the process in a kernel32-specific thread pool. Once the object is
//! notified (e.g. the process exits) then we have a callback that basically
//! just completes a `Oneshot`.
//!
//! The `poll_exit` implementation will attempt to wait for the process in a
//! nonblocking fashion, but failing that it'll fire off a
//! `RegisterWaitForSingleObject` and then wait on the other end of the oneshot
//! from then on out.

extern crate winapi;
extern crate mio_named_pipes;

use std::fmt;
use std::io;
use std::os::windows::prelude::*;
use std::os::windows::process::ExitStatusExt;
use std::process::{self, ExitStatus};
use std::ptr;

use futures::future::Fuse;
use futures::sync::oneshot;
use futures::{Future, Poll, Async};
use kill::Kill;
use self::mio_named_pipes::NamedPipe;
use self::winapi::shared::minwindef::*;
use self::winapi::shared::winerror::*;
use self::winapi::um::handleapi::*;
use self::winapi::um::processthreadsapi::*;
use self::winapi::um::synchapi::*;
use self::winapi::um::threadpoollegacyapiset::*;
use self::winapi::um::winbase::*;
use self::winapi::um::winnt::*;
use super::SpawnedChild;
use tokio_reactor::{Handle, PollEvented};

#[must_use = "futures do nothing unless polled"]
pub struct Child {
    child: process::Child,
    waiting: Option<Waiting>,
}

impl fmt::Debug for Child {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Child")
            .field("pid", &self.id())
            .field("child", &self.child)
            .field("waiting", &"..")
            .finish()
    }
}

struct Waiting {
    rx: Fuse<oneshot::Receiver<()>>,
    wait_object: HANDLE,
    tx: *mut Option<oneshot::Sender<()>>,
}

unsafe impl Sync for Waiting {}
unsafe impl Send for Waiting {}

pub(crate) fn spawn_child(cmd: &mut process::Command, handle: &Handle) -> io::Result<SpawnedChild> {
    let mut child = cmd.spawn()?;
    let stdin = stdio(child.stdin.take(), handle)?;
    let stdout = stdio(child.stdout.take(), handle)?;
    let stderr = stdio(child.stderr.take(), handle)?;

    Ok(SpawnedChild {
        child: Child {
            child,
            waiting: None,
        },
        stdin,
        stdout,
        stderr,
    })
}

impl Child {
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    pub(crate) fn try_wait(&self) -> io::Result<Option<ExitStatus>> {
        unsafe {
            match WaitForSingleObject(self.child.as_raw_handle(), 0) {
                WAIT_OBJECT_0 => {}
                WAIT_TIMEOUT => return Ok(None),
                _ => return Err(io::Error::last_os_error()),
            }
            let mut status = 0;
            let rc = GetExitCodeProcess(self.child.as_raw_handle(), &mut status);
            if rc == FALSE {
                Err(io::Error::last_os_error())
            } else {
                Ok(Some(ExitStatus::from_raw(status)))
            }
        }
    }
}

impl Kill for Child {
    fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }
}

impl Future for Child {
    type Item = ExitStatus;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            if let Some(ref mut w) = self.waiting {
                match w.rx.poll().expect("should not be canceled") {
                    Async::Ready(()) => {}
                    Async::NotReady => return Ok(Async::NotReady),
                }
                let status = try!(self.try_wait()).expect("not ready yet");
                return Ok(status.into())
            }

            if let Some(e) = try!(self.try_wait()) {
                return Ok(e.into())
            }
            let (tx, rx) = oneshot::channel();
            let ptr = Box::into_raw(Box::new(Some(tx)));
            let mut wait_object = ptr::null_mut();
            let rc = unsafe {
                RegisterWaitForSingleObject(&mut wait_object,
                                            self.child.as_raw_handle(),
                                            Some(callback),
                                            ptr as *mut _,
                                            INFINITE,
                                            WT_EXECUTEINWAITTHREAD |
                                              WT_EXECUTEONLYONCE)
            };
            if rc == 0 {
                let err = io::Error::last_os_error();
                drop(unsafe { Box::from_raw(ptr) });
                return Err(err)
            }
            self.waiting = Some(Waiting {
                rx: rx.fuse(),
                wait_object,
                tx: ptr,
            });
        }
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        unsafe {
            let rc = UnregisterWaitEx(self.wait_object, INVALID_HANDLE_VALUE);
            if rc == 0 {
                panic!("failed to unregister: {}", io::Error::last_os_error());
            }
            drop(Box::from_raw(self.tx));
        }
    }
}

unsafe extern "system" fn callback(ptr: PVOID,
                                   _timer_fired: BOOLEAN) {
    let complete = &mut *(ptr as *mut Option<oneshot::Sender<()>>);
    let _ = complete.take().unwrap().send(());
}

pub type ChildStdin = PollEvented<NamedPipe>;
pub type ChildStdout = PollEvented<NamedPipe>;
pub type ChildStderr = PollEvented<NamedPipe>;

fn stdio<T>(option: Option<T>, handle: &Handle)
            -> io::Result<Option<PollEvented<NamedPipe>>>
    where T: IntoRawHandle,
{
    let io = match option {
        Some(io) => io,
        None => return Ok(None),
    };
    let pipe = unsafe { NamedPipe::from_raw_handle(io.into_raw_handle()) };
    let io = try!(PollEvented::new_with_handle(pipe, handle));
    Ok(Some(io))
}
