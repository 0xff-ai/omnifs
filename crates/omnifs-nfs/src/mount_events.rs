//! The events of the kernel that wake the owner of a mount: each unmount of
//! the host, and the release of a restart.

use std::io;
use std::sync::Arc;

/// What woke the owner of a mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MountEvent {
    /// The mount table of the host changed.
    Changed,
    /// A restart released the mount.
    Released,
}

/// The events that wake the owner of a mount: each unmount of the host, a
/// kqueue `EVFILT_FS` with `VQ_UNMOUNT` on macOS and `POLLPRI` of
/// `/proc/self/mountinfo` at each change of the mount table on Linux, and the
/// release of a restart, which its [`Release`] triggers, a kqueue
/// `EVFILT_USER` on macOS and an eventfd on Linux.
#[derive(Debug)]
pub struct MountEvents {
    queue: Arc<Queue>,
}

/// The trigger of the release of the mount of a [`MountEvents`].
#[derive(Debug, Clone)]
pub struct Release {
    queue: Arc<Queue>,
}

impl MountEvents {
    /// The events of a mount, from now on, with the trigger of its release.
    pub fn new() -> io::Result<(Self, Release)> {
        let queue = Arc::new(Queue::new()?);
        Ok((
            Self {
                queue: Arc::clone(&queue),
            },
            Release { queue },
        ))
    }

    /// Waits for the next event.
    pub(crate) fn wait(&self) -> io::Result<MountEvent> {
        self.queue.wait()
    }
}

impl Release {
    /// Wakes the owner of the mount with its release.
    pub fn release(&self) -> io::Result<()> {
        self.queue.release()
    }
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct Queue {
    kqueue: nix::sys::event::Kqueue,
}

#[cfg(target_os = "macos")]
impl Queue {
    /// The ident of the user event of the release.
    const RELEASE: usize = 1;
    /// `VQ_UNMOUNT` of `<sys/mount.h>`: a filesystem left the mount table.
    const VQ_UNMOUNT: u32 = 0x0010;

    fn new() -> io::Result<Self> {
        use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};
        let kqueue = Kqueue::new()?;
        let changes = [
            KEvent::new(
                0,
                EventFilter::EVFILT_FS,
                EvFlags::EV_ADD | EvFlags::EV_CLEAR,
                FilterFlag::from_bits_retain(Self::VQ_UNMOUNT),
                0,
                0,
            ),
            KEvent::new(
                Self::RELEASE,
                EventFilter::EVFILT_USER,
                EvFlags::EV_ADD | EvFlags::EV_CLEAR,
                FilterFlag::empty(),
                0,
                0,
            ),
        ];
        kqueue.kevent(&changes, &mut [], Some(now()))?;
        Ok(Self { kqueue })
    }

    fn release(&self) -> io::Result<()> {
        use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent};
        let trigger = KEvent::new(
            Self::RELEASE,
            EventFilter::EVFILT_USER,
            EvFlags::empty(),
            FilterFlag::NOTE_TRIGGER,
            0,
            0,
        );
        self.kqueue.kevent(&[trigger], &mut [], Some(now()))?;
        Ok(())
    }

    fn wait(&self) -> io::Result<MountEvent> {
        use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent};
        let mut events = [KEvent::new(
            0,
            EventFilter::EVFILT_FS,
            EvFlags::empty(),
            FilterFlag::empty(),
            0,
            0,
        )];
        loop {
            match self.kqueue.kevent(&[], &mut events, None) {
                Ok(0) | Err(nix::errno::Errno::EINTR) => {},
                Ok(_) => {
                    return Ok(match events[0].filter()? {
                        EventFilter::EVFILT_USER => MountEvent::Released,
                        _ => MountEvent::Changed,
                    });
                },
                Err(error) => return Err(error.into()),
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn now() -> nix::libc::timespec {
    nix::libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct Queue {
    mountinfo: std::fs::File,
    release: nix::sys::eventfd::EventFd,
}

#[cfg(target_os = "linux")]
impl Queue {
    fn new() -> io::Result<Self> {
        use nix::sys::eventfd::{EfdFlags, EventFd};
        Ok(Self {
            mountinfo: std::fs::File::open("/proc/self/mountinfo")?,
            release: EventFd::from_value_and_flags(
                0,
                EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK,
            )?,
        })
    }

    fn release(&self) -> io::Result<()> {
        self.release.write(1)?;
        Ok(())
    }

    fn wait(&self) -> io::Result<MountEvent> {
        use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
        use std::os::fd::AsFd as _;
        loop {
            let mut fds = [
                PollFd::new(self.release.as_fd(), PollFlags::POLLIN),
                PollFd::new(self.mountinfo.as_fd(), PollFlags::POLLPRI),
            ];
            match poll(&mut fds, PollTimeout::NONE) {
                Ok(0) | Err(nix::errno::Errno::EINTR) => continue,
                Ok(_) => {},
                Err(error) => return Err(error.into()),
            }
            if fds[0]
                .revents()
                .is_some_and(|events| events.contains(PollFlags::POLLIN))
            {
                self.release.read()?;
                return Ok(MountEvent::Released);
            }
            return Ok(MountEvent::Changed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MountEvent, MountEvents};

    /// The release of a restart wakes the owner of a mount.
    #[test]
    fn the_release_of_a_restart_wakes_the_owner_of_the_mount() {
        let (events, release) = MountEvents::new().unwrap();
        release.release().unwrap();
        assert_eq!(events.wait().unwrap(), MountEvent::Released);
    }
}
