use mio::{unix::SourceFd, Events, Interest, Poll};
use std::{
    collections::HashMap,
    env,
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::net::UnixDatagram,
    },
    sync::mpsc,
};

use crate::{
    error::CommonError,
    event_loop::{itimerspec_to_libc, EventLoopTrait, Itimerspec, Token},
    libc_call,
    socket::Socket,
};

pub type Sources<T> = (T, Box<dyn FnMut(&mut T) -> Result<i32, CommonError>>);
pub type TimedSources<T> = (
    RawFd,
    Token,
    Box<dyn FnMut(&mut T) -> Result<i32, CommonError>>,
);

#[derive(Debug)]
pub enum IPCMessage {
    RegisterFd(Token),
}

pub struct LinuxEventLoop<T: AsRawFd + for<'a> Socket<'a, T>> {
    poll: Poll,
    events: Events,
    pub sources: HashMap<Token, Sources<T>>,
    timed_sources: HashMap<Token, TimedSources<T>>,
    next_token: usize,
    registration_sender: mpsc::Sender<Sources<T>>,
    registration_receiver: mpsc::Receiver<Sources<T>>,
}

impl<T: AsRawFd + for<'a> Socket<'a, T>> LinuxEventLoop<T> {
    /// Returns the path to the Inter-Process Communication (IPC) socket
    pub fn get_communication_channel(&self) -> mpsc::Sender<Sources<T>> {
        self.registration_sender.clone()
    }
}

impl<T: AsRawFd + for<'a> Socket<'a, T> + 'static> EventLoopTrait<T> for LinuxEventLoop<T> {
    fn new(event_capacity: usize) -> Result<Self, CommonError> {
        // Create the poll
        let poll = Poll::new()?;

        let events = Events::with_capacity(event_capacity);
        // Create a temporary file path
        let temp_dir = env::temp_dir();
        let temp_file_path = temp_dir.join(format!("event_loop_socket_{}", std::process::id()));
        // Create a Unix domain socket
        // Bind the socket to the temporary file path
        let ipc_socket = UnixDatagram::bind(temp_file_path.clone()).map_err(CommonError::Io)?;
        // Set the socket to non-blocking
        ipc_socket.set_nonblocking(true).map_err(CommonError::Io)?;

        // Register the Unix domain socket with the poll
        let ipc_token = mio::Token(usize::MAX); // Use a special token for the IPC socket
        let raw_fd = &ipc_socket.as_raw_fd();
        let mut ipc_source = SourceFd(raw_fd);
        poll.registry()
            .register(&mut ipc_source, ipc_token, Interest::READABLE)?;
        let (registration_sender, registration_receiver) = mpsc::channel();
        Ok(Self {
            poll,
            events,
            sources: HashMap::new(),
            timed_sources: HashMap::new(),
            next_token: 0,
            registration_sender,
            registration_receiver,
        })
    }

    fn generate_token(&mut self) -> Token {
        let token = Token(self.next_token);
        self.next_token += 1;
        token
    }

    fn register_event_source<F>(
        &mut self,
        event_source: T,
        callback: F,
    ) -> Result<Token, CommonError>
    where
        F: FnMut(&mut T) -> Result<i32, CommonError> + 'static,
    {
        let binding = &event_source.as_raw_fd();
        let mut source = SourceFd(binding);
        let generate_token = self.generate_token();
        let token = mio::Token(generate_token.0);
        self.poll
            .registry()
            .register(&mut source, token, Interest::READABLE)?;
        self.sources
            .insert(generate_token, (event_source, Box::new(callback)));
        Ok(generate_token)
    }

    fn run(&mut self) -> Result<(), CommonError> {
        'outer: loop {
            while let Ok((event_source, callback)) = self.registration_receiver.try_recv() {
                let _inner_token = self.register_event_source(event_source, callback)?;
            }
            self.poll.poll(
                &mut self.events,
                Some(std::time::Duration::from_millis(100)),
            )?;
            for event in self.events.iter() {
                if event.is_readable() {
                    let token = event.token();

                    let generate_token = Token(token.0);
                    if let Some((source, callback)) = self.sources.get_mut(&generate_token) {
                        callback(source)?;
                    } else if let Some((timer_source, inner_token, callback)) =
                        self.timed_sources.get_mut(&generate_token)
                    {
                        if let Some((source, _)) = self.sources.get_mut(inner_token) {
                            callback(source)?;
                            reset_timer(timer_source)?;
                        }
                    } else {
                        break 'outer;
                    }
                }
            }
        }

        Ok(())
    }

    fn add_timer<F>(
        &mut self,
        time_spec: &Itimerspec,
        token: &Token,
        callback: F,
    ) -> Result<Token, CommonError>
    where
        F: FnMut(&mut T) -> Result<i32, CommonError> + 'static,
    {
        let timer_fd = unsafe {
            let fd = libc::timerfd_create(libc::CLOCK_REALTIME, libc::TFD_NONBLOCK);
            let itimer_spec = itimerspec_to_libc(time_spec);

            libc::timerfd_settime(fd, 0, &itimer_spec, std::ptr::null_mut());
            fd
        };
        let mut timer_source = SourceFd(&timer_fd);
        let new_token = self.generate_token();
        let mio_token = mio::Token(new_token.0);
        self.poll
            .registry()
            .register(&mut timer_source, mio_token, Interest::READABLE)?;
        if let Some((_source, _)) = self.sources.get_mut(token) {
            self.timed_sources
                .insert(new_token, (timer_fd, *token, Box::new(callback)));
        }

        Ok(new_token)
    }

    fn add_duration(&mut self, time_spec: &Itimerspec) -> Result<Token, CommonError> {
        let timer_fd = unsafe {
            let fd = libc::timerfd_create(libc::CLOCK_REALTIME, libc::TFD_NONBLOCK);
            let itimer_spec = itimerspec_to_libc(time_spec);
            libc::timerfd_settime(fd, 0, &itimer_spec, std::ptr::null_mut());
            fd
        };

        let mut timer_source = SourceFd(&timer_fd);
        let new_token = self.generate_token();
        let mio_token = mio::Token(new_token.0);
        self.poll
            .registry()
            .register(&mut timer_source, mio_token, Interest::READABLE)?;

        Ok(new_token)
    }
}

pub fn reset_timer(timer_raw: &mut RawFd) -> Result<(), CommonError> {
    let timer_spec = &mut libc::itimerspec {
        it_interval: libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        it_value: libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
    };
    let gettime_result: Result<i32, CommonError> =
        libc_call!(timerfd_gettime(timer_raw.as_raw_fd(), timer_spec));
    gettime_result?;
    let settime_result: Result<i32, CommonError> = libc_call!(timerfd_settime(
        timer_raw.as_raw_fd(),
        0,
        timer_spec,
        timer_spec
    ));
    settime_result?;

    Ok(())
}

pub fn create_non_blocking_unix_datagram() -> Result<UnixDatagram, CommonError> {
    let socket_fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0) };
    if socket_fd < 0 {
        return Err(CommonError::Io(std::io::Error::last_os_error()));
    }

    let flags = unsafe { libc::fcntl(socket_fd, libc::F_GETFL) };
    if flags < 0 {
        let _ = unsafe { libc::close(socket_fd) };
        return Err(CommonError::Io(std::io::Error::last_os_error()));
    }

    let result = unsafe { libc::fcntl(socket_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if result < 0 {
        let _ = unsafe { libc::close(socket_fd) };
        return Err(CommonError::Io(std::io::Error::last_os_error()));
    }

    Ok(unsafe { UnixDatagram::from_raw_fd(socket_fd) })
}
