use common::bytesrepr::{deserialize, BytesRepr, Error as BytesReprError};
use common::key::Key;
use common::value::Value;
use storage::{Error as StorageError, ExecutionEffect, GlobalState, TrackingCopy};
use wasmi::memory_units::Pages;
use wasmi::{
    Error as InterpreterError, Externals, FuncInstance, FuncRef, HostError, ImportsBuilder,
    MemoryDescriptor, MemoryInstance, MemoryRef, ModuleImportResolver, ModuleInstance, ModuleRef,
    RuntimeArgs, RuntimeValue, Signature, Trap, ValueType,
};

use argsparser::Args;
use parity_wasm::elements::{Error as ParityWasmError, Module};
use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt;

#[derive(Debug)]
pub enum Error {
    Interpreter(InterpreterError),
    Storage(StorageError),
    BytesRepr(BytesReprError),
    ValueTypeSizeMismatch { value_type: u32, value_size: usize },
    ForgedReference(Key),
    NoImportedMemory,
    ArgIndexOutOfBounds(usize),
    FunctionNotFound(String),
    ParityWasm(ParityWasmError),
    Ret,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl From<ParityWasmError> for Error {
    fn from(e: ParityWasmError) -> Self {
        Error::ParityWasm(e)
    }
}

impl From<InterpreterError> for Error {
    fn from(e: InterpreterError) -> Self {
        Error::Interpreter(e)
    }
}

impl From<StorageError> for Error {
    fn from(e: StorageError) -> Self {
        Error::Storage(e)
    }
}

impl From<BytesReprError> for Error {
    fn from(e: BytesReprError) -> Self {
        Error::BytesRepr(e)
    }
}

impl HostError for Error {}

pub struct Runtime<'a, T: TrackingCopy + 'a> {
    args: Vec<Vec<u8>>,
    memory: MemoryRef,
    known_urefs: HashSet<Key>,
    state: &'a mut T,
    module: Module,
    result: Vec<u8>,
    host_buf: Vec<u8>,
}

impl<'a, T: TrackingCopy + 'a> Runtime<'a, T> {
    fn effect(&self) -> ExecutionEffect {
        self.state.effect()
    }

    fn key_from_mem(&mut self, key_ptr: u32, key_size: u32) -> Result<Key, Error> {
        let bytes = self.memory.get(key_ptr, key_size as usize)?;
        deserialize(&bytes).map_err(|e| e.into())
    }

    fn value_from_mem(&mut self, value_ptr: u32, value_size: u32) -> Result<Value, Error> {
        let bytes = self.memory.get(value_ptr, value_size as usize)?;
        deserialize(&bytes).map_err(|e| e.into())
    }

    fn string_from_mem(&mut self, ptr: u32, size: u32) -> Result<String, Trap> {
        let bytes = self
            .memory
            .get(ptr, size as usize)
            .map_err(|e| Error::Interpreter(e))?;
        deserialize(&bytes).map_err(|e| Error::BytesRepr(e).into())
    }

    fn rename_export_to_call(module: &mut Module, name: String) {
        let main_export = module
            .export_section_mut()
            .unwrap()
            .entries_mut()
            .into_iter()
            .find(|e| e.field() == name)
            .unwrap()
            .field_mut();
        main_export.clear();
        main_export.push_str("call");
    }

    fn get_function_by_name(&mut self, name_ptr: u32, name_size: u32) -> Result<Vec<u8>, Trap> {
        let name = self.string_from_mem(name_ptr, name_size)?;

        let has_name: bool = self
            .module
            .export_section()
            .and_then(|es| es.entries().iter().find(|e| e.field() == name))
            .is_some();

        if has_name {
            let mut module = self.module.clone();
            //We only want the function exported under `name` to be callable;
            //`optimize` removes all code that is not reachable from the exports
            //listed in the second argument.
            let _ = pwasm_utils::optimize(&mut module, vec![&name]).unwrap();
            Self::rename_export_to_call(&mut module, name);

            parity_wasm::serialize(module).map_err(|e| Error::ParityWasm(e).into())
        } else {
            Err(Error::FunctionNotFound(name).into())
        }
    }

