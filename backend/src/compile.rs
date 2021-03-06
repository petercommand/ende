use std::os::raw::c_char;
use std::collections::{HashSet, HashMap};

use llvm_sys::prelude::*;
use llvm_sys::core::*;

use ast::*;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Levity {
    Boxed,
    Unboxed(i32),
}

trait ToRaw: Into<Vec<u8>> {
    fn to_raw(self) -> Result<*const c_char, Vec<String>>;
}

impl<'a> ToRaw for &'a str {
    fn to_raw(self: &'a str) -> Result<*const c_char, Vec<String>> {
        use std::error::Error;
        use std::ffi::CString;
        CString::new(self).map(|str| str.as_ptr() as *const i8)
                          .map_err(|err| vec![err.description().to_string()])
    }
}

pub type Map<'a> = HashMap<&'a str, (LLVMValueRef, Levity)>;

impl<'a> Term<'a> {
    pub fn rhs_vars(self: &'a Term<'a>) -> HashSet<&'a str> {
        use ast::Term::*;
        match *self {
            Literal(_) => HashSet::new(),
            Var(name) => {
                let mut set = HashSet::new();
                set.insert(name);
                set
            }
            Infix(ref left, _, ref right) => left.rhs_vars()
                                                 .union(&right.rhs_vars())
                                                 .cloned()
                                                 .collect(),
            Call(_, args) =>
                args.iter()
                    .map(|arg| arg.rhs_vars())
                    .fold(HashSet::new(), |l, r| l.union(&r).cloned().collect()),
            Scope(ref block) => block.rhs_vars(),
            While(ref cond, ref block) =>
                cond.rhs_vars().union(&block.rhs_vars()).cloned().collect(),
        }
    }
}

impl<'a> Statement<'a> {
    pub fn rhs_vars(self: &'a Statement<'a>,) -> HashSet<&str> {
        use ast::Statement::*;
        match *self {
            TermSemicolon(ref term) => term.rhs_vars(),
            Let(_, ref rhs) => rhs.rhs_vars(),
            LetMut(_, ref rhs) => rhs.rhs_vars(),
            Mutate(_, ref rhs) => rhs.rhs_vars(),
        }
    }
}

impl<'a> Block<'a> {
    pub fn rhs_vars(self: &'a Block<'a>) -> HashSet<&str> {
        let stmts_rhs_vars = self.stmts
                                 .iter()
                                 .map(|stmt| stmt.rhs_vars())
                                 .fold(HashSet::new(), |l, r| l.union(&r).cloned().collect());
        stmts_rhs_vars.union(&self.end.rhs_vars()).cloned().collect()
    }
}

pub trait FuncsDecl<'a> {
    fn find_funcs(self: &'a Self) -> Result<HashSet<&'a FunctionCall<'a>>, String>;
    unsafe fn decl_funcs(self: &'a Self, module: LLVMModuleRef) -> Result<(), String> {
        let funcs = try!(self.find_funcs());
        for func in funcs {
            let ret_ty = LLVMInt32Type();
            let args_ty = (&mut *vec![LLVMInt32Type(); func.arity as usize]).as_mut_ptr();
            let func_ty = LLVMFunctionType(ret_ty, args_ty, func.arity, 0);
            LLVMAddFunction(
                // Actually unnessasary clone.
                module, try!(func.name.to_raw().map_err(|err| err[0].clone())), func_ty
            );
        }
        Ok(())
    }
}

pub trait Compile<'a>: FuncsDecl<'a> {

    type Env;

    fn new_env() -> Self::Env;

    unsafe fn build(self: &'a Self,
                    module: LLVMModuleRef,
                    func: LLVMValueRef,
                    entry: LLVMBasicBlockRef,
                    builder: LLVMBuilderRef,
                    env: Self::Env) -> Result<LLVMValueRef, Vec<String>>;

    unsafe fn init_module(self: &'a Self,
                          module: LLVMModuleRef,
                          func: LLVMValueRef,
                          builder: LLVMBuilderRef) -> Result<(), Vec<String>> {
        let entry = LLVMAppendBasicBlock(func, "entry\0".as_ptr() as *const i8);
        LLVMPositionBuilderAtEnd(builder, entry);
        match self.build(module, func, entry, builder, Self::new_env()) {
            Ok(val) => {
                LLVMBuildRet(builder, val);
                Ok(())
            }
            Err(vec) => Err(vec),
        }
    }

