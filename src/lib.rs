#[macro_use]
extern crate napi_derive;

use std::time::{SystemTime, UNIX_EPOCH};

use napi::{
    check_status, sys, Env, Error, JsBigInt, JsBoolean, JsBuffer, JsBufferValue, JsUnknown,
    NapiValue, Ref, ValueType,
};

mod arena;
mod backend;
mod graph;

use graph::{Scalar, RVal};

fn err_region<S: Into<String>>(msg: S) -> Error {
    Error::from_reason(msg.into())
}
fn err_user(msg: &str) -> Error {
    Error::from_reason(msg.to_string())
}
fn err_napi(e: napi::Error) -> Error {
    e
}
fn err_full() -> Error {
    err_region("region capacity exceeded")
}

// ---------------------------------------------------------------------------
// SharedRegion (napi class)
// ---------------------------------------------------------------------------

#[napi(object)]
pub struct CreateOpts {
    /// Region size in bytes (default 1 MiB).
    pub size: Option<u32>,
    /// Short alphanumeric token used to derive the shared-memory id.
    pub id: Option<String>,
    /// Backend: "mmap" (default) or "sab".
    pub backend: Option<String>,
}

#[napi(object)]
pub struct AttachOpts {
    /// Backend of the region to attach ("mmap"; "sab" uses wrap()).
    pub backend: Option<String>,
}

#[napi]
pub struct SharedRegion {
    base: usize,
    capacity: usize,
    id: String,
    _mmap: Option<backend::MappedRegion>,
    _sab: Option<Ref<JsBufferValue>>,
}

#[napi]
impl SharedRegion {
    #[napi]
    pub fn create(opts: CreateOpts) -> Result<Self, Error> {
        let backend_name = opts.backend.as_deref().unwrap_or("mmap");
        if backend_name != "mmap" {
            return Err(err_region("sab regions are created via wrap(arrayBuffer)"));
        }
        let size = opts.size.unwrap_or(1 << 20) as usize;
        let token = match opts.id {
            Some(id) if !id.is_empty() => id,
            _ => {
                let nanos = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                format!("{:x}", nanos)
            }
        };
        let reg = unsafe { backend::create_mmap(&token, size) }.map_err(err_region)?;
        Ok(SharedRegion {
            base: reg.base as usize,
            capacity: reg.capacity,
            id: token,
            _mmap: Some(reg),
            _sab: None,
        })
    }

    #[napi]
    pub fn attach(opts: AttachOpts, id: String) -> Result<Self, Error> {
        let backend_name = opts.backend.as_deref().unwrap_or("mmap");
        if backend_name != "mmap" {
            return Err(err_region("sab regions are attached via wrap(arrayBuffer)"));
        }
        let reg = unsafe { backend::attach_mmap(&id) }.map_err(err_region)?;
        let cap = reg.capacity;
        Ok(SharedRegion {
            base: reg.base as usize,
            capacity: cap,
            id,
            _mmap: Some(reg),
            _sab: None,
        })
    }

    #[napi]
    pub fn wrap(buffer: JsBuffer) -> Result<Self, Error> {
        let rf = buffer.into_ref().map_err(err_napi)?;
        let slice: &[u8] = rf.as_ref();
        let base = slice.as_ptr() as usize;
        let len = slice.len();
        if base == 0 {
            return Err(err_region("buffer is not backed by memory"));
        }
        unsafe {
            arena::ensure_init(base as *mut u8, len, 2);
        }
        Ok(SharedRegion {
            base,
            capacity: len,
            id: "sab".to_string(),
            _mmap: None,
            _sab: Some(rf),
        })
    }

    #[napi]
    pub fn root(&self) -> Result<NodeRef, Error> {
        let handle = unsafe { graph::root_handle(self.base as *mut u8) }
            .ok_or_else(|| err_region("region full"))?;
        Ok(NodeRef {
            base: self.base,
            handle,
        })
    }

    #[napi]
    pub fn create_object(&self, capacity: u32) -> Result<NodeRef, Error> {
        let h = unsafe { graph::create_object(self.base as *mut u8, capacity) }
            .ok_or_else(err_full)?;
        Ok(NodeRef {
            base: self.base,
            handle: h,
        })
    }