    fn kv_from_mem(
        &mut self,
        key_ptr: u32,
        key_size: u32,
        value_ptr: u32,
        value_size: u32,
    ) -> Result<(Key, Value), Error> {
        let key = self.key_from_mem(key_ptr, key_size)?;
        let value = self.value_from_mem(value_ptr, value_size)?;
        Ok((key, value))
    }

    //Load the i-th argument invoked as part of a `sub_call` into
    //the runtime buffer so that a subsequent `get_arg` can return it
    //to the caller.
    pub fn load_arg(&mut self, i: usize) -> Result<usize, Trap> {
        if i < self.args.len() {
            self.host_buf = self.args[i].clone();
            Ok(self.host_buf.len())
        } else {
            Err(Error::ArgIndexOutOfBounds(i).into())
        }
    }

    //Load the ith uref into the wasm memory
    pub fn get_uref(&mut self, i: usize, dest_ptr: u32) -> Result<(), Trap> {
        //FIX-ME: obviously travsersing the set in an arbitary order is bad.
        //This will make more sense when we use human-readable names and a Map.
        let uref_bytes = self.known_urefs.iter().nth(i).unwrap().to_bytes();
        
        self.memory
            .set(dest_ptr, &uref_bytes)
            .map_err(|e| Error::Interpreter(e).into())
    }

    pub fn set_mem_from_buf(&mut self, dest_ptr: u32) -> Result<(), Trap> {
        self.memory
            .set(dest_ptr, &self.host_buf)
            .map_err(|e| Error::Interpreter(e).into())
    }

    //Return a some bytes from the memory and terminate the current `sub_call`.
    //Note that the return type is `Trap`, indicating that this function will
    //always kill the current wasm instance.
    pub fn ret(&mut self, value_ptr: u32, value_size: usize) -> Trap {
        let mem_get = self.memory.get(value_ptr, value_size);
        match mem_get {
            Ok(buf) => {
                //Set the result field in the runtime and return
                //the proper element of the `Error` enum indicating
                //that the reason for exiting the module was a call to ret.
                self.result = buf;
                Error::Ret.into()
            }
            Err(e) => Error::Interpreter(e).into(),
        }
    }

    fn call_contract(
        &mut self,
        fn_ptr: u32,
        fn_size: usize,
        args_ptr: u32,
        args_size: usize,
        refs_ptr: u32,
        refs_size: usize,
    ) -> Result<usize, Error> {
        let fn_bytes = self.memory.get(fn_ptr, fn_size)?;
        let args_bytes = self.memory.get(args_ptr, args_size)?;
        let refs_bytes = self.memory.get(refs_ptr, refs_size)?;

        let args: Vec<Vec<u8>> = deserialize(&args_bytes)?;
        let refs: Vec<Key> = deserialize(&refs_bytes)?;
        let serialized_module: Vec<u8> = deserialize(&fn_bytes)?;
        let module = parity_wasm::deserialize_buffer(&serialized_module)?;

        let result = sub_call(module, args, refs, self)?;
        self.host_buf = result;
        Ok(self.host_buf.len())
    }

    pub fn serialize_function(&mut self, name_ptr: u32, name_size: u32) -> Result<usize, Trap> {
        let fn_bytes = self.get_function_by_name(name_ptr, name_size)?;
        self.host_buf = fn_bytes;
        Ok(self.host_buf.len())
    }

    pub fn write(
        &mut self,
        key_ptr: u32,
        key_size: u32,
        value_ptr: u32,
        value_size: u32,
    ) -> Result<(), Trap> {
        let (key, value) = self.kv_from_mem(key_ptr, key_size, value_ptr, value_size)?;
        self.state.write(key, value).map_err(|e| e.into())
    }

    pub fn add(
        &mut self,
        key_ptr: u32,
        key_size: u32,
        value_ptr: u32,
        value_size: u32,
    ) -> Result<(), Trap> {
        let (key, value) = self.kv_from_mem(key_ptr, key_size, value_ptr, value_size)?;
        self.state.add(key, value).map_err(|e| e.into())
    }