    unsafe fn gen_module(self: &'a Self) -> Result<LLVMModuleRef, Vec<String>> {
        let name = try!("Main".to_raw());
        let module = LLVMModuleCreateWithName(name);
        let args: &mut [LLVMTypeRef] = &mut [];
        let func_ty = LLVMFunctionType(LLVMInt32Type(), args.as_mut_ptr() , 0, 0);
        let func = LLVMAddFunction(module, try!("main".to_raw()), func_ty);
        let builder = LLVMCreateBuilder();
        try!(self.decl_funcs(module).map_err(|err| vec![err]));
        try!(self.init_module(module, func, builder));
        Ok(module)
    }

}

impl<'a> FuncsDecl<'a> for Term<'a> {
    fn find_funcs(self: &'a Term<'a>) -> Result<HashSet<&'a FunctionCall<'a>>, String> {
        use ast::Term::*;
        match *self {
            Literal(_) | Var(_) => Ok(HashSet::new()),
            Infix(ref left, _, ref right) => {
                Ok(try!(left.find_funcs()).union(&try!(right.find_funcs())).cloned().collect())
            }
            Call(ref call, ref args) => {
                let mut func_calls = HashSet::new();
                let bool =
                    func_calls.iter().any(|old_call: &&FunctionCall| old_call.name == call.name);
                if bool {
                    return Err(call.name.to_string() + " is called with different parameters.")
                }
                func_calls.insert(call);
                for arg in *args {
                    func_calls = func_calls.union(&try!(arg.find_funcs())).cloned().collect();
                }
                Ok(func_calls)
            }
            Scope(ref block) => {
                Ok(try!(block.find_funcs()))
            },
            While(ref cond, ref block) => {
                Ok(try!(cond.find_funcs()).union(&try!(block.find_funcs())).cloned().collect())
            },
        }
    }
}

