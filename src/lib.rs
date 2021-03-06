//! We need to expose the Wren API in a Rust-y way
use wren_sys::{WrenVM, WrenHandle, WrenConfiguration, WrenErrorType, WrenForeignClassMethods};
use std::sync::mpsc::{channel, Sender, Receiver};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::cell::RefCell;

pub use wren_sys;

use std::{mem, ffi, os::raw, any, marker};

#[cfg(test)]
mod tests;

#[derive(Debug)]
pub enum WrenError {
    Compile(String, i32, String),
    Runtime(String),
    StackTrace(String, i32, String),
}

// Force Wren to use Rust's allocator to allocate memory
// Done because sometimes Wren forces us to allocate memory and give *it* ownership
// Rust might not use the standard allocator, so we move Wren to use *our* allocator
extern "C" fn wren_realloc(memory: *mut ffi::c_void, new_size: wren_sys::size_t) -> *mut ffi::c_void {
    unsafe {
        if memory.is_null() { // If memory == NULL
            // allocate new memory
            std::alloc::alloc_zeroed(std::alloc::Layout::from_size_align(new_size as usize, 8).unwrap()) as *mut _
        } else {
            // Memory is an actual pointer to a location.
            if new_size == 0 {
                std::alloc::dealloc(memory as *mut _, std::alloc::Layout::from_size_align(0, 8).unwrap());
                std::ptr::null_mut()
            } else {
                std::alloc::realloc(memory as *mut _, std::alloc::Layout::from_size_align(new_size as usize, 8).unwrap(), new_size as usize) as *mut _
            }
        }
    }
}

extern "C" fn wren_error(vm: *mut WrenVM, typ: WrenErrorType, module: *const raw::c_char, line: raw::c_int, message: *const raw::c_char) {
    let conf = unsafe { &mut *(wren_sys::wrenGetUserData(vm) as *mut UserData) };
    match typ {
        wren_sys::WrenErrorType_WREN_ERROR_COMPILE => {
            let module_str = unsafe { ffi::CStr::from_ptr(module) };
            let message_str = unsafe { ffi::CStr::from_ptr(message) };
            conf.error_channel.send(WrenError::Compile(module_str.to_string_lossy().to_string(), line as i32, message_str.to_string_lossy().to_string())).unwrap();
        },
        wren_sys::WrenErrorType_WREN_ERROR_RUNTIME => {
            let message_str = unsafe { ffi::CStr::from_ptr(message) };
            conf.error_channel.send(WrenError::Runtime(message_str.to_string_lossy().to_string())).unwrap();
        },
        wren_sys::WrenErrorType_WREN_ERROR_STACK_TRACE => {
            let module_str = unsafe { ffi::CStr::from_ptr(module) };
            let message_str = unsafe { ffi::CStr::from_ptr(message) };
            conf.error_channel.send(WrenError::StackTrace(module_str.to_string_lossy().to_string(), line as i32, message_str.to_string_lossy().to_string())).unwrap();
        },
        _ => unreachable!()
    }
}

extern "C" fn wren_print(vm: *mut WrenVM, message: *const raw::c_char) {
    let conf = unsafe { &mut *(wren_sys::wrenGetUserData(vm) as *mut UserData) };
    let message_str = unsafe { ffi::CStr::from_ptr(message) };
    conf.printer.print(message_str.to_string_lossy().to_string());
}

extern "C" fn wren_bind_foreign_method(vm: *mut WrenVM, mdl: *const raw::c_char, class: *const raw::c_char, is_static: bool, sgn: *const raw::c_char) -> Option<unsafe extern "C" fn(*mut WrenVM)> {
    let conf = unsafe { &mut *(wren_sys::wrenGetUserData(vm) as *mut UserData) };
    let module = unsafe { ffi::CStr::from_ptr(mdl) };
    let class = unsafe { ffi::CStr::from_ptr(class) };
    let signature = unsafe { ffi::CStr::from_ptr(sgn) };

    if let Some(ref library) = conf.library {
        if let Some(rc) = library.get_foreign_class(module.to_string_lossy(), class.to_string_lossy()) {
            rc.methods.function_pointers.iter().find(|mp| {
                mp.signature.as_wren_string() == signature.to_string_lossy() && mp.is_static == is_static
            }).map(|mp| mp.pointer)
        } else {
            None
        }
    } else {
        None
    }
}

extern "C" fn wren_bind_foreign_class(vm: *mut WrenVM, mdl: *const raw::c_char, class: *const raw::c_char) -> WrenForeignClassMethods {
    let mut fcm = WrenForeignClassMethods {
        allocate: None,
        finalize: None
    };

    let conf = unsafe { &mut *(wren_sys::wrenGetUserData(vm) as *mut UserData) };
    let module = unsafe { ffi::CStr::from_ptr(mdl) };
    let class = unsafe { ffi::CStr::from_ptr(class) };

    if let Some(ref library) = conf.library {
        let rc = library.get_foreign_class(module.to_string_lossy(), class.to_string_lossy());
        if let Some(rc) = rc {
            fcm.allocate = Some(rc.construct);
            fcm.finalize = Some(rc.destruct);
        }
    }
    fcm
}