    fn value_from_key(&mut self, key_ptr: u32, key_size: u32) -> Result<&Value, Trap> {
        let key = self.key_from_mem(key_ptr, key_size)?;
        self.state.read(key).map_err(|e| e.into())
    }

    pub fn read_value(&mut self, key_ptr: u32, key_size: u32) -> Result<usize, Trap> {
        let value_bytes = {
            let value = self.value_from_key(key_ptr, key_size)?;
            value.to_bytes()
        };
        self.host_buf = value_bytes;
        Ok(self.host_buf.len())
    }

    pub fn new_uref(&mut self, key_ptr: u32) -> Result<(), Trap> {
        let key = self.state.new_uref();
        self.known_urefs.insert(key);
        self.memory
            .set(key_ptr, &key.to_bytes())
            .map_err(|e| Error::Interpreter(e).into())
    }
}

fn as_usize(u: u32) -> usize {
    u as usize
}

const WRITE_FUNC_INDEX: usize = 0;
const READ_FUNC_INDEX: usize = 1;
const ADD_FUNC_INDEX: usize = 2;
const NEW_FUNC_INDEX: usize = 3;
const GET_READ_FUNC_INDEX: usize = 4;
const SER_FN_FUNC_INDEX: usize = 5;
const GET_FN_FUNC_INDEX: usize = 6;
const LOAD_ARG_FUNC_INDEX: usize = 7;
const GET_ARG_FUNC_INDEX: usize = 8;
const RET_FUNC_INDEX: usize = 9;
const GET_CALL_RESULT_FUNC_INDEX: usize = 10;
const CALL_CONTRACT_FUNC_INDEX: usize = 11;
const GET_UREF_FUNC_INDEX: usize = 12;

impl<'a, T: TrackingCopy + 'a> Externals for Runtime<'a, T> {
    fn invoke_index(
        &mut self,
        index: usize,
        args: RuntimeArgs,
    ) -> Result<Option<RuntimeValue>, Trap> {
        match index {
            READ_FUNC_INDEX => {
                //args(0) = pointer to key in wasm memory
                //args(1) = size of key in wasm memory
                let (key_ptr, key_size) = Args::parse(args)?;
                let size = self.read_value(key_ptr, key_size)?;
                Ok(Some(RuntimeValue::I32(size as i32)))
            }

            SER_FN_FUNC_INDEX => {
                //args(0) = pointer to name in wasm memory
                //args(1) = size of name in wasm memory
                let (name_ptr, name_size) = Args::parse(args)?;
                let size = self.serialize_function(name_ptr, name_size)?;
                Ok(Some(RuntimeValue::I32(size as i32)))
            }

            WRITE_FUNC_INDEX => {
                //args(0) = pointer to key in wasm memory
                //args(1) = size of key
                //args(2) = pointer to value
                //args(3) = size of value
                let (key_ptr, key_size, value_ptr, value_size) = Args::parse(args)?;
                let _ = self.write(key_ptr, key_size, value_ptr, value_size)?;
                Ok(None)
            }

            ADD_FUNC_INDEX => {
                //args(0) = pointer to key in wasm memory
                //args(1) = size of key
                //args(2) = pointer to value
                //args(3) = size of value
                let (key_ptr, key_size, value_ptr, value_size) = Args::parse(args)?;
                let _ = self.add(key_ptr, key_size, value_ptr, value_size)?;
                Ok(None)
            }

            NEW_FUNC_INDEX => {
                //args(0) = pointer to key destination in wasm memory
                let key_ptr = Args::parse(args)?;
                let _ = self.new_uref(key_ptr)?;
                Ok(None)
            }

            GET_READ_FUNC_INDEX => {
                //args(0) = pointer to destination in wasm memory
                let dest_ptr = Args::parse(args)?;
                let _ = self.set_mem_from_buf(dest_ptr)?;
                Ok(None)
            }

            GET_FN_FUNC_INDEX => {
                //args(0) = pointer to destination in wasm memory
                let dest_ptr = Args::parse(args)?;
                let _ = self.set_mem_from_buf(dest_ptr)?;
                Ok(None)
            }

            LOAD_ARG_FUNC_INDEX => {
                //args(0) = index of host runtime arg to load
                let i = Args::parse(args)?;
                let size = self.load_arg(i)?;
                Ok(Some(RuntimeValue::I32(size as i32)))
            }

            GET_ARG_FUNC_INDEX => {
                //args(0) = pointer to destination in wasm memory
                let dest_ptr = Args::parse(args)?;
                let _ = self.set_mem_from_buf(dest_ptr)?;
                Ok(None)
            }

            RET_FUNC_INDEX => {
                //args(0) = pointer to value
                //args(1) = size of value
                let (value_ptr, value_size): (u32, u32) = Args::parse(args)?;

                Err(self.ret(value_ptr, value_size as usize))
            }

            CALL_CONTRACT_FUNC_INDEX => {
                //args(0) = pointer to serialized function in wasm memory
                //args(1) = size of function
                //args(2) = pointer to function arguments in wasm memory
                //args(3) = size of arguments
                //args(4) = pointer to function's known urefs in wasm memory
                //args(5) = size of urefs
                let (fn_ptr, fn_size, args_ptr, args_size, refs_ptr, refs_size) =
                    Args::parse(args)?;

                let size = self.call_contract(
                    fn_ptr,
                    as_usize(fn_size),
                    args_ptr,
                    as_usize(args_size),
                    refs_ptr,
                    as_usize(refs_size),
                )?;
                Ok(Some(RuntimeValue::I32(size as i32)))
            }

            GET_CALL_RESULT_FUNC_INDEX => {
                //args(0) = pointer to destination in wasm memory
                let dest_ptr = Args::parse(args)?;
                let _ = self.set_mem_from_buf(dest_ptr)?;
                Ok(None)
            }

            GET_UREF_FUNC_INDEX => {
                //args(0) = index of host runtime arg to load
                //args(1) = pointer to destination in wasm memory
                let (i, dest_ptr) = Args::parse(args)?;
                let _ = self.get_uref(as_usize(i), dest_ptr)?;
                Ok(None)
            }

            _ => panic!("unknown function index"),
        }
    }
}

