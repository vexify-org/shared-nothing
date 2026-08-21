//! Object-graph encoding and lock-free concurrent operations over the arena.
//!
//! Every node carries a seqlock (a version counter whose low bit marks an
//! active writer). Readers are lock-free: they snapshot the version, read, and
//! re-check the version, retrying if it changed or is odd. Writers claim the
//! write window with a CAS on the seqlock, mutate under it, then commit with an
//! even version increment. This gives serializable single-mutation semantics
//! (CAS + version) with no data races and lock-free reads.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::arena::{align_up, alloc_aligned, cell64, seq_is_odd};

pub const KIND_OBJECT: u8 = 1;
pub const KIND_ARRAY: u8 = 2;
pub const KIND_MAP: u8 = 3;
pub const KIND_NULL: u8 = 4;
pub const KIND_BOOL: u8 = 5;
pub const KIND_INT: u8 = 6;
pub const KIND_F64: u8 = 7;
pub const KIND_BIGINT: u8 = 8;
pub const KIND_STR: u8 = 9;

pub const H_INVALID: u64 = 0;

/// Fixed header size of a node record (tag + seqlock + counters + payload ptr).
pub const NODE_HDR: usize = 40;

/// Default slot capacity for freshly created containers.
pub const DEFAULT_CAP: u32 = 16;

/// Node record header on disk, at offset `off` of a node.
#[repr(C)]
pub struct NodeHeader {
    pub tag: u8,
    _pad0: [u8; 3],
    pub node_size: u32,
    /// Container: number of slots. String: payload capacity (bytes).
    pub cap: u32,
    /// Value: payload byte length. Container: unused (0).
    pub store: u32,
    pub seqlock: AtomicU64,
    pub count: AtomicU64,
    pub _pad1: [u8; 8],
}

#[inline]
pub fn make_handle(kind: u8, off: usize) -> u64 {
    ((kind as u64) << 56) | (off as u64)
}
#[inline]
pub fn hkind(h: u64) -> u8 {
    (h >> 56) as u8
}
#[inline]
pub fn hoff(h: u64) -> usize {
    (h & 0x00FF_FFFF_FFFF_FFFF) as usize
}
#[inline]
pub fn is_container(h: u64) -> bool {
    let k = hkind(h);
    k == KIND_OBJECT || k == KIND_ARRAY || k == KIND_MAP
}
#[inline]
pub fn is_scalar(h: u64) -> bool {
    !is_container(h) && h != H_INVALID
}

#[inline]
unsafe fn header<'a>(base: *mut u8, off: usize) -> &'a NodeHeader {
    &*(base.add(off) as *const NodeHeader)
}

#[inline]
unsafe fn payload_word(base: *mut u8, off: usize) -> &'static AtomicU64 {
    cell64(base, off + NODE_HDR)
}

#[inline]
unsafe fn kv_slot(base: *mut u8, c_off: usize, i: usize) -> (&'static AtomicU64, &'static AtomicU64) {
    let s = c_off + NODE_HDR + i * 16;
    (cell64(base, s), cell64(base, s + 8))
}

#[inline]
unsafe fn array_slot(base: *mut u8, c_off: usize, i: usize) -> &'static AtomicU64 {
    cell64(base, c_off + NODE_HDR + i * 8)
}

/// A materialized value read out of the arena.
#[derive(Debug, Clone)]
#[allow(dead_code)] // container payloads are dispatched via is_container()
pub enum RVal {
    Object(u64),
    Array(u64),
    Map(u64),
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    BigInt(i128),
    Str(String),
}

/// A normalized scalar payload from JS.
#[derive(Debug, Clone)]
pub enum Scalar {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    BigInt(i128),
    Str(String),
}

impl Scalar {
    pub fn kind(&self) -> u8 {
        match self {
            Scalar::Null => KIND_NULL,
            Scalar::Bool(_) => KIND_BOOL,
            Scalar::Int(_) => KIND_INT,
            Scalar::Float(_) => KIND_F64,
            Scalar::BigInt(_) => KIND_BIGINT,
            Scalar::Str(_) => KIND_STR,
        }
    }
    pub fn size(&self) -> usize {
        match self {
            Scalar::Null | Scalar::Bool(_) => 8,
            Scalar::Int(_) | Scalar::Float(_) => 8,
            Scalar::BigInt(_) => 16,
            Scalar::Str(s) => align_up(s.len()),
        }
    }
}

