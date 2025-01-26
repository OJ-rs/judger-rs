use crate::error::JudgeCoreError;
use crate::judge::common::run_checker;
use crate::judge::result::{check_user_result, JudgeVerdict};
use crate::run::executor::Executor;
use crate::run::process_listener::{ProcessExitMessage, ProcessListener};
use crate::run::sandbox::ExecutorSandbox;
use crate::sandbox::{SandboxExitInfo, SCRIPT_LIMIT_CONFIG};
use crate::utils::get_pathbuf_str;

use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};
use nix::unistd::{pipe, read, write};
use std::fs::File;
use std::os::fd::BorrowedFd;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::time::Duration;

use super::result::JudgeResultInfo;
use super::JudgeConfig;

const USER_EXIT_SIGNAL: u8 = 41u8;
const INTERACTOR_EXIT_SIGNAL: u8 = 42u8;

fn set_fd_non_blocking(fd: RawFd) -> Result<libc::c_int, JudgeCoreError> {
    log::debug!("Setting fd={} to non blocking", fd);
    Ok(fcntl(fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?)
}

/// write the content of `from` to `to`, record to output.
/// `from` will be set to non-blocking mode.
fn pump_proxy_pipe(from: RawFd, to: RawFd, output: RawFd) -> Result<(), JudgeCoreError> {
    log::debug!("Pumping from {} to {} with output {}", from, to, output);
    set_fd_non_blocking(from)?;

    let mut buf = [0; 1024];
    loop {
        match read(from, &mut buf) {
            Ok(nread) => {
                log::debug!("{} read. {} -> {}", nread, from, to);
                // We should be really careful here
                // not using OwnedFd here because it will close the fd
                write(unsafe { BorrowedFd::borrow_raw(to) }, &buf[..nread])?;
                write(unsafe { BorrowedFd::borrow_raw(output) }, &buf[..nread])?;
            }
            Err(e) => {
                if e == Errno::EAGAIN || e == Errno::EWOULDBLOCK {
                    return Ok(());
                }
                panic!("failed to read from pipe");
            }
        }
    }
}

/// `from` will be set to non-blocking mode.
fn read_string_from_fd(from: RawFd) -> Result<String, JudgeCoreError> {
    set_fd_non_blocking(from)?;

    let mut res_buf = Vec::new();
    let mut buf = [0; 1024];
    log::debug!("Reading from fd={}", from);
    loop {
        log::debug!("Reading from fd={}", from);
        match read(from, &mut buf) {
            Ok(nread) => {
                log::debug!("{} read. {}", nread, from);
                res_buf.extend_from_slice(&buf[..nread]);
            }
            Err(e) => {
                if e == Errno::EAGAIN || e == Errno::EWOULDBLOCK {
                    let buf_string = String::from_utf8(res_buf)?;
                    return Ok(buf_string);
                }
                panic!("failed to read from pipe");
            }
        }
    }
}

fn read_msg_from_fd(from: RawFd) -> Result<ProcessExitMessage, JudgeCoreError> {
    let buf_string = read_string_from_fd(from as RawFd)?;
    log::debug!("Raw Result info: {}", buf_string);
    let msg: ProcessExitMessage = serde_json::from_str(&buf_string)?;
    Ok(msg)
}

fn add_epoll_fd(epoll: &Epoll, fd: RawFd) -> Result<(), JudgeCoreError> {
    let event = EpollEvent::new(EpollFlags::EPOLLIN, fd as u64);
    log::debug!("Adding fd={} to epoll", fd);
    Ok(epoll.add(unsafe { BorrowedFd::borrow_raw(fd) }, event)?)
}

pub fn run_interact(
    config: &JudgeConfig,
    mut interactor_executor: Executor,
    output_path: &PathBuf,
) -> Result<Option<JudgeResultInfo>, JudgeCoreError> {
    log::debug!("Creating epoll");
    let epoll = Epoll::new(EpollCreateFlags::EPOLL_CLOEXEC)?;

    log::debug!("Creating interact pipes");
    let (proxy_read_user, user_write_proxy) = pipe()?;
    let (proxy_read_interactor, interactor_write_proxy) = pipe()?;
    let (user_read_proxy, proxy_write_user) = pipe()?;
    let (interactor_read_proxy, proxy_write_interactor) = pipe()?;

    log::debug!("Adding read proxy fds to epoll");
    add_epoll_fd(&epoll, proxy_read_user.as_raw_fd())?;
    add_epoll_fd(&epoll, proxy_read_interactor.as_raw_fd())?;

    log::debug!("Creating exit report pipes with epoll");
    let (user_exit_read, user_exit_write) = pipe()?;
    let (interactor_exit_read, interactor_exit_write) = pipe()?;
    add_epoll_fd(&epoll, user_exit_read.as_raw_fd())?;
    add_epoll_fd(&epoll, interactor_exit_read.as_raw_fd())?;

    let mut user_listener = ProcessListener::new()?;
    let mut interact_listener = ProcessListener::new()?;
    user_listener.setup_exit_report(user_exit_write.as_raw_fd(), USER_EXIT_SIGNAL);
    interact_listener.setup_exit_report(interactor_exit_write.as_raw_fd(), INTERACTOR_EXIT_SIGNAL);

    if !PathBuf::from(&output_path).exists() {
        File::create(output_path)?;
    }
    let output_file = File::options()
        .write(true)
        .truncate(true) // Overwrite the whole content of this file
        .open(output_path)?;
    let output_raw_fd: RawFd = output_file.as_raw_fd();

    let mut user_sandbox = ExecutorSandbox::new(
        config.program.executor.clone(),
        config.runtime.rlimit_configs.clone(),
        Some(user_read_proxy.as_raw_fd()),
        Some(user_write_proxy.as_raw_fd()),
        true,
    )?;
    user_listener.spawn_with_sandbox(&mut user_sandbox)?;

    let first_args: String = String::from("");
    let interact_args = vec![
        first_args,
        get_pathbuf_str(&config.test_data.input_file_path)?,
        get_pathbuf_str(&config.program.output_file_path)?,
        get_pathbuf_str(&config.test_data.answer_file_path)?,
    ];
    interactor_executor.set_additional_args(interact_args);
    let mut interact_sandbox = ExecutorSandbox::new(
        interactor_executor,
        SCRIPT_LIMIT_CONFIG.clone(),
        Some(interactor_read_proxy.as_raw_fd()),
        Some(interactor_write_proxy.as_raw_fd()),
        false,
    )?;
    interact_listener.spawn_with_sandbox(&mut interact_sandbox)?;

    log::debug!("Starting epoll");
    let mut events = [EpollEvent::empty(); 128];
    let mut user_exited = false;
    let mut interactor_exited = false;
    let mut option_user_result: Option<SandboxExitInfo> = None;
    loop {
        let num_events = epoll.wait(&mut events, EpollTimeout::NONE)?;
        log::debug!("{} events found!", num_events);

        for event in events.iter().take(num_events) {
            log::debug!("Event: {:?}", event);
            let fd = event.data() as RawFd;
            if fd == user_exit_read.as_raw_fd() {
                log::debug!("{:?} user fd exited", fd);
                user_exited = true;
                let exit_msg = read_msg_from_fd(fd)?;
                option_user_result = exit_msg.option_run_result;
            }
            if fd == interactor_exit_read.as_raw_fd() {
                log::debug!("{:?} interactor fd exited", fd);
                interactor_exited = true;
                let _interactor_result: ProcessExitMessage = read_msg_from_fd(fd)?;
            }
            if fd == proxy_read_user.as_raw_fd() {
                log::debug!("proxy_read_user {} fd read", fd);
                pump_proxy_pipe(
                    proxy_read_user.as_raw_fd(),
                    proxy_write_interactor.as_raw_fd(),
                    output_raw_fd.as_raw_fd(),
                )?;
            }
            if fd == proxy_read_interactor.as_raw_fd() {
                log::debug!("proxy_read_interactor {} fd read", fd);
                pump_proxy_pipe(
                    proxy_read_interactor.as_raw_fd(),
                    proxy_write_user.as_raw_fd(),
                    output_raw_fd.as_raw_fd(),
                )?;
            }
        }
        if user_exited && interactor_exited {
            log::debug!("Both user and interactor exited");
            break;
        }
    }
    log::debug!("Epoll finished!");

    if let Some(user_result) = option_user_result {
        let option_user_verdict = check_user_result(config, &user_result);
        if let Some(verdict) = option_user_verdict {
            return Ok(Some(JudgeResultInfo {
                verdict,
                time_usage: user_result.real_time_cost,
                memory_usage_bytes: user_result.resource_usage.max_rss,
                exit_status: user_result.exit_status,
                checker_exit_status: 0,
            }));
        }
        log::debug!("Running checker process");
        if let Some(_checker_executor) = config.checker.executor.clone() {
            let (verdict, checker_exit_status) = run_checker(config)?;
            Ok(Some(JudgeResultInfo {
                verdict,
                time_usage: user_result.real_time_cost,
                memory_usage_bytes: user_result.resource_usage.max_rss,
                exit_status: user_result.exit_status,
                checker_exit_status,
            }))
        } else {
            Err(JudgeCoreError::AnyhowError(anyhow::anyhow!(
                "Checker path is not provided"
            )))
        }
    } else {
        // interactor output should be checked here
        Ok(Some(JudgeResultInfo {
            verdict: JudgeVerdict::IdlenessLimitExceeded,
            time_usage: Duration::new(0, 0),
            memory_usage_bytes: 0,
            exit_status: 0,
            checker_exit_status: 0,
        }))
    }
}