    #[napi]
    pub fn create_array(&self, capacity: u32) -> Result<NodeRef, Error> {
        let h = unsafe { graph::create_array(self.base as *mut u8, capacity) }
            .ok_or_else(err_full)?;
        Ok(NodeRef {
            base: self.base,
            handle: h,
        })
    }

    #[napi]
    pub fn create_map(&self, capacity: u32) -> Result<NodeRef, Error> {
        let h = unsafe { graph::create_map(self.base as *mut u8, capacity) }
            .ok_or_else(err_full)?;
        Ok(NodeRef {
            base: self.base,
            handle: h,
        })
    }

    #[napi]
    pub fn base(&self) -> u64 {
        self.base as u64
    }

    #[napi]
    pub fn capacity(&self) -> u64 {
        self.capacity as u64
    }

    /// Shared-memory id. Pass this to worker threads so they can `attach()`.
    #[napi]
    pub fn id(&self) -> String {
        self.id.clone()
    }

    /// Detach/unlink the backing object. For the mmap backend this removes the
    /// POSIX name so no further attachments can be made (existing maps stay
    /// valid). Call this after all workers have attached. No-op for SAB.
    #[napi]
    pub fn close(&mut self) {
        if let Some(m) = self._mmap.as_mut() {
            m.unlink_backing();
        }
    }
}

// ---------------------------------------------------------------------------
// NodeRef (napi class)
// ---------------------------------------------------------------------------

#[napi]
pub struct NodeRef {
    base: usize,
    handle: u64,
}

impl NodeRef {
    fn base(&self) -> *mut u8 {
        self.base as *mut u8
    }

    fn kind(&self) -> u8 {
        graph::hkind(self.handle)
    }
}

/// Parsed key argument: an array index or an object/map string key.
enum KeyArg {
    Index(u32),
    Key(String),
}

fn key_arg(_env: &Env, k: JsUnknown) -> Result<KeyArg, Error> {
    match k.get_type().map_err(err_napi)? {
        ValueType::Number => {
            let n = k.coerce_to_number().map_err(err_napi)?.get_int64().map_err(err_napi)?;
            if n < 0 {
                return Err(err_user("index cannot be negative"));
            }
            Ok(KeyArg::Index(n as u32))
        }
        _ => {
            let s = k
                .coerce_to_string()
                .map_err(err_napi)?
                .into_utf8()
                .map_err(err_napi)?
                .as_str()
                .map_err(err_napi)?
                .to_string();
            Ok(KeyArg::Key(s))
        }
    }
}

fn js_scalar(_env: &Env, v: JsUnknown) -> Result<Option<Scalar>, Error> {
    match v.get_type().map_err(err_napi)? {
        ValueType::Null | ValueType::Undefined => Ok(Some(Scalar::Null)),
        ValueType::Boolean => {
            let b = unsafe { v.cast::<JsBoolean>() }.get_value().map_err(err_napi)?;
            Ok(Some(Scalar::Bool(b)))
        }
        ValueType::Number => {
            let n = v.coerce_to_number().map_err(err_napi)?.get_double().map_err(err_napi)?;
            if n.fract() == 0.0 && n >= i64::MIN as f64 && n <= i64::MAX as f64 {
                Ok(Some(Scalar::Int(n as i64)))
            } else {
                Ok(Some(Scalar::Float(n)))
            }
        }
        ValueType::String => {
            let s = v
                .coerce_to_string()
                .map_err(err_napi)?
                .into_utf8()
                .map_err(err_napi)?
                .as_str()
                .map_err(err_napi)?
                .to_string();
            Ok(Some(Scalar::Str(s)))
        }
        ValueType::BigInt => {
            let mut bi = unsafe { v.cast::<JsBigInt>() };
            let (_sign, words) = bi.get_words().map_err(err_napi)?;
            let lo = words.first().copied().unwrap_or(0) as u64;
            let hi = words.get(1).copied().unwrap_or(0) as u64;
            let val = (lo as i128) | ((hi as i128) << 64);
            Ok(Some(Scalar::BigInt(val)))
        }
        _ => Ok(None), // not a scalar
    }
}

