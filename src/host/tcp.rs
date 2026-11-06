//! The TCP sockets processes hold: the check that both sides of an implementation comparison talk to
//! the bench file's fixture, and not to a server of their own (SPEC §9.10). One read of the OS's socket
//! table (Windows) or of the given processes' sockets (Linux, macOS); never a helper process.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// One TCP socket a process holds: listening on `local`, or an established connection from `local`
/// to `remote`. Addresses are canonical (an IPv4-mapped IPv6 address is its IPv4 one), so the two
/// ends of a loopback connection name each other exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Socket {
    pub pid: u32,
    pub local: (IpAddr, u16),
    pub remote: (IpAddr, u16),
    pub listening: bool,
}

/// The listening and established TCP sockets (IPv4 and IPv6) of `pids`. None when the OS gave no
/// answer.
pub fn sockets(pids: &[u32]) -> Option<Vec<Socket>> {
    os::sockets(pids)
}

/// An IPv4 address from its four bytes in network order, or IPv6 from sixteen.
fn ip(bytes: &[u8]) -> IpAddr {
    match bytes.len() {
        4 => IpAddr::V4(Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3])),
        _ => {
            let mut b = [0u8; 16];
            b.copy_from_slice(&bytes[..16]);
            IpAddr::V6(Ipv6Addr::from(b)).to_canonical()
        }
    }
}

fn unspecified() -> IpAddr {
    IpAddr::V4(Ipv4Addr::UNSPECIFIED)
}

#[cfg(windows)]
mod os {
    use super::{Socket, ip, unspecified};
    use windows_sys::Win32::NetworkManagement::IpHelper::{GetExtendedTcpTable, TCP_TABLE_OWNER_PID_ALL};

    /// `GetExtendedTcpTable` for IPv4 and IPv6, every socket with its owning pid.
    pub fn sockets(pids: &[u32]) -> Option<Vec<Socket>> {
        const LISTEN: u32 = 2;
        const ESTABLISHED: u32 = 5;
        let mut out = Vec::new();
        // (address family, row size, address length, offsets of the local address and port, the remote
        // address and port, the state and the pid in a row)
        for (af, row, len, la, lp, ra, rp, state, pid) in [(2u32, 24usize, 4usize, 4usize, 8usize, 12usize, 16usize, 0usize, 20usize), (23, 56, 16, 0, 20, 24, 44, 48, 52)] {
            let mut size = 0u32;
            unsafe { GetExtendedTcpTable(std::ptr::null_mut(), &mut size, 0, af, TCP_TABLE_OWNER_PID_ALL, 0) };
            let mut buf = vec![0u8; size as usize + 4096];
            let mut got = buf.len() as u32;
            if unsafe { GetExtendedTcpTable(buf.as_mut_ptr().cast(), &mut got, 0, af, TCP_TABLE_OWNER_PID_ALL, 0) } != 0 {
                return None;
            }
            let u32_at = |at: usize| buf.get(at..at + 4).map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]])).unwrap_or(0);
            // Ports are in network byte order in the low 16 bits; addresses in network order.
            let port_at = |at: usize| u16::from_be_bytes([buf.get(at).copied().unwrap_or(0), buf.get(at + 1).copied().unwrap_or(0)]);
            let addr_at = |at: usize| buf.get(at..at + len).map(ip).unwrap_or_else(unspecified);
            let n = u32_at(0) as usize;
            // The table's rows start after its count, aligned to 4 bytes.
            for i in 0..n {
                let at = 4 + i * row;
                let (owner, st) = (u32_at(at + pid), u32_at(at + state));
                if pids.contains(&owner) && (st == LISTEN || st == ESTABLISHED) {
                    let remote = if st == LISTEN { (unspecified(), 0) } else { (addr_at(at + ra), port_at(at + rp)) };
                    out.push(Socket { pid: owner, local: (addr_at(at + la), port_at(at + lp)), remote, listening: st == LISTEN });
                }
            }
        }
        Some(out)
    }
}

#[cfg(target_os = "linux")]
mod os {
    use super::{Socket, ip, unspecified};
    use std::collections::HashMap;
    use std::net::IpAddr;

    /// `ADDR:PORT` of `/proc/net/tcp`: the address as 32-bit words in host (little-endian) order.
    fn endpoint(s: &str) -> Option<(IpAddr, u16)> {
        let (a, p) = s.split_once(':')?;
        let mut bytes = Vec::with_capacity(16);
        for word in (0..a.len()).step_by(8) {
            bytes.extend(u32::from_str_radix(a.get(word..word + 8)?, 16).ok()?.to_le_bytes());
        }
        if bytes.len() != 4 && bytes.len() != 16 {
            return None;
        }
        Some((ip(&bytes), u16::from_str_radix(p, 16).ok()?))
    }