impl<'a> Compile<'a> for Term<'a> {

    type Env = Map<'a>;

    fn new_env() -> Self::Env { Map::new() }

    unsafe fn build(self: &'a Term<'a>,
                    module: LLVMModuleRef,
                    func: LLVMValueRef,
                    entry: LLVMBasicBlockRef,
                    builder: LLVMBuilderRef,
                    env: Self::Env) -> Result<LLVMValueRef, Vec<String>> {
        use ast::Term::*;
        // Build the instructions.
        match *self {
            Literal(i) => Ok(LLVMConstInt(LLVMIntType(32), i as u64, 0)),
            Infix(ref left, ref op, ref right) => {
                use ast::Operator::*;
                let another_env = env.clone();
                let left = try!(left.build(module, func, entry, builder, env));
                let right = try!(right.build(module, func, entry, builder, another_env));
                match *op {
                    Add => Ok(LLVMBuildAdd(
                        builder, left, right, try!("add".to_raw())
                    )),
                    Sub => Ok(LLVMBuildSub(
                        builder, left, right, try!("sub".to_raw())
                    )),
                    Mul => Ok(LLVMBuildMul(
                        builder, left, right, try!("mul".to_raw())
                    )),
                    Div => Ok(LLVMBuildSDiv(
                        builder, left, right, try!("div".to_raw())
                    )),
                }
            }
            Call(ref func_call, ref args) => {
                let llvm_func =
                    LLVMGetNamedFunction(module,
                        try!(func_call.name.to_raw())
                    );
                let results: Vec<Result<LLVMValueRef, Vec<String>>> =
                    args.iter()
                        .map(|term| term.build(module, func, entry, builder, env.clone()))
                        .collect();
                // It's really so painful.
                // Read the types of `results` and `result_args` to know what I'm doing.
                let result_args: Result<Vec<LLVMValueRef>, Vec<String>> =
                    results.iter()
                           .fold(Ok(Vec::new()),
                                 |left, right| {
                                     match *right {
                                         Ok(right_val) => match left {
                                             Ok(mut vec) => {
                                                 vec.push(right_val);
                                                 Ok(vec)
                                             }
                                             Err(err) => Err(err),
                                         },
                                         Err(ref left_errors) => match left {
                                             Ok(_) => Err(left_errors.clone()),
                                             Err(ref right_errors) => {
                                                 let mut new_vec = left_errors.clone();
                                                 new_vec.append(&mut right_errors.clone());
                                                 Err(new_vec)
                                             }
                                         }
                                     }
                                 });
                // let mut raw_args = try!(result_args).as_mut_ptr();
                // The above line makes the program segfault. Wierd.
                let mut args: Vec<LLVMValueRef> = try!(result_args);
                let raw_args = args.as_mut_ptr();
                let name = &*("call".to_string() + func_call.name);
                let value = LLVMBuildCall(builder,
                                          llvm_func,
                                          raw_args,
                                          func_call.arity,
                                          try!(name.to_raw())
                                         );
                Ok(value)
            }
            Var(ref str) => {
                match env.get(str) {
                    Some(pair) => {
                        use self::Levity::*;
                        match pair.1 {
                            Boxed => Ok(LLVMBuildLoad(
                                builder, pair.0, try!("load".to_raw())
                            )),
                            Unboxed(_) => Ok(pair.0),
                        }
                    }
                    None => Err(vec![String::from("Variable ") + str + " isn't declared yet."]),
                }
            }
            Scope(ref block) => {
                let new_env = env.clone();
                let block_result = block.build(module, func, entry, builder, Box::new(new_env));
                let block = try!(block_result);
                Ok(block)
            }
            While(ref cond, ref block) => {
                // Build the condition.
                // It has to be done first because it could mutate variables.
                let built_cond = try!(cond.build(module, func, entry, builder, env.clone()));
                // And check if the condition equals to zero.
                let zero = LLVMConstInt(LLVMIntType(32), 0, 0);
                use llvm_sys::LLVMIntPredicate::LLVMIntEQ;
                let is_zero = LLVMBuildICmp(
                    builder, LLVMIntEQ, built_cond, zero, try!("iszero".to_raw())
                );
                // Create the basic blocks.
                let loop_block = LLVMAppendBasicBlock(func, try!("loop".to_raw()));
                let after_loop = LLVMAppendBasicBlock(func, try!("afterloop".to_raw()));
                LLVMBuildCondBr(builder, is_zero, after_loop, loop_block);
                // Now go inside the loop.
                LLVMPositionBuilderAtEnd(builder, loop_block);
                // Create a new environment.
                let mut new_env = env.clone();
                // Build the phi nodes.
                for (key, pair) in &env {
                    if cond.rhs_vars().contains(key) {
                        use self::Levity::*;
                        match pair.1 {
                            Boxed => {
                                let ty = LLVMPointerType(LLVMIntType(32), 0);
                                let phi = LLVMBuildPhi(builder, ty, key.as_ptr() as *const i8);
                                let old_ptr = (&env.get(key)).unwrap().0;
                                LLVMAddIncoming(phi,
                                                [old_ptr, phi].as_mut_ptr(),
                                                [entry, loop_block].as_mut_ptr(),
                                                2);
                                new_env.insert(key, (phi, Boxed));
                            }
                            Unboxed(_) => {
                                let name = try!((*key).to_raw());
                                let phi =
                                    LLVMBuildPhi(builder, LLVMIntType(32), name);
                                let pair = *new_env.get(key).unwrap(); // Safe here.
                                LLVMAddIncoming(phi,
                                                [pair.0, phi].as_mut_ptr(),
                                                [entry, loop_block].as_mut_ptr(),
                                                2);
                                // Update the enviroment.
                                new_env.insert(key, (phi, pair.1));
                            }
                        }
                    }
                }
                try!(block.build(module, func, entry, builder, Box::new(new_env.clone())));
                // Check the condition for next iteration.
                let built_cond = try!(cond.build(module, func, entry, builder, new_env));
                let is_zero = LLVMBuildICmp(
                    builder, LLVMIntEQ, built_cond, zero, try!("iszero".to_raw())
                );
                LLVMBuildCondBr(builder, is_zero, after_loop, loop_block);
                LLVMPositionBuilderAtEnd(builder, after_loop);
                Ok(zero)
            }
        }
    }

}

impl<'a> FuncsDecl<'a> for Statement<'a> {
    fn find_funcs(self: &'a Statement<'a>) -> Result<HashSet<&'a FunctionCall<'a>>, String> {
        use ast::Statement::*;
        match *self {
            TermSemicolon(ref term) => term.find_funcs(),
            Let(_, ref rhs) => rhs.find_funcs(),
            LetMut(_, ref rhs) => rhs.find_funcs(),
            Mutate(_, ref rhs) => rhs.find_funcs(),
        }
    }
}

