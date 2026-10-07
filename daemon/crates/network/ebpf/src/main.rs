//! Per-process network accounting, blocking and priority - the kernel half.
//!
//! Seven small programs, all attached to the root cgroup so they see every
//! socket on the machine:
//!
//! | program | hook | what it does |
//! |---|---|---|
//! | `pyren_sock_create` | `cgroup/sock_create` | remembers which process made the socket |
//! | `pyren_connect4/6`, `pyren_sendmsg4/6` | `cgroup/connect*`, `cgroup/sendmsg*` | the same, for a socket that predates the daemon |
//! | `pyren_egress`, `pyren_ingress` | `cgroup_skb/*` | counts the packet against its owner, then drops it or tags it |
//!
//! ## Why the owner is recorded at the socket and not read per packet
//!
//! A packet has no process. Egress often runs in the sender's context, but
//! a retransmit or an ACK leaves from a softirq on whatever task happened
//! to be interrupted, and ingress is never in the receiver's context at
//! all. The hooks in the first two rows do run in the caller's context, so
//! the thread-group id is taken there and kept in socket-local storage,
//! where both packet hooks can find it. `BPF_F_CLONE` hands the same owner
//! to every socket `accept()` clones off a listener.
//!
//! ## What user space decides
//!
//! Everything that is policy. `POLICY` maps a thread-group id straight to
//! what to do with its packets - [`POLICY_BLOCK`], or the `skb->priority`
//! to stamp on the way out - so this file knows nothing about rules,
//! process names, or which qdisc is listening for that priority.
#![no_std]
#![no_main]

use aya_ebpf::{
    bindings::{
        __sk_buff, bpf_map_type::BPF_MAP_TYPE_SK_STORAGE, bpf_sock, BPF_F_CLONE,
        BPF_F_NO_PREALLOC, BPF_SK_STORAGE_GET_F_CREATE,
    },
    helpers::{bpf_get_current_pid_tgid, generated::bpf_sk_storage_get},
    macros::{btf_map, cgroup_skb, cgroup_sock, cgroup_sock_addr, map},
    maps::{HashMap, LruPerCpuHashMap},
    programs::{SkBuffContext, SockAddrContext, SockContext},
};

/// Bytes that reached, or left, one process. One slot per CPU, so the
/// packet path never contends on it; user space sums the slots.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Traffic {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// `POLICY` value: drop every packet of this process, both ways.
const POLICY_BLOCK: u32 = u32::MAX;

/// `lo` is interface 1 in every network namespace. Traffic to yourself is
/// not what a network page is about, and blocking it would cut a process
/// off from its own helpers rather than from the network.
const LOOPBACK_IFINDEX: u32 = 1;

const ALLOW: i32 = 1;
const DROP: i32 = 0;

const MAX_PROCESSES: u32 = 4096;

const SOCK_OWNER_FLAGS: usize = (BPF_F_NO_PREALLOC | BPF_F_CLONE) as usize;

/// Socket-local storage holding the owning thread-group id.
///
/// Spelled out rather than taken from `aya_ebpf::btf_maps::SkStorage`
/// because that type fixes the flags at `BPF_F_NO_PREALLOC`, and the clone
/// flag is the whole reason accepted connections have an owner. The field
/// layout is the BTF map definition every loader expects.
#[repr(C)]
pub struct SockOwner {
    r#type: *const [i32; BPF_MAP_TYPE_SK_STORAGE as usize],
    key: *const i32,
    value: *const u32,
    max_entries: *const [i32; 0],
    map_flags: *const [i32; SOCK_OWNER_FLAGS],
}

// The fields are placeholders the loader patches; nothing dereferences them.
unsafe impl Sync for SockOwner {}

#[btf_map]
static SOCK_OWNER: SockOwner = SockOwner {
    r#type: core::ptr::null(),
    key: core::ptr::null(),
    value: core::ptr::null(),
    max_entries: core::ptr::null(),
    map_flags: core::ptr::null(),
};

#[map]
static TRAFFIC: LruPerCpuHashMap<u32, Traffic> =
    LruPerCpuHashMap::with_max_entries(MAX_PROCESSES, 0);

#[map]
static POLICY: HashMap<u32, u32> = HashMap::with_max_entries(MAX_PROCESSES, 0);

