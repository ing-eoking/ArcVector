use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

pub const NOBODY: &str = "-";

static OURS: OnceLock<String> = OnceLock::new();

pub fn install() -> &'static str {
    ours()
}

pub fn ours() -> &'static str {
    OURS.get_or_init(|| {
        let node = match node_address() {
            Some(mac) => mac.iter().fold(String::new(), |mut out, byte| {
                use std::fmt::Write as _;
                let _ = write!(out, "{byte:02x}");
                out
            }),
            None => format!("{:016x}", unpredictable()),
        };
        format!("{node}/{}/{}", std::process::id(), started())
    })
}

fn started() -> u128 {
    static AT: OnceLock<u128> = OnceLock::new();
    *AT.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos())
    })
}

fn unpredictable() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

#[cfg(test)]
const LINUX_SYSFS: bool = cfg!(any(target_os = "linux", target_os = "android"));

#[cfg(any(target_os = "linux", target_os = "android"))]
fn node_address() -> Option<[u8; 6]> {
    lowest_in(std::path::Path::new("/sys/class/net"))
}

#[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
fn lowest_in(root: &std::path::Path) -> Option<[u8; 6]> {
    let mut names: Vec<_> = std::fs::read_dir(root)
        .ok()?
        .filter_map(|entry| Some(entry.ok()?.file_name()))
        .collect();
    names.sort();
    names.iter().find_map(|name| {
        if name.as_encoded_bytes().starts_with(b"lo") {
            return None;
        }
        parse_address(&std::fs::read_to_string(root.join(name).join("address")).ok()?)
    })
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
fn node_address() -> Option<[u8; 6]> {
    link_layer::lowest_named()
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
)))]
fn node_address() -> Option<[u8; 6]> {
    None
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
mod link_layer {
    use std::ffi::CStr;
    use std::os::raw::{c_char, c_int, c_uint, c_void};

    const AF_LINK: u8 = 18;
    const SDL_DATA_AT: usize = 8;

    #[repr(C)]
    struct Ifaddrs {
        next: *mut Ifaddrs,
        name: *mut c_char,
        flags: c_uint,
        addr: *mut SockaddrHeader,
        netmask: *mut c_void,
        broadcast: *mut c_void,
        data: *mut c_void,
    }

    #[repr(C)]
    struct SockaddrHeader {
        len: u8,
        family: u8,
    }

    #[repr(C)]
    struct SockaddrDl {
        len: u8,
        family: u8,
        index: u16,
        kind: u8,
        name_len: u8,
        addr_len: u8,
        selector_len: u8,
    }

    unsafe extern "C" {
        fn getifaddrs(list: *mut *mut Ifaddrs) -> c_int;
        fn freeifaddrs(list: *mut Ifaddrs);
    }

    pub fn lowest_named() -> Option<[u8; 6]> {
        let mut list: *mut Ifaddrs = std::ptr::null_mut();
        if unsafe { getifaddrs(&raw mut list) } != 0 || list.is_null() {
            return None;
        }
        let found = walk(list);
        unsafe { freeifaddrs(list) };
        found
    }

    fn walk(list: *mut Ifaddrs) -> Option<[u8; 6]> {
        let mut best: Option<(String, [u8; 6])> = None;
        let mut at = list;
        while !at.is_null() {
            let entry = unsafe { &*at };
            at = entry.next;
            if entry.name.is_null() {
                continue;
            }
            let Ok(name) = unsafe { CStr::from_ptr(entry.name) }.to_str() else {
                continue;
            };
            if name.starts_with("lo") {
                continue;
            }
            let Some(mac) = address_of(entry.addr) else {
                continue;
            };
            if best.as_ref().is_none_or(|(seen, _)| name < seen.as_str()) {
                best = Some((name.to_owned(), mac));
            }
        }
        best.map(|(_, mac)| mac)
    }

    fn address_of(addr: *mut SockaddrHeader) -> Option<[u8; 6]> {
        if addr.is_null() {
            return None;
        }
        let header = unsafe { &*addr };
        if header.family != AF_LINK {
            return None;
        }
        let link = unsafe { &*addr.cast::<SockaddrDl>() };
        let (name_len, addr_len) = (link.name_len as usize, link.addr_len as usize);
        if addr_len != 6 || SDL_DATA_AT + name_len + addr_len > link.len as usize {
            return None;
        }
        let mut mac = [0u8; 6];
        let from = unsafe { addr.cast::<u8>().add(SDL_DATA_AT + name_len) };
        unsafe { std::ptr::copy_nonoverlapping(from, mac.as_mut_ptr(), 6) };
        (mac != [0u8; 6]).then_some(mac)
    }
}

#[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
fn parse_address(text: &str) -> Option<[u8; 6]> {
    let mut out = [0u8; 6];
    let mut parts = text.trim().split(':');
    for slot in &mut out {
        *slot = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    if parts.next().is_some() || out == [0u8; 6] {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::{NOBODY, ours, parse_address};

    #[test]
    fn an_address_reads_back_as_six_bytes() {
        assert_eq!(
            parse_address("02:42:ac:11:00:02\n"),
            Some([0x02, 0x42, 0xac, 0x11, 0x00, 0x02])
        );
    }

    #[test]
    fn what_is_not_an_address_is_refused() {
        assert_eq!(parse_address("00:00:00:00:00:00"), None, "an unset card");
        assert_eq!(parse_address("02:42:ac:11:00"), None, "too few");
        assert_eq!(parse_address("02:42:ac:11:00:02:03"), None, "too many");
        assert_eq!(parse_address("zz:42:ac:11:00:02"), None, "not hex");
        assert_eq!(parse_address(""), None);
    }

    #[test]
    fn sysfs_takes_the_lowest_named_card_that_has_an_address() {
        let root = std::env::temp_dir().join(format!("arcv-net-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (name, address) in [
            ("lo", "00:00:00:00:00:00"),
            ("veth9", "aa:bb:cc:dd:ee:01"),
            ("eth0", "02:42:ac:11:00:02"),
            ("bond0", "00:00:00:00:00:00"),
        ] {
            std::fs::create_dir_all(root.join(name)).unwrap();
            std::fs::write(root.join(name).join("address"), format!("{address}\n")).unwrap();
        }

        assert_eq!(
            super::lowest_in(&root),
            Some([0x02, 0x42, 0xac, 0x11, 0x00, 0x02]),
            "bond0 has no address and lo is skipped, so eth0 wins over veth9 by name"
        );
        assert_eq!(super::lowest_in(&root.join("nowhere")), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn this_machine_answers_with_an_address() {
        let found = super::node_address();
        if super::LINUX_SYSFS {
            return;
        }
        assert!(
            found.is_some(),
            "getifaddrs found no link-layer address on a platform that has them"
        );
        println!("{:02x?}", found.unwrap());
    }

    #[test]
    fn the_token_is_one_value_for_the_life_of_the_process() {
        let first = ours();
        assert_eq!(first, ours());
        assert_ne!(first, NOBODY);
        let parts: Vec<_> = first.split('/').collect();
        assert_eq!(parts.len(), 3, "node, pid, start: {first}");
        assert_eq!(
            parts[1],
            std::process::id().to_string(),
            "the middle part is this process"
        );
        assert!(!parts[0].is_empty() && !parts[2].is_empty());
    }
}
