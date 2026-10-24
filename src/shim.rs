//! The init every Tugport container starts with (this binary mounted at /run/tugport/init).
//!
//! `init -- cmd...`      load the env file, run cmd as a child, forward signals, reap zombies.
//! `init exec -- cmd...` load the env file and exec cmd (health checks, consoles, dumps).

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
    let Some((program, rest)) = cmd.split_first() else { fail(format_args!("nothing to run")) };
    load_env();
    if exec_mode || std::process::id() != 1 {
        let err = std::process::Command::new(program).args(rest).exec();
        fail(format_args!("cannot run {program}: {err}"));
    }
    supervise(program, rest)
}

/// A line on stderr. Unlike `eprintln!` it never panics (on a closed stderr, say): PID 1 must not.
fn say(msg: std::fmt::Arguments) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "tugport init: {msg}");
}

fn fail(msg: std::fmt::Arguments) -> ! {
    say(msg);
    std::process::exit(127)
}

fn strip_dashes(a: &[String]) -> &[String] {
    match a {
        [f, rest @ ..] if f == "--" => rest,
        _ => a,
    }
}

fn load_env() {
    let Ok(text) = std::fs::read(crate::secrets::ENV_PATH) else { return };
    let Ok(vars) = serde_json::from_slice::<std::collections::BTreeMap<String, String>>(&text) else {
        say(format_args!("the env file is unreadable"));
        return;
    };
    for (k, v) in vars {
        // set_var panics on these, and PID 1 must not.
        if k.is_empty() || k.contains(['=', '\0']) || v.contains('\0') {
            say(format_args!("skipping the unusable variable {k:?}"));
            continue;
        }
        // SAFETY: still single-threaded; nothing else reads the environment yet.
        unsafe { std::env::set_var(k, v) };
    }
}

fn supervise(program: &str, args: &[String]) -> ! {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for &s in FORWARDED {
            libc::sigaddset(&mut set, s);
        }
        libc::sigaddset(&mut set, libc::SIGCHLD);
        libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());

        let child = std::process::Command::new(program)
            .args(args)
            .pre_exec(|| {
                let mut empty: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut empty);
                libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
                Ok(())
            })
            .spawn();
        let child = match child {
            Ok(c) => c.id() as libc::pid_t,
            Err(e) => fail(format_args!("cannot run {program}: {e}")),
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
    std::env::args_os().next().is_some_and(|a| {
        let a = a.as_encoded_bytes();
        a.ends_with(b"/init") || a == b"init"
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_leading_dashes_argument_is_dropped() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(super::strip_dashes(&v(&["--", "sh", "-c"])), v(&["sh", "-c"]));
        assert_eq!(super::strip_dashes(&v(&["sh", "--"])), v(&["sh", "--"]));
        assert!(super::strip_dashes(&v(&["--"])).is_empty());
        assert!(super::strip_dashes(&[]).is_empty());
    }
}