pub struct RuntimeModuleImportResolver {
    memory: RefCell<Option<MemoryRef>>,
    max_memory: u32,
}

impl RuntimeModuleImportResolver {
    pub fn new() -> RuntimeModuleImportResolver {
        RuntimeModuleImportResolver {
            memory: RefCell::new(None),
            max_memory: 256,
        }
    }

    pub fn mem_ref(&self) -> Result<MemoryRef, Error> {
        let maybe_mem: &Option<MemoryRef> = &self.memory.borrow();
        match maybe_mem {
            Some(mem) => Ok(mem.clone()),
            None => Err(Error::NoImportedMemory),
        }
    }
}

impl ModuleImportResolver for RuntimeModuleImportResolver {
    fn resolve_func(
        &self,
        field_name: &str,
        _signature: &Signature,
    ) -> Result<FuncRef, InterpreterError> {
        let func_ref = match field_name {
            "read_value" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32; 2][..], Some(ValueType::I32)),
                READ_FUNC_INDEX,
            ),
            "serialize_function" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32; 2][..], Some(ValueType::I32)),
                SER_FN_FUNC_INDEX,
            ),
            "write" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32; 4][..], None),
                WRITE_FUNC_INDEX,
            ),
            "get_read" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32; 1][..], None),
                GET_READ_FUNC_INDEX,
            ),
            "get_function" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32; 1][..], None),
                GET_FN_FUNC_INDEX,
            ),
            "add" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32; 4][..], None),
                ADD_FUNC_INDEX,
            ),
            "new_uref" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32; 1][..], None),
                NEW_FUNC_INDEX,
            ),
            "load_arg" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32; 1][..], Some(ValueType::I32)),
                LOAD_ARG_FUNC_INDEX,
            ),
            "get_arg" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32; 1][..], None),
                GET_ARG_FUNC_INDEX,
            ),
            "ret" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32; 2][..], None),
                RET_FUNC_INDEX,
            ),
            "call_contract" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32; 6][..], Some(ValueType::I32)),
                CALL_CONTRACT_FUNC_INDEX,
            ),
            "get_call_result" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32; 1][..], None),
                GET_CALL_RESULT_FUNC_INDEX,
            ),
            "get_uref" => FuncInstance::alloc_host(
                Signature::new(&[ValueType::I32; 2][..], None),
                GET_UREF_FUNC_INDEX,
            ),
            _ => {
                return Err(InterpreterError::Function(format!(
                    "host module doesn't export function with name {}",
                    field_name
                )))
            }
        };
        Ok(func_ref)
    }

    fn resolve_memory(
        &self,
        field_name: &str,
        descriptor: &MemoryDescriptor,
    ) -> Result<MemoryRef, InterpreterError> {
        if field_name == "memory" {
            let effective_max = descriptor.maximum().unwrap_or(self.max_memory + 1);
            if descriptor.initial() > self.max_memory || effective_max > self.max_memory {
                Err(InterpreterError::Instantiation(
                    "Module requested too much memory".to_owned(),
                ))
            } else {
                //Note: each "page" is 64 KiB
                let mem = MemoryInstance::alloc(
                    Pages(descriptor.initial() as usize),
                    descriptor.maximum().map(|x| Pages(x as usize)),
                )?;
                *self.memory.borrow_mut() = Some(mem.clone());
                Ok(mem)
            }
        } else {
            Err(InterpreterError::Instantiation(
                "Memory imported under unknown name".to_owned(),
            ))
        }
    }
}