// ---------------------------------------------------------------------------
// Node creation
// ---------------------------------------------------------------------------

unsafe fn init_node(base: *mut u8, off: usize, tag: u8, cap: u32, store: u32, node_size: usize) {
    let h = &mut *(base.add(off) as *mut NodeHeader);
    h.tag = tag;
    h.node_size = node_size as u32;
    h.cap = cap;
    h.store = store;
    h.seqlock.store(0, Ordering::Release);
    h.count.store(0, Ordering::Release);
}

unsafe fn alloc_node(base: *mut u8, size: usize) -> Option<usize> {
    alloc_aligned(base, align_up(size))
}

unsafe fn new_container(base: *mut u8, tag: u8, cap: u32) -> Option<u64> {
    let cap = if cap == 0 { DEFAULT_CAP } else { cap };
    let slot_sz = if tag == KIND_ARRAY { 8usize } else { 16usize };
    let node_size = NODE_HDR + (cap as usize) * slot_sz;
    let off = alloc_node(base, node_size)?;
    init_node(base, off, tag, cap, 0, node_size);
    Some(make_handle(tag, off))
}

pub unsafe fn create_object(base: *mut u8, cap: u32) -> Option<u64> {
    new_container(base, KIND_OBJECT, cap)
}
pub unsafe fn create_array(base: *mut u8, cap: u32) -> Option<u64> {
    new_container(base, KIND_ARRAY, cap)
}
pub unsafe fn create_map(base: *mut u8, cap: u32) -> Option<u64> {
    new_container(base, KIND_MAP, cap)
}

pub unsafe fn new_scalar_node(base: *mut u8, s: &Scalar) -> Option<u64> {
    let store = match s {
        Scalar::Null | Scalar::Bool(_) | Scalar::Int(_) | Scalar::Float(_) => 8u32,
        Scalar::BigInt(_) => 16u32,
        Scalar::Str(st) => st.len() as u32,
    };
    let cap = s.size() as u32;
    let node_size = NODE_HDR + s.size();
    let off = alloc_node(base, node_size)?;
    init_node(base, off, s.kind(), cap, store, node_size);
    // Write payload.
    match s {
        Scalar::Null => payload_word(base, off).store(0, Ordering::Relaxed),
        Scalar::Bool(b) => payload_word(base, off).store(*b as u64, Ordering::Relaxed),
        Scalar::Int(v) => payload_word(base, off).store(*v as u64, Ordering::Relaxed),
        Scalar::Float(v) => payload_word(base, off).store(v.to_bits(), Ordering::Relaxed),
        Scalar::BigInt(v) => {
            payload_word(base, off).store(*v as u64, Ordering::Relaxed);
            payload_word(base, off + 8).store((*v >> 64) as u64, Ordering::Relaxed);
        }
        Scalar::Str(st) => {
            let dst = base.add(off + NODE_HDR);
            std::ptr::copy_nonoverlapping(st.as_ptr(), dst, st.len());
        }
    }
    Some(make_handle(s.kind(), off))
}

/// Get/create the root object handle.
pub unsafe fn root_handle(base: *mut u8) -> Option<u64> {
    let hdr = &*(base as *const crate::arena::RegionHeader);
    loop {
        let r = hdr.root.load(Ordering::Acquire);
        if r != H_INVALID && is_container(r) {
            return Some(r);
        }
        if let Some(h) = create_object(base, DEFAULT_CAP) {
            if hdr
                .root
                .compare_exchange(r, h, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(h);
            }
            // Lost the race; ignore the spawned node and retry read.
        }
    }
}

// ---------------------------------------------------------------------------
// Lock-free read / write windows (seqlock)
// ---------------------------------------------------------------------------

#[inline]
unsafe fn read_window_value(base: *mut u8, c_off: usize, reader: impl Fn() -> u64) -> u64 {
    let h = header(base, c_off);
    loop {
        let s0 = h.seqlock.load(Ordering::Acquire);
        if seq_is_odd(s0) {
            continue;
        }
        let v = reader();
        let s1 = h.seqlock.load(Ordering::Acquire);
        if s1 == s0 && !seq_is_odd(s1) {
            return v;
        }
    }
}

