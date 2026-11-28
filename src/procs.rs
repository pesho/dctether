//! Ending the user's command with its session (container side).
//!
//! Docker and Podman don't end a process started by `exec` when the exec
//! client goes away, so the user's command would outlive a session whose
//! host side was stopped or killed. The command therefore runs under
//! `dctether run`, which registers itself with the agent and then
//! replaces itself with the command (`exec`, same PID). Docker and Podman
//! make that process the leader of a new session, with or without a tty.
//! When the session ends, the agent signals every process still in that
//! session: the command and whatever it left in the background. Processes
//! that started a session of their own (`setsid`, daemons, a tmux server)
//! are not in it and keep running.
//!
//! Parsing and target selection are pure and unit-tested; reading `/proc`
//! and sending signals is in [`end`].

/// The fields of `/proc/<pid>/stat` that are used here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub state: char,
    pub sid: i32,
    /// Controlling terminal, 0 for none.
    pub tty_nr: i32,
    /// Start time in clock ticks since boot: together with the PID, it
    /// identifies a process even after its PID is reused.
    pub start: u64,
}

/// Parse `/proc/<pid>/stat`. The command name (field 2) can contain spaces
/// and parentheses, so the fields are counted from the last `)`.
pub fn parse_stat(s: &str) -> Option<Stat> {
    let rest = &s[s.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_ascii_whitespace().collect();
    // After the name: state(3) ppid(4) pgrp(5) session(6) tty_nr(7) ...
    // starttime(22), numbered as in proc(5).
    Some(Stat {
        state: f.first()?.chars().next()?,
        sid: f.get(3)?.parse().ok()?,
        tty_nr: f.get(4)?.parse().ok()?,
        start: f.get(19)?.parse().ok()?,
    })
}

/// A registered command: the leader of the session to end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Leader {
    pub pid: i32,
    pub start: u64,
}

impl Leader {
    /// The leader to record for a process that registered itself, or why
    /// not: only the leader of its own session is tracked, so ending it can
    /// never reach another `exec`'s processes.
    pub fn from_registrant(pid: i32, stat: &Stat) -> Result<Leader, &'static str> {
        if pid <= 1 || stat.sid != pid {
            return Err("not a session leader");
        }
        Ok(Leader { pid, start: stat.start })
    }
}

/// What to signal to end `leader`'s session, given the current process
/// table (`(pid, stat)` of every process).
///
/// - Every live process in the session gets SIGTERM.
/// - A leader that has a controlling terminal also gets SIGHUP, as on a
///   terminal hangup: interactive shells ignore SIGTERM, and on SIGHUP they
///   pass it on to their jobs and exit.
/// - If the session's PID belongs to a different process now (same PID,
///   other start time), the session is not ours any more: nothing is
///   signalled. While any process of a session is alive, its ID can't be
///   reused, so a session whose members are still around is never mistaken
///   for another one.
///
/// Zombies are skipped: they are already dead, and under an init that
/// doesn't reap they would never go away. `exclude` (the agent itself) is
/// never a target.
pub fn targets(leader: Leader, procs: &[(i32, Stat)], exclude: i32) -> Vec<(i32, i32)> {
    if procs
        .iter()
        .any(|(pid, st)| *pid == leader.pid && st.start != leader.start)
    {
        return vec![];
    }
    let mut out = vec![];
    for (pid, st) in procs {
        if st.sid != leader.pid || *pid == exclude || matches!(st.state, 'Z' | 'X') {
            continue;
        }
        out.push((*pid, libc::SIGTERM));
        if *pid == leader.pid && st.tty_nr != 0 {
            out.push((*pid, libc::SIGHUP));
        }
    }
    out
}

/// How long processes get to exit after SIGTERM before they get SIGKILL.
pub const GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// Every process this agent can see, from `/proc`.
fn process_table() -> Vec<(i32, Stat)> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return vec![];
    };
    dir.filter_map(|e| {
        let pid: i32 = e.ok()?.file_name().to_str()?.parse().ok()?;
        let st = parse_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)?;
        Some((pid, st))
    })
    .collect()
}

