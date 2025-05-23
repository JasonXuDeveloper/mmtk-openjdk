use crate::OpenJDKSlot;

use super::abi::*;
use super::UPCALLS;
use crate::SCAN_ORDER_CONFIG;
use libc::c_char;
use mmtk::util::opaque_pointer::*;
use mmtk::util::{Address, ObjectReference};
use mmtk::vm::SlotVisitor;
use rand::seq::SliceRandom;
use rand::thread_rng;
use std::cell::UnsafeCell;
use std::{mem, slice};
use yaml_rust::Yaml;

type S<const COMPRESSED: bool> = OpenJDKSlot<COMPRESSED>;

trait OopIterate: Sized {
    fn oop_iterate<const COMPRESSED: bool>(
        &self,
        oop: Oop,
        closure: &mut impl SlotVisitor<OpenJDKSlot<COMPRESSED>>,
    );
}

impl OopIterate for OopMapBlock {
    fn oop_iterate<const COMPRESSED: bool>(
        &self,
        oop: Oop,
        closure: &mut impl SlotVisitor<S<COMPRESSED>>,
    ) {
        let log_bytes_in_oop = if COMPRESSED { 2 } else { 3 };
        let start = oop.get_field_address(self.offset);
        for i in 0..self.count as usize {
            let slot = (start + (i << log_bytes_in_oop)).into();
            closure.visit_slot(slot);
        }
    }
}

impl OopIterate for InstanceKlass {
    fn oop_iterate<const COMPRESSED: bool>(
        &self,
        oop: Oop,
        closure: &mut impl SlotVisitor<S<COMPRESSED>>,
    ) {
        let mut slots = Vec::new();
        let mut scan_closure = |slot: S<COMPRESSED>| {
            slots.push(slot);
        };
        let oop_maps = self.nonstatic_oop_maps();
        for map in oop_maps {
            map.oop_iterate::<COMPRESSED>(oop, &mut scan_closure);
        }

        if cfg!(feature = "random_fields") {
            let mut rng = thread_rng();
            slots.shuffle(&mut rng);
        } else if cfg!(feature = "reversed_fields") {
            slots.reverse();
        } else if cfg!(feature = "config_fields") {
            unsafe {
                let size: usize = 4096;
                let mut name: Vec<c_char> = Vec::with_capacity(size);
                name.set_len(size);
                let name_ptr = name.as_mut_ptr();
                ((*UPCALLS).symbol_as_c_string)(self.klass.name, name_ptr, size);
                let typename = std::ffi::CStr::from_ptr(name_ptr)
                    .to_str()
                    .unwrap()
                    .replace("/", ".");
                let yamls = &SCAN_ORDER_CONFIG;
                if !yamls.is_empty() {
                    let config = &yamls[0]["field-dereference-counts"][typename.as_str()];
                    if let Yaml::Hash(ref field_map) = *config {
                        let mut temp_slots = vec![];
                        for (_, field_val) in field_map {
                            let offset = field_val["offset"].as_i64().unwrap_or(-1);
                            if offset == -1 {
                                break;
                            }
                            assert!(offset != 0, "offset should not be 0");
                            let addr = oop.get_field_address(offset as i32);
                            let s: S<COMPRESSED> = addr.into();
                            let idx = slots.iter().position(|x| *x == s);
                            if let Some(i) = idx {
                                temp_slots.push(slots.remove(i));
                            }
                        }
                        for slot in temp_slots {
                            closure.visit_slot(slot);
                        }
                    }
                }
            }
        }

        for slot in slots {
            closure.visit_slot(slot);
        }
    }
}

impl OopIterate for InstanceMirrorKlass {
    fn oop_iterate<const COMPRESSED: bool>(
        &self,
        oop: Oop,
        closure: &mut impl SlotVisitor<S<COMPRESSED>>,
    ) {
        self.instance_klass.oop_iterate::<COMPRESSED>(oop, closure);

        // static fields
        let start = Self::start_of_static_fields(oop);
        let len = Self::static_oop_field_count(oop);
        if COMPRESSED {
            let start: *const NarrowOop = start.to_ptr::<NarrowOop>();
            let slice = unsafe { slice::from_raw_parts(start, len as _) };
            for narrow_oop in slice {
                closure.visit_slot(narrow_oop.slot().into());
            }
        } else {
            let start: *const Oop = start.to_ptr::<Oop>();
            let slice = unsafe { slice::from_raw_parts(start, len as _) };
            for oop in slice {
                closure.visit_slot(Address::from_ref(oop as &Oop).into());
            }
        }
    }
}

impl OopIterate for InstanceClassLoaderKlass {
    fn oop_iterate<const COMPRESSED: bool>(
        &self,
        oop: Oop,
        closure: &mut impl SlotVisitor<S<COMPRESSED>>,
    ) {
        self.instance_klass.oop_iterate::<COMPRESSED>(oop, closure);
    }
}

impl OopIterate for ObjArrayKlass {
    fn oop_iterate<const COMPRESSED: bool>(
        &self,
        oop: Oop,
        closure: &mut impl SlotVisitor<S<COMPRESSED>>,
    ) {
        let array = unsafe { oop.as_array_oop() };
        if COMPRESSED {
            for narrow_oop in unsafe { array.data::<NarrowOop, COMPRESSED>(BasicType::T_OBJECT) } {
                closure.visit_slot(narrow_oop.slot().into());
            }
        } else {
            for oop in unsafe { array.data::<Oop, COMPRESSED>(BasicType::T_OBJECT) } {
                closure.visit_slot(Address::from_ref(oop as &Oop).into());
            }
        }
    }
}

