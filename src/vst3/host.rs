//! Host-side COM objects handed to VST3 plugins.

use std::cell::{Cell, RefCell, UnsafeCell};
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use vst3::Steinberg::Vst::IAttributeList_::AttrID;
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComWrapper, Interface};

pub const HOST_NAME: &str = "nodeplayer";

pub fn str_to_tchar(s: &str, dst: &mut [TChar]) {
    let mut n = 0;
    for (d, c) in dst.iter_mut().zip(s.encode_utf16()) {
        *d = c as TChar;
        n += 1;
    }
    if n < dst.len() {
        dst[n] = 0;
    } else if let Some(last) = dst.last_mut() {
        *last = 0;
    }
}

pub fn tchar_to_string(src: &[TChar]) -> String {
    let len = src.iter().position(|&c| c == 0).unwrap_or(src.len());
    String::from_utf16_lossy(&src[..len])
}

pub fn cchar_to_string(src: &[c_char]) -> String {
    let bytes: Vec<u8> = src.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

pub fn tuid_eq(a: *const TUID, iid: &[u8; 16]) -> bool {
    unsafe { std::slice::from_raw_parts(a as *const u8, 16) == iid }
}

// ---------------------------------------------------------------------------
// IHostApplication

pub struct HostApp;

impl Class for HostApp {
    type Interfaces = (IHostApplication,);
}

impl IHostApplicationTrait for HostApp {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        unsafe { str_to_tchar(HOST_NAME, &mut *name) };
        kResultOk
    }

    unsafe fn createInstance(
        &self,
        cid: *mut TUID,
        iid: *mut TUID,
        obj: *mut *mut c_void,
    ) -> tresult {
        unsafe {
            let created = if tuid_eq(cid, &IMessage::IID) {
                ComWrapper::new(Message::new()).to_com_ptr::<FUnknown>()
            } else if tuid_eq(cid, &IAttributeList::IID) {
                ComWrapper::new(AttributeList::default()).to_com_ptr::<FUnknown>()
            } else {
                None
            };
            match created {
                Some(unk) => {
                    let ptr = unk.as_ptr();
                    ((*(*ptr).vtbl).queryInterface)(ptr, iid as *const TUID, obj)
                }
                None => {
                    *obj = std::ptr::null_mut();
                    kResultFalse
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// IMessage / IAttributeList (used by plugins for processor <-> controller talk)

pub struct Message {
    id: RefCell<Option<CString>>,
    attrs: ComWrapper<AttributeList>,
}

impl Message {
    fn new() -> Self {
        Self { id: RefCell::new(None), attrs: ComWrapper::new(AttributeList::default()) }
    }
}

impl Class for Message {
    type Interfaces = (IMessage,);
}

impl IMessageTrait for Message {
    unsafe fn getMessageID(&self) -> FIDString {
        self.id.borrow().as_ref().map_or(std::ptr::null(), |s| s.as_ptr())
    }

    unsafe fn setMessageID(&self, id: FIDString) {
        *self.id.borrow_mut() =
            (!id.is_null()).then(|| unsafe { CStr::from_ptr(id) }.to_owned());
    }

    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        self.attrs.as_com_ref::<IAttributeList>().unwrap().as_ptr()
    }
}

enum Attr {
    Int(i64),
    Float(f64),
    Str(Vec<TChar>),
    Bin(Vec<u8>),
}

#[derive(Default)]
pub struct AttributeList {
    map: RefCell<HashMap<CString, Attr>>,
}

impl Class for AttributeList {
    type Interfaces = (IAttributeList,);
}

impl AttributeList {
    fn set(&self, id: AttrID, v: Attr) -> tresult {
        if id.is_null() {
            return kInvalidArgument;
        }
        let key = unsafe { CStr::from_ptr(id) }.to_owned();
        self.map.borrow_mut().insert(key, v);
        kResultOk
    }

    fn with<R>(&self, id: AttrID, f: impl FnOnce(&Attr) -> Option<R>) -> Option<R> {
        if id.is_null() {
            return None;
        }
        let key = unsafe { CStr::from_ptr(id) };
        self.map.borrow().get(key).and_then(f)
    }
}

impl IAttributeListTrait for AttributeList {
    unsafe fn setInt(&self, id: AttrID, value: int64) -> tresult {
        self.set(id, Attr::Int(value))
    }

    unsafe fn getInt(&self, id: AttrID, value: *mut int64) -> tresult {
        match self.with(id, |a| if let Attr::Int(v) = a { Some(*v) } else { None }) {
            Some(v) => {
                unsafe { *value = v };
                kResultOk
            }
            None => kResultFalse,
        }
    }

    unsafe fn setFloat(&self, id: AttrID, value: f64) -> tresult {
        self.set(id, Attr::Float(value))
    }

    unsafe fn getFloat(&self, id: AttrID, value: *mut f64) -> tresult {
        match self.with(id, |a| if let Attr::Float(v) = a { Some(*v) } else { None }) {
            Some(v) => {
                unsafe { *value = v };
                kResultOk
            }
            None => kResultFalse,
        }
    }

    unsafe fn setString(&self, id: AttrID, string: *const TChar) -> tresult {
        if string.is_null() {
            return kInvalidArgument;
        }
        let mut v = Vec::new();
        unsafe {
            let mut p = string;
            while *p != 0 {
                v.push(*p);
                p = p.add(1);
            }
        }
        v.push(0);
        self.set(id, Attr::Str(v))
    }

    unsafe fn getString(&self, id: AttrID, string: *mut TChar, size_in_bytes: uint32) -> tresult {
        let cap = size_in_bytes as usize / std::mem::size_of::<TChar>();
        if string.is_null() || cap == 0 {
            return kInvalidArgument;
        }
        let ok = self.with(id, |a| {
            let Attr::Str(s) = a else { return None };
            let n = s.len().min(cap);
            unsafe {
                std::ptr::copy_nonoverlapping(s.as_ptr(), string, n);
                *string.add(n.min(cap - 1)) = 0;
            }
            Some(())
        });
        if ok.is_some() { kResultOk } else { kResultFalse }
    }

    unsafe fn setBinary(&self, id: AttrID, data: *const c_void, size_in_bytes: uint32) -> tresult {
        let bytes = if data.is_null() {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(data as *const u8, size_in_bytes as usize) }.to_vec()
        };
        self.set(id, Attr::Bin(bytes))
    }

    unsafe fn getBinary(
        &self,
        id: AttrID,
        data: *mut *const c_void,
        size_in_bytes: *mut uint32,
    ) -> tresult {
        // The returned pointer stays valid while the attribute is not overwritten.
        let found = self.with(id, |a| {
            let Attr::Bin(b) = a else { return None };
            Some((b.as_ptr() as *const c_void, b.len() as uint32))
        });
        match found {
            Some((p, n)) => {
                unsafe {
                    *data = p;
                    *size_in_bytes = n;
                }
                kResultOk
            }
            None => kResultFalse,
        }
    }
}

// ---------------------------------------------------------------------------
// IComponentHandler: edits coming from the plugin's own editor

/// Parameter change sent from the UI thread to the audio thread.
#[derive(Clone, Copy, Debug)]
pub struct ParamChange {
    pub id: ParamID,
    pub value: f64,
}

pub type ParamSender = Arc<Mutex<rtrb::Producer<ParamChange>>>;

pub struct ComponentHandler {
    pub to_audio: ParamSender,
    /// Accumulated `RestartFlags` requested by the plugin, polled by the UI.
    pub restart_flags: Arc<AtomicI32>,
}

impl Class for ComponentHandler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for ComponentHandler {
    unsafe fn beginEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn performEdit(&self, id: ParamID, value: ParamValue) -> tresult {
        if let Ok(mut tx) = self.to_audio.lock() {
            let _ = tx.push(ParamChange { id, value });
        }
        kResultOk
    }

    unsafe fn endEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn restartComponent(&self, flags: int32) -> tresult {
        self.restart_flags.fetch_or(flags, Ordering::AcqRel);
        kResultOk
    }
}

// ---------------------------------------------------------------------------
// IParameterChanges / IParamValueQueue: preallocated, used on the audio thread

const MAX_POINTS: usize = 16;

pub struct ParamQueue {
    id: Cell<ParamID>,
    points: UnsafeCell<Vec<(int32, ParamValue)>>,
}

impl Class for ParamQueue {
    type Interfaces = (IParamValueQueue,);
}

impl ParamQueue {
    fn new() -> Self {
        Self { id: Cell::new(0), points: UnsafeCell::new(Vec::with_capacity(MAX_POINTS)) }
    }

    fn with_points<R>(&self, f: impl FnOnce(&mut Vec<(int32, ParamValue)>) -> R) -> R {
        // Only touched from the thread currently running `process`, never re-entrantly.
        f(unsafe { &mut *self.points.get() })
    }
}

impl IParamValueQueueTrait for ParamQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id.get()
    }

    unsafe fn getPointCount(&self) -> int32 {
        self.with_points(|p| p.len() as int32)
    }

    unsafe fn getPoint(&self, index: int32, offset: *mut int32, value: *mut ParamValue) -> tresult {
        match self.with_points(|p| p.get(index as usize).copied()) {
            Some((o, v)) => {
                unsafe {
                    *offset = o;
                    *value = v;
                }
                kResultOk
            }
            None => kInvalidArgument,
        }
    }

    unsafe fn addPoint(&self, offset: int32, value: ParamValue, index: *mut int32) -> tresult {
        // Keep points sorted; replace a point at the same offset.
        let pos = self.with_points(|pts| {
            let pos = pts.partition_point(|&(o, _)| o < offset);
            if pts.get(pos).is_some_and(|&(o, _)| o == offset) {
                pts[pos].1 = value;
            } else if pts.len() < MAX_POINTS {
                pts.insert(pos, (offset, value));
            } else {
                // Out of preallocated room: overwrite the last point instead of allocating.
                let last = pts.len() - 1;
                pts[last] = (offset, value);
            }
            pos
        });
        if !index.is_null() {
            unsafe { *index = pos as int32 };
        }
        kResultOk
    }
}

pub struct ParamChanges {
    queues: Vec<ComWrapper<ParamQueue>>,
    used: Cell<usize>,
}

impl Class for ParamChanges {
    type Interfaces = (IParameterChanges,);
}

impl ParamChanges {
    pub fn new(capacity: usize) -> Self {
        Self {
            queues: (0..capacity).map(|_| ComWrapper::new(ParamQueue::new())).collect(),
            used: Cell::new(0),
        }
    }

    pub fn clear(&self) {
        for q in &self.queues[..self.used.get()] {
            q.with_points(Vec::clear);
        }
        self.used.set(0);
    }

    /// Adds a point at sample offset 0, merging with an existing queue for the same id.
    pub fn push(&self, change: ParamChange) {
        self.push_at(change, 0);
    }

    pub fn push_at(&self, change: ParamChange, offset: i32) {
        unsafe {
            let q = self.addParameterData(&change.id, std::ptr::null_mut());
            if !q.is_null() {
                let mut idx = 0;
                ((*(*q).vtbl).addPoint)(q, offset, change.value, &mut idx);
            }
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = ParamChange> + '_ {
        self.queues[..self.used.get()].iter().filter_map(|q| {
            let (_, value) = q.with_points(|p| p.last().copied())?;
            Some(ParamChange { id: q.id.get(), value })
        })
    }
}

impl IParameterChangesTrait for ParamChanges {
    unsafe fn getParameterCount(&self) -> int32 {
        self.used.get() as int32
    }

    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        match self.queues[..self.used.get()].get(index as usize) {
            Some(q) => q.as_com_ref::<IParamValueQueue>().unwrap().as_ptr(),
            None => std::ptr::null_mut(),
        }
    }

    unsafe fn addParameterData(&self, id: *const ParamID, index: *mut int32) -> *mut IParamValueQueue {
        let id = unsafe { *id };
        let used = self.used.get();
        let slot = match self.queues[..used].iter().position(|q| q.id.get() == id) {
            Some(i) => i,
            None if used < self.queues.len() => {
                let q = &self.queues[used];
                q.id.set(id);
                q.with_points(Vec::clear);
                self.used.set(used + 1);
                used
            }
            None => return std::ptr::null_mut(),
        };
        if !index.is_null() {
            unsafe { *index = slot as int32 };
        }
        self.queues[slot].as_com_ref::<IParamValueQueue>().unwrap().as_ptr()
    }
}

// ---------------------------------------------------------------------------
// IEventList: preallocated, used on the audio thread

pub struct EventList {
    events: UnsafeCell<Vec<Event>>,
}

impl Class for EventList {
    type Interfaces = (IEventList,);
}

impl EventList {
    pub const CAPACITY: usize = 512;

    pub fn new() -> Self {
        Self { events: UnsafeCell::new(Vec::with_capacity(Self::CAPACITY)) }
    }

    fn with<R>(&self, f: impl FnOnce(&mut Vec<Event>) -> R) -> R {
        // Only touched from the thread currently running `process`, never re-entrantly.
        f(unsafe { &mut *self.events.get() })
    }

    pub fn clear(&self) {
        self.with(Vec::clear);
    }

    pub fn push(&self, e: Event) {
        self.with(|v| {
            if v.len() < Self::CAPACITY {
                v.push(e);
            }
        });
    }

    pub fn for_each(&self, f: impl FnMut(&Event)) {
        self.with(|v| v.iter().for_each(f));
    }
}

impl IEventListTrait for EventList {
    unsafe fn getEventCount(&self) -> int32 {
        self.with(|v| v.len() as int32)
    }

    unsafe fn getEvent(&self, index: int32, e: *mut Event) -> tresult {
        match self.with(|v| v.get(index as usize).copied()) {
            Some(ev) if !e.is_null() => {
                unsafe { *e = ev };
                kResultOk
            }
            _ => kInvalidArgument,
        }
    }

    unsafe fn addEvent(&self, e: *mut Event) -> tresult {
        if e.is_null() {
            return kInvalidArgument;
        }
        let ev = unsafe { *e };
        if self.with(|v| v.len() < Self::CAPACITY) {
            self.push(ev);
            kResultOk
        } else {
            kResultFalse
        }
    }
}

// ---------------------------------------------------------------------------
// IBStream backed by a Vec<u8> (component state -> controller, presets)

#[derive(Default)]
pub struct MemoryStream {
    data: RefCell<Vec<u8>>,
    pos: Cell<usize>,
}

impl Class for MemoryStream {
    type Interfaces = (IBStream,);
}

impl MemoryStream {
    #[allow(dead_code)] // for project files
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self { data: RefCell::new(bytes), pos: Cell::new(0) }
    }

    pub fn rewind(&self) {
        self.pos.set(0);
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.data.borrow().clone()
    }
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(&self, buffer: *mut c_void, num_bytes: int32, num_read: *mut int32) -> tresult {
        let data = self.data.borrow();
        let pos = self.pos.get().min(data.len());
        let n = (num_bytes.max(0) as usize).min(data.len() - pos);
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr().add(pos), buffer as *mut u8, n);
            if !num_read.is_null() {
                *num_read = n as int32;
            }
        }
        self.pos.set(pos + n);
        kResultOk
    }

    unsafe fn write(&self, buffer: *mut c_void, num_bytes: int32, num_written: *mut int32) -> tresult {
        let n = num_bytes.max(0) as usize;
        let src = unsafe { std::slice::from_raw_parts(buffer as *const u8, n) };
        let mut data = self.data.borrow_mut();
        let pos = self.pos.get();
        if data.len() < pos + n {
            data.resize(pos + n, 0);
        }
        data[pos..pos + n].copy_from_slice(src);
        self.pos.set(pos + n);
        if !num_written.is_null() {
            unsafe { *num_written = n as int32 };
        }
        kResultOk
    }

    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        use IBStream_::IStreamSeekMode_::*;
        let len = self.data.borrow().len() as i64;
        #[cfg(windows)]
        let (seek_set, seek_cur, seek_end) = (kIBSeekSet as u32, kIBSeekCur as u32, kIBSeekEnd as u32);
        #[cfg(not(windows))]
        let (seek_set, seek_cur, seek_end) = (kIBSeekSet, kIBSeekCur, kIBSeekEnd);
        let base = match mode as u32 {
            m if m == seek_set => 0,
            m if m == seek_cur => self.pos.get() as i64,
            m if m == seek_end => len,
            _ => return kInvalidArgument,
        };
        let new = (base + pos).max(0);
        self.pos.set(new as usize);
        if !result.is_null() {
            unsafe { *result = new };
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if pos.is_null() {
            return kInvalidArgument;
        }
        unsafe { *pos = self.pos.get() as int64 };
        kResultOk
    }
}

/// Decodes `restartComponent` flags for logging.
pub fn restart_flags_str(flags: i32) -> String {
    use RestartFlags_::*;
    let names = [
        (kReloadComponent, "ReloadComponent"),
        (kIoChanged, "IoChanged"),
        (kParamValuesChanged, "ParamValuesChanged"),
        (kLatencyChanged, "LatencyChanged"),
        (kParamTitlesChanged, "ParamTitlesChanged"),
    ];
    names
        .iter()
        .filter(|(f, _)| flags & *f != 0)
        .map(|(_, n)| *n)
        .collect::<Vec<_>>()
        .join("|")
}