/// Read the registrant's `stat`.
pub fn stat_of(pid: i32) -> Option<Stat> {
    parse_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// End the sessions of `leaders`: SIGTERM (and SIGHUP, see [`targets`]),
/// then SIGKILL for whatever is left after [`GRACE`]. Returns when nothing
/// of them is left, or right after the SIGKILL.
pub async fn end(leaders: &[Leader]) {
    let me = std::process::id() as i32;
    let live = |procs: &[(i32, Stat)]| -> Vec<(i32, i32)> {
        leaders.iter().flat_map(|l| targets(*l, procs, me)).collect()
    };
    let first = live(&process_table());
    if first.is_empty() {
        return;
    }
    for (pid, sig) in &first {
        crate::debug!("ending session: signal {sig} to {pid}");
        unsafe { libc::kill(*pid, *sig) };
    }
    let deadline = tokio::time::Instant::now() + GRACE;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if live(&process_table()).is_empty() {
            return;
        }
    }
    for (pid, sig) in live(&process_table()) {
        if sig == libc::SIGTERM {
            crate::debug!("ending session: SIGKILL to {pid}");
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// Name of the agent's registration socket in the session directory.
pub const RUN_SOCKET: &str = "run.sock";

/// `dctether run COMMAND [ARGS...]`: register with the session's agent
/// (found through `DCTETHER_SESSION`), then `exec` the command. The agent
/// learns this process from the socket's peer credentials, so nothing is
/// sent but a one-byte answer back. If registering fails, the command
/// still runs, with a warning that it may outlive the session.
pub fn run_main(args: &[String]) -> i32 {
    use std::io::Read;
    use std::os::unix::process::CommandExt;

    let Some((cmd, rest)) = args.split_first() else {
        eprintln!("usage: dctether run COMMAND [ARGS...]");
        return 2;
    };
    let registered = (|| -> std::io::Result<bool> {
        let dir = std::env::var_os("DCTETHER_SESSION").ok_or_else(|| std::io::Error::other("DCTETHER_SESSION is not set"))?;
        let mut s = std::os::unix::net::UnixStream::connect(std::path::Path::new(&dir).join(RUN_SOCKET))?;
        s.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
        let mut answer = [0u8; 1];
        s.read_exact(&mut answer)?;
        Ok(answer[0] == b'1')
    })();
    match registered {
        Ok(true) => {}
        Ok(false) => eprintln!("dctether: this command can't be tied to the session (not a session leader); it may outlive it"),
        Err(e) => eprintln!("dctether: couldn't tie this command to the session ({e}); it may outlive it"),
    }
    let e = std::process::Command::new(cmd).args(rest).exec();
    eprintln!("dctether: {cmd}: {e}");
    // As shells report it.
    if e.kind() == std::io::ErrorKind::NotFound { 127 } else { 126 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(state: char, sid: i32, tty_nr: i32, start: u64) -> Stat {
        Stat { state, sid, tty_nr, start }
    }

    #[test]
    fn parses_stat_from_the_last_paren() {
        let line = "4242 (a) b (c)) S 1 4242 4242 34816 4242 4194560 108 0 0 0 0 0 0 0 20 0 1 0 987654 2433024 197 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0";
        assert_eq!(parse_stat(line), Some(st('S', 4242, 34816, 987654)));
        assert_eq!(parse_stat("1 (x) S 0 1"), None);
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn tracks_only_session_leaders() {
        assert_eq!(
            Leader::from_registrant(50, &st('S', 50, 0, 7)),
            Ok(Leader { pid: 50, start: 7 })
        );
        assert!(Leader::from_registrant(50, &st('S', 40, 0, 7)).is_err());
        assert!(Leader::from_registrant(0, &st('S', 0, 0, 7)).is_err());
        assert!(Leader::from_registrant(1, &st('S', 1, 0, 7)).is_err());
    }

    #[test]
    fn targets_the_whole_session_and_nothing_else() {
        let leader = Leader { pid: 10, start: 100 };
        let procs = [
            (1, st('S', 1, 0, 1)),        // container init
            (10, st('S', 10, 0, 100)),    // the command
            (11, st('S', 10, 0, 101)),    // its background job
            (12, st('Z', 10, 0, 102)),    // a zombie in the session
            (13, st('S', 13, 0, 103)),    // a daemon that called setsid
            (20, st('S', 20, 0, 50)),     // another exec's session
            (30, st('S', 10, 0, 104)),    // the agent (never a target)
        ];
        assert_eq!(
            targets(leader, &procs, 30),
            vec![(10, libc::SIGTERM), (11, libc::SIGTERM)]
        );
    }

    #[test]
    fn hangs_up_a_leader_with_a_terminal() {
        let leader = Leader { pid: 10, start: 100 };
        let procs = [(10, st('S', 10, 34816, 100)), (11, st('S', 10, 34816, 101))];
        assert_eq!(
            targets(leader, &procs, 0),
            vec![(10, libc::SIGTERM), (10, libc::SIGHUP), (11, libc::SIGTERM)]
        );
    }

    #[test]
    fn leftovers_of_an_exited_leader_are_still_targets() {
        let leader = Leader { pid: 10, start: 100 };
        let procs = [(11, st('S', 10, 0, 101))];
        assert_eq!(targets(leader, &procs, 0), vec![(11, libc::SIGTERM)]);
    }

    #[test]
    fn a_reused_leader_pid_is_left_alone() {
        let leader = Leader { pid: 10, start: 100 };
        let procs = [(10, st('S', 10, 0, 900)), (11, st('S', 10, 0, 901))];
        assert!(targets(leader, &procs, 0).is_empty());
    }
}
