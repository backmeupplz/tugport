//! The init every Tugport container starts with (this binary mounted at /run/tugport/init).
//!
//! `init -- cmd...`      load the env file, run cmd as a child, forward signals, reap zombies.
//! `init exec -- cmd...` load the env file and exec cmd (health checks, consoles, dumps).

use std::ffi::CString;
use std::os::unix::process::CommandExt;

const FORWARDED: &[libc::c_int] = &[
    libc::SIGTERM,
    libc::SIGINT,
    libc::SIGHUP,
    libc::SIGQUIT,
    libc::SIGUSR1,
    libc::SIGUSR2,
    libc::SIGWINCH,
    libc::SIGCONT,
];

pub fn run(args: Vec<String>) -> ! {
    let (exec_mode, cmd) = match args.split_first() {
        Some((m, rest)) if m == "exec" => (true, strip_dashes(rest)),
        _ => (false, strip_dashes(&args)),
    };
    if cmd.is_empty() {
        eprintln!("tugport init: nothing to run");
        std::process::exit(127);
    }
    load_env();
    if exec_mode || std::process::id() != 1 {
        let err = std::process::Command::new(&cmd[0]).args(&cmd[1..]).exec();
        eprintln!("tugport init: cannot run {}: {err}", cmd[0]);
        std::process::exit(127);
    }
    supervise(&cmd)
}

fn strip_dashes(a: &[String]) -> Vec<String> {
    match a.first() {
        Some(f) if f == "--" => a[1..].to_vec(),
        _ => a.to_vec(),
    }
}

fn load_env() {
    let Ok(text) = std::fs::read(crate::secrets::ENV_PATH) else { return };
    let Ok(vars) = serde_json::from_slice::<std::collections::BTreeMap<String, String>>(&text) else {
        eprintln!("tugport init: the env file is unreadable");
        return;
    };
    for (k, v) in vars {
        // SAFETY: still single-threaded; nothing else reads the environment yet.
        unsafe { std::env::set_var(k, v) };
    }
}

fn supervise(cmd: &[String]) -> ! {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for &s in FORWARDED {
            libc::sigaddset(&mut set, s);
        }
        libc::sigaddset(&mut set, libc::SIGCHLD);
        libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());

        let child = std::process::Command::new(&cmd[0])
            .args(&cmd[1..])
            .pre_exec(|| {
                let mut empty: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut empty);
                libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
                Ok(())
            })
            .spawn();
        let child = match child {
            Ok(c) => c.id() as libc::pid_t,
            Err(e) => {
                eprintln!("tugport init: cannot run {}: {e}", cmd[0]);
                std::process::exit(127);
            }
        };
        loop {
            let mut sig: libc::c_int = 0;
            if libc::sigwait(&set, &mut sig) != 0 {
                continue;
            }
            if sig == libc::SIGCHLD {
                loop {
                    let mut status = 0;
                    let pid = libc::waitpid(-1, &mut status, libc::WNOHANG);
                    if pid <= 0 {
                        break;
                    }
                    if pid == child {
                        let code = if libc::WIFEXITED(status) {
                            libc::WEXITSTATUS(status)
                        } else if libc::WIFSIGNALED(status) {
                            128 + libc::WTERMSIG(status)
                        } else {
                            1
                        };
                        std::process::exit(code);
                    }
                }
            } else {
                libc::kill(child, sig);
            }
        }
    }
}

/// True when this process was started as a container init.
pub fn invoked_as_init() -> bool {
    std::env::args_os()
        .next()
        .and_then(|a| CString::new(a.into_encoded_bytes()).ok())
        .is_some_and(|a| a.to_bytes().ends_with(b"/init") || a.to_bytes() == b"init")
}
