//! C structs shared with the kernel, built and decoded without `unsafe`.
//!
//! Instead of zero-initialising `libc` types (which needs `unsafe` because
//! some have private padding fields), each type here either mirrors the
//! kernel ABI with plain `#[repr(C)]` fields ([`MsgHdr`]) or is an aligned
//! byte buffer read at `offset_of!` positions ([`StatxBuf`], [`AddrBuf`]).
//! Compile-time assertions check every layout, field offset and field type
//! against the `libc` definition, so a mismatch fails the build.

use std::ffi::OsStr;
use std::io;
use std::mem::{align_of, offset_of, size_of};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};
use std::os::unix::ffi::OsStrExt;

use socket2::SockAddr;

// ---------------------------------------------------------------- msghdr

/// `struct msghdr` exactly as the kernel reads it (`struct user_msghdr`).
///
/// It differs from `libc::msghdr` only in having no private padding fields,
/// so it can be built safely. (musl splits the `size_t` lengths into an `int`
/// plus padding; the bytes the kernel sees are the same.)
#[repr(C)]
pub(crate) struct MsgHdr {
    pub(crate) name: *mut libc::c_void,
    pub(crate) namelen: libc::socklen_t,
    pub(crate) iov: *mut libc::iovec,
    pub(crate) iovlen: usize,
    pub(crate) control: *mut libc::c_void,
    pub(crate) controllen: usize,
    pub(crate) flags: libc::c_int,
}

const _: () = {
    assert!(size_of::<MsgHdr>() == size_of::<libc::msghdr>());
    assert!(align_of::<MsgHdr>() == align_of::<libc::msghdr>());
    assert!(offset_of!(MsgHdr, name) == offset_of!(libc::msghdr, msg_name));
    assert!(offset_of!(MsgHdr, namelen) == offset_of!(libc::msghdr, msg_namelen));
    assert!(offset_of!(MsgHdr, iov) == offset_of!(libc::msghdr, msg_iov));
    assert!(offset_of!(MsgHdr, iovlen) == offset_of!(libc::msghdr, msg_iovlen));
    assert!(offset_of!(MsgHdr, control) == offset_of!(libc::msghdr, msg_control));
    assert!(offset_of!(MsgHdr, controllen) == offset_of!(libc::msghdr, msg_controllen));
    assert!(offset_of!(MsgHdr, flags) == offset_of!(libc::msghdr, msg_flags));
};

impl MsgHdr {
    /// Null pointers and zero lengths.
    pub(crate) fn empty() -> Self {
        Self {
            name: std::ptr::null_mut(),
            namelen: 0,
            iov: std::ptr::null_mut(),
            iovlen: 0,
            control: std::ptr::null_mut(),
            controllen: 0,
            flags: 0,
        }
    }

    pub(crate) fn as_ptr(&self) -> *const libc::msghdr {
        (self as *const Self).cast()
    }

    pub(crate) fn as_mut_ptr(&mut self) -> *mut libc::msghdr {
        (self as *mut Self).cast()
    }
}

// ---------------------------------------------------------------- statx

/// A zeroed, aligned buffer the kernel fills with a `struct statx`.
#[repr(C, align(8))]
pub(crate) struct StatxBuf([u8; size_of::<libc::statx>()]);

const _: () = assert!(align_of::<StatxBuf>() >= align_of::<libc::statx>());

impl StatxBuf {
    pub(crate) fn zeroed() -> Box<Self> {
        Box::new(Self([0; size_of::<libc::statx>()]))
    }

    pub(crate) fn as_mut_ptr(&mut self) -> *mut libc::statx {
        (self as *mut Self).cast()
    }

    fn read<const N: usize>(&self, offset: usize) -> [u8; N] {
        <[u8; N]>::try_from(&self.0[offset..offset + N]).expect("slice length equals N")
    }
}

/// Generates `StatxBuf` getters that read a `libc::statx` field by offset.
/// The `const` closure fails to compile unless the field has exactly the
/// stated type, so the read size always matches the field.
macro_rules! statx_getters {
    ($($get:ident: $ty:ty = $($field:ident).+;)*) => {
        impl StatxBuf {
            $(
                pub(crate) fn $get(&self) -> $ty {
                    const _: fn(&libc::statx) -> $ty = |st: &libc::statx| -> $ty { st.$($field).+ };
                    <$ty>::from_ne_bytes(self.read(offset_of!(libc::statx, $($field).+)))
                }
            )*
        }
    };
}