extern "C" fn wren_load_module(vm: *mut WrenVM, name: *const raw::c_char) -> *mut raw::c_char {
    // The whoooole reason we wrote wren_realloc - to force Wren into Rust's allocation space
    let conf = unsafe { &mut *(wren_sys::wrenGetUserData(vm) as *mut UserData) };
    let module_name = unsafe { ffi::CStr::from_ptr(name) };
    match conf.loader.load_script(module_name.to_string_lossy().to_string()) {
        Some(string) => {
            ffi::CString::new(string).unwrap_or_else(|_| panic!("Failed to convert source to C string for {}", module_name.to_string_lossy())).into_raw()
        },
        None => std::ptr::null_mut()
    }
}

extern "C" fn wren_canonicalize(_: *mut WrenVM, importer: *const raw::c_char, name: *const raw::c_char) -> *const raw::c_char {
    let _importer = unsafe { ffi::CStr::from_ptr(importer) };
    let _name = unsafe { ffi::CStr::from_ptr(name) };
    let _importer = _importer.to_string_lossy();
    let _name = _name.to_string_lossy();

    if let Some('@') = _name.chars().next() {
        let real_name: String = _name.chars().skip(1).collect();
        ffi::CString::new(format!("{}/{}", _importer, real_name))
            .unwrap_or_else(|_| panic!("Failed to convert name {}/{} to C string", _importer, real_name))
            .into_raw() as *const _
    } else {
        name
    }
}

#[derive(Debug, Clone)]
pub enum VMError {
    Compile {
        module: String,
        line: i32,
        error: String
    },
    Runtime {
        error: String,
        frames: Vec<VMStackFrameError>
    }
}

#[derive(Debug, Clone)]
pub struct VMStackFrameError {
    pub module: String,
    pub line: i32,
    pub function: String
}


impl std::fmt::Display for VMError {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            VMError::Compile { module, line, error } => write!(fmt, "Compile Error ({}:{}): {}", module, line, error),
            VMError::Runtime { error, frames } => {
                writeln!(fmt, "Runtime Error: {}", error)?;
                for frame in frames {
                    if frame.function == "" {
                        writeln!(fmt, "\tin {}:{}: <constructor>", frame.module, frame.line)?;
                    } else {
                        writeln!(fmt, "\tin {}:{}: {}", frame.module, frame.line, frame.function)?;
                    }
                }
                Ok(())
            },
        }
    }
}

impl std::error::Error for VMError {}

/// A handle to a Wren object
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handle<'a> {
    handle: *mut WrenHandle,
    wvm: *mut WrenVM,
    vm: marker::PhantomData<&'a VM>
}

impl<'a> Drop for Handle<'a> {
    fn drop(&mut self) {
        unsafe {
            wren_sys::wrenReleaseHandle(self.wvm, self.handle);
        }
    }
}

/// A handle to a Wren method call
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionHandle<'a>(Handle<'a>);

/// Simulates a module structure for foreign functions
#[derive(Debug, Clone, Default)]
pub struct ModuleLibrary {
    modules: HashMap<String, Module>,
}

impl ModuleLibrary {
    pub fn new() -> ModuleLibrary {
        ModuleLibrary {
            modules: HashMap::new()
        }
    }

    pub fn module<N: Into<String>>(&mut self, name: N, modl: Module) {
        self.modules.insert(name.into(), modl);
    }

    fn get_foreign_class<M: AsRef<str>, C: AsRef<str>>(&self, module: M, class: C) -> Option<&RuntimeClass> {
        self.modules.get(module.as_ref()).and_then(|md| md.classes.get(class.as_ref()))
    }
}

#[derive(Debug, Clone)]
struct RuntimeClass {
    construct: extern "C" fn(*mut WrenVM),
    destruct: extern "C" fn(*mut ffi::c_void),
    methods: ClassObjectPointers,

    // Use for "loading in" appropriate objects
    type_id: any::TypeId,
}

#[derive(Debug, Clone, Default)]
pub struct Module {
    classes: HashMap<String, RuntimeClass>,
}

#[derive(Debug, Clone)]
pub struct ClassObjectPointers {
    pub function_pointers: Vec<MethodPointer>,
}

#[derive(Debug, Clone)]
pub struct MethodPointer {
    pub is_static: bool,
    pub signature: FunctionSignature,
    pub pointer: unsafe extern "C" fn(*mut WrenVM),
}

impl Module {
    pub fn new() -> Module {
        Module {
            classes: HashMap::new()
        }
    }

    pub fn class<C: 'static + ClassObject, S: Into<String>>(&mut self, name: S) -> &mut Self {
        let cp = C::generate_pointers();
        let init = C::initialize_pointer();
        let deinit = C::finalize_pointer();
        self.classes.insert(name.into(), RuntimeClass {
            construct: init,
            destruct: deinit,
            methods: cp,
            type_id: any::TypeId::of::<C>(),
        });
        self
    }
}

// Trait that all Wren "class" objects implement
pub trait Class {
    fn initialize(_: &VM) -> Self where Self: Sized;
}

pub trait ClassObject: Class {
    fn initialize_pointer() -> extern "C" fn(*mut WrenVM) where Self: Sized;
    fn finalize_pointer() -> extern "C" fn(*mut ffi::c_void) where Self: Sized;
    fn generate_pointers() -> ClassObjectPointers where Self: Sized;
}