fn js_null(env: &Env) -> Result<JsUnknown, Error> {
    let mut v: sys::napi_value = std::ptr::null_mut();
    check_status!(unsafe { sys::napi_get_null(env.raw(), &mut v) })?;
    Ok(unsafe { JsUnknown::from_raw_unchecked(env.raw(), v) })
}

fn rval_to_js(env: &Env, rv: RVal) -> Result<JsUnknown, Error> {
    Ok(match rv {
        RVal::Null => js_null(env)?,
        RVal::Bool(b) => env.get_boolean(b).map_err(err_napi)?.into_unknown(),
        RVal::Int(i) => env.create_int64(i).map_err(err_napi)?.into_unknown(),
        RVal::Float(f) => env.create_double(f).map_err(err_napi)?.into_unknown(),
        RVal::BigInt(b) => env
            .create_bigint_from_i128(b)
            .map_err(err_napi)?
            .into_unknown()
            .map_err(err_napi)?,
        RVal::Str(s) => env.create_string(&s).map_err(err_napi)?.into_unknown(),
        RVal::Object(_) | RVal::Array(_) | RVal::Map(_) => js_null(env)?,
    })
}

#[napi]
impl NodeRef {
    /// Type name: "Object" | "Array" | "Map" | scalar.
    #[napi]
    pub fn type_name(&self) -> String {
        match self.kind() {
            graph::KIND_OBJECT => "Object".into(),
            graph::KIND_ARRAY => "Array".into(),
            graph::KIND_MAP => "Map".into(),
            graph::KIND_BOOL => "Boolean".into(),
            graph::KIND_INT => "Number".into(),
            graph::KIND_F64 => "Number".into(),
            graph::KIND_BIGINT => "BigInt".into(),
            graph::KIND_STR => "String".into(),
            _ => "Null".into(),
        }
    }

    #[napi]
    pub fn is_object(&self) -> bool {
        self.kind() == graph::KIND_OBJECT
    }
    #[napi]
    pub fn is_array(&self) -> bool {
        self.kind() == graph::KIND_ARRAY
    }
    #[napi]
    pub fn is_map(&self) -> bool {
        self.kind() == graph::KIND_MAP
    }
    #[napi]
    pub fn length(&self) -> u32 {
        unsafe { graph::length(self.base(), self.handle) }.unwrap_or(0)
    }

    // Resolve the raw value handle at a key/index (H_INVALID if missing).
    fn read_handle(&self, env: &Env, key: JsUnknown) -> Result<u64, Error> {
        let b = self.base();
        match self.kind() {
            graph::KIND_ARRAY => match key_arg(env, key)? {
                KeyArg::Index(i) => Ok(unsafe { graph::read_index_handle(b, self.handle, i as usize) }),
                KeyArg::Key(_) => Err(err_user("use an array index (number) for arrays")),
            },
            _ => match key_arg(env, key)? {
                KeyArg::Key(s) => Ok(unsafe { graph::read_key_handle(b, self.handle, &s) }),
                KeyArg::Index(_) => Err(err_user("object/map keys must be strings")),
            },
        }
    }

    /// Return the container child at `key`, or null if it is a scalar/missing.
    #[napi(js_name = "get_node")]
    pub fn get_node(&self, env: Env, key: JsUnknown) -> Result<Option<NodeRef>, Error> {
        let h = self.read_handle(&env, key)?;
        if h != graph::H_INVALID && graph::is_container(h) {
            Ok(Some(NodeRef {
                base: self.base,
                handle: h,
            }))
        } else {
            Ok(None)
        }
    }

    /// Materialize the scalar at `key` (null for containers/missing).
    #[napi(js_name = "get_value")]
    pub fn get_value(&self, env: Env, key: JsUnknown) -> Result<JsUnknown, Error> {
        let h = self.read_handle(&env, key)?;
        if h == graph::H_INVALID || graph::is_container(h) {
            return js_null(&env);
        }
        let rv = unsafe { graph::rval(self.base(), h) };
        rval_to_js(&env, rv)
    }