statx_getters! {
    size: u64 = stx_size;
    mode: u16 = stx_mode;
    uid: u32 = stx_uid;
    gid: u32 = stx_gid;
    nlink: u32 = stx_nlink;
    ino: u64 = stx_ino;
    blksize: u32 = stx_blksize;
    blocks: u64 = stx_blocks;
    atime_sec: i64 = stx_atime.tv_sec;
    atime_nsec: u32 = stx_atime.tv_nsec;
    mtime_sec: i64 = stx_mtime.tv_sec;
    mtime_nsec: u32 = stx_mtime.tv_nsec;
    ctime_sec: i64 = stx_ctime.tv_sec;
    ctime_nsec: u32 = stx_ctime.tv_nsec;
}

// ---------------------------------------------------------------- sockaddr

/// A zeroed, aligned `sockaddr_storage`-sized buffer the kernel fills with a
/// peer address (`recvmsg`'s `msg_name`).
#[repr(C, align(8))]
pub(crate) struct AddrBuf([u8; size_of::<libc::sockaddr_storage>()]);

const _: () = {
    assert!(align_of::<AddrBuf>() >= align_of::<libc::sockaddr_storage>());
    // The fields decoded below have the types we read them as.
    let _: fn(&libc::sockaddr) -> libc::sa_family_t =
        |sa: &libc::sockaddr| -> libc::sa_family_t { sa.sa_family };
    let _: fn(&libc::sockaddr_in) -> u16 = |sin: &libc::sockaddr_in| -> u16 { sin.sin_port };
    let _: fn(&libc::in_addr) -> u32 = |a: &libc::in_addr| -> u32 { a.s_addr };
    let _: fn(&libc::sockaddr_in6) -> u16 = |sin6: &libc::sockaddr_in6| -> u16 { sin6.sin6_port };
    let _: fn(&libc::sockaddr_in6) -> u32 =
        |sin6: &libc::sockaddr_in6| -> u32 { sin6.sin6_flowinfo };
    let _: fn(&libc::sockaddr_in6) -> u32 =
        |sin6: &libc::sockaddr_in6| -> u32 { sin6.sin6_scope_id };
    let _: fn(&libc::in6_addr) -> [u8; 16] = |a: &libc::in6_addr| -> [u8; 16] { a.s6_addr };
};

impl AddrBuf {
    pub(crate) fn zeroed() -> Self {
        Self([0; size_of::<libc::sockaddr_storage>()])
    }

    pub(crate) fn as_mut_ptr(&mut self) -> *mut libc::c_void {
        (self as *mut Self).cast()
    }

    pub(crate) fn capacity(&self) -> libc::socklen_t {
        self.0.len() as libc::socklen_t
    }

    fn read<const N: usize>(&self, offset: usize) -> [u8; N] {
        <[u8; N]>::try_from(&self.0[offset..offset + N]).expect("slice length equals N")
    }

    /// Decodes the `len`-byte address the kernel wrote, using only socket2's
    /// safe constructors. Supports IPv4, IPv6 and Unix (pathname, abstract and
    /// unnamed) addresses.
    pub(crate) fn decode(&self, len: libc::socklen_t) -> io::Result<SockAddr> {
        let len: usize = (len as usize).min(self.0.len());
        if len == 0 {
            // Linux reports a datagram from an unbound Unix socket with a
            // zero-length address, as std's `UnixDatagram::recv_from` notes.
            return Ok(unnamed_unix());
        }
        let family_at: usize = offset_of!(libc::sockaddr, sa_family);
        if len < family_at + size_of::<libc::sa_family_t>() {
            return Err(bad_addr("truncated socket address"));
        }
        let family: libc::sa_family_t = libc::sa_family_t::from_ne_bytes(self.read(family_at));
        match libc::c_int::from(family) {
            libc::AF_INET => {
                if len < size_of::<libc::sockaddr_in>() {
                    return Err(bad_addr("truncated IPv4 address"));
                }
                let port: u16 =
                    u16::from_be_bytes(self.read(offset_of!(libc::sockaddr_in, sin_port)));
                let ip: [u8; 4] = self.read(offset_of!(libc::sockaddr_in, sin_addr));
                Ok(SockAddr::from(SocketAddrV4::new(Ipv4Addr::from(ip), port)))
            }
            libc::AF_INET6 => {
                if len < size_of::<libc::sockaddr_in6>() {
                    return Err(bad_addr("truncated IPv6 address"));
                }
                let port: u16 =
                    u16::from_be_bytes(self.read(offset_of!(libc::sockaddr_in6, sin6_port)));
                // Like std and socket2, flowinfo and scope id are kept as-is.
                let flowinfo: u32 =
                    u32::from_ne_bytes(self.read(offset_of!(libc::sockaddr_in6, sin6_flowinfo)));
                let scope_id: u32 =
                    u32::from_ne_bytes(self.read(offset_of!(libc::sockaddr_in6, sin6_scope_id)));
                let ip: [u8; 16] = self.read(offset_of!(libc::sockaddr_in6, sin6_addr));
                Ok(SockAddr::from(SocketAddrV6::new(
                    Ipv6Addr::from(ip),
                    port,
                    flowinfo,
                    scope_id,
                )))
            }
            libc::AF_UNIX => {
                let path: &[u8] = &self.0[offset_of!(libc::sockaddr_un, sun_path).min(len)..len];
                match path.first() {
                    None => Ok(unnamed_unix()),
                    // Abstract names start with NUL and may contain NULs.
                    Some(0) => SockAddr::unix(OsStr::from_bytes(path)),
                    // Pathnames may carry a trailing NUL; cut at the first one.
                    Some(_) => {
                        let end: usize = path
                            .iter()
                            .position(|&b: &u8| -> bool { b == 0 })
                            .unwrap_or(path.len());
                        SockAddr::unix(OsStr::from_bytes(&path[..end]))
                    }
                }
            }
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported socket address family {family}"),
            )),
        }
    }
}

