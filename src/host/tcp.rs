//! Which processes hold a TCP connection to a local port: the check that both sides of an
//! implementation comparison talk to the bench file's fixture (SPEC §9.10). One read of the OS's
//! socket table (Windows) or of the given processes' sockets (Linux, macOS); never a helper process.

/// The processes among `pids` with an established TCP connection whose remote end is `port` (any
/// address: the fixture listens on loopback). None when the OS gave no answer.
pub fn connected(port: u16, pids: &[u32]) -> Option<Vec<u32>> {
    os::connected(port, pids)
}

#[cfg(windows)]
mod os {
    use windows_sys::Win32::NetworkManagement::IpHelper::{GetExtendedTcpTable, TCP_TABLE_OWNER_PID_CONNECTIONS};

    /// `GetExtendedTcpTable` for IPv4 and IPv6, connections with their owning pids.
    pub fn connected(port: u16, pids: &[u32]) -> Option<Vec<u32>> {
        const ESTABLISHED: u32 = 5;
        let mut out = Vec::new();
        // (address family, row size in bytes, offset of the remote port, the state, the pid in a row)
        for (af, row, remote, state, pid) in [(2u32, 24usize, 16usize, 0usize, 20usize), (23, 56, 44, 48, 52)] {
            let mut size = 0u32;
            unsafe { GetExtendedTcpTable(std::ptr::null_mut(), &mut size, 0, af, TCP_TABLE_OWNER_PID_CONNECTIONS, 0) };
            let mut buf = vec![0u8; size as usize + 4096];
            let mut len = buf.len() as u32;
            if unsafe { GetExtendedTcpTable(buf.as_mut_ptr().cast(), &mut len, 0, af, TCP_TABLE_OWNER_PID_CONNECTIONS, 0) } != 0 {
                return None;
            }
            let u32_at = |at: usize| buf.get(at..at + 4).map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]])).unwrap_or(0);
            let n = u32_at(0) as usize;
            // The table's rows start after its count, aligned to 4 bytes.
            for i in 0..n {
                let at = 4 + i * row;
                // Ports are in network byte order in the low 16 bits.
                let rport = u16::from_be_bytes([buf.get(at + remote).copied().unwrap_or(0), buf.get(at + remote + 1).copied().unwrap_or(0)]);
                let owner = u32_at(at + pid);
                if u32_at(at + state) == ESTABLISHED && rport == port && pids.contains(&owner) && !out.contains(&owner) {
                    out.push(owner);
                }
            }
        }
        Some(out)
    }
}

#[cfg(target_os = "linux")]
mod os {
    use std::collections::HashSet;

    /// The socket inodes of established connections to `port` (`/proc/net/tcp`, `tcp6`), then which
    /// of the processes hold one among their descriptors (`/proc/<pid>/fd`).
    pub fn connected(port: u16, pids: &[u32]) -> Option<Vec<u32>> {
        let mut inodes = HashSet::new();
        let mut read = false;
        for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
            let Ok(text) = std::fs::read_to_string(table) else { continue };
            read = true;
            for line in text.lines().skip(1) {
                let f: Vec<&str> = line.split_whitespace().collect();
                let (Some(rem), Some(st), Some(inode)) = (f.get(2), f.get(3), f.get(9)) else { continue };
                let rport = rem.rsplit(':').next().and_then(|p| u16::from_str_radix(p, 16).ok());
                if *st == "01" && rport == Some(port) {
                    inodes.insert(format!("socket:[{inode}]"));
                }
            }
        }
        if !read {
            return None;
        }
        Some(
            pids.iter()
                .copied()
                .filter(|pid| {
                    std::fs::read_dir(format!("/proc/{pid}/fd")).into_iter().flatten().flatten().any(|e| std::fs::read_link(e.path()).is_ok_and(|l| inodes.contains(&l.to_string_lossy().to_string())))
                })
                .collect(),
        )
    }
}

#[cfg(target_os = "macos")]
mod os {
    /// Each process's descriptors (`PROC_PIDLISTFDS`), and of its sockets the TCP ones whose foreign
    /// port is `port` (`PROC_PIDFDSOCKETINFO`, read from `struct socket_fdinfo`).
    pub fn connected(port: u16, pids: &[u32]) -> Option<Vec<u32>> {
        const PROC_PIDFDSOCKETINFO: libc::c_int = 3;
        // struct socket_fdinfo: proc_fileinfo (24 bytes), then socket_info: vinfo_stat (136), so,
        // pcb, type, protocol, family, …, two sockbuf_info, kind, rfu, then the protocol's info,
        // whose in_sockinfo starts with the foreign port. Offsets from the start of the struct.
        const PROTOCOL: usize = 180;
        const FAMILY: usize = 184;
        const FOREIGN_PORT: usize = 264;
        // soi_kind (SOCKINFO_TCP: the protocol info is a tcp_sockinfo) and, past its in_sockinfo (80
        // bytes), tcpsi_state (TSI_S_ESTABLISHED).
        const KIND: usize = 256;
        const SOCKINFO_TCP: i32 = 2;
        const TCP_STATE: usize = FOREIGN_PORT + 80;
        const ESTABLISHED: i32 = 4;
        let mut out = Vec::new();
        for &pid in pids {
            let size = unsafe { libc::proc_pidinfo(pid as i32, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
            if size <= 0 {
                continue;
            }
            let n = size as usize / std::mem::size_of::<libc::proc_fdinfo>() + 16;
            let mut fds: Vec<libc::proc_fdinfo> = vec![unsafe { std::mem::zeroed() }; n];
            let got = unsafe { libc::proc_pidinfo(pid as i32, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), (n * std::mem::size_of::<libc::proc_fdinfo>()) as i32) };
            if got <= 0 {
                continue;
            }
            fds.truncate(got as usize / std::mem::size_of::<libc::proc_fdinfo>());
            let held = fds.iter().filter(|f| f.proc_fdtype as i32 == libc::PROX_FDTYPE_SOCKET).any(|f| {
                let mut buf = [0u8; 1024];
                let r = unsafe { libc::proc_pidfdinfo(pid as i32, f.proc_fd, PROC_PIDFDSOCKETINFO, buf.as_mut_ptr().cast(), buf.len() as i32) };
                let i32_at = |at: usize| i32::from_ne_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]);
                // An established TCP connection over IPv4 or IPv6 to the port; the port is in network
                // byte order in the low 16 bits.
                r >= (TCP_STATE + 4) as i32
                    && i32_at(PROTOCOL) == libc::IPPROTO_TCP
                    && matches!(i32_at(FAMILY), libc::AF_INET | libc::AF_INET6)
                    && i32_at(KIND) == SOCKINFO_TCP
                    && i32_at(TCP_STATE) == ESTABLISHED
                    && u16::from_be_bytes([buf[FOREIGN_PORT], buf[FOREIGN_PORT + 1]]) == port
            });
            if held {
                out.push(pid);
            }
        }
        Some(out)
    }
}