impl<'a> FuncsDecl<'a> for Block<'a> {
    fn find_funcs(self: &'a Block<'a>) -> Result<HashSet<&'a FunctionCall<'a>>, String> {
        let mut funcs = HashSet::new();
        for stmt in self.stmts {
            funcs = funcs.union(&try!(stmt.find_funcs())).cloned().collect()
        }
        funcs = funcs.union(&try!(self.end.find_funcs())).cloned().collect();
        Ok(funcs)
    }
}

impl<'a> Compile<'a> for Block<'a> {

    type Env = Box<Map<'a>>;

    fn new_env() ->  Self::Env { Box::new(Map::new()) }

    unsafe fn build(self: &'a Block<'a>,
                    module: LLVMModuleRef,
                    func: LLVMValueRef,
                    entry: LLVMBasicBlockRef,
                    builder: LLVMBuilderRef,
                    mut env: Self::Env) -> Result<LLVMValueRef, Vec<String>> {
        use self::Levity::*;
        use ast::Statement::*;
        for stmt in self.stmts {
            match *stmt {
                TermSemicolon(ref term) => {
                    try!(term.build(module, func, entry, builder, *env.clone()));
                }
                Let(ref lhs, ref rhs) => {
                    let value = try!(rhs.build(module, func, entry, builder, *env.clone()));
                    let content_val = LLVMConstIntGetSExtValue(value) as i32;
                    env.insert(lhs, (value, Unboxed(content_val)));
                }
                LetMut(ref lhs, ref rhs) => {
                    let alloca =
                        LLVMBuildAlloca(builder, LLVMInt32Type(), lhs.as_ptr() as *const i8);
                    let built_rhs = try!(rhs.build(module, func, entry, builder, *env.clone()));
                    LLVMBuildStore(builder, built_rhs, alloca);
                    env.insert(lhs, (alloca, Boxed));
                }
                Mutate(ref lhs, ref rhs) => {
                    let var_result = match env.get(lhs) {
                        Some(var) => Ok(*var),
                        None => Err(
                            vec![String::from("Variable ") + lhs + " isn't declared yet."]
                        ),
                    };
                    let built_rhs = try!(rhs.build(module, func, entry, builder, *env.clone()));
                    let pair = try!(var_result);
                    match pair.1 {
                        Boxed => { LLVMBuildStore(builder, built_rhs, pair.0); }
                        Unboxed(_) =>
                            return Err(vec![String::from("Variable ") +
                                            lhs + " is immutable, so it cannot be mutated."]),
                    }
                }
            }
        }
        self.end.build(module, func, entry, builder, *env)
    }
}

// Doesn't work right now. Will try to fix.
pub unsafe fn emit_obj(module: LLVMModuleRef) {
    use llvm_sys::target::*;
    use llvm_sys::target_machine::*;
    let triple = LLVMGetDefaultTargetTriple();
    LLVM_InitializeNativeTarget();
    let target = LLVMGetFirstTarget();
    let cpu = "x86-64\0".as_ptr() as *const i8;
    let feature = "\0".as_ptr() as *const i8;
    let opt_level = LLVMCodeGenOptLevel::LLVMCodeGenLevelNone;
    let reloc_mode = LLVMRelocMode::LLVMRelocDefault;
    let code_model = LLVMCodeModel::LLVMCodeModelDefault;
    let target_machine =
        LLVMCreateTargetMachine(target, triple, cpu, feature, opt_level, reloc_mode, code_model);
    let file_type = LLVMCodeGenFileType::LLVMObjectFile;
    // TODO: error handling here.
    LLVMTargetMachineEmitToFile(target_machine,
                                module,
                                "/Users/andyshiue/Desktop/main.o".to_raw().unwrap() as *mut i8,
                                file_type,
                                ["Cannot init_module file.\0".as_ptr()] // This is wrong.
                                    .as_mut_ptr() as *mut *mut i8);
}


pub unsafe fn emit_ir(module: LLVMModuleRef) {
    use llvm_sys::bit_writer::*;
    LLVMWriteBitcodeToFile(module, "/Users/andyshiue/Desktop/main.bc".to_raw().unwrap());
}