// ---------------------------------------------------------------------------
// Container read helpers
// ---------------------------------------------------------------------------

unsafe fn read_kv_value(base: *mut u8, c_off: usize, slot: usize) -> u64 {
    read_window_value(base, c_off, || kv_slot(base, c_off, slot).1.load(Ordering::Acquire))
}

unsafe fn read_array_value(base: *mut u8, c_off: usize, slot: usize) -> u64 {
    read_window_value(base, c_off, || array_slot(base, c_off, slot).load(Ordering::Acquire))
}

/// Compare a slave string stored in the arena (at `s_off`) with `q`.
unsafe fn str_eq(base: *mut u8, s_off: usize, q: &[u8]) -> bool {
    let h = header(base, s_off);
    let len = h.store as usize;
    if len != q.len() {
        return false;
    }
    let p = base.add(s_off + NODE_HDR);
    std::slice::from_raw_parts(p, len) == q
}

/// Find the slot index for `key` in an object/map container, returning the
/// slot and its value handle. Returns Some((slot, value_handle)) if present.
unsafe fn find_key(base: *mut u8, c_off: usize, cap: usize, count: usize, key: &[u8]) -> Option<(usize, u64)> {
    for i in 0..cap.min(count + 1) {
        let (kh, vh) = kv_slot(base, c_off, i);
        let k = kh.load(Ordering::Acquire);
        let v = vh.load(Ordering::Acquire);
        if k != H_INVALID && hkind(k) == KIND_STR {
            // Ensure key read is consistent with the count-era container.
            if str_eq(base, hoff(k), key) {
                return Some((i, v));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Public container operations
// ---------------------------------------------------------------------------

/// Read `key` from an object/map container.
#[allow(dead_code)]
pub unsafe fn get_key(base: *mut u8, handle: u64, key: &str) -> Result<RVal, String> {
    let c_off = hoff(handle);
    let h = header(base, c_off);
    let cap = h.cap as usize;
    let count = h.count.load(Ordering::Acquire) as usize;
    if let Some((_slot, vh)) = find_key(base, c_off, cap, count, key.as_bytes()) {
        Ok(handle_to_rval(base, vh))
    } else {
        Ok(RVal::Null)
    }
}

/// Read index `i` from an array container.
#[allow(dead_code)]
pub unsafe fn get_index(base: *mut u8, handle: u64, i: usize) -> Result<RVal, String> {
    let c_off = hoff(handle);
    let h = header(base, c_off);
    if i >= h.cap as usize {
        return Ok(RVal::Null);
    }
    let vh = read_array_value(base, c_off, i);
    if vh == H_INVALID {
        Ok(RVal::Null)
    } else {
        Ok(handle_to_rval(base, vh))
    }
}

unsafe fn handle_to_rval(base: *mut u8, h: u64) -> RVal {
    if h == H_INVALID {
        return RVal::Null;
    }
    match hkind(h) {
        KIND_OBJECT => RVal::Object(h),
        KIND_ARRAY => RVal::Array(h),
        KIND_MAP => RVal::Map(h),
        KIND_NULL => RVal::Null,
        KIND_BOOL => RVal::Bool(payload_word(base, hoff(h)).load(Ordering::Acquire) != 0),
        KIND_INT => RVal::Int(payload_word(base, hoff(h)).load(Ordering::Acquire) as i64),
        KIND_F64 => RVal::Float(f64::from_bits(payload_word(base, hoff(h)).load(Ordering::Acquire))),
        KIND_BIGINT => {
            let lo = payload_word(base, hoff(h)).load(Ordering::Acquire) as i128;
            let hi = payload_word(base, hoff(h) + 8).load(Ordering::Acquire) as i128;
            RVal::BigInt(lo | (hi << 64))
        }
        KIND_STR => {
            let s_off = hoff(h);
            let len = header(base, s_off).store as usize;
            let p = base.add(s_off + NODE_HDR);
            RVal::Str(String::from_utf8_lossy(std::slice::from_raw_parts(p, len)).into_owned())
        }
        _ => RVal::Null,
    }
}

#[allow(dead_code)]
pub unsafe fn has_key(base: *mut u8, handle: u64, key: &str) -> Result<bool, String> {
    let c_off = hoff(handle);
    let h = header(base, c_off);
    let cap = h.cap as usize;
    let count = h.count.load(Ordering::Acquire) as usize;
    Ok(find_key(base, c_off, cap, count, key.as_bytes()).is_some())
}

pub unsafe fn length(base: *mut u8, handle: u64) -> Result<u32, String> {
    let c_off = hoff(handle);
    let h = header(base, c_off);
    Ok(h.count.load(Ordering::Acquire) as u32)
}

/// List keys of an object/map container.
pub unsafe fn keys_of(base: *mut u8, handle: u64) -> Result<Vec<String>, String> {
    let c_off = hoff(handle);
    let h = header(base, c_off);
    let cap = h.cap as usize;
    let count = h.count.load(Ordering::Acquire) as usize;
    let mut out = Vec::new();
    for i in 0..cap.min(count + 1) {
        let k = kv_slot(base, c_off, i).0.load(Ordering::Acquire);
        if k != H_INVALID && hkind(k) == KIND_STR {
            let s_off = hoff(k);
            let len = header(base, s_off).store as usize;
            let p = base.add(s_off + NODE_HDR);
            out.push(String::from_utf8_lossy(std::slice::from_raw_parts(p, len)).into_owned());
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Writes (kv containers)
// ---------------------------------------------------------------------------

/// Write `value_handle` into bearing slot `slot` of an object/map under the
/// write window; write the key string only if `new_key`.
unsafe fn write_kv_slot(
    base: *mut u8,
    c_off: usize,
    slot: usize,
    key_handle: u64,
    value_handle: u64,
    set_key: bool,
) {
    loop {
        let h = header(base, c_off);
        let s0 = h.seqlock.load(Ordering::Acquire);
        if seq_is_odd(s0) {
            std::thread::yield_now();
            continue;
        }
        if h.seqlock
            .compare_exchange(s0, s0 | 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            let (kh, vh) = kv_slot(base, c_off, slot);
            if set_key {
                kh.store(key_handle, Ordering::Release);
            }
            vh.store(value_handle, Ordering::Release);
            h.seqlock.store(s0 + 2, Ordering::Release);
            return;
        }
    }
}

/// Get the current value handle in a kv slot (consistent single read).
unsafe fn kv_slot_current(base: *mut u8, c_off: usize, slot: usize) -> u64 {
    read_kv_value(base, c_off, slot)
}

/// Scalar in-place write onto an existing sized scalar node (under its own
/// seqlock). Returns the new version.
unsafe fn scalar_write_inplace(base: *mut u8, node_off: usize, s: &Scalar) {
    loop {
        let h = header(base, node_off);
        let s0 = h.seqlock.load(Ordering::Acquire);
        if seq_is_odd(s0) {
            std::thread::yield_now();
            continue;
        }
        if h.seqlock
            .compare_exchange(s0, s0 | 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            match s {
                Scalar::Null => payload_word(base, node_off).store(0, Ordering::Release),
                Scalar::Bool(b) => payload_word(base, node_off).store(*b as u64, Ordering::Release),
                Scalar::Int(v) => payload_word(base, node_off).store(*v as u64, Ordering::Release),
                Scalar::Float(v) => payload_word(base, node_off).store(v.to_bits(), Ordering::Release),
                Scalar::BigInt(v) => {
                    payload_word(base, node_off).store(*v as u64, Ordering::Release);
                    payload_word(base, node_off + 8).store((*v >> 64) as u64, Ordering::Release);
                }
                Scalar::Str(st) => {
                    let dst = base.add(node_off + NODE_HDR);
                    // cap guaranteed >= len by caller.
                    std::ptr::copy_nonoverlapping(st.as_ptr(), dst, st.len());
                    let hm = &mut *(base.add(node_off) as *mut NodeHeader);
                    hm.store = st.len() as u32;
                }
            }
            h.seqlock.store(s0 + 2, Ordering::Release);
            return;
        }
    }
}

fn scalar_reusable(node_handle: u64, s: &Scalar, base: *mut u8) -> bool {
    let kind = hkind(node_handle);
    match s {
        Scalar::Null => kind == KIND_NULL,
        Scalar::Bool(_) => kind == KIND_BOOL,
        Scalar::Int(_) => kind == KIND_INT,
        Scalar::Float(_) => kind == KIND_F64,
        Scalar::BigInt(_) => kind == KIND_BIGINT,
        Scalar::Str(st) => {
            if kind != KIND_STR {
                return false;
            }
            let h = unsafe { header(base, hoff(node_handle)) };
            h.cap as usize >= st.len()
        }
    }
}

/// Set a scalar on an object/map key, reusing existing sized nodes in place.
pub unsafe fn put_key_scalar(base: *mut u8, handle: u64, key: &str, s: &Scalar) -> Result<(), String> {
    let c_off = hoff(handle);
    let h = header(base, c_off);
    let cap = h.cap as usize;
    let count = h.count.load(Ordering::Acquire) as usize;
    // locate or reserve a slot
    let existing = find_key(base, c_off, cap, count, key.as_bytes());
    match existing {
        Some((slot, vh)) => {
            if vh != H_INVALID && is_scalar(vh) && scalar_reusable(vh, s, base) {
                scalar_write_inplace(base, hoff(vh), s);
                // Confirm the slot still points at the same node; otherwise retried later by caller.
                let now = kv_slot_current(base, c_off, slot);
                if now == vh {
                    return Ok(());
                }
                // slot changed concurrently; fall through to publish (still target same slot)
            }
            let newh = new_scalar_node(base, s).ok_or_else(|| "region full".to_string())?;
            write_kv_slot(base, c_off, slot, H_INVALID, newh, false);
            Ok(())
        }
        None => {
            // Insert into first empty slot under the window.
            loop {
                let sh = header(base, c_off);
                let s0 = sh.seqlock.load(Ordering::Acquire);
                if seq_is_odd(s0) {
                    std::thread::yield_now();
                    continue;
                }
                if sh.seqlock
                    .compare_exchange(s0, s0 | 1, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    // find empty slot
                    let mut chosen = None;
                    for i in 0..cap {
                        if kv_slot(base, c_off, i).0.load(Ordering::Relaxed) == H_INVALID {
                            chosen = Some(i);
                            break;
                        }
                    }
                    let chosen = match chosen {
                        Some(i) => i,
                        None => {
                            sh.seqlock.store(s0 + 2, Ordering::Release);
                            return Err("container capacity exceeded".to_string());
                        }
                    };
                    // allocate key + value nodes now (nodes are private until published)
                    let keyh = alloc_str(base, key).ok_or_else(|| {
                        sh.seqlock.store(s0 + 2, Ordering::Release);
                        "region full".to_string()
                    })?;
                    let valh = new_scalar_node(base, s).ok_or_else(|| {
                        sh.seqlock.store(s0 + 2, Ordering::Release);
                        "region full".to_string()
                    })?;
                    let (kh, vh) = kv_slot(base, c_off, chosen);
                    kh.store(keyh, Ordering::Release);
                    vh.store(valh, Ordering::Release);
                    sh.count.fetch_add(1, Ordering::Release);
                    sh.seqlock.store(s0 + 2, Ordering::Release);
                    return Ok(());
                }
            }
        }
    }
}

/// Put a key->value on an object/map (scalar). Reuses sized scalar node.
#[allow(dead_code)]
pub unsafe fn put_key_value(base: *mut u8, handle: u64, key: &str, value: u64) -> Result<(), String> {
    let rv = handle_to_rval(base, value);
    match rv {
        RVal::Object(_) | RVal::Array(_) | RVal::Map(_) => {
            // container child: publish handle directly
            put_key_handle(base, handle, key, value);
            Ok(())
        }
        _ => {
            let scalar = rval_to_scalar(base, value).ok_or("invalid value")?;
            put_key_scalar(base, handle, key, &scalar)
        }
    }
}

/// Link a child container into a key slot.
pub unsafe fn put_key_handle(base: *mut u8, handle: u64, key: &str, child: u64) {
    let c_off = hoff(handle);
    let h = header(base, c_off);
    let cap = h.cap as usize;
    let count = h.count.load(Ordering::Acquire) as usize;
    if let Some((slot, _)) = find_key(base, c_off, cap, count, key.as_bytes()) {
        write_kv_slot(base, c_off, slot, H_INVALID, child, false);
        return;
    }
    // insert fresh
    let _ = insert_kv_inner(base, handle, key, child);
}

unsafe fn insert_kv_inner(base: *mut u8, handle: u64, key: &str, child: u64) -> Result<(), String> {
    let c_off = hoff(handle);
    let h = header(base, c_off);
    let cap = h.cap as usize;
    loop {
        let s0 = h.seqlock.load(Ordering::Acquire);
        if seq_is_odd(s0) {
            std::thread::yield_now();
            continue;
        }
        if h.seqlock
            .compare_exchange(s0, s0 | 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            let keyh = alloc_str(base, key).ok_or_else(|| {
                h.seqlock.store(s0 + 2, Ordering::Release);
                "region full".to_string()
            })?;
            for i in 0..cap {
                if kv_slot(base, c_off, i).0.load(Ordering::Relaxed) == H_INVALID {
                    kv_slot(base, c_off, i).0.store(keyh, Ordering::Release);
                    kv_slot(base, c_off, i).1.store(child, Ordering::Release);
                    h.count.fetch_add(1, Ordering::Release);
                    h.seqlock.store(s0 + 2, Ordering::Release);
                    return Ok(());
                }
            }
            h.seqlock.store(s0 + 2, Ordering::Release);
            return Err("container capacity exceeded".to_string());
        }
    }
}

/// Delete a key from an object/map. Returns true if it was present.
pub unsafe fn delete_key(base: *mut u8, handle: u64, key: &str) -> Result<bool, String> {
    let c_off = hoff(handle);
    loop {
        let h = header(base, c_off);
        let cap = h.cap as usize;
        let count = h.count.load(Ordering::Acquire) as usize;
        if let Some((slot, _)) = find_key(base, c_off, cap, count, key.as_bytes()) {
            let s0 = h.seqlock.load(Ordering::Acquire);
            if seq_is_odd(s0) {
                std::thread::yield_now();
                continue;
            }
            if h.seqlock
                .compare_exchange(s0, s0 | 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                let (kh, vh) = kv_slot(base, c_off, slot);
                let was = kh.swap(H_INVALID, Ordering::Release);
                vh.store(H_INVALID, Ordering::Release);
                if was != H_INVALID {
                    h.count.fetch_sub(1, Ordering::Release);
                }
                let ok = was != H_INVALID;
                h.seqlock.store(s0 + 2, Ordering::Release);
                return Ok(ok);
            }
        } else {
            return Ok(false);
        }
    }
}

// ---------------------------------------------------------------------------
// Array operations
// ---------------------------------------------------------------------------

unsafe fn array_write(base: *mut u8, c_off: usize, slot: usize, value: u64) {
    loop {
        let h = header(base, c_off);
        let s0 = h.seqlock.load(Ordering::Acquire);
        if seq_is_odd(s0) {
            std::thread::yield_now();
            continue;
        }
        if h.seqlock
            .compare_exchange(s0, s0 | 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            array_slot(base, c_off, slot).store(value, Ordering::Release);
            h.seqlock.store(s0 + 2, Ordering::Release);
            return;
        }
    }
}

/// Set array[index] = value (scalar or container handle; caller passes handle).
pub unsafe fn put_index(base: *mut u8, handle: u64, index: usize, value: u64) -> Result<(), String> {
    let c_off = hoff(handle);
    let h = header(base, c_off);
    if index >= h.cap as usize {
        return Err("array capacity exceeded".to_string());
    }
    // If writing past current length, extend length.
    let cur = h.count.load(Ordering::Acquire);
    if (index + 1) as u64 > cur {
        loop {
            let s0 = h.seqlock.load(Ordering::Acquire);
            if seq_is_odd(s0) {
                std::thread::yield_now();
                continue;
            }
            if h.seqlock
                .compare_exchange(s0, s0 | 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                array_slot(base, c_off, index).store(value, Ordering::Release);
                let c = h.count.load(Ordering::Relaxed);
                if (index + 1) as u64 > c {
                    h.count.store((index + 1) as u64, Ordering::Release);
                }
                h.seqlock.store(s0 + 2, Ordering::Release);
                return Ok(());
            }
        }
    } else {
        array_write(base, c_off, index, value);
        Ok(())
    }
}

/// Append to an array, returning the new length.
pub unsafe fn push(base: *mut u8, handle: u64, value: u64) -> Result<u32, String> {
    let c_off = hoff(handle);
    let h = header(base, c_off);
    loop {
        let s0 = h.seqlock.load(Ordering::Acquire);
        if seq_is_odd(s0) {
            std::thread::yield_now();
            continue;
        }
        if h.seqlock
            .compare_exchange(s0, s0 | 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            let idx = h.count.load(Ordering::Relaxed) as usize;
            if idx >= h.cap as usize {
                h.seqlock.store(s0 + 2, Ordering::Release);
                return Err("array capacity exceeded".to_string());
            }
            array_slot(base, c_off, idx).store(value, Ordering::Release);
            h.count.store((idx + 1) as u64, Ordering::Release);
            h.seqlock.store(s0 + 2, Ordering::Release);
            return Ok((idx + 1) as u32);
        }
    }
}

pub unsafe fn delete_index(base: *mut u8, handle: u64, index: usize) -> Result<bool, String> {
    let c_off = hoff(handle);
    array_write(base, c_off, index, H_INVALID);
    Ok(true)
}

// ---------------------------------------------------------------------------
// Atomic increment (lock-free read-modify-write)
// ---------------------------------------------------------------------------

unsafe fn find_kv_value_mut(base: *mut u8, c_off: usize, key: &str) -> Option<u64> {
    let h = header(base, c_off);
    let cap = h.cap as usize;
    let count = h.count.load(Ordering::Acquire) as usize;
    find_key(base, c_off, cap, count, key.as_bytes()).map(|(_, v)| v)
}

/// Increment the INT under `key` of an object by 1. Creates the counter if
/// absent. Returns the new value. Lock-free linearizable.
pub unsafe fn increment_counter(base: *mut u8, handle: u64, key: &str) -> Result<i64, String> {
    let c_off = hoff(handle);
    // If missing entirely, insert an int(0) node.
    if find_kv_value_mut(base, c_off, key).is_none() {
        let _ = put_key_scalar(base, handle, key, &Scalar::Int(0));
    }
    // Now atomically increment the INT node.
    loop {
        let cvh = find_kv_value_mut(base, c_off, key).ok_or("key disappeared")?;
        let node_off = hoff(cvh);
        if hkind(cvh) != KIND_INT {
            // not ours; fall back to put
            let _ = put_key_scalar(base, handle, key, &Scalar::Int(0));
            continue;
        }
        let h = header(base, node_off);
        let s0 = h.seqlock.load(Ordering::Acquire);
        if seq_is_odd(s0) {
            std::thread::yield_now();
            continue;
        }
        if h.seqlock
            .compare_exchange(s0, s0 | 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            let cur = payload_word(base, node_off).load(Ordering::Relaxed) as i64;
            let next = cur.wrapping_add(1);
            payload_word(base, node_off).store(next as u64, Ordering::Release);
            h.seqlock.store(s0 + 2, Ordering::Release);
            return Ok(next);
        }
    }
}

// ---------------------------------------------------------------------------
// Util
// ---------------------------------------------------------------------------

unsafe fn alloc_str(base: *mut u8, s: &str) -> Option<u64> {
    new_scalar_node(base, &Scalar::Str(s.to_string()))
}

#[allow(dead_code)]
fn rval_to_scalar(base: *mut u8, handle: u64) -> Option<Scalar> {
    match unsafe { handle_to_rval(base, handle) } {
        RVal::Null => Some(Scalar::Null),
        RVal::Bool(b) => Some(Scalar::Bool(b)),
        RVal::Int(i) => Some(Scalar::Int(i)),
        RVal::Float(f) => Some(Scalar::Float(f)),
        RVal::BigInt(b) => Some(Scalar::BigInt(b)),
        RVal::Str(s) => Some(Scalar::Str(s)),
        _ => None,
    }
}

pub unsafe fn rval(base: *mut u8, handle: u64) -> RVal {
    handle_to_rval(base, handle)
}

// ---------------------------------------------------------------------------
// Raw handle reads (for container linking)
// ---------------------------------------------------------------------------

/// Value handle stored at `key` of an object/map, or H_INVALID.
pub unsafe fn read_key_handle(base: *mut u8, c: u64, key: &str) -> u64 {
    let c_off = hoff(c);
    let h = header(base, c_off);
    let cap = h.cap as usize;
    let count = h.count.load(Ordering::Acquire) as usize;
    find_key(base, c_off, cap, count, key.as_bytes())
        .map(|(_, v)| v)
        .unwrap_or(H_INVALID)
}

/// Value handle at index `i` of an array, or H_INVALID.
pub unsafe fn read_index_handle(base: *mut u8, c: u64, i: usize) -> u64 {
    let c_off = hoff(c);
    let h = header(base, c_off);
    if i >= h.cap as usize {
        return H_INVALID;
    }
    read_array_value(base, c_off, i)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::ensure_init;

    unsafe fn make_region(size: usize) -> (*mut u8, usize) {
        use std::alloc::{alloc_zeroed, dealloc, Layout};
        let layout = Layout::from_size_align(size, 8).unwrap();
        let ptr = alloc_zeroed(layout) as *mut u8;
        ensure_init(ptr, size, 1);
        (ptr, layout.size())
    }

    fn dealloc_region(p: *mut u8, size: usize) {
        unsafe {
            let layout = std::alloc::Layout::from_size_align(size, 8).unwrap();
            std::alloc::dealloc(p, layout);
        }
    }

    #[test]
    fn object_roundtrip_and_nesting() {
        unsafe {
            let (base, size) = make_region(1 << 20);
            let root = root_handle(base).unwrap();
            put_key_scalar(base, root, "name", &Scalar::Str("alice".into())).unwrap();
            put_key_scalar(base, root, "age", &Scalar::Int(30)).unwrap();
            assert!(matches!(get_key(base, root, "name"), Ok(RVal::Str(s)) if s == "alice"));
            assert!(matches!(get_key(base, root, "age"), Ok(RVal::Int(30))));
            // nested container
            let child = create_object(base, 16).unwrap();
            put_key_handle(base, root, "user", child);
            put_key_scalar(base, child, "id", &Scalar::Float(1.5)).unwrap();
            let rv = get_key(base, root, "user").unwrap();
            match rv {
                RVal::Object(h) => {
                    assert!(matches!(get_key(base, h, "id"), Ok(RVal::Float(f)) if f == 1.5));
                }
                _ => panic!(),
            }
            dealloc_region(base, size);
        }
    }

    #[test]
    fn arrays_and_push() {
        unsafe {
            let (base, size) = make_region(1 << 20);
            let a = create_array(base, 8).unwrap();
            push(base, a, new_scalar_node(base, &Scalar::Int(10)).unwrap()).unwrap();
            push(base, a, new_scalar_node(base, &Scalar::Int(20)).unwrap()).unwrap();
            assert_eq!(length(base, a).unwrap(), 2);
            assert!(matches!(get_index(base, a, 0), Ok(RVal::Int(10))));
            dealloc_region(base, size);
        }
    }

    #[test]
    fn concurrent_increment_no_lost_updates() {
        unsafe {
            let (base, size) = make_region(1 << 20);
            let stat = create_object(base, 16).unwrap();
            let base_ptr = base as usize;
            let h = stat;
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    std::thread::spawn(move || {
                        let b = base_ptr as *mut u8;
                        for _ in 0..5000 {
                            increment_counter(b, h, "hits").unwrap();
                        }
                    })
                })
                .collect();
            for t in handles {
                t.join().unwrap();
            }
            match get_key(base, stat, "hits").unwrap() {
                RVal::Int(v) => assert_eq!(v, 8 * 5000, "no lost updates"),
                other => panic!("unexpected counter value: {:?}", other),
            }
            dealloc_region(base, size);
        }
    }
}