#[derive(Debug, Copy, Clone)]
pub struct ForeignObject<T> {
    pub object: *mut T,
    pub type_id: any::TypeId,
}

/// Creates a function at $modl::publish_module, that takes a &mut ModuleLibrary
/// and handles Module object creation and registration
/// 
/// Also internally creates all the necessary extern "C" functions for Wren's callbacks
#[macro_export]
macro_rules! create_module {
    (
        $(
            class($mname:expr) $name:ty => $md:ident {
                $(
                    $si:ident($lbls:ident $($sgns:expr),+) $id:ident
                ),*
            }
        )+

        module => $modl:ident
    ) => {
        $(
            mod $md {
                use std::panic::{take_hook, set_hook, catch_unwind, AssertUnwindSafe};

                pub(in super) extern "C" fn _constructor(vm: *mut $crate::wren_sys::WrenVM) {
                    use $crate::Class;
                    unsafe {
                        let conf = &mut *($crate::wren_sys::wrenGetUserData(vm) as *mut $crate::UserData);
                        let vm = std::rc::Weak::upgrade(&conf.vm).expect(&format!("Failed to access VM at {:p}", &conf.vm));
                        let wptr = $crate::wren_sys::wrenSetSlotNewForeign(vm.borrow().vm, 0, 0, std::mem::size_of::<$crate::ForeignObject<$name>>() as $crate::wren_sys::size_t);
                        // Allocate a new object, and move it onto the heap
                        set_hook(Box::new(|_| {}));
                        let vm_borrow = AssertUnwindSafe(vm.borrow());
                        let object = match catch_unwind(|| <$name as Class>::initialize(&*vm_borrow)) {
                            Ok(obj) => Some(obj),
                            Err(err) => {
                                let err_string = if let Some(strg) = err.downcast_ref::<String>() {
                                    strg.clone()
                                } else if let Some(strg) = err.downcast_ref::<&str>() {
                                    strg.to_string()
                                } else {
                                    "Non-string panic message".into()
                                };

                                vm_borrow.set_slot_string(0, err_string);
                                vm_borrow.abort_fiber(0);
                                None
                            }
                        };
                        drop(take_hook());
                        // Copy the object pointer if we were successful
                        if let Some(object) = object {
                            std::ptr::write(wptr as *mut _, $crate::ForeignObject {
                                object: Box::into_raw(Box::new(object)),
                                type_id: std::any::TypeId::of::<$name>(),
                            });
                        }
                    }
                }

                pub(in super) extern "C" fn _destructor(data: *mut std::ffi::c_void) {
                    unsafe {
                        let mut fo: &mut $crate::ForeignObject<$name> = &mut *(data as *mut _);
                        if !fo.object.is_null() { // If we haven't dropped an object, work on dropping it.
                            drop(Box::from_raw(fo.object));
                            fo.object = std::ptr::null_mut();
                        }
                    }
                }

                $(
                    $crate::create_module!(@fn $si $name => $id);
                )*
            }

            impl $crate::ClassObject for $name {
                fn initialize_pointer() -> extern "C" fn(*mut $crate::wren_sys::WrenVM) { $md::_constructor }
                fn finalize_pointer() -> extern "C" fn(*mut std::ffi::c_void) { $md::_destructor }
                fn generate_pointers() -> $crate::ClassObjectPointers {
                    $crate::ClassObjectPointers {
                        function_pointers: vec![
                            $(
                                $crate::create_module!(@md $si $id $lbls $md $($sgns),+)
                            ),*
                        ]
                    }
                }
            }
        )+

        mod $modl {
            pub fn publish_module(lib: &mut $crate::ModuleLibrary) {
                let mut module = $crate::Module::new();
                module
                $(
                    .class::<$name, _>($mname)
                )+;
                lib.module(stringify!($modl).replace("_", "/"), module);
            }
        }
    };

    (@md static $id:ident $lbls:ident $md:ident $($sgns: expr),+) => {
        $crate::MethodPointer {
            pointer: $md::$id,
            signature: $crate::create_module!(@sgn $lbls $($sgns),+),
            is_static: true,
        }
    };

    (@md instance $id:ident $lbls:ident $md:ident $($sgns: expr),+) => {
        $crate::MethodPointer {
            pointer: $md::$id,
            signature: $crate::create_module!(@sgn $lbls $($sgns),+),
            is_static: false,
        }
    };

    (@sgn fn $nom:expr, $arity:expr) => {
        $crate::FunctionSignature::new_function($nom, $arity)
    };

    (@sgn getter $name:expr) => {
        $crate::FunctionSignature::new_getter($name)
    };

    (@sgn setter $name:expr) => {
        $crate::FunctionSignature::new_setter($name)
    };

    (@fn static $name:ty => $s:ident) => {
        pub(in super) unsafe extern "C" fn $s(vm: *mut $crate::wren_sys::WrenVM) {
            use std::panic::{take_hook, set_hook, catch_unwind, AssertUnwindSafe};

            let conf = &mut *($crate::wren_sys::wrenGetUserData(vm) as *mut $crate::UserData);
            let vm = std::rc::Weak::upgrade(&conf.vm).expect(&format!("Failed to access VM at {:p}", &conf.vm));
            set_hook(Box::new(|_| {}));
            let vm_borrow = AssertUnwindSafe(vm.borrow());
            match catch_unwind(|| <$name>::$s(&*vm_borrow)) {
                Ok(_) => (),
                Err(err) => {
                    let err_string = if let Some(strg) = err.downcast_ref::<String>() {
                        strg.clone()
                    } else if let Some(strg) = err.downcast_ref::<&str>() {
                        strg.to_string()
                    } else {
                        "Non-string panic message".into()
                    };

                    vm_borrow.set_slot_string(0, err_string);
                    vm_borrow.abort_fiber(0);
                }
            };
            drop(take_hook());
        }
    };

    (@fn instance $name:ty => $inf:ident) => {
        pub(in super) unsafe extern "C" fn $inf(vm: *mut $crate::wren_sys::WrenVM) {
            use std::panic::{take_hook, set_hook, catch_unwind, AssertUnwindSafe};
            
            let conf = &mut *($crate::wren_sys::wrenGetUserData(vm) as *mut $crate::UserData);
            let vm = std::rc::Weak::upgrade(&conf.vm).expect(&format!("Failed to access VM at {:p}", &conf.vm));
            set_hook(Box::new(|_| {}));
            let vm_borrow = AssertUnwindSafe(vm.borrow());
            match catch_unwind(|| {
                vm_borrow.ensure_slots(1);
                let inst = vm_borrow.get_slot_foreign_mut::<$name>(0)
                    .expect(&format!("Tried to call {0} of {1} on non-{1} type", stringify!($inf), std::any::type_name::<$name>()));
                inst.$inf(&*vm_borrow)
            }) {
                Ok(_) => (),
                Err(err) => {
                    let err_string = if let Some(strg) = err.downcast_ref::<String>() {
                        strg.clone()
                    } else if let Some(strg) = err.downcast_ref::<&str>() {
                        strg.to_string()
                    } else {
                        "Non-string panic message".into()
                    };

                    vm_borrow.set_slot_string(0, err_string);
                    vm_borrow.abort_fiber(0);
                }
            };
            drop(take_hook());
        }
    }
}

