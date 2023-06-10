use crate::compiler::Language;
use crate::error::JudgeCoreError;
use crate::run::executor::Executor;
use crate::run::process_listener::{ProcessListener, ProcessExitMessage};
use crate::run::sandbox::{RawRunResultInfo, Sandbox, SCRIPT_LIMIT_CONFIG};

use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::sys::epoll::{
    epoll_create1, epoll_ctl, epoll_wait, EpollCreateFlags, EpollEvent, EpollFlags, EpollOp,
};
use nix::unistd::{pipe, read, write};
use std::fs::File;
use std::io::{BufReader, BufRead};
use std::os::fd::FromRawFd;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;

use super::JudgeConfig;

fn set_non_blocking(fd: RawFd) -> Result<libc::c_int, JudgeCoreError> {
    log::debug!("Setting fd={} to non blocking", fd);
    Ok(fcntl(fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?)
}

// write the content of `from` to `to`, record to output
fn pump_proxy_pipe(from: RawFd, to: RawFd, output: RawFd) {
    let mut buf = [0; 1024];
    loop {
        match read(from, &mut buf) {
            Ok(nread) => {
                log::debug!("{} read. {} -> {}", nread, from, to);
                write(to, &buf[..nread]).ok();
                write(output, &buf[..nread]).ok();
            }
            Err(e) => {
                if e == Errno::EAGAIN || e == Errno::EWOULDBLOCK {
                    return;
                }
                panic!("failed to read from pipe");
            }
        }
    }
}

fn add_epoll_fd(epoll_fd: RawFd, fd: RawFd) -> Result<(), JudgeCoreError> {
    let mut event = EpollEvent::new(EpollFlags::EPOLLIN, fd as u64);
    log::debug!("Adding fd={} to epoll", fd);
    Ok(epoll_ctl(
        epoll_fd,
        EpollOp::EpollCtlAdd,
        fd,
        Some(&mut event),
    )?)
}

pub fn run_interact(
    runner_config: &JudgeConfig,
    interactor_path: &str,
    output_path: &String,
) -> Result<Option<RawRunResultInfo>, JudgeCoreError> {
    log::debug!("Creating sandbox for user process");
    let mut user_listener = ProcessListener::new()?;
    log::debug!("Creating sandbox for interactor process");
    let mut interact_listener = ProcessListener::new()?;

    log::debug!("Creating pipes");
    let (proxy_read_user, user_write_proxy) = pipe()?;
    let (proxy_read_interactor, interactor_write_proxy) = pipe()?;
    let (user_read_proxy, proxy_write_user) = pipe()?;
    let (interactor_read_proxy, proxy_write_interactor) = pipe()?;

    // epoll will listen to the write event
    // when should it be non blocking???
    log::debug!("Setting pipes to non blocking");
    set_non_blocking(user_write_proxy)?;
    set_non_blocking(interactor_write_proxy)?;
    set_non_blocking(proxy_read_user)?;
    set_non_blocking(proxy_read_interactor)?;

    log::debug!("Creating epoll");
    let epoll_fd = epoll_create1(EpollCreateFlags::EPOLL_CLOEXEC)?;

    log::debug!("Adding fds to epoll");
    add_epoll_fd(epoll_fd, proxy_read_user)?;
    add_epoll_fd(epoll_fd, proxy_read_interactor)?;

    log::debug!("Creating exit pipes");
    let (user_exit_read, user_exit_write) = pipe()?;
    let (interactor_exit_read, interactor_exit_write) = pipe()?;

    log::debug!("Adding exit fds to epoll");
    add_epoll_fd(epoll_fd, user_exit_read)?;
    add_epoll_fd(epoll_fd, interactor_exit_read)?;
    user_listener.set_exit_fd(user_exit_write, 41u8);
    interact_listener.set_exit_fd(interactor_exit_write, 42u8);

    log::debug!(
        "Opening output file path={}",
        runner_config.output_file_path
    );
    if !PathBuf::from(&output_path).exists() {
        File::create(output_path)?;
    }
    let output_file = File::options()
        .write(true)
        .truncate(true) // Overwrite the whole content of this file
        .open(output_path)?;
    let output_raw_fd: RawFd = output_file.as_raw_fd();
    let user_executor = Executor::new(
        runner_config.language,
        PathBuf::from(runner_config.program_path.to_owned()),
        vec![String::from("")],
    )?;
    let mut user_sandbox = Sandbox::new(
        user_executor,
        runner_config.rlimit_configs.clone(),
        Some(user_read_proxy),
        Some(user_write_proxy),
        true,
    )?;
    let _user_spawn = user_listener.spawn_with_sandbox(&mut user_sandbox)?;

    let first_args: String = String::from("");
    let interact_args = vec![
        first_args,
        runner_config.input_file_path.to_owned(),
        runner_config.output_file_path.to_owned(),
        runner_config.answer_file_path.to_owned(),
    ];
    let interact_executor = Executor::new(
        Language::Cpp,
        PathBuf::from(interactor_path.to_string()),
        interact_args,
    )?;
    let mut interact_sandbox = Sandbox::new(
        interact_executor,
        SCRIPT_LIMIT_CONFIG,
        Some(interactor_read_proxy),
        Some(interactor_write_proxy),
        false,
    )?;
    let _interact_spawn = interact_listener.spawn_with_sandbox(&mut interact_sandbox)?;

    log::debug!("Starting epoll");
    let mut events = [EpollEvent::empty(); 128];
    loop {
        let num_events = epoll_wait(epoll_fd, &mut events, -1)?;
        log::debug!("{} events found!", num_events);
        let mut user_exited = false;
        let mut interactor_exited = false;
        for event in events.iter().take(num_events) {
            log::debug!("Event: {:?}", event);
            let fd = event.data() as RawFd;
            if fd == user_exit_read {
                log::debug!("{:?} fd exited", fd);
                user_exited = true;
                let mut buf: Vec<u8> = Vec::new();
                unsafe {
                    let mut reader = BufReader::new(File::from_raw_fd(fd as RawFd));
                    reader.read_until(b'\n', &mut buf)?;
                }
                let buf_string = String::from_utf8(buf).unwrap().trim().to_owned();
                log::debug!("Raw Result info: {}", buf_string);
                let result_info: ProcessExitMessage = serde_json::from_str(&buf_string)?;
                log::debug!("Result info: {:?}", result_info);
            }

            if fd == interactor_exit_read {
                log::debug!("{:?} fd exited", fd);
                interactor_exited = true;
                let mut buf: Vec<u8> = Vec::new();
                unsafe {
                    let mut reader = BufReader::new(File::from_raw_fd(fd as RawFd));
                    reader.read_until(b'\n', &mut buf)?;
                }
                let buf_string = String::from_utf8(buf).unwrap().trim().to_owned();
                log::debug!("Raw Result info: {}", buf_string);
                let result_info: ProcessExitMessage = serde_json::from_str(&buf_string)?;
                log::debug!("Result info: {:?}", result_info);
            }
            if fd == proxy_read_user {
                log::debug!("proxy_read_user {} fd read", fd);
                pump_proxy_pipe(proxy_read_user, proxy_write_interactor, output_raw_fd);
            }
            if fd == proxy_read_interactor {
                log::debug!("proxy_read_interactor {} fd read", fd);
                pump_proxy_pipe(proxy_read_interactor, proxy_write_user, output_raw_fd);
            }
        }
        if user_exited && interactor_exited {
            break;
        }
    }

    log::debug!("Epoll finished!");

    // TODO: get result from listener
    // let _user_result = user_process.wait()?;
    // let _interact_result = interact_process.wait()?;
    log::debug!("Creating sandbox for checker process");
    if let Some(checker_path) = runner_config.custom_checker_path.clone() {
        let first_args = String::from("");
        let checker_args = vec![
            first_args,
            runner_config.input_file_path.to_owned(),
            runner_config.output_file_path.to_owned(),
            runner_config.answer_file_path.to_owned(),
            runner_config.check_file_path.to_owned(),
        ];
        let checker_executor =
            Executor::new(Language::Cpp, PathBuf::from(checker_path), checker_args)?;
        let mut checker_sandbox =
            Sandbox::new(checker_executor, SCRIPT_LIMIT_CONFIG, None, None, false)?;

        log::debug!("Spawning checker process");
        let _checker_spawn = checker_sandbox.spawn()?;
        log::debug!("Waiting for checker process");
        let checker_result = checker_sandbox.wait()?;
        return Ok(Some(checker_result));
    }
    Err(JudgeCoreError::AnyhowError(anyhow::anyhow!(
        "Checker path is not provided"
    )))
}

#[cfg(test)]
pub mod interact_judge_test {
    use crate::{compiler::Language, judge::JudgeConfig, run::sandbox::RlimitConfigs};

    use super::run_interact;

    const TEST_CONFIG: RlimitConfigs = RlimitConfigs {
        stack_limit: Some((64 * 1024 * 1024, 64 * 1024 * 1024)),
        as_limit: Some((64 * 1024 * 1024, 64 * 1024 * 1024)),
        cpu_limit: Some((1, 2)),
        nproc_limit: Some((1, 1)),
        fsize_limit: Some((1024, 1024)),
    };

    fn init() {
        let _ = env_logger::builder()
            .filter_level(log::LevelFilter::Debug)
            .try_init();
    }

    #[test]
    fn test_run_interact() {
        init();
        let runner_config = JudgeConfig {
            language: Language::Cpp,
            program_path: "./../test-collection/dist/programs/read_and_write".to_owned(),
            custom_checker_path: Some("./../test-collection/dist/checkers/lcmp".to_owned()),
            input_file_path: "../tmp/in".to_owned(),
            output_file_path: "../tmp/out".to_owned(),
            answer_file_path: "../tmp/ans".to_owned(),
            check_file_path: "../tmp/check".to_owned(),
            rlimit_configs: TEST_CONFIG,
        };
        let result = run_interact(
            &runner_config,
            &String::from("../test-collection/dist/checkers/interactor-a-plus-b"),
            &String::from("../tmp/interactor"),
        );
        match result {
            Ok(Some(result)) => {
                log::debug!("{:?}", result);
            }
            Ok(None) => {
                log::debug!("Ignoring this result, for it's from a fork child process");
            }
            Err(e) => {
                log::error!("meet error: {:?}", e);
                assert!(false);
            }
        }
    }
}