#[inline(always)]
fn sock_owner_map() -> *mut core::ffi::c_void {
    core::ptr::addr_of!(SOCK_OWNER).cast_mut().cast()
}

/// Records the calling process as the owner of `sk`, unless it has one.
///
/// "Unless" matters for the connect and sendmsg hooks: a socket handed to
/// a child still belongs, for accounting, to whoever opened it, and one
/// answer that never changes is easier to reason about than the last
/// process to touch it.
#[inline(always)]
fn claim(sk: *mut core::ffi::c_void) {
    if sk.is_null() {
        return;
    }
    let existing = unsafe { bpf_sk_storage_get(sock_owner_map(), sk, core::ptr::null_mut(), 0) };
    if !existing.is_null() {
        return;
    }
    let mut tgid = (bpf_get_current_pid_tgid() >> 32) as u32;
    unsafe {
        bpf_sk_storage_get(
            sock_owner_map(),
            sk,
            core::ptr::addr_of_mut!(tgid).cast(),
            BPF_SK_STORAGE_GET_F_CREATE as u64,
        );
    }
}

#[cgroup_sock(sock_create)]
pub fn pyren_sock_create(ctx: SockContext) -> i32 {
    let sk: *mut bpf_sock = ctx.sock;
    claim(sk.cast());
    ALLOW
}

#[inline(always)]
fn claim_addr(ctx: &SockAddrContext) -> i32 {
    let sk = unsafe { (*ctx.sock_addr).__bindgen_anon_1.sk };
    claim(sk.cast());
    ALLOW
}

#[cgroup_sock_addr(connect4)]
pub fn pyren_connect4(ctx: SockAddrContext) -> i32 {
    claim_addr(&ctx)
}

#[cgroup_sock_addr(connect6)]
pub fn pyren_connect6(ctx: SockAddrContext) -> i32 {
    claim_addr(&ctx)
}

#[cgroup_sock_addr(sendmsg4)]
pub fn pyren_sendmsg4(ctx: SockAddrContext) -> i32 {
    claim_addr(&ctx)
}

#[cgroup_sock_addr(sendmsg6)]
pub fn pyren_sendmsg6(ctx: SockAddrContext) -> i32 {
    claim_addr(&ctx)
}

#[inline(always)]
fn packet(skb: *mut __sk_buff, egress: bool) -> i32 {
    if unsafe { (*skb).ifindex } == LOOPBACK_IFINDEX {
        return ALLOW;
    }
    let sk = unsafe { (*skb).__bindgen_anon_2.sk };
    if sk.is_null() {
        return ALLOW;
    }
    let owner =
        unsafe { bpf_sk_storage_get(sock_owner_map(), sk.cast(), core::ptr::null_mut(), 0) };
    if owner.is_null() {
        return ALLOW;
    }
    let tgid = unsafe { *owner.cast::<u32>() };

    if let Some(policy) = POLICY.get_ptr(&tgid) {
        let policy = unsafe { *policy };
        if policy == POLICY_BLOCK {
            return DROP;
        }
        // Only on the way out: a priority is a hint to the qdisc the
        // packet is about to be queued on, and there is no queue of ours
        // in front of a packet that has already arrived.
        if egress && policy != 0 {
            unsafe { (*skb).priority = policy };
        }
    }

    let len = unsafe { (*skb).len } as u64;
    match TRAFFIC.get_ptr_mut(&tgid) {
        Some(traffic) => unsafe {
            if egress {
                (*traffic).tx_bytes += len;
            } else {
                (*traffic).rx_bytes += len;
            }
        },
        None => {
            let first = if egress {
                Traffic {
                    rx_bytes: 0,
                    tx_bytes: len,
                }
            } else {
                Traffic {
                    rx_bytes: len,
                    tx_bytes: 0,
                }
            };
            let _ = TRAFFIC.insert(&tgid, &first, 0);
        }
    }
    ALLOW
}

#[cgroup_skb(egress)]
pub fn pyren_egress(ctx: SkBuffContext) -> i32 {
    packet(ctx.skb.skb, true)
}

#[cgroup_skb(ingress)]
pub fn pyren_ingress(ctx: SkBuffContext) -> i32 {
    packet(ctx.skb.skb, false)
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[link_section = "license"]
#[no_mangle]
static LICENSE: [u8; 4] = *b"GPL\0";