/// Checks if the slot type is correct at the given slot.
/// If not, will panic.
/// If it is, will return the item at the given slot.
// We can do unwraps because we manually check the type beforehand, so we are *sure* it is there.
#[macro_export]
macro_rules! get_slot_checked {
    ($vm:expr => num $slot:expr) => {
        {
            if $vm.get_slot_type($slot) != $crate::SlotType::Num { panic!("rust error [{}:{}]: Slot {} is not a <num>", file!(), line!(), $slot) }
            $vm.get_slot_double($slot).unwrap()
        }
    };

    ($vm:expr => bool $slot:expr) => {
        {
            if $vm.get_slot_type($slot) != $crate::SlotType::Bool { panic!("rust error [{}:{}]: Slot {} is not a <bool>", file!(), line!(), $slot) }
            $vm.get_slot_bool($slot).unwrap()
        }
    };

    ($vm:expr => string $slot:expr) => {
        {
            if $vm.get_slot_type($slot) != $crate::SlotType::String { panic!("rust error [{}:{}]: Slot {} is not a <string>", file!(), line!(), $slot) }
            $vm.get_slot_string($slot).unwrap()
        }
    };

    ($vm:expr => bytes $slot:expr) => {
        {
            if $vm.get_slot_type($slot) != $crate::SlotType::String { panic!("rust error [{}:{}]: Slot {} is not a <string>", file!(), line!(), $slot) }
            $vm.get_slot_bytes($slot).unwrap()
        }
    };

    ($vm:expr => foreign $t:ty => $slot:expr) => {
        {
            if $vm.get_slot_type($slot) != $crate::SlotType::Foreign { panic!("rust error [{}:{}]: Slot {} is not a <foreign>", file!(), line!(), $slot)}
            match $vm.get_slot_foreign::<$t>($slot) {
                Some(ty) => ty,
                None => panic!("rust error [{}:{}]: Slot {} is not a foreign of type {}", file!(), line!(), $slot, std::any::type_name::<$t>())
            }
        }
    };

    ($vm:expr => foreign_mut $t:ty => $slot:expr) => {
        {
            if $vm.get_slot_type($slot) != $crate::SlotType::Foreign { panic!("rust error [{}:{}]: Slot {} is not a <foreign>", file!(), line!(), $slot)}
            match $vm.get_slot_foreign_mut::<$t>($slot) {
                Some(ty) => ty,
                None => panic!("rust error [{}:{}]: Slot {} is not a foreign of type {}", file!(), line!(), $slot, std::any::type_name::<$t>())
            }
        }
    };
}

pub fn type_name_of<T>(_: &T) -> &'static str {
    any::type_name::<T>()
}

/// Sends a foreign object [$obj] as an object of [$class] in module [$modl] to slot [$slot]
#[macro_export]
macro_rules! send_foreign {
    ($vm:expr, $modl:expr, $class:expr, $obj:expr => $slot:expr) => {
        {
            let obj_name = $crate::type_name_of(&$obj);
            match $vm.set_slot_new_foreign($modl, $class, $obj, $slot) {
                Err(e) => panic!("rust error [{}:{}]: Could not send type {:?} as [{}] {}: {}", file!(), line!(), obj_name, $modl, $class, e),
                Ok(rf) => rf
            }
        }
    }
}