fn instance_and_memory(parity_module: Module) -> Result<(ModuleRef, MemoryRef), Error> {
    let module = wasmi::Module::from_parity_wasm_module(parity_module)?;
    let resolver = RuntimeModuleImportResolver::new();
    let mut imports = ImportsBuilder::new();
    imports.push_resolver("env", &resolver);
    let instance = ModuleInstance::new(&module, &imports)?.assert_no_start();

    let memory = resolver.mem_ref()?;
    Ok((instance, memory))
}

fn sub_call<T: TrackingCopy>(
    parity_module: Module,
    args: Vec<Vec<u8>>,
    refs: Vec<Key>,
    current_runtime: &mut Runtime<T>,
) -> Result<Vec<u8>, Error> {
    let (instance, memory) = instance_and_memory(parity_module.clone())?;
    let known_urefs = {
        let mut tmp: HashSet<Key> = HashSet::new();
        for r in refs.into_iter() {
            tmp.insert(r);
        }
        tmp
    };
    let mut runtime = Runtime {
        args,
        memory,
        state: current_runtime.state,
        known_urefs,
        module: parity_module,
        result: Vec::new(),
        host_buf: Vec::new(),
    };

    let result = instance.invoke_export("call", &[], &mut runtime);

    match result {
        Ok(_) => Ok(runtime.result),
        Err(e) => {
            if let Some(host_error) = e.as_host_error() {
                //If the "error" was in fact a trap caused by calling `ret` then
                //this is normal operation and we should return the value captured
                //in the Runtime result field.
                if let Error::Ret = host_error.downcast_ref::<Error>().unwrap() {
                    return Ok(runtime.result);
                }
            }
            Err(Error::Interpreter(e))
        }
    }
}

pub fn exec<T: TrackingCopy, G: GlobalState<T>>(
    parity_module: Module,
    account_addr: [u8; 20],
    gs: &G,
) -> Result<ExecutionEffect, Error> {
    let (instance, memory) = instance_and_memory(parity_module.clone())?;
    let account = gs.get(&Key::Account(account_addr))?.as_account();
    let mut state = gs.tracking_copy();
    let mut known_urefs: HashSet<Key> = HashSet::new();
    for r in account.urefs() {
        known_urefs.insert(*r);
    }
    let mut runtime = Runtime {
        args: Vec::new(),
        memory,
        state: &mut state,
        known_urefs,
        module: parity_module,
        result: Vec::new(),
        host_buf: Vec::new(),
    };
    let _ = instance.invoke_export("call", &[], &mut runtime)?;

    Ok(runtime.effect())
}