    /// The listening and established sockets by inode (`/proc/net/tcp`, `tcp6`), then which of the
    /// processes hold each among their descriptors (`/proc/<pid>/fd`).
    pub fn sockets(pids: &[u32]) -> Option<Vec<Socket>> {
        type Ends = ((IpAddr, u16), (IpAddr, u16), bool);
        let mut by_inode: HashMap<String, Ends> = HashMap::new();
        let mut read = false;
        for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
            let Ok(text) = std::fs::read_to_string(table) else { continue };
            read = true;
            for line in text.lines().skip(1) {
                let f: Vec<&str> = line.split_whitespace().collect();
                let (Some(loc), Some(rem), Some(st), Some(inode)) = (f.get(1), f.get(2), f.get(3), f.get(9)) else { continue };
                let (Some(local), Some(remote)) = (endpoint(loc), endpoint(rem)) else { continue };
                match *st {
                    "0A" => by_inode.insert(format!("socket:[{inode}]"), (local, (unspecified(), 0), true)),
                    "01" => by_inode.insert(format!("socket:[{inode}]"), (local, remote, false)),
                    _ => None,
                };
            }
        }
        if !read {
            return None;
        }
        let mut out = Vec::new();
        for &pid in pids {
            for e in std::fs::read_dir(format!("/proc/{pid}/fd")).into_iter().flatten().flatten() {
                if let Some(&(local, remote, listening)) = std::fs::read_link(e.path()).ok().and_then(|l| by_inode.get(&*l.to_string_lossy()))
                    && !out.contains(&Socket { pid, local, remote, listening })
                {
                    out.push(Socket { pid, local, remote, listening });
                }
            }
        }
        Some(out)
    }
}

#[cfg(target_os = "macos")]
mod os {
    use super::{Socket, ip, unspecified};

    /// Each process's descriptors (`PROC_PIDLISTFDS`), and of its sockets the listening and
    /// established TCP ones (`PROC_PIDFDSOCKETINFO`, read from `struct socket_fdinfo`).
    pub fn sockets(pids: &[u32]) -> Option<Vec<Socket>> {
        const PROC_PIDFDSOCKETINFO: libc::c_int = 3;
        // struct socket_fdinfo: proc_fileinfo (24 bytes), then socket_info: vinfo_stat (136), so,
        // pcb, type, protocol, family, …, two sockbuf_info, kind, rfu, then the protocol's info,
        // whose in_sockinfo holds the foreign port, the local port, …, insi_vflag (+24), then the
        // foreign and local addresses (+32, +48: 16 bytes each, an IPv4 one in the last 4). Offsets
        // from the start of the struct.
        const PROTOCOL: usize = 180;
        const FAMILY: usize = 184;
        const FOREIGN_PORT: usize = 264;
        const LOCAL_PORT: usize = 268;
        const VFLAG: usize = FOREIGN_PORT + 24;
        const FOREIGN_ADDR: usize = FOREIGN_PORT + 32;
        const LOCAL_ADDR: usize = FOREIGN_PORT + 48;
        const INI_IPV4: u8 = 1;
        // soi_kind (SOCKINFO_TCP: the protocol info is a tcp_sockinfo) and, past its in_sockinfo (80
        // bytes), tcpsi_state.
        const KIND: usize = 256;
        const SOCKINFO_TCP: i32 = 2;
        const TCP_STATE: usize = FOREIGN_PORT + 80;
        const LISTEN: i32 = 1;
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
            for f in fds.iter().filter(|f| f.proc_fdtype as i32 == libc::PROX_FDTYPE_SOCKET) {
                let mut buf = [0u8; 1024];
                let r = unsafe { libc::proc_pidfdinfo(pid as i32, f.proc_fd, PROC_PIDFDSOCKETINFO, buf.as_mut_ptr().cast(), buf.len() as i32) };
                let i32_at = |at: usize| i32::from_ne_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]);
                // Ports are in network byte order in the low 16 bits.
                let port_at = |at: usize| u16::from_be_bytes([buf[at], buf[at + 1]]);
                let v4 = buf[VFLAG] & INI_IPV4 != 0;
                let addr_at = |at: usize| if v4 { ip(&buf[at + 12..at + 16]) } else { ip(&buf[at..at + 16]) };
                if r < (TCP_STATE + 4) as i32 || i32_at(PROTOCOL) != libc::IPPROTO_TCP || !matches!(i32_at(FAMILY), libc::AF_INET | libc::AF_INET6) || i32_at(KIND) != SOCKINFO_TCP {
                    continue;
                }
                let socket = match i32_at(TCP_STATE) {
                    LISTEN => Socket { pid, local: (addr_at(LOCAL_ADDR), port_at(LOCAL_PORT)), remote: (unspecified(), 0), listening: true },
                    ESTABLISHED => Socket { pid, local: (addr_at(LOCAL_ADDR), port_at(LOCAL_PORT)), remote: (addr_at(FOREIGN_ADDR), port_at(FOREIGN_PORT)), listening: false },
                    _ => continue,
                };
                if !out.contains(&socket) {
                    out.push(socket);
                }
            }
        }
        Some(out)
    }
}