/// Enables one to enable module loading for Wren
pub trait ModuleScriptLoader {
    fn load_script(&mut self, name: String) -> Option<String>;
}

impl<T> ModuleScriptLoader for T where T: FnMut(String) -> Option<String> {
    fn load_script(&mut self, name: String) -> Option<String> {
        (*self)(name)
    }
}

type EVM = Rc<RefCell<VM>>;

pub trait Printer {
    fn print(&mut self, s: String);
}

impl<T> Printer for T where T: FnMut(String) {
    fn print(&mut self, s: String) {
        (*self)(s)
    }
}

struct PrintlnPrinter;
impl Printer for PrintlnPrinter {
    fn print(&mut self, s: String) {
        print!("{}", s);
    }
}

struct NullLoader;
impl ModuleScriptLoader for NullLoader {
    fn load_script(&mut self, _: String) -> Option<String> { None }
}

#[derive(Debug)]
pub struct VM {
    pub vm: *mut WrenVM,
    error_recv: Receiver<WrenError>,
}

/// A mostly internal class that is exposed so that some externally generated code can access it.
pub struct UserData {
    error_channel: Sender<WrenError>,
    printer: Box<dyn Printer>,
    pub vm: Weak<RefCell<VM>>, // is used a *lot* by externally generated code.
    library: Option<ModuleLibrary>,
    loader: Box<dyn ModuleScriptLoader>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotType {
    Num,
    Bool,
    List,
    Null,
    String,
    Foreign,
    Unknown
}

pub type SlotId = usize;

#[derive(Debug, Clone)]
pub enum FunctionSignature {
    Function {
        name: String,
        arity: usize
    },
    Getter(String),
    Setter(String),
}

impl FunctionSignature {
    pub fn new_function<N: Into<String>>(name: N, arity: usize) -> FunctionSignature {
        FunctionSignature::Function {
            name: name.into(),
            arity
        }
    }

    pub fn new_getter<N: Into<String>>(name: N) -> FunctionSignature {
        FunctionSignature::Getter(name.into())
    }

    pub fn new_setter<N: Into<String>>(name: N) -> FunctionSignature {
        FunctionSignature::Setter(name.into())
    }

    fn as_wren_string(&self) -> String {
        match self {
            FunctionSignature::Function { name, arity } => format!("{}({})", name, vec!["_".to_string(); *arity].join(",")),
            FunctionSignature::Getter(name) => name.clone(),
            FunctionSignature::Setter(name) => format!("{}=(_)", name),
        }
    }

    pub fn arity(&self) -> usize {
        match self {
            FunctionSignature::Function { arity, .. } => *arity,
            FunctionSignature::Getter(_) => 0,
            FunctionSignature::Setter(_) => 1,
        }
    }
}

#[derive(Debug, Clone)]
pub struct VMWrapper(EVM);

impl VMWrapper {
    pub fn call(&self, signature: FunctionSignature) -> Result<(), VMError> {
        let handle = self.make_call_handle(signature);
        self.call_handle(&handle)
    }

    pub fn call_handle(&self, handle: &FunctionHandle) -> Result<(), VMError> {
        let vm = self.0.borrow();
        match unsafe { wren_sys::wrenCall(vm.vm, handle.0.handle) } {
            wren_sys::WrenInterpretResult_WREN_RESULT_SUCCESS => Ok(()),
            wren_sys::WrenInterpretResult_WREN_RESULT_COMPILE_ERROR => unreachable!("wrenCall doesn't compile anything"),
            wren_sys::WrenInterpretResult_WREN_RESULT_RUNTIME_ERROR => {
                let mut error = "".to_string();
                let mut frames = vec![];
                while let Ok(err) = vm.error_recv.try_recv() {
                    match err {
                        WrenError::Runtime(msg) => {error = msg; },
                        WrenError::StackTrace(module, line, msg) => {frames.push(VMStackFrameError {
                            module, line, function: msg
                        }); },
                        _ => unreachable!()
                    }
                }
                Err(VMError::Runtime{
                    error,
                    frames
                })
            },
            _ => unreachable!()
        }
    }

    pub fn interpret<M: AsRef<str>, C: AsRef<str>>(&self, module: M, code: C) -> Result<(), VMError> {
        let module = ffi::CString::new(module.as_ref()).expect("module name conversion failed");
        let code = ffi::CString::new(code.as_ref()).expect("code conversion failed");
        let vm = self.0.borrow();
        match unsafe { wren_sys::wrenInterpret(vm.vm, module.as_ptr() as *const i8, code.as_ptr() as *const i8) } {
            wren_sys::WrenInterpretResult_WREN_RESULT_SUCCESS => Ok(()),
            wren_sys::WrenInterpretResult_WREN_RESULT_COMPILE_ERROR => match vm.error_recv.try_recv() {
                Ok(WrenError::Compile(module, line, msg)) => {
                    Err(VMError::Compile { module, line, error: msg })
                }
                _ => unreachable!()
            },
            wren_sys::WrenInterpretResult_WREN_RESULT_RUNTIME_ERROR => {
                let mut error = "".to_string();
                let mut frames = vec![];
                while let Ok(err) = vm.error_recv.try_recv() {
                    match err {
                        WrenError::Runtime(msg) => {error = msg; },
                        WrenError::StackTrace(module, line, msg) => {frames.push(VMStackFrameError {
                            module, line, function: msg
                        }); },
                        _ => unreachable!()
                    }
                }
                Err(VMError::Runtime{
                    error,
                    frames
                })
            },
            _ => unreachable!()
        }
    }