fn unnamed_unix() -> SockAddr {
    SockAddr::unix("").expect("an empty Unix path is always valid")
}

fn bad_addr(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    /// Writes each `(offset, bytes)` into a zeroed buffer, as the kernel would.
    fn buf_with(bytes: &[(usize, &[u8])]) -> AddrBuf {
        let mut buf: AddrBuf = AddrBuf::zeroed();
        for (offset, data) in bytes {
            buf.0[*offset..*offset + data.len()].copy_from_slice(data);
        }
        buf
    }

    #[test]
    fn decodes_ipv4() {
        let buf: AddrBuf = buf_with(&[
            (
                offset_of!(libc::sockaddr_in, sin_family),
                &(libc::AF_INET as libc::sa_family_t).to_ne_bytes(),
            ),
            (
                offset_of!(libc::sockaddr_in, sin_port),
                &8080u16.to_be_bytes(),
            ),
            (offset_of!(libc::sockaddr_in, sin_addr), &[127, 0, 0, 1]),
        ]);
        let addr: SockAddr = buf.decode(size_of::<libc::sockaddr_in>() as u32).unwrap();
        assert_eq!(
            addr.as_socket(),
            Some("127.0.0.1:8080".parse::<SocketAddr>().unwrap())
        );
    }

    #[test]
    fn decodes_ipv6() {
        let ip: Ipv6Addr = "::1".parse().unwrap();
        let buf: AddrBuf = buf_with(&[
            (
                offset_of!(libc::sockaddr_in6, sin6_family),
                &(libc::AF_INET6 as libc::sa_family_t).to_ne_bytes(),
            ),
            (
                offset_of!(libc::sockaddr_in6, sin6_port),
                &443u16.to_be_bytes(),
            ),
            (
                offset_of!(libc::sockaddr_in6, sin6_scope_id),
                &7u32.to_ne_bytes(),
            ),
            (offset_of!(libc::sockaddr_in6, sin6_addr), &ip.octets()),
        ]);
        let addr: SockAddr = buf.decode(size_of::<libc::sockaddr_in6>() as u32).unwrap();
        assert_eq!(
            addr.as_socket_ipv6(),
            Some(SocketAddrV6::new(ip, 443, 0, 7))
        );
    }

    #[test]
    fn decodes_unix_variants() {
        let family: [u8; 2] = (libc::AF_UNIX as libc::sa_family_t).to_ne_bytes();
        let path_at: usize = offset_of!(libc::sockaddr_un, sun_path);

        // Pathname with the trailing NUL the kernel includes.
        let buf: AddrBuf = buf_with(&[(0, &family), (path_at, b"/tmp/x.sock\0")]);
        let addr: SockAddr = buf.decode((path_at + 12) as u32).unwrap();
        assert_eq!(
            addr.as_pathname(),
            Some(std::path::Path::new("/tmp/x.sock"))
        );

        // Abstract name.
        let buf: AddrBuf = buf_with(&[(0, &family), (path_at, b"\0name")]);
        let addr: SockAddr = buf.decode((path_at + 5) as u32).unwrap();
        assert_eq!(addr.as_abstract_namespace(), Some(&b"name"[..]));

        // Unnamed: zero length, or just the family.
        assert!(AddrBuf::zeroed().decode(0).unwrap().is_unnamed());
        let buf: AddrBuf = buf_with(&[(0, &family)]);
        assert!(buf.decode(path_at as u32).unwrap().is_unnamed());
    }

    #[test]
    fn rejects_unknown_family_and_truncation() {
        let buf: AddrBuf = buf_with(&[(0, &(libc::AF_PACKET as libc::sa_family_t).to_ne_bytes())]);
        assert_eq!(
            buf.decode(16).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        let buf: AddrBuf = buf_with(&[(0, &(libc::AF_INET as libc::sa_family_t).to_ne_bytes())]);
        assert_eq!(
            buf.decode(4).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