    #[napi]
    pub fn has(&self, env: Env, key: JsUnknown) -> Result<bool, Error> {
        Ok(self.read_handle(&env, key)? != graph::H_INVALID)
    }

    #[napi]
    pub fn set(&self, env: Env, key: JsUnknown, value: JsUnknown) -> Result<(), Error> {
        let scalar = match js_scalar(&env, value)? {
            Some(s) => s,
            None => return Err(err_user("use set_node(key, nodeRef) for containers")),
        };
        match key_arg(&env, key)? {
            KeyArg::Key(s) => unsafe {
                graph::put_key_scalar(self.base(), self.handle, &s, &scalar).map_err(err_region)
            },
            KeyArg::Index(i) => {
                if self.kind() != graph::KIND_ARRAY {
                    return Err(err_user("object/map keys must be strings"));
                }
                let node_h = scalar_to_handle(self.base, &scalar)?;
                unsafe { graph::put_index(self.base(), self.handle, i as usize, node_h).map_err(err_region) }
            }
        }
    }

    #[napi(js_name = "set_node")]
    pub fn set_node(&self, env: Env, key: JsUnknown, child: &NodeRef) -> Result<(), Error> {
        match key_arg(&env, key)? {
            KeyArg::Key(s) => {
                unsafe { graph::put_key_handle(self.base(), self.handle, &s, child.handle) };
                Ok(())
            }
            KeyArg::Index(i) => {
                if self.kind() == graph::KIND_ARRAY {
                    unsafe { graph::put_index(self.base(), self.handle, i as usize, child.handle).map_err(err_region) }
                } else {
                    Err(err_user("object/map keys must be strings"))
                }
            }
        }
    }

    #[napi]
    pub fn push(&self, env: Env, value: JsUnknown) -> Result<u32, Error> {
        if self.kind() != graph::KIND_ARRAY {
            return Err(err_user("push is only valid on arrays"));
        }
        let scalar = match js_scalar(&env, value)? {
            Some(s) => s,
            None => return Err(err_user("use push_node for containers")),
        };
        let node_h = scalar_to_handle(self.base, &scalar)?;
        unsafe { graph::push(self.base(), self.handle, node_h).map_err(err_region) }
    }

    #[napi(js_name = "push_node")]
    pub fn push_node(&self, child: &NodeRef) -> Result<u32, Error> {
        if self.kind() != graph::KIND_ARRAY {
            return Err(err_user("push is only valid on arrays"));
        }
        unsafe { graph::push(self.base(), self.handle, child.handle).map_err(err_region) }
    }

    #[napi]
    pub fn increment(&self, env: Env, key: JsUnknown) -> Result<i64, Error> {
        let s = match key_arg(&env, key)? {
            KeyArg::Key(s) => s,
            KeyArg::Index(_) => return Err(err_user("increment requires a string key")),
        };
        unsafe { graph::increment_counter(self.base(), self.handle, &s).map_err(err_region) }
    }

    #[napi]
    pub fn delete(&self, env: Env, key: JsUnknown) -> Result<bool, Error> {
        match key_arg(&env, key)? {
            KeyArg::Key(s) => unsafe { graph::delete_key(self.base(), self.handle, &s).map_err(err_region) },
            KeyArg::Index(i) => unsafe {
                if self.kind() == graph::KIND_ARRAY {
                    graph::delete_index(self.base(), self.handle, i as usize).map_err(err_region)
                } else {
                    Ok(false)
                }
            },
        }
    }

    #[napi]
    pub fn keys(&self) -> Result<Vec<String>, Error> {
        unsafe { graph::keys_of(self.base(), self.handle) }.map_err(err_region)
    }
}

fn scalar_to_handle(base: usize, s: &Scalar) -> Result<u64, Error> {
    unsafe { graph::new_scalar_node(base as *mut u8, s) }.ok_or_else(err_full)
}

// The #[napi] derive generates the module registration from the items above.