    pub fn execute<T, F>(&self, f: F) -> T where F: FnOnce(&VM) -> T {
        f(&self.0.borrow())
    }

    pub fn get_slot_handle(&self, slot: SlotId) -> Rc<Handle> {
        Rc::new(Handle {
            handle: unsafe {
                wren_sys::wrenGetSlotHandle(self.0.borrow().vm, slot as raw::c_int)
            },
            wvm: self.0.borrow().vm,
            vm: marker::PhantomData
        })
    }

    pub fn set_slot_handle(&self, slot: SlotId, handle: &Handle) {
        unsafe {
            wren_sys::wrenSetSlotHandle(self.0.borrow().vm, slot as raw::c_int, handle.handle)
        }
    }

    pub fn make_call_handle(&self, signature: FunctionSignature) -> Rc<FunctionHandle> {
        VM::make_call_handle(self.0.borrow().vm, signature)
    }

    /// Instruct Wren to start a garbage collection cycle
    pub fn collect_garbage(&self) {
        unsafe {
            wren_sys::wrenCollectGarbage(self.0.borrow().vm)
        }
    }
}

pub struct VMConfig {
    printer: Box<dyn Printer>,
    script_loader: Box<dyn ModuleScriptLoader>,
    library: Option<ModuleLibrary>,
    initial_heap_size: usize,
    min_heap_size: usize,
    heap_growth_percent: usize,

    enable_relative_import: bool, // Uses @module, to mean [module] loaded relative to this one
}

impl Default for VMConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl VMConfig {
    pub fn new() -> VMConfig {
        VMConfig {
            printer: Box::new(PrintlnPrinter),
            script_loader: Box::new(NullLoader),
            library: None,
            initial_heap_size: 1024 * 1024 * 10,
            min_heap_size: 1024 * 1024,
            heap_growth_percent: 50,
            enable_relative_import: false,
        }
    }

    pub fn printer<P: 'static + Printer>(mut self, p: P) -> Self {
        self.printer = Box::new(p);
        self
    }

    pub fn script_loader<L: 'static + ModuleScriptLoader>(mut self, l: L) -> Self {
        self.script_loader = Box::new(l);
        self
    }

    pub fn library(mut self, l: &ModuleLibrary) -> Self {
        self.library = Some(l.clone());
        self
    }

    pub fn no_library(mut self) -> Self {
        self.library = None;
        self
    }

    pub fn initial_heap_size(mut self, ihs: usize) -> Self {
        self.initial_heap_size = ihs;
        self
    }

    pub fn min_heap_size(mut self, mhs: usize) -> Self {
        self.min_heap_size = mhs;
        self
    }

    pub fn heap_growth_percent(mut self, hgp: usize) -> Self {
        self.heap_growth_percent = hgp;
        self
    }

    pub fn enable_relative_import(mut self, eri: bool) -> Self {
        self.enable_relative_import = eri;
        self
    }

    pub fn build(self) -> VMWrapper {
        let (etx, erx) = channel();

        // Have an uninitialized VM...
        let wvm = Rc::new(RefCell::new(VM {
            vm: std::ptr::null_mut(),
            error_recv: erx
        }));

        let vm_config = Box::into_raw(Box::new(UserData {
            error_channel: etx,
            printer: self.printer,
            vm: Rc::downgrade(&wvm),
            loader: self.script_loader,
            library: self.library,
        }));

        // Configure the Wren side of things
        let mut config = unsafe {
            let mut uconfig = mem::MaybeUninit::<WrenConfiguration>::zeroed();
            wren_sys::wrenInitConfiguration(uconfig.as_mut_ptr());
            let mut config = uconfig.assume_init();
            config.errorFn = Some(wren_error);
            config.writeFn = Some(wren_print);
            config.reallocateFn = Some(wren_realloc);
            config.bindForeignMethodFn = Some(wren_bind_foreign_method);
            config.bindForeignClassFn = Some(wren_bind_foreign_class);
            config.loadModuleFn = Some(wren_load_module);
            config.resolveModuleFn = if self.enable_relative_import {
                Some(wren_canonicalize)
            } else {
                None
            };
            config.initialHeapSize = self.initial_heap_size as wren_sys::size_t;
            config.minHeapSize = self.min_heap_size as wren_sys::size_t;
            config.heapGrowthPercent = self.heap_growth_percent as raw::c_int;
            config.userData = vm_config as *mut ffi::c_void;
            config
        };

        let vm = unsafe { wren_sys::wrenNewVM(&mut config) };
        wvm.borrow_mut().vm = vm;
        VMWrapper(wvm)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignSendError {
    NoForeignClass,
    NoWrenClass,
    NoMemory,
    ClassMismatch,

}

impl std::fmt::Display for ForeignSendError {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            ForeignSendError::NoForeignClass => write!(fmt, "no foreign class"),
            ForeignSendError::NoWrenClass => write!(fmt, "no Wren class"),
            ForeignSendError::NoMemory => write!(fmt, "unable to allocate memory"),
            ForeignSendError::ClassMismatch => write!(fmt, "class mismatch"),
        }
    }
}