impl OopIterate for TypeArrayKlass {
    fn oop_iterate<const COMPRESSED: bool>(
        &self,
        _oop: Oop,
        _closure: &mut impl SlotVisitor<S<COMPRESSED>>,
    ) {
        // Performance tweak: We skip processing the klass pointer since all
        // TypeArrayKlasses are guaranteed processed via the null class loader.
    }
}

impl OopIterate for InstanceRefKlass {
    fn oop_iterate<const COMPRESSED: bool>(
        &self,
        oop: Oop,
        closure: &mut impl SlotVisitor<S<COMPRESSED>>,
    ) {
        use crate::abi::*;
        use crate::api::{add_phantom_candidate, add_soft_candidate, add_weak_candidate};
        self.instance_klass.oop_iterate::<COMPRESSED>(oop, closure);

        if Self::should_scan_weak_refs::<COMPRESSED>() {
            let reference = ObjectReference::from(oop);
            match self.instance_klass.reference_type {
                ReferenceType::None => {
                    panic!("oop_iterate on InstanceRefKlass with reference_type as None")
                }
                ReferenceType::Weak => add_weak_candidate(reference),
                ReferenceType::Soft => add_soft_candidate(reference),
                ReferenceType::Phantom => add_phantom_candidate(reference),
                // Process these two types normally (as if they are strong refs)
                // We will handle final reference later
                ReferenceType::Final | ReferenceType::Other => {
                    Self::process_ref_as_strong(oop, closure)
                }
            }
        } else {
            Self::process_ref_as_strong(oop, closure);
        }
    }
}

impl InstanceRefKlass {
    fn should_scan_weak_refs<const COMPRESSED: bool>() -> bool {
        !*crate::singleton::<COMPRESSED>()
            .get_options()
            .no_reference_types
    }
    fn process_ref_as_strong<const COMPRESSED: bool>(
        oop: Oop,
        closure: &mut impl SlotVisitor<S<COMPRESSED>>,
    ) {
        let referent_addr = Self::referent_address::<COMPRESSED>(oop);
        closure.visit_slot(referent_addr);
        let discovered_addr = Self::discovered_address::<COMPRESSED>(oop);
        closure.visit_slot(discovered_addr);
    }
}

#[allow(unused)]
fn oop_iterate_slow<const COMPRESSED: bool, V: SlotVisitor<S<COMPRESSED>>>(
    oop: Oop,
    closure: &mut V,
    tls: OpaquePointer,
) {
    unsafe {
        CLOSURE.with(|x| *x.get() = closure as *mut V as *mut u8);
        ((*UPCALLS).scan_object)(
            mem::transmute::<*const unsafe extern "C" fn(Address), *mut libc::c_void>(
                scan_object_fn::<COMPRESSED, V> as *const unsafe extern "C" fn(slot: Address),
            ),
            mem::transmute::<&OopDesc, ObjectReference>(oop),
            tls,
        );
    }
}

fn oop_iterate<const COMPRESSED: bool>(oop: Oop, closure: &mut impl SlotVisitor<S<COMPRESSED>>) {
    let klass = oop.klass::<COMPRESSED>();
    let klass_id = klass.id;
    assert!(
        klass_id as i32 >= 0 && (klass_id as i32) < 6,
        "Invalid klass-id: {:x} for oop: {:x}",
        klass_id as i32,
        unsafe { mem::transmute::<Oop, ObjectReference>(oop) }
    );
    match klass_id {
        KlassID::Instance => {
            let instance_klass = unsafe { klass.cast::<InstanceKlass>() };
            instance_klass.oop_iterate::<COMPRESSED>(oop, closure);
        }
        KlassID::InstanceClassLoader => {
            let instance_klass = unsafe { klass.cast::<InstanceClassLoaderKlass>() };
            instance_klass.oop_iterate::<COMPRESSED>(oop, closure);
        }
        KlassID::InstanceMirror => {
            let instance_klass = unsafe { klass.cast::<InstanceMirrorKlass>() };
            instance_klass.oop_iterate::<COMPRESSED>(oop, closure);
        }
        KlassID::ObjArray => {
            let array_klass = unsafe { klass.cast::<ObjArrayKlass>() };
            array_klass.oop_iterate::<COMPRESSED>(oop, closure);
        }
        KlassID::TypeArray => {
            // Skip scanning primitive arrays as they contain no reference fields.
        }
        KlassID::InstanceRef => {
            let instance_klass = unsafe { klass.cast::<InstanceRefKlass>() };
            instance_klass.oop_iterate::<COMPRESSED>(oop, closure);
        }
    }
}

thread_local! {
    static CLOSURE: UnsafeCell<*mut u8> = const { UnsafeCell::new(std::ptr::null_mut()) };
}

pub unsafe extern "C" fn scan_object_fn<
    const COMPRESSED: bool,
    V: SlotVisitor<OpenJDKSlot<COMPRESSED>>,
>(
    slot: Address,
) {
    let ptr: *mut u8 = CLOSURE.with(|x| *x.get());
    let closure = &mut *(ptr as *mut V);
    closure.visit_slot(slot.into());
}

pub fn scan_object<const COMPRESSED: bool>(
    object: ObjectReference,
    closure: &mut impl SlotVisitor<S<COMPRESSED>>,
    _tls: VMWorkerThread,
) {
    unsafe {
        oop_iterate::<COMPRESSED>(mem::transmute::<ObjectReference, &OopDesc>(object), closure)
    }
}