impl std::error::Error for ForeignSendError {}

impl VM {
    // Slot and Handle API
    pub fn ensure_slots(&self, count: usize) {
        unsafe {
            wren_sys::wrenEnsureSlots(self.vm, count as raw::c_int)
        }
    }

    pub fn get_slot_count(&self) -> usize {
        unsafe {
            wren_sys::wrenGetSlotCount(self.vm) as usize
        }
    }

    pub fn set_slot_bool(&self, slot: SlotId, val: bool) {
        unsafe {
            wren_sys::wrenSetSlotBool(self.vm, slot as raw::c_int, val)
        }
    }

    pub fn set_slot_double(&self, slot: SlotId, val: f64) {
        unsafe {
            wren_sys::wrenSetSlotDouble(self.vm, slot as raw::c_int, val)
        }
    }

    pub fn set_slot_null(&self, slot: SlotId) {
        unsafe {
            wren_sys::wrenSetSlotNull(self.vm, slot as raw::c_int)
        }
    }

    pub fn set_slot_bytes(&self, slot: SlotId, bytes: &[u8]) {
        unsafe {
            wren_sys::wrenSetSlotBytes(self.vm, slot as raw::c_int, bytes as *const _ as *const raw::c_char, bytes.len() as wren_sys::size_t);
        }
    }

    pub fn set_slot_string<S: AsRef<str>>(&self, slot: SlotId, string: S) {
        let string = string.as_ref();
        unsafe {
            wren_sys::wrenSetSlotBytes(self.vm, slot as raw::c_int, string.as_ptr() as *const _, string.len() as wren_sys::size_t);
        }
    }

    pub fn get_slot_bool(&self, slot: SlotId) -> Option<bool> {
        if self.get_slot_type(slot) != SlotType::Bool {
            None
        } else {
            unsafe {
                Some(wren_sys::wrenGetSlotBool(self.vm, slot as raw::c_int))
            }
        }
    }

    pub fn get_slot_double(&self, slot: SlotId) -> Option<f64> {
        if self.get_slot_type(slot) != SlotType::Num {
            None
        } else {
            unsafe {
                Some(wren_sys::wrenGetSlotDouble(self.vm, slot as raw::c_int))
            }
        }
    }

    pub fn get_slot_bytes(&self, slot: SlotId) -> Option<Vec<u8>> {
        if self.get_slot_type(slot) != SlotType::String {
            None
        } else {
            let mut length = 0 as raw::c_int;
            let ptr = unsafe {
                wren_sys::wrenGetSlotBytes(self.vm, slot as raw::c_int, &mut length as *mut _)
            };
            let mut bytes = vec![];

            // Do some pointer maths to get the vector. Hurrah!
            for offset in 0..length {
                unsafe {
                    bytes.push(*ptr.offset(offset as isize) as u8)
                }
            }

            Some(bytes)
        }
    }

    pub fn get_slot_string(&self, slot: SlotId) -> Option<String> {
        if self.get_slot_type(slot) != SlotType::String {
            None
        } else {
            let ptr = unsafe {
                wren_sys::wrenGetSlotString(self.vm, slot as raw::c_int)
            };
    
            let cstr = unsafe{ ffi::CStr::from_ptr(ptr) };
    
            Some(cstr.to_string_lossy().to_string())
        }
    }

    pub fn get_slot_type(&self, slot: SlotId) -> SlotType {
        match unsafe { wren_sys::wrenGetSlotType(self.vm, slot as raw::c_int) } {
            wren_sys::WrenType_WREN_TYPE_NUM => SlotType::Num,
            wren_sys::WrenType_WREN_TYPE_BOOL => SlotType::Bool,
            wren_sys::WrenType_WREN_TYPE_LIST => SlotType::List,
            wren_sys::WrenType_WREN_TYPE_NULL => SlotType::Null,
            wren_sys::WrenType_WREN_TYPE_STRING => SlotType::String,
            wren_sys::WrenType_WREN_TYPE_FOREIGN => SlotType::Foreign,
            wren_sys::WrenType_WREN_TYPE_UNKNOWN => SlotType::Unknown,
            _ => unreachable!()
        }
    }

    pub fn get_variable<M: AsRef<str>, N: AsRef<str>>(&self, module: M, name: N, slot: SlotId) {
        let module = ffi::CString::new(module.as_ref()).expect("module name conversion failed");
        let name = ffi::CString::new(name.as_ref()).expect("variable name conversion failed");
        unsafe {
            wren_sys::wrenGetVariable(self.vm, module.as_ptr(), name.as_ptr(), slot as raw::c_int)
        }
    }

    pub fn set_slot_new_list(&self, slot: SlotId) {
        unsafe {
            wren_sys::wrenSetSlotNewList(self.vm, slot as raw::c_int)
        }
    }

    pub fn insert_in_list(&self, list_slot: SlotId, index: i32, element_slot: SlotId) {
        unsafe {
            wren_sys::wrenInsertInList(
                self.vm, 
                list_slot as raw::c_int,
                index as raw::c_int,
                element_slot as raw::c_int
            )
        }
    }

    pub fn get_list_element(&self, list_slot: SlotId, index: i32, element_slot: SlotId) {
        unsafe {
            wren_sys::wrenGetListElement(
                self.vm, 
                list_slot as raw::c_int,
                index as raw::c_int,
                element_slot as raw::c_int
            )
        }
    }

    pub fn get_list_count(&self, slot: SlotId) -> usize {
        unsafe {
            wren_sys::wrenGetListCount(self.vm, slot as raw::c_int) as usize
        }
    }

    pub fn get_slot_foreign<T: 'static + ClassObject>(&self, slot: SlotId) -> Option<&T> {
        self.get_slot_foreign_mut(slot).map(|mr| &*mr)
    }

    pub fn get_slot_foreign_mut<T: 'static + ClassObject>(&self, slot: SlotId) -> Option<&mut T> {
        unsafe {
            let ptr = wren_sys::wrenGetSlotForeign(self.vm, slot as raw::c_int);
            if !ptr.is_null() {
                let fo = &mut *(ptr as *mut ForeignObject<T>);
                if fo.type_id == any::TypeId::of::<T>() {
                    // Safe to downcast
                    fo.object.as_mut()
                } else {
                    // Incorrect type, unsafe to downcast
                    None
                }
            } else {
                None
            }
        }
    }

    /// Looks up the specifed [module] for the specified [class]
    /// If it's type matches with type T, will create a new instance in [slot]
    ///  
    /// WARNING: This *will* overwrite slot 0, so be careful.
    pub fn set_slot_new_foreign<M: AsRef<str>, C: AsRef<str>, T: 'static + ClassObject>(&self, module: M, class: C, object: T, slot: SlotId) 
        -> Result<&mut T, ForeignSendError> 
    {
        let conf = unsafe { &mut *(wren_sys::wrenGetUserData(self.vm) as *mut UserData) };

        self.ensure_slots((slot + 1) as usize);
        // Even if slot == 0, we can just load the class into slot 0, then use wrenSetSlotNewForeign to "create" a new object
        match conf.library.as_ref().and_then(|lib| lib.get_foreign_class(module.as_ref(), class.as_ref())) {
            None => Err(ForeignSendError::NoForeignClass), // Couldn't find the corresponding class
            Some(runtime_class) => {
                if runtime_class.type_id == any::TypeId::of::<T>() {
                    // The Wren foreign class corresponds with this real object.
                    // We can coerce it and treat this object as that class, even if not instantiated by Wren.

                    // Create the new ForeignObject
                    let new_obj = ForeignObject {
                        object: Box::into_raw(Box::new(object)),
                        type_id: any::TypeId::of::<T>(),
                    };

                    // Load the Wren class object into slot 0.
                    self.get_variable(module, class, 0);

                    // Make sure the class isn't null (undeclared in Wren code)
                    match self.get_slot_type(0) {
                        SlotType::Null => Err(ForeignSendError::NoWrenClass), // You haven't declared the foreign class to Wren
                        SlotType::Unknown => unsafe { // A Wren class
                            // Create the Wren foreign pointer
                            let wptr = wren_sys::wrenSetSlotNewForeign(self.vm, slot as raw::c_int, 0, mem::size_of::<ForeignObject<T>>() as wren_sys::size_t);

                            // Move the ForeignObject into the pointer
                            std::ptr::write(wptr as *mut _, new_obj);

                            // Reinterpret the pointer as an object if we were successful
                            match (wptr as *mut ForeignObject<T>).as_mut() {
                                Some(ptr) => Ok(ptr.object.as_mut().unwrap()),
                                None => Err(ForeignSendError::NoMemory)
                            }
                        },
                        _ => Err(ForeignSendError::NoWrenClass)
                    }
                } else {
                    // The classes do not match. Avoid.
                    Err(ForeignSendError::ClassMismatch)
                }
            }
        }
    }

    fn make_call_handle<'b>(vm: *mut WrenVM, signature: FunctionSignature) -> Rc<FunctionHandle<'b>> {
        let signature = ffi::CString::new(signature.as_wren_string()).expect("signature conversion failed");
        Rc::new(FunctionHandle(Handle {
            handle: unsafe {
                wren_sys::wrenMakeCallHandle(vm, signature.as_ptr())
            },
            wvm: vm,
            vm: marker::PhantomData
        }))
    }

    pub fn abort_fiber(&self, slot: SlotId) {
        unsafe {
            wren_sys::wrenAbortFiber(self.vm, slot as raw::c_int)
        }
    }
}

impl Drop for VM {
    fn drop(&mut self) {
        unsafe {
            let conf = wren_sys::wrenGetUserData(self.vm);
            let _: Box<UserData> = Box::from_raw(conf as *mut _); // Drop the userdata
            wren_sys::wrenFreeVM(self.vm);
        }
    }
